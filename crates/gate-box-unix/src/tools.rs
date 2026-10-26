//! The repo's dotnet local tools, restored before the command of a routed run, as the Windows box restores them: the
//! manifests `dotnet` would read for the place folder, hashed together and compared with the work folder's
//! `<work>.tools` record, so `dotnet tool restore` runs in the place folder only when a manifest changed. The restore
//! runs along the login shell's `PATH` as the leader of a process group of its own, recorded in `<work>.tools.pgid` so
//! `box stop` ends it, both its streams forwarded like the command's, with no deadline; a client gone ends it. A restore
//! that fails or cannot start refuses the run with status 75 and the `tools` tag, so the PC runs the command itself.

use std::cell::Cell;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gate_wire::{Refusal, refused_line, tools_line};
use sha2::{Digest, Sha256};

use crate::command::signals_reset;
use crate::console::{Forwarding, GAT, LineSink, Output, Stamp, Stream};
use crate::files::{self, HeldLock};
use crate::git::CLOSED_STATUS;
use crate::lines::{Along, FAILED_STATUS};
use crate::process::{self, EndedBy, Ending, Forward, GroupRecord, NotRan, Watch, run_watched};
use crate::run::{REFUSED_STATUS, Request, Unready};

/// The manifest names `dotnet` reads in one folder, the one under `.config` first, as `dotnet` looks for them.
const MANIFEST_NAMES: [&str; 2] = [".config/dotnet-tools.json", "dotnet-tools.json"];
/// How long the restore's group has, once asked to end with `SIGTERM`, before `SIGKILL`: the command's grace.
const END_GRACE: Duration = Duration::from_secs(2);

/// The manifests `dotnet` would read for `place`: from `place` up to the work folder `work`, never above it, each
/// folder's `.config/dotnet-tools.json` else its `dotnet-tools.json`, the folder itself first. `place` is at or below
/// `work`, both absolute.
fn manifests(work: &Path, place: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut here = Some(place);
    while let Some(folder) = here.filter(|folder| folder.starts_with(work)) {
        if let Some(found) = MANIFEST_NAMES.iter().map(|name| folder.join(name)).find(|path| path.is_file()) {
            paths.push(found);
        }
        if folder == work {
            break;
        }
        here = folder.parent();
    }
    paths
}

/// The work folder `work` and the manifests `dotnet` would read for `place`, both resolved first, every link on their
/// way followed: `dotnet` reads from its physical working folder up. Else why a folder could not be resolved.
fn walk(work: &Path, place: &Path) -> io::Result<(PathBuf, Vec<PathBuf>)> {
    let (work, place) = (fs::canonicalize(work)?, fs::canonicalize(place)?);
    let paths = manifests(&work, &place);
    Ok((work, paths))
}

/// The group record of the restore in the work folder `work`, `<work>.tools.pgid`, which `box stop` reads and the
/// next run looks at before its first git step.
pub(crate) fn tools_group_record(work: &str) -> String {
    format!("{work}.tools.pgid")
}

/// The one hash of a walk's manifests: each manifest's path relative to `work`, `/`-separated, and its bytes, in walk
/// order, each followed by a NUL byte, hashed with SHA-256 and written as lower-case hex. A change to any manifest, or
/// to which manifests the walk finds, changes it. The record is the box's own, so its text need not match the Windows
/// box's. Else the line saying which manifest could not be read.
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
        hasher.update(relative.as_os_str().as_bytes());
        hasher.update([0_u8]);
        let content = fs::read(path).map_err(|error| format!("gate: remote could not read the tool manifest {}: {error}", path.display()))?;
        hasher.update(&content);
        hasher.update([0_u8]);
    }
    Ok(hasher.finalize().iter().fold(String::with_capacity(64), |mut hex, byte| {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
        hex
    }))
}

/// How running the restore ended.
enum Restored {
    /// It exited with this status.
    Exited(i32),
    /// A signal ended it, a `box stop`'s among them.
    Signalled(i32),
    /// It could not be started, for this.
    NotStarted(String),
    /// The run ends with this status, its lines said: the client is gone, or the box could not see the restore to its
    /// end.
    Ended(u8),
}

/// Restores the work folder's dotnet tools before the command runs, under the work lock `lock`: the manifests `dotnet`
/// would read for `place` ([`manifests`]), hashed ([`hash`]) and compared with the record `<work>.tools`, so nothing
/// runs when there is no manifest or the record already holds the hash; else `dotnet tool restore` runs in `place`,
/// along `along` ([`restored`]), and its hash is written to the record ([`files::replace_plain`]) once it
/// exits 0. Its lines are said through `output`.
///
/// # Errors
///
/// How the run ends, its lines said: refused when the restore exited nonzero or could not start, after the failed
/// line, the `tools` tag and [`tools_line`], said together; status 1 when a manifest could not be read, the record
/// could not be written, a signal ended the restore, or the box could not see it to its end; 255 when the client
/// closed the connection during it.
pub(crate) fn update<S: LineSink + 'static>(
    request: &Request<'_>,
    place: &Path,
    lock: &HeldLock,
    along: Along<'_>,
    output: &Arc<Output<S>>,
) -> Result<(), Unready> {
    // Bounded by the main repo's tree: the work folder itself on a run of one repo, `<work>/<leaf>` on a pair run.
    let (work, paths) = walk(Path::new(request.tree()), place).map_err(|error| {
        let line = format!(
            "gate: remote could not resolve the work folder {} or the folder {}: {error}",
            request.tree(),
            place.display()
        );
        Unready::Ended(said(output, &[&line], FAILED_STATUS))
    })?;
    if paths.is_empty() {
        return Ok(());
    }
    let digest = hash(&paths, &work).map_err(|line| Unready::Ended(said(output, &[&line], FAILED_STATUS)))?;
    let record = format!("{}.tools", request.work);
    match files::read_plain(Path::new(&record), files::RECORD_LIMIT) {
        // The record holds this walk's hash: the tools are the ones the command will read, so nothing runs.
        Ok(text) if text == digest.as_bytes() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            // A note: the restore runs, and its record is written anew.
            let _ = output.say(
                Stream::Err,
                GAT,
                &format!("gate: remote could not read {record}: {error}; restoring the tools"),
            );
        }
    }
    let status = match restored(request, place, lock, along, output) {
        Restored::Exited(0) => return record_written(&record, &digest, lock, output).map_err(Unready::Ended),
        Restored::Exited(status) => status,
        Restored::Signalled(signal) => {
            let line = format!("gate: remote tool restore failed (signal {signal})");
            return Err(Unready::Ended(said(output, &[&line], FAILED_STATUS)));
        }
        Restored::NotStarted(why) => {
            let line = format!("gate: remote could not start the dotnet tool restore: {why}");
            let _ = output.say(Stream::Err, GAT, &line);
            i32::from(FAILED_STATUS)
        }
        Restored::Ended(status) => return Err(Unready::Ended(status)),
    };
    let _ = output.say(Stream::Err, GAT, &format!("gate: remote tool restore failed (status {status})"));
    // Said together, so no heartbeat lands between the tag and the line it tags.
    let tag = refused_line(request.nonce, Refusal::Tools);
    said_as(output, &[(Stamp::Bare, &tag), (GAT, &tools_line(request.key))], REFUSED_STATUS);
    Err(Unready::Refused)
}

/// Writes `digest` as the whole of the record `record` while the work lock `lock` is held; a failure is said and
/// fails the run with status 1.
fn record_written<S: LineSink>(record: &str, digest: &str, lock: &HeldLock, output: &Output<S>) -> Result<(), u8> {
    let written = match lock.still_held() {
        Ok(true) => files::replace_plain(Path::new(record), digest.as_bytes()).map_err(|failure| failure.to_string()),
        Ok(false) => Err(format!("the work folder lock {} is no longer held", lock.path().display())),
        Err(error) => Err(format!("the work folder lock {} could not be looked at: {error}", lock.path().display())),
    };
    written.map_err(|why| said(output, &[&format!("gate: remote could not write {record}: {why}")], FAILED_STATUS))
}

/// Runs `dotnet tool restore` in `place`, found along `along.path` and its `TMPDIR` `along.tmp` when given, as the
/// leader of a process group of its own recorded in `<work>.tools.pgid` under the job `<job>-tools`, which `box stop`
/// reads; both its streams forwarded through `output`, no deadline, and a client gone ending its group. A restore that
/// exited or was ended by a signal is followed by a line saying how long it ran.
fn restored<S: LineSink + 'static>(request: &Request<'_>, place: &Path, lock: &HeldLock, along: Along<'_>, output: &Arc<Output<S>>) -> Restored {
    let record = GroupRecord::new(PathBuf::from(tools_group_record(request.work)), &format!("{}-tools", request.job), lock);
    let mut command = Command::new("dotnet");
    command.args(["tool", "restore"]).current_dir(place);
    process::with_path(&mut command, along.path);
    if let Some(tmp) = along.tmp {
        command.env("TMPDIR", tmp);
    }
    // SAFETY: the closure runs in the child between fork and exec and only resets the umask, signal dispositions and the
    // mask with async-signal-safe calls ([`signals_reset`]), as the command's does.
    unsafe {
        command.pre_exec(signals_reset);
    }
    // Set once the restore's readers started: a failure before it, its group record not readied included, means the
    // restore never ran, which falls back as a start that failed does.
    let ran_at_all = Cell::new(false);
    let on_started = |_group: i32| ran_at_all.set(true);
    let watch = Watch {
        cap: 0,
        deadline: None,
        ending: Ending::TermThenKill(END_GRACE),
        output: Some(Arc::clone(output) as Arc<dyn Forwarding>),
        forward: Forward::Both(GAT, GAT),
        ends_on_gone: true,
        record: Some(&record),
        check: None,
        on_started: Some(&on_started),
        on_leader_exit: None,
        processes: None,
    };
    let started = Instant::now();
    let ran = match run_watched(command, &watch) {
        Ok(ran) => ran,
        Err(NotRan::Start(error)) => return Restored::NotStarted(error.to_string()),
        Err(NotRan::Failed(why)) if !ran_at_all.get() && !output.client_gone() => return Restored::NotStarted(why),
        Err(not) if output.client_gone() => {
            drop(not);
            return Restored::Ended(CLOSED_STATUS);
        }
        Err(not) => {
            let line = format!("gate: remote could not see the dotnet tool restore to its end: {}", unran(not));
            return Restored::Ended(said(output, &[&line], FAILED_STATUS));
        }
    };
    if output.client_gone() || ran.ended_by == Some(EndedBy::Gone) {
        return Restored::Ended(CLOSED_STATUS);
    }
    // Said after the restore's own output and before any line saying it failed.
    let took = format!("gate: dotnet tool restore took {:.1} s", started.elapsed().as_secs_f64());
    let _ = output.say(Stream::Err, GAT, &took);
    match (ran.status.code(), ran.status.signal()) {
        (Some(code), _) => Restored::Exited(code),
        (None, Some(signal)) => Restored::Signalled(signal),
        (None, None) => Restored::Ended(said(
            output,
            &["gate: remote could not see the dotnet tool restore to its end: it gave no status"],
            FAILED_STATUS,
        )),
    }
}

/// Why the runner gave no exit for the restore, `not`.
fn unran(not: NotRan) -> String {
    match not {
        NotRan::Failed(why) => why,
        NotRan::Start(error) => error.to_string(),
        NotRan::TimedOut { group } => format!("its process group {group} ran past its deadline"),
        NotRan::OverCap => "it printed more than the box keeps".to_owned(),
    }
}

/// Says `lines` on stderr together, stamped `gat`, unless the client is gone; `status`.
fn said<S: LineSink>(output: &Output<S>, lines: &[&str], status: u8) -> u8 {
    let lines: Vec<(Stamp, &str)> = lines.iter().map(|line| (GAT, *line)).collect();
    said_as(output, &lines, status)
}

/// Says `lines` on stderr together, each as its stamp says, unless the client is gone; `status`.
fn said_as<S: LineSink>(output: &Output<S>, lines: &[(Stamp, &str)], status: u8) -> u8 {
    if !output.client_gone() {
        let lines: Vec<(Stream, Stamp, &str)> = lines.iter().map(|(stamp, line)| (Stream::Err, *stamp, *line)).collect();
        let _ = output.say_together(&lines);
    }
    status
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders or files must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{hash, manifests, walk};

    /// A folder for one test, removed when it ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(test: &str) -> Scratch {
            let folder = std::env::temp_dir().join(format!("gate-box-unix-tools-{}-{test}", std::process::id()));
            let _ = fs::remove_dir_all(&folder);
            fs::create_dir_all(&folder).expect("the scratch folder can be made");
            Scratch(fs::canonicalize(&folder).expect("the scratch folder resolves"))
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

    #[test]
    fn walk_goes_up_from_the_physical_folder_of_a_linked_place() {
        let scratch = Scratch::new("walk_goes_up_from_the_physical_folder_of_a_linked_place");
        let work = scratch.0.join("work");
        let physical = work.join("a").join("b").join("sub");
        fs::create_dir_all(&physical).expect("the place's physical folder is made");
        std::os::unix::fs::symlink(&physical, work.join("link")).expect("the link is made");
        let between = write(&work.join("a").join("dotnet-tools.json"), "between");
        let root = write(&work.join("dotnet-tools.json"), "root");

        let (resolved, paths) = walk(&work, &work.join("link")).expect("the folders resolve");

        assert_eq!(resolved, work);
        assert_eq!(paths, [between, root], "the walk climbs the physical folders, as dotnet does");
    }

    /// What the hash case writes at the work folder's root.
    const ROOT_MANIFEST: &str = "{a:1}";
    /// What the hash case writes in the place folder.
    const LEAF_MANIFEST: &str = "{b:2}";
    /// The SHA-256 of the two manifests above, in walk order, each preceded by its `/`-separated path relative to the
    /// work folder and a NUL byte and followed by a NUL byte, as lower-case hex. Pinned byte for byte, so a change to
    /// the layout fails this test.
    const TWO_MANIFEST_HASH: &str = "0c781a7292327af9970c10d24fe75d73770bbe42c2f0a381d59f93d94909fa1a";

    #[test]
    fn hash_layout_is_relative_path_nul_bytes_nul() {
        let scratch = Scratch::new("hash_layout_is_relative_path_nul_bytes_nul");
        let work = scratch.0.join("work");
        let place = work.join("src");
        write(&work.join(".config").join("dotnet-tools.json"), ROOT_MANIFEST);
        write(&place.join("dotnet-tools.json"), LEAF_MANIFEST);
        let walked = manifests(&work, &place);

        assert_eq!(walked, [place.join("dotnet-tools.json"), work.join(".config").join("dotnet-tools.json")]);
        assert_eq!(
            hash(&walked, &work),
            Ok(TWO_MANIFEST_HASH.to_owned()),
            "the path, a NUL, the bytes, a NUL"
        );
    }

    #[test]
    fn hash_reports_a_manifest_it_cannot_read() {
        let scratch = Scratch::new("hash_reports_a_manifest_it_cannot_read");
        let work = scratch.0.join("work");
        // A folder where a manifest's file is expected: `fs::read` refuses a folder, which the walk itself never adds.
        let folder = work.join("dotnet-tools.json");
        fs::create_dir_all(&folder).expect("the folder can be made");

        let said = hash(std::slice::from_ref(&folder), &work).expect_err("a manifest that cannot be read is an error");
        assert!(
            said.starts_with(&format!("gate: remote could not read the tool manifest {}: ", folder.display())),
            "{said}"
        );
    }
}
