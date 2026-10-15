//! The repo's dotnet local tools, restored before the command of a routed run: the manifests `dotnet` would read for
//! the place folder, hashed together and compared with the work folder's `<work>.tools` record, so `dotnet tool
//! restore` runs in the place folder only when a manifest changed. The restore runs in a job of its own that ends every
//! process in it when the box's handle to it closes, so a box that dies or is killed takes the restore with it; its
//! two streams are forwarded like the command's. It ends when it ends, when a stop sets the run's stop signal, or when
//! a write to the box's own output fails, the client's connection gone; its job is terminated however it ends. A
//! restore that fails refuses the run with status 75, as the PowerShell wrapper's `Update-ToolRestore` does, so the
//! gate falls back to a local run.

use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::time::Instant;

use sha2::{Digest, Sha256};

use gate_win32::{Job, PipedConsole, PipedLaunch, PipedSuspended};

use crate::boxside::forward::{DRAIN, Forwarders};
use crate::boxside::payload::ordinal_upper;
use crate::boxside::preflight::locate;
use crate::boxside::run::{
    Beat, FAILED_STATUS, FILE_FLAG_OPEN_REPARSE_POINT, REFUSED_STATUS, Request, Stopper, WaitEnd, Watched, Watching, before_the_job, end_job,
    said_stopped,
};
use crate::boxside::stop::{restore_job_name, terminate_and_wait};
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::{Refusal, refused_line};
use crate::run::{ERROR_FILE_NOT_FOUND, Written, written};

/// The manifest names `dotnet` reads in one folder, the one under `.config` first, as `dotnet` looks for them.
const MANIFEST_NAMES: [&str; 2] = [r".config\dotnet-tools.json", "dotnet-tools.json"];
/// The words of the restore the box runs, after the `dotnet` it found.
const RESTORE_WORDS: [&str; 2] = ["tool", "restore"];
/// The hex digit of each hash byte's high nibble, the low nibble reading from the same table.
const HEX: &[u8; 16] = b"0123456789abcdef";
/// What the box does instead when it cannot read the stop signal just before it starts the restore.
const STARTING_RESTORE: &str = "starting the restore";

/// The line the box says when the work folder `key` could not restore its dotnet tools, just after its refusal tag.
pub(crate) fn tools_line(key: &str) -> String {
    format!("gate: remote work folder {key} could not restore its dotnet tools")
}

/// The manifests `dotnet` would read for `place`: from `place` up to the work folder's root, never above it, each
/// folder's `.config\dotnet-tools.json` else its `dotnet-tools.json`, the folder itself first. Both are full paths and
/// `place` is at or below `work`, as the wrapper's `Get-ToolManifestPath` leaves them.
fn manifests(work: &Path, place: &Path) -> Vec<PathBuf> {
    let work_text = ordinal_upper(&work.to_string_lossy());
    let below = format!("{work_text}\\");
    let mut here = place.to_owned();
    let mut paths = Vec::new();
    loop {
        let here_text = ordinal_upper(&here.to_string_lossy());
        if here_text != work_text && !here_text.starts_with(&below) {
            break;
        }
        for name in MANIFEST_NAMES {
            let path = here.join(name);
            if path.is_file() {
                paths.push(path);
                break;
            }
        }
        if here_text == work_text {
            break;
        }
        here = match here.parent() {
            Some(parent) => parent.to_owned(),
            None => break,
        };
    }
    paths
}

/// The one hash of a walk's manifests, as the wrapper's `Get-ToolManifestHash` computes it: each manifest's path
/// relative to `work` — backslashes, the case of `place`'s spelling — and its bytes, in walk order, each followed by a
/// NUL byte, hashed with SHA-256 and written as lower-case hex. A change to any manifest, or to which manifests the
/// walk finds, changes it.
fn hash(paths: &[PathBuf], work: &Path) -> Result<String, String> {
    let mut hasher = Sha256::new();
    for path in paths {
        let relative = path.strip_prefix(work).map_err(|_| {
            format!(
                "gate: remote could not place the tool manifest {} under the work folder {}",
                path.display(),
                work.display()
            )
        })?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0_u8]);
        let content = fs::read(path).map_err(|error| format!("gate: remote could not read the tool manifest {}: {error}", path.display()))?;
        hasher.update(&content);
        hasher.update([0_u8]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for &byte in &digest {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(hex)
}

/// The text of the record `record` beside the work folder, `<Work>.tools` or `<Work>.layout`, `Ok(None)` when there is
/// none: opened without following a link there and checked a plain file, as the work folder's lock is opened, so a
/// record that is a link, a folder or a file with more than one name is not read.
pub(super) fn read_record(record: &str) -> io::Result<Option<String>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(record)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    gate_win32::check_plain_file(&file)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

/// Writes `digest` at the record `<Work>.tools`; a failure is said and fails the run with status 1.
fn write_record(record: &str, digest: &str, console: &mut Console) -> Result<(), u32> {
    if let Err(error) = write_record_file(record, digest) {
        console.err(&format!("gate: remote could not write {record}: {error}"));
        return Err(FAILED_STATUS);
    }
    Ok(())
}

/// Writes `text` as the whole of `record`, without following a link there and only into a plain file, as the work
/// folder's lock is opened. The file is opened without truncating it and emptied only once it is checked, so a record
/// that is another file's second name is refused with that file's bytes untouched.
pub(super) fn write_record_file(record: &str, text: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(record)?;
    gate_win32::check_plain_file(&file)?;
    file.set_len(0)?;
    file.write_all(text.as_bytes())
}

/// How running the restore ended.
enum Restored {
    /// The restore ran to its end with this status.
    Status(i32),
    /// The run ended while the restore ran — a stop before or during it, or a client that closed the connection — with
    /// the status to answer, its line already said. No record is written.
    Ended(u32),
}

/// The launch of `dotnet tool restore` in `place`, the `dotnet` found on the box's `PATH` and the box's own
/// environment given; else why it cannot be launched.
fn restore_launch(place: &Path) -> Result<PipedLaunch, String> {
    let dotnet = locate("dotnet")?.ok_or_else(|| gate_win32::win32_message(ERROR_FILE_NOT_FOUND))?;
    let variables = Variables::from_process();
    let environment = variables.block().map_err(|error| format!("cannot build its environment: {error}"))?;
    let words: Vec<OsString> = RESTORE_WORDS.into_iter().map(OsString::from).collect();
    match written(&dotnet, &words, &variables) {
        Written::Line(application, command_line) => Ok(PipedLaunch {
            application,
            command_line,
            directory: place.to_owned(),
            environment,
            console: PipedConsole::Hidden,
            token: None,
        }),
        Written::Refused(line) | Written::NoShell(line) => Err(line),
    }
}

/// Runs `dotnet tool restore` in `place`, in a job of its own named after the run's job ([`restore_job_name`]), its two
/// streams forwarded through the box's output lock as the command's are, and sees it to its end: [`Restored::Status`]
/// with the status it exited with, after a line saying how long it ran, or [`Restored::Ended`] when a stop set the run's
/// signal before or while it ran, when the box could not wait for it, or when a write to the box's own output failed,
/// the client's connection gone. `Err` carries the message of a restore that could not be started. Its job kills on
/// close, so a box that dies or is killed ends it, and is terminated on every way out of here, so nothing dotnet
/// started outlives the restore.
fn restore(request: &Request<'_>, place: &Path, beat: &Beat<'_>, console: &mut Console) -> Result<Restored, String> {
    // A stop that came while the snapshot was checked out ends the run here, before dotnet is even looked for.
    if let Err(status) = before_the_job(beat.stop.look(STARTING_RESTORE), request.job, console) {
        return Ok(Restored::Ended(status));
    }
    let launch = restore_launch(place)?;
    let name = restore_job_name(request.job);
    let job = Job::create_kill_on_close(&name).map_err(|error| format!("cannot create its job {name}: {error}"))?;
    let _stopper = Stopper { job: &job, armed: true };
    let started = Instant::now();
    let PipedSuspended { suspended, stdout, stderr } = launch.start_suspended(&job).map_err(|error| error.to_string())?;
    suspended.assign(&job).map_err(|error| error.to_string())?;
    let running = suspended.resume().map_err(|error| error.to_string())?;
    let forwarders = match Forwarders::start(stdout, stderr, beat.lock) {
        Ok(forwarders) => forwarders,
        Err(error) => {
            end_job(&job, console);
            return Err(format!("cannot read its output: {error}"));
        }
    };
    let watched = Watched {
        job: &job,
        running: &running,
        forwarders,
        watching: Watching::Restore,
    };
    // No ceiling watches the restore, so no kill ends its wait.
    Ok(match watched.wait(Some(&beat.stop), |_| None::<Infallible>, console) {
        WaitEnd::Failed(status) => Restored::Ended(status),
        WaitEnd::ClientGone => Restored::Ended(watched.client_gone(console)),
        WaitEnd::Exited { stopped: true, .. } => Restored::Ended(stopped(watched, console)),
        WaitEnd::Exited { code, stopped: false } => {
            let took = started.elapsed();
            let drained = watched.drained(console);
            // A process the restore left running ends here, after the line saying it held the output.
            end_job(&job, console);
            match drained {
                Ok(()) => {
                    // Said after the restore's own output and before any line saying it failed.
                    console.err(&format!("gate: dotnet tool restore took {:.1} s", took.as_secs_f64()));
                    Restored::Status(code.cast_signed())
                }
                Err(status) => Restored::Ended(status),
            }
        }
    })
}

/// Ends a restore a stop came during: its job terminated and waited for, then its readers drained and stopped, so no
/// line of the restore comes after the stop line, which is said last; status 1.
fn stopped(watched: Watched<'_>, console: &mut Console) -> u32 {
    let ending = terminate_and_wait(watched.job);
    watched.forwarders.drain(DRAIN);
    said_stopped(watched.job, ending, console)
}

/// Restores the work folder's dotnet tools before the command runs: the manifests `dotnet` would read for `place`,
/// hashed and compared with the work folder's `<work>.tools` record, so nothing runs when there is no manifest or the
/// record already holds the hash, and `dotnet tool restore` runs in `place` when it must. `Ok` when nothing ran, the
/// restore passed, or the record skipped it; else `Err` with the status the run ends on, its lines already said: 75
/// when the restore failed or could not start; 1 when a manifest could not be read, the record could not be written,
/// a stop came before or during the restore, or the box could not wait for it; 255 when the client closed the
/// connection during it.
pub(crate) fn update(request: &Request<'_>, work: &Path, place: &Path, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
    let paths = manifests(work, place);
    if paths.is_empty() {
        return Ok(());
    }
    let digest = match hash(&paths, work) {
        Ok(digest) => digest,
        Err(line) => {
            console.err(&line);
            return Err(FAILED_STATUS);
        }
    };
    let record = format!("{}.tools", request.work);
    match read_record(&record) {
        // The record holds this walk's hash: the tools are the ones the command will read, so nothing runs.
        Ok(Some(text)) if text == digest => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => console.err(&format!("gate: remote could not read {record}: {error}; restoring the tools")),
    }
    let status = match restore(request, place, beat, console) {
        Ok(Restored::Status(status)) => status,
        Ok(Restored::Ended(status)) => return Err(status),
        Err(message) => {
            console.err(&format!("gate: remote could not start the dotnet tool restore: {message}"));
            FAILED_STATUS.cast_signed()
        }
    };
    if status == 0 {
        return write_record(&record, &digest, console);
    }
    console.err(&format!("gate: remote tool restore failed (status {status})"));
    {
        // Held across both lines, so no heartbeat lands between the tag and the line it tags.
        let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
        console.err(&refused_line(request.nonce, Refusal::Tools));
        console.err(&tools_line(request.key));
    }
    Err(REFUSED_STATUS)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders or files must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{hash, manifests, tools_line};

    /// A folder for one test, removed when it ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(test: &str) -> Scratch {
            let folder = std::env::temp_dir().join(format!("gate-tools-{}-{test}", std::process::id()));
            let _ = fs::remove_dir_all(&folder);
            fs::create_dir_all(&folder).expect("the scratch folder can be made");
            Scratch(folder)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Writes `content` at `path`, making its folder; the path.
    fn write(path: &Path, content: &str) -> PathBuf {
        fs::create_dir_all(path.parent().expect("a manifest has a folder")).expect("the manifest's folder can be made");
        fs::write(path, content).expect("the manifest can be written");
        path.to_owned()
    }

    #[test]
    fn walk_prefers_config_manifest_in_each_folder() {
        let scratch = Scratch::new("walk_prefers_config_manifest_in_each_folder");
        let work = scratch.0.join("work");
        let place = work.join("src");
        write(&place.join("dotnet-tools.json"), "other");
        let config = write(&place.join(".config").join("dotnet-tools.json"), "config");
        let root = write(&work.join(".config").join("dotnet-tools.json"), "root");

        assert_eq!(
            manifests(&work, &place),
            [config, root],
            "the folder itself first, `.config` beating its sibling"
        );
    }

    #[test]
    fn walk_goes_from_place_up_to_work_and_never_above() {
        let scratch = Scratch::new("walk_goes_from_place_up_to_work_and_never_above");
        let work = scratch.0.join("work");
        let place = work.join("src").join("app");
        write(&scratch.0.join("dotnet-tools.json"), "above");
        let root = write(&work.join("dotnet-tools.json"), "root");
        let leaf = write(&place.join("dotnet-tools.json"), "leaf");

        assert_eq!(manifests(&work, &place), [leaf, root], "the manifest above the work folder is never read");
    }

    /// What the hash case writes at the work folder's root.
    const ROOT_MANIFEST: &str = "{a:1}";
    /// What the hash case writes in the place folder.
    const LEAF_MANIFEST: &str = "{b:2}";
    /// The SHA-256 of the two manifests above, in walk order, each preceded by its path relative to the work folder
    /// and a NUL byte and followed by a NUL byte, as lower-case hex. Pinned byte for byte, so a change to the layout
    /// fails this test.
    const TWO_MANIFEST_HASH: &str = "d18ab9d11f29d935ae2bb8caa7b5905850752f40fc658848796577c4c812c158";

    #[test]
    fn hash_layout_is_relative_path_nul_bytes_nul() {
        let scratch = Scratch::new("hash_layout_is_relative_path_nul_bytes_nul");
        let work = scratch.0.join("work");
        let place = work.join("src");
        write(&work.join(".config").join("dotnet-tools.json"), ROOT_MANIFEST);
        write(&place.join("dotnet-tools.json"), LEAF_MANIFEST);
        let walk = manifests(&work, &place);

        assert_eq!(walk, [place.join("dotnet-tools.json"), work.join(".config").join("dotnet-tools.json")]);
        assert_eq!(hash(&walk, &work), Ok(TWO_MANIFEST_HASH.to_owned()), "the path, a NUL, the bytes, a NUL");
    }

    #[test]
    fn hash_reports_a_manifest_it_cannot_read() {
        let scratch = Scratch::new("hash_reports_a_manifest_it_cannot_read");
        let work = scratch.0.join("work");
        // A folder where a manifest's file is expected: `fs::read` refuses a folder, which the walk itself never adds.
        let folder = work.join("dotnet-tools.json");
        fs::create_dir_all(&folder).expect("the folder can be made");

        let refused = hash(std::slice::from_ref(&folder), &work);
        let said = refused.expect_err("a manifest that cannot be read is an error");
        assert!(
            said.starts_with(&format!("gate: remote could not read the tool manifest {}: ", folder.display())),
            "{said:?}"
        );
    }

    #[test]
    fn hash_reports_a_manifest_outside_the_work_folder() {
        let scratch = Scratch::new("hash_reports_a_manifest_outside_the_work_folder");
        let work = scratch.0.join("work");
        let elsewhere = write(&scratch.0.join("dotnet-tools.json"), ROOT_MANIFEST);

        let refused = hash(std::slice::from_ref(&elsewhere), &work);
        let said = refused.expect_err("a manifest outside the work folder is an error");
        assert_eq!(
            said,
            format!(
                "gate: remote could not place the tool manifest {} under the work folder {}",
                elsewhere.display(),
                work.display()
            )
        );
    }

    #[test]
    fn tools_line_text() {
        assert_eq!(
            tools_line("app-1a2b3c4d"),
            "gate: remote work folder app-1a2b3c4d could not restore its dotnet tools"
        );
    }
}
