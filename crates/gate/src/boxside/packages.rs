//! The repo's pnpm packages, installed before a routed `pnpm` command, after the dotnet tool restore: the
//! `pnpm-lock.yaml` nearest the folder pnpm starts in, never above the main repo's tree, has its folder's package files
//! hashed and compared with that folder's line in the work folder's `<Work>.packages` record, so `pnpm install
//! --frozen-lockfile` runs there only when a package file changed or `node_modules` is gone, and only when git ignores
//! `node_modules` and no folder from the work root down to the lockfile's is a link or a junction. The install runs
//! with the command's restricted token, its `safe.directory` and `CI=true`, in a job of its own that ends every process
//! in it when the box's handle to it closes; its two streams are forwarded like the command's. It ends when it ends,
//! when a stop sets the run's stop signal, or when a write to the box's own output fails. An install that fails or
//! cannot start refuses the run with status 75, as a failed tool restore does, so the gate falls back to a local run.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::PoisonError;
use std::time::Instant;

use sha2::{Digest, Sha256};

use gate_win32::{Job, PipedConsole, PipedLaunch, PipedSuspended, RestrictedToken, Running};

use crate::boxside::forward::{DRAIN, Forwarders};
use crate::boxside::payload::{escaped, ordinal_upper};
use crate::boxside::preflight::{is_real_folder, locate};
use crate::boxside::run::{
    Beat, FAILED_STATUS, FILE_FLAG_OPEN_REPARSE_POINT, Folders, Look, REFUSED_STATUS, Request, Stopper, WaitEnd, Watched, Watching, WorkGit,
    before_the_job, end_job, full_path, said_stopped, with_safe_directory,
};
use crate::boxside::stop::{packages_job_name, terminate_and_wait};
use crate::boxside::tools::{read_record, write_record_file};
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::{Refusal, refused_line};
use crate::run::{ERROR_FILE_NOT_FOUND, Written, written};

/// The lockfile whose folder pnpm installs in.
const LOCKFILE: &str = "pnpm-lock.yaml";
/// The files of a lockfile's folder the record hashes, in hash order, each only when it exists.
const HASHED: [&str; 4] = [LOCKFILE, "package.json", "pnpm-workspace.yaml", ".npmrc"];
/// The hex digit of each hash byte's high nibble, the low nibble reading from the same table.
const HEX: &[u8; 16] = b"0123456789abcdef";
/// The words of the install the box runs, after the `pnpm` it found.
const INSTALL_WORDS: [&str; 2] = ["install", "--frozen-lockfile"];
/// The path git is asked about: with its trailing separator, so a folder pattern such as `node_modules/` matches it
/// before the folder exists.
const NODE_MODULES: &str = "node_modules/";
/// What the box does instead when it cannot read the stop signal just before it starts the install.
const STARTING_INSTALL: &str = "starting the pnpm install";

/// The line the box says when the work folder `key` could not restore its pnpm packages, just after its refusal tag.
pub(crate) fn packages_line(key: &str) -> String {
    format!("gate: remote work folder {key} could not restore its pnpm packages")
}

/// Whether the command's first word `word` names pnpm: its file name, after any folder or drive, less its last
/// extension, is `pnpm` in any case.
fn is_pnpm(word: &str) -> bool {
    let name = word.rsplit(['\\', '/']).next().unwrap_or(word);
    let name = match name.as_bytes().get(1) {
        Some(b':') => &name[2..],
        _ => name,
    };
    let stem = match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    };
    stem.eq_ignore_ascii_case("pnpm")
}

/// The folder pnpm is told to start in by `words`, the command's words after its first: the value of the last
/// `--dir <d>`, `--dir=<d>`, `-C <d>` or `-C<d>` among pnpm's own options. They end at the first word that is neither
/// an option nor the value of `--dir` or `-C` (the script, the bin or `exec`'s program, whose words follow it), at a
/// lone `--`, or at a `--dir` or `-C` without a value. None when there is none or its value is empty.
fn start_word(words: &[String]) -> Option<&str> {
    let mut named = None;
    let mut index = 0;
    while let Some(word) = words.get(index) {
        if word == "--" || !word.starts_with('-') {
            break;
        }
        let value = if word == "--dir" || word == "-C" {
            index += 1;
            match words.get(index) {
                Some(value) if value != "--" => Some(value.as_str()),
                _ => break,
            }
        } else {
            word.strip_prefix("--dir=").or_else(|| word.strip_prefix("-C"))
        };
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            named = Some(value);
        }
        index += 1;
    }
    named
}

/// Whether `path` is the full path `work` or a folder below it, names compared as `OrdinalIgnoreCase` compares them.
fn is_within(work: &Path, path: &Path) -> bool {
    let (work_text, path_text) = (ordinal_upper(&work.to_string_lossy()), ordinal_upper(&path.to_string_lossy()));
    path_text == work_text || path_text.starts_with(&format!("{work_text}\\"))
}

/// The folder the lockfile walk starts in: the folder [`start_word`] names in `words`, joined to `place` when relative
/// and folded, else `place`; `Err` with the folder when it lies outside the work root `work`.
fn start_folder(work: &Path, place: &Path, words: &[String]) -> Result<PathBuf, PathBuf> {
    let Some(named) = start_word(words) else {
        return Ok(place.to_owned());
    };
    let joined = place.join(named);
    match full_path(&joined) {
        Some(full) if is_within(work, &full) => Ok(full),
        Some(full) => Err(full),
        None => Err(joined),
    }
}

/// The folder of the first `pnpm-lock.yaml` that is a file, from `start` up to the work root `work` inclusive, never
/// above it; none when there is none. A lockfile whose metadata cannot be read for any reason but its absence is an
/// error, the line saying so.
fn lockfile_folder(work: &Path, start: &Path) -> Result<Option<PathBuf>, String> {
    let mut here = start.to_owned();
    while is_within(work, &here) {
        let path = here.join(LOCKFILE);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => return Ok(Some(here)),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(unreadable(&path, LOCKFILE, &error)),
        }
        if !here.pop() {
            break;
        }
    }
    Ok(None)
}

/// The line saying the package file `name` at `path` could not be read for `error`.
fn unreadable(path: &Path, name: &str, error: &io::Error) -> String {
    let what = if name == LOCKFILE { "the lockfile" } else { "the package file" };
    format!(
        "gate: remote could not read {what} {}: {}",
        escaped(&path.to_string_lossy()),
        escaped(&error.to_string())
    )
}

/// The first folder from the work root `work` down to `folder`, both included, that is a link or a junction rather
/// than a folder itself, relative to the work root; none when every one is a folder.
fn linked(work: &Path, folder: &Path) -> Option<String> {
    let mut here = work.to_owned();
    let below = relative(work, folder);
    let steps = if below == "." { Vec::new() } else { below.split('\\').collect() };
    if !is_real_folder(&here) {
        return Some(".".to_owned());
    }
    for step in steps {
        here.push(step);
        if !is_real_folder(&here) {
            return Some(relative(work, &here));
        }
    }
    None
}

/// The `<Work>.packages` record: for each folder whose install passed, its path relative to the work root and the hash
/// of its package files then, one `{folder}\t{hash}` line each, sorted by folder.
#[derive(Debug, Default, PartialEq, Eq)]
struct Record(BTreeMap<String, String>);

impl Record {
    /// The record of `text`, a line that is not a folder, a tab and 64 lower-case hex digits dropped.
    fn parse(text: &str) -> Record {
        let lines = text.lines().filter_map(|line| {
            let (folder, digest) = line.split_once('\t')?;
            let valid = !folder.is_empty() && digest.len() == 64 && digest.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
            valid.then(|| (folder.to_owned(), digest.to_owned()))
        });
        Record(lines.collect())
    }

    /// The line of `folder`, its name compared in any case.
    fn find(&self, folder: &str) -> Option<&String> {
        let upper = ordinal_upper(folder);
        self.0.iter().find(|(held, _)| ordinal_upper(held) == upper).map(|(_, digest)| digest)
    }

    /// Whether the line of `folder` holds `digest`.
    fn holds(&self, folder: &str, digest: &str) -> bool {
        self.find(folder).is_some_and(|held| held == digest)
    }

    /// Sets the line of `folder` to `digest`, replacing the one it had in any case, the others kept.
    fn set(&mut self, folder: &str, digest: &str) {
        let upper = ordinal_upper(folder);
        self.0.retain(|held, _| ordinal_upper(held) != upper);
        self.0.insert(folder.to_owned(), digest.to_owned());
    }

    /// The record's text, a line for each folder, each ending in LF.
    fn text(&self) -> String {
        let mut text = String::new();
        for (folder, digest) in &self.0 {
            text.push_str(folder);
            text.push('\t');
            text.push_str(digest);
            text.push('\n');
        }
        text
    }
}

/// The folder whose lockfile the command `request` runs pnpm with: none when its first word is not pnpm, when the
/// folder it starts in lies outside the work root (said), when no lockfile is found up to the work root, or when a
/// folder from the work root down to the lockfile's is a link or a junction (said). `Err` with status 1 when a
/// lockfile's metadata cannot be read, its line said.
fn lockfile_for(request: &Request<'_>, folders: &Folders<'_>, console: &mut Console) -> Result<Option<PathBuf>, u32> {
    let Some((word, rest)) = request.command().split_first() else {
        return Ok(None);
    };
    if !is_pnpm(word) {
        return Ok(None);
    }
    let start = match start_folder(folders.work, folders.place, rest) {
        Ok(start) => start,
        Err(outside) => {
            let outside = escaped(&outside.to_string_lossy());
            console.err(&format!("gate: remote pnpm install skipped: {outside} is outside the work root"));
            return Ok(None);
        }
    };
    let folder = lockfile_folder(folders.work, &start).map_err(|line| {
        console.err(&line);
        FAILED_STATUS
    })?;
    Ok(folder.filter(|folder| match linked(folders.work, folder) {
        Some(link) => {
            console.err(&format!(
                "gate: remote pnpm install skipped: {} is reached through a link",
                escaped(&link)
            ));
            false
        }
        None => true,
    }))
}

/// The hash of `folder`'s package files: for each of [`HASHED`] that exists, its name, a NUL byte, its bytes and a NUL
/// byte, hashed with SHA-256 and written as lower-case hex; else the line saying which file could not be read.
fn hash(folder: &Path) -> Result<String, String> {
    let mut hasher = Sha256::new();
    for name in HASHED {
        let path = folder.join(name);
        let content = match read_plain(&path) {
            Ok(Some(content)) => content,
            Ok(None) => continue,
            Err(error) => return Err(unreadable(&path, name, &error)),
        };
        hasher.update(name.as_bytes());
        hasher.update([0_u8]);
        hasher.update(&content);
        hasher.update([0_u8]);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(hex)
}

/// The bytes of the file at `path`, `Ok(None)` when there is none: opened without following a link there and checked a
/// plain file, as the tool manifests' record is, so a link, a folder or a file with more than one name is not read.
fn read_plain(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = match OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    gate_win32::check_plain_file(&file)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;
    Ok(Some(content))
}

/// `folder`'s path relative to the work root `work`, at or below which it lies, `.` for the root itself.
fn relative(work: &Path, folder: &Path) -> String {
    let skipped = work.to_string_lossy().chars().count();
    let rest: String = folder.to_string_lossy().chars().skip(skipped).collect();
    match rest.trim_start_matches('\\') {
        "" => ".".to_owned(),
        rest => rest.to_owned(),
    }
}

/// The record at `path`, empty when there is none; one that cannot be read is said, and read as empty.
fn read(path: &str, console: &mut Console) -> Record {
    match read_record(path) {
        Ok(Some(text)) => Record::parse(&text),
        Ok(None) => Record::default(),
        Err(error) => {
            console.err(&format!(
                "gate: remote could not read {}: {}; installing the packages",
                escaped(path),
                escaped(&error.to_string())
            ));
            Record::default()
        }
    }
}

/// Whether the install in `folder`, named `key` in `record`, can be skipped: the record holds `digest` for it and its
/// `node_modules` is a folder itself, not a link or a junction.
fn installed(record: &Record, key: &str, digest: &str, folder: &Path) -> bool {
    record.holds(key, digest) && is_real_folder(&folder.join("node_modules"))
}

/// Whether git ignores `node_modules` in the folder `key` below the work root `work`, asked with the box's own git as
/// it runs in the work tree ([`WorkGit::args`]), its repository and work tree named, never found from the folder;
/// when it does not, or cannot say, the line saying why the install is skipped is said.
fn ignored(git: &WorkGit, work: &Path, key: &str, console: &mut Console) -> bool {
    let work_text = work.to_string_lossy();
    let git_dir = format!(r"--git-dir={work_text}\.git");
    let work_tree = format!("--work-tree={work_text}");
    // `:(top)` reads the path from the work tree's top, wherever the box's own working folder is.
    let path = if key == "." {
        format!(":(top){NODE_MODULES}")
    } else {
        format!(":(top){}/{NODE_MODULES}", key.replace('\\', "/"))
    };
    let args = git.args(&[&git_dir, &work_tree, "check-ignore", "-q", &path]);
    let why = match git.git.code(&args, Stdio::null(), Stdio::null()) {
        Ok(0) => return true,
        Ok(1) => {
            console.err(&format!(
                "gate: remote pnpm install skipped: node_modules is not ignored in {}",
                escaped(key)
            ));
            return false;
        }
        Ok(status) => format!("status {status}"),
        Err(message) => escaped(&message),
    };
    console.err(&format!(
        "gate: remote could not ask git whether node_modules is ignored in {}: {why}",
        escaped(key)
    ));
    false
}

/// How running the install ended.
enum Installed {
    /// The install ran to its end with this status.
    Status(i32),
    /// The run ended while the install ran, or before it started, with the status to answer, its line already said.
    Ended(u32),
}

/// The launch of `pnpm install --frozen-lockfile` in `folder`: the `pnpm` found on the box's `PATH`, the box's own
/// environment with git's `safe.directory` set to `*` over it as the command's is ([`with_safe_directory`]) and
/// `CI=true` over that, and the restricted token the command gets; else why it cannot be launched.
fn install_launch(folder: &Path) -> Result<PipedLaunch, String> {
    let pnpm = locate("pnpm")?.ok_or_else(|| gate_win32::win32_message(ERROR_FILE_NOT_FOUND))?;
    let token = RestrictedToken::of_this_process().map_err(|error| format!("cannot make its restricted token: {error}"))?;
    let variables = with_safe_directory(Variables::from_process()).with("CI", "true");
    let environment = variables.block().map_err(|error| format!("cannot build its environment: {error}"))?;
    let words: Vec<OsString> = INSTALL_WORDS.into_iter().map(OsString::from).collect();
    match written(&pnpm, &words, &variables) {
        Written::Line(application, command_line) => Ok(PipedLaunch {
            application,
            command_line,
            directory: folder.to_owned(),
            environment,
            console: PipedConsole::Hidden,
            token: Some(token),
        }),
        Written::Refused(line) | Written::NoShell(line) => Err(line),
    }
}

/// Runs the install in `folder`, named `key`, in a job of its own named after the run's job ([`packages_job_name`]), its
/// two streams forwarded through the box's output lock, and sees it to its end: [`Installed::Status`] with its status,
/// after a line saying how long it ran, or [`Installed::Ended`] when a stop set the run's signal before or while it ran,
/// when the box could not wait for it, or when the client closed the connection. `Err` carries the message of an
/// install that could not be started. Its job kills on close and is terminated on every way out of here.
fn install(request: &Request<'_>, folder: &Path, key: &str, beat: &Beat<'_>, console: &mut Console) -> Result<Installed, String> {
    if let Err(status) = before_the_job(beat.stop.look(STARTING_INSTALL), request.job, console) {
        return Ok(Installed::Ended(status));
    }
    let launch = install_launch(folder)?;
    let name = packages_job_name(request.job);
    let job = Job::create_kill_on_close(&name).map_err(|error| format!("cannot create its job {name}: {error}"))?;
    let _stopper = Stopper { job: &job, armed: true };
    let started = Instant::now();
    let (running, forwarders) = match start(&launch, &job, beat, console)? {
        Ok(started) => started,
        Err(status) => return Ok(Installed::Ended(status)),
    };
    let watched = Watched {
        job: &job,
        running: &running,
        forwarders,
        watching: Watching::Packages,
    };
    // No ceiling watches the install, so no kill ends its wait.
    Ok(match watched.wait(Some(&beat.stop), |_| None::<Infallible>, console) {
        WaitEnd::Failed(status) => Installed::Ended(status),
        WaitEnd::ClientGone => Installed::Ended(watched.client_gone(console)),
        WaitEnd::Exited { stopped: true, .. } => Installed::Ended(stopped(watched, console)),
        WaitEnd::Exited { code, stopped: false } => {
            let took = started.elapsed();
            let drained = watched.drained(console);
            end_job(&job, console);
            match drained {
                Ok(()) => {
                    let place = if key == "." { String::new() } else { format!(" in {}", escaped(key)) };
                    console.err(&format!("gate: pnpm install took {:.1} s{place}", took.as_secs_f64()));
                    Installed::Status(code.cast_signed())
                }
                Err(status) => Installed::Ended(status),
            }
        }
    })
}

/// Starts `launch` suspended in `job`, then, unless a stop came meanwhile, resumes it and starts forwarding its two
/// streams through the box's output lock: the install running and its readers, or `Ok(Err)` with status 1 for a stop
/// that came first, its job terminated and its line said; `Err` with why it could not be started.
fn start(launch: &PipedLaunch, job: &Job, beat: &Beat<'_>, console: &mut Console) -> Result<Result<(Running, Forwarders), u32>, String> {
    let PipedSuspended { suspended, stdout, stderr } = launch.start_suspended(job).map_err(|error| error.to_string())?;
    let assigned = suspended.assign(job);
    // A stop that came while the install was created ends it before it runs, as the command's launch looks: whether
    // the install went in the job or the stopped job refused it, it never runs, and is ended as it is dropped.
    match beat.stop.look(STARTING_INSTALL) {
        Look::Stopped => return Ok(Err(said_stopped(job, terminate_and_wait(job), console))),
        Look::Go(Some(unread)) => console.err(&unread),
        Look::Go(None) => {}
    }
    let running = assigned.and_then(|()| suspended.resume()).map_err(|error| error.to_string())?;
    match Forwarders::start(stdout, stderr, beat.lock) {
        Ok(forwarders) => Ok(Ok((running, forwarders))),
        Err(error) => {
            end_job(job, console);
            Err(format!("cannot read its output: {error}"))
        }
    }
}

/// Ends an install a stop came during: its job terminated and waited for, then its readers drained, so the stop line,
/// said last, follows every line of the install; status 1.
fn stopped(watched: Watched<'_>, console: &mut Console) -> u32 {
    let ending = terminate_and_wait(watched.job);
    watched.forwarders.drain(DRAIN);
    said_stopped(watched.job, ending, console)
}

/// Writes `record` at `path`; a failure is said and fails the run with status 1.
fn write(path: &str, record: &Record, console: &mut Console) -> Result<(), u32> {
    if let Err(error) = write_record_file(path, &record.text()) {
        console.err(&format!(
            "gate: remote could not write {}: {}",
            escaped(path),
            escaped(&error.to_string())
        ));
        return Err(FAILED_STATUS);
    }
    Ok(())
}

/// Refuses the run for an install that ended with `status`: the status line, then the `tools` tag and the packages
/// line under the heartbeat's lock; status 75.
fn refused(request: &Request<'_>, status: i32, beat: &Beat<'_>, console: &mut Console) -> u32 {
    console.err(&format!("gate: remote package restore failed (status {status})"));
    // Held across both lines, so no heartbeat lands between the tag and the line it tags.
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    console.err(&refused_line(request.nonce, Refusal::Tools));
    console.err(&packages_line(request.key));
    REFUSED_STATUS
}

/// Installs the pnpm packages of a `pnpm` command before it runs: nothing for another command, without a lockfile,
/// when the record holds the lockfile folder's hash and its `node_modules` is there, or when git does not ignore
/// `node_modules` there (said); else `pnpm install --frozen-lockfile` in the lockfile's folder, its line in
/// `<Work>.packages` rewritten when it passes. `Ok` when nothing ran or the install passed; else `Err` with the status
/// the run ends on, its lines said: 75 when the install failed or could not start; 1 when a package file could not be
/// read, the record could not be written, a stop came before or during the install, or the box could not wait for it;
/// 255 when the client closed the connection during it.
pub(super) fn update(request: &Request<'_>, git: &WorkGit, folders: &Folders<'_>, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
    let Some(folder) = lockfile_for(request, folders, console)? else {
        return Ok(());
    };
    let digest = hash(&folder).map_err(|line| {
        console.err(&line);
        FAILED_STATUS
    })?;
    let key = relative(folders.work, &folder);
    let path = format!("{}.packages", request.work);
    let mut record = read(&path, console);
    if installed(&record, &key, &digest, &folder) || !ignored(git, folders.work, &key, console) {
        return Ok(());
    }
    let status = match install(request, &folder, &key, beat, console) {
        Ok(Installed::Status(status)) => status,
        Ok(Installed::Ended(status)) => return Err(status),
        Err(message) => {
            console.err(&format!("gate: remote could not start the pnpm install: {}", escaped(&message)));
            FAILED_STATUS.cast_signed()
        }
    };
    if status != 0 {
        return Err(refused(request, status, beat, console));
    }
    record.set(&key, &digest);
    write(&path, &record, console)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders or files must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{Record, hash, is_pnpm, lockfile_folder, start_folder, start_word};

    /// A folder for one test, removed when it ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(test: &str) -> Scratch {
            let folder = crate::test_support::temp_dir().join(format!("gate-packages-{}-{test}", std::process::id()));
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

    /// `words` as the owned words a request holds.
    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|&word| word.to_owned()).collect()
    }

    /// Writes an empty lockfile in `folder`, making it.
    fn lockfile(folder: &Path) {
        fs::create_dir_all(folder).expect("the lockfile's folder can be made");
        fs::write(folder.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").expect("the lockfile can be written");
    }

    /// The work root and place folders of the start-folder cases, under a drive that need not exist.
    fn root_and_place() -> (PathBuf, PathBuf) {
        (PathBuf::from(r"C:\work\app"), PathBuf::from(r"C:\work\app\web"))
    }

    #[test]
    fn a_pnpm_word_triggers_in_any_case_and_form() {
        for word in ["pnpm", "PNPM.CMD", r"C:\tools\pnpm.exe", "pnpm.ps1", "/usr/bin/pnpm", "C:pnpm.cmd"] {
            assert!(is_pnpm(word), "{word} names pnpm");
        }
        for word in ["pnpmx", "npx", "node", "corepack", "", r"C:\pnpm\node.exe", "xpnpm.cmd"] {
            assert!(!is_pnpm(word), "{word} does not name pnpm");
        }
    }

    #[test]
    fn the_last_dir_or_c_before_a_lone_dash_dash_names_the_start_folder() {
        assert_eq!(start_word(&words(&["--dir", "a", "run", "x"])), Some("a"));
        assert_eq!(start_word(&words(&["--dir=a", "install"])), Some("a"));
        assert_eq!(start_word(&words(&["-C", "a", "test"])), Some("a"));
        assert_eq!(start_word(&words(&["-Ca", "test"])), Some("a"));
        assert_eq!(start_word(&words(&["--dir", "a", "-C", "b", "run", "x"])), Some("b"), "the last wins");
        assert_eq!(
            start_word(&words(&["-Cb", "run", "--dir=c"])),
            Some("b"),
            "after the script is the script's"
        );
        assert_eq!(start_word(&words(&["vitest", "run", "--dir", "src"])), None, "a bin's own `--dir`");
        assert_eq!(
            start_word(&words(&["exec", "git", "-C", r"..\x", "status"])),
            None,
            "exec's program's `-C`"
        );
        assert_eq!(
            start_word(&words(&["--filter", "x", "--dir", "a"])),
            None,
            "another option's value ends the scan"
        );
        assert_eq!(
            start_word(&words(&["--dir", "a", "exec", "--", "--dir", "b"])),
            Some("a"),
            "after `--` is the program's"
        );
        assert_eq!(start_word(&words(&["exec", "--", "-Cb"])), None);
        assert_eq!(start_word(&words(&["run", "x", "--dir"])), None, "a trailing `--dir` has no value");
        assert_eq!(start_word(&words(&["-C", "a", "--dir"])), Some("a"), "a trailing `--dir` changes nothing");
        assert_eq!(start_word(&words(&["--dir", "--", "x"])), None, "a lone `--` is never a value");
        assert_eq!(start_word(&words(&["--dir=", "-C"])), None, "an empty value is none");
        assert_eq!(start_word(&words(&["run", "build"])), None);
        assert_eq!(start_word(&[]), None);
    }

    #[test]
    fn a_start_folder_outside_the_work_root_is_refused() {
        let (work, place) = root_and_place();
        assert_eq!(
            start_folder(&work, &place, &words(&["run", "x"])),
            Ok(place.clone()),
            "no folder named: the place"
        );
        assert_eq!(
            start_folder(&work, &place, &words(&["--dir", r"..\api"])),
            Ok(PathBuf::from(r"C:\work\app\api")),
            "relative to the place"
        );
        assert_eq!(
            start_folder(&work, &place, &words(&["-C", ".."])),
            Ok(work.clone()),
            "the work root itself"
        );
        assert_eq!(
            start_folder(&work, &place, &words(&["--dir", r"C:\WORK\APP\lib"])),
            Ok(PathBuf::from(r"C:\WORK\APP\lib")),
            "an absolute folder inside, in another case"
        );
        assert_eq!(
            start_folder(&work, &place, &words(&["--dir", r"..\..\x"])),
            Err(PathBuf::from(r"C:\work\x")),
            "climbing out of the work root"
        );
        assert_eq!(
            start_folder(&work, &place, &words(&["--dir", r"D:\elsewhere"])),
            Err(PathBuf::from(r"D:\elsewhere")),
            "an absolute folder elsewhere"
        );
        assert_eq!(
            start_folder(&work, &place, &words(&[r"--dir=C:\work\application"])),
            Err(PathBuf::from(r"C:\work\application")),
            "a sibling whose name starts with the root's is outside"
        );
    }

    #[test]
    fn the_walk_finds_the_nearest_lockfile_up_to_the_work_root() {
        let scratch = Scratch::new("the_walk_finds_the_nearest_lockfile_up_to_the_work_root");
        let work = scratch.0.join("work");
        let place = work.join("web").join("app");
        fs::create_dir_all(&place).expect("the place can be made");
        lockfile(&scratch.0);
        assert_eq!(lockfile_folder(&work, &place), Ok(None), "a lockfile above the work root is never read");

        lockfile(&work);
        assert_eq!(
            lockfile_folder(&work, &place),
            Ok(Some(work.clone())),
            "the work root's, reached from below"
        );

        lockfile(&work.join("web"));
        assert_eq!(lockfile_folder(&work, &place), Ok(Some(work.join("web"))), "the nearest parent's");

        fs::create_dir_all(place.join("pnpm-lock.yaml")).expect("a folder named as the lockfile can be made");
        assert_eq!(
            lockfile_folder(&work, &place),
            Ok(Some(work.join("web"))),
            "a folder named as the lockfile is skipped"
        );

        let leaf = work.join("leaf");
        lockfile(&leaf);
        assert_eq!(lockfile_folder(&work, &leaf), Ok(Some(leaf.clone())), "the start folder's own");
    }

    /// The SHA-256 of the four package files [`hash_layout_is_name_nul_bytes_nul`] writes, in hash order, each its
    /// name, a NUL byte, its bytes and a NUL byte, as lower-case hex; computed apart from this code, so a change to the
    /// layout fails the test.
    const FOUR_FILES_HASH: &str = "803501eb14e70316e17f89138a20bf9d4eae7d8008fce25f332918b8974ea19d";
    /// The same of the lockfile and `.npmrc` alone, the two others absent.
    const TWO_FILES_HASH: &str = "aba0ef6ce5e9c3c599a0e90e593c255d130d10113869fa0ab30fd5dd8a891675";

    #[test]
    fn hash_layout_is_name_nul_bytes_nul() {
        let scratch = Scratch::new("hash_layout_is_name_nul_bytes_nul");
        let folder = scratch.0.join("web");
        fs::create_dir_all(&folder).expect("the folder can be made");
        // Written in another order than the hash's, which the hash ignores.
        fs::write(folder.join(".npmrc"), "a=1\n").expect("the file can be written");
        fs::write(folder.join("pnpm-lock.yaml"), "lock\n").expect("the file can be written");
        assert_eq!(hash(&folder), Ok(TWO_FILES_HASH.to_owned()), "an absent file is skipped");

        fs::write(folder.join("pnpm-workspace.yaml"), "packages: []\n").expect("the file can be written");
        fs::write(folder.join("package.json"), "{}\n").expect("the file can be written");
        assert_eq!(hash(&folder), Ok(FOUR_FILES_HASH.to_owned()), "the name, a NUL, the bytes, a NUL");
    }

    #[test]
    fn the_record_keeps_other_folders_lines_sorted() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        let text = format!(
            "web\t{a}\nnot a line\n.\t{b}\nshort\tabc\nUPPER\t{}\napi\t{b}\textra\n\t{a}\n",
            "A".repeat(64)
        );
        let mut record = Record::parse(&text);

        assert!(record.holds("web", &a));
        assert!(record.holds("WEB", &a), "folders compare in any case");
        assert!(record.holds(".", &b));
        assert!(!record.holds("web", &b), "another hash");
        assert!(!record.holds("short", "abc"), "a malformed hash is dropped");
        assert!(!record.holds("UPPER", &"A".repeat(64)), "an upper-case hash is dropped");
        assert!(!record.holds("api", &b), "a line with a third field is dropped");
        assert_eq!(record.text(), format!(".\t{b}\nweb\t{a}\n"), "the valid lines, sorted");

        record.set("Web", &c);
        record.set(r"tests\browser", &a);
        assert_eq!(
            record.text(),
            format!(".\t{b}\nWeb\t{c}\ntests\\browser\t{a}\n"),
            "a folder's line replaced, a new one added, the others kept, sorted"
        );
        assert_eq!(Record::parse(""), Record::default());
        assert_eq!(Record::parse(&record.text()), record, "the text reads back");
    }
}
