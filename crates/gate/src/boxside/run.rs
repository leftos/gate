//! `gate box run`: the work folder's lock taken, the subfolder checked, the snapshot checked out, the command started
//! in the payload's job with its output forwarded, the tree it left committed back, and the command's status passed
//! whole, as the PowerShell wrapper's `Invoke-Run` does. git starts through [`gate_win32::spawn`], under the spawn lock
//! the command's own launch holds while its inheritable handle copies are open, so no git inherits a handle meant for
//! the command.
//!
//! The payload is data: it is refused unless well formed, and runs nothing but its command and writes nothing but the
//! work folder's own files. The command, and whatever it writes into the work folder, `.git` included, is the user's
//! own code running as the user, and is trusted. The box's git runs with no hooks and no file-system monitor only so
//! that a repository's own hooks or monitor, such as one a build installed, do not run in it.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use gate_win32::{Job, LaunchError, PipedLaunch, PipedSuspended, Running};

use crate::answer::USAGE_STATUS;
use crate::boxside::forward::{DRAIN, Forwarders, OutputLock};
use crate::boxside::payload::{Payload, ordinal_upper};
use crate::boxside::preflight::{Failed, GIT_REDIRECTS, Git, exit_code};
use crate::boxside::stop::{STOPPED, still_holding, terminate_and_wait};
use crate::console::Console;
use crate::environment::Variables;
use crate::run::{Written, find_program, written};

/// The status of a run refused because another run holds the work folder.
const BUSY_STATUS: u32 = 75;
/// The status of a run the box failed on its own, and of a passed command whose tree could not be committed back.
const FAILED_STATUS: u32 = 1;
/// The status of a command that could not be started.
const NOT_STARTED: u32 = 127;
/// The status of a run whose client closed the connection: the box stopped the job and exits as a lost connection does.
const CLOSED_STATUS: u32 = 255;
/// How long one wait for the command's exit lasts before the box looks again whether the client is still there.
const WAIT_STEP: Duration = Duration::from_millis(100);
/// The Win32 errors of an open refused because another process has the file open.
const SHARING_VIOLATION: i32 = 32;
const LOCK_VIOLATION: i32 = 33;
/// The Win32 error `CreateProcessW` gives when the box's own job refuses breakaway, among other refusals.
const ERROR_ACCESS_DENIED: u32 = 5;
/// `FILE_FLAG_OPEN_REPARSE_POINT`: opens a link or junction itself, never its target.
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// The identity variables the payload may set for the back commit.
const IDENTITY_NAMES: [&str; 6] = [
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
];

/// What a run reads of its payload.
struct Request<'a> {
    key: &'a str,
    bare: &'a str,
    work: &'a str,
    lock: &'a str,
    index: &'a str,
    job: &'a str,
    command: Vec<String>,
    subfolder: &'a str,
    identity: Vec<(String, String)>,
}

impl Request<'_> {
    fn read(payload: &Payload) -> Result<Request<'_>, String> {
        let (key, work) = payload.work_folder()?;
        Ok(Request {
            key,
            bare: payload.path("Bare")?,
            work,
            lock: payload.beside_work("Lock", work, "lock")?,
            index: payload.beside_work("Index", work, "index")?,
            job: payload.job(key)?,
            command: payload.words("Command")?,
            subfolder: payload.optional_text("Subfolder")?,
            identity: identity(payload)?,
        })
    }
}

/// The payload's `Identity`: only the git author and committer variables, none holding a NUL; else the usage line.
fn identity(payload: &Payload) -> Result<Vec<(String, String)>, String> {
    let pairs = payload.pairs("Identity")?;
    for (name, value) in &pairs {
        if !IDENTITY_NAMES.contains(&name.as_str()) {
            return Err(format!(
                "gate: box payload Identity may set only GIT_AUTHOR_* and GIT_COMMITTER_* names: {}",
                name.escape_debug()
            ));
        }
        if value.contains('\0') {
            return Err(format!("gate: box payload Identity {name} holds a NUL"));
        }
    }
    Ok(pairs)
}

/// Runs the payload's command in its work folder and returns the status: the command's own whole 32-bit code, 127
/// when it could not be started, 75 when another run holds the work folder, 2 for a payload or a word refused, a
/// failing git step's status, 1 when the box failed or the tree of a passed command could not be committed back, or
/// 255 when the client closed the connection and the box stopped the job.
pub(crate) fn run(payload: &Payload, console: &mut Console) -> u32 {
    let request = match Request::read(payload) {
        Ok(request) => request,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    // Held until the run returns; the command does not inherit it, so no process it leaves running keeps the work
    // folder busy.
    let _lock = match take_lock(request.lock) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            console.err(&format!("gate: remote work folder {} is busy", request.key));
            return BUSY_STATUS;
        }
        Err(line) => {
            console.err(&line);
            return FAILED_STATUS;
        }
    };
    match in_work_folder(&request, console) {
        Ok(status) | Err(status) => status,
    }
}

/// The run once the lock is held: `Ok` with the status after the back commit, or `Err` with the status of a run that
/// ended before it, its lines said.
fn in_work_folder(request: &Request<'_>, console: &mut Console) -> Result<u32, u32> {
    let git = WorkGit::ready(request);
    // Refused before the checkout, so a payload naming a folder outside the work folder changes nothing there; the
    // earlier run's back ref goes, as the checkout's would, so the client finds none to fetch.
    let Some(place) = subfolder_path(request.work, request.subfolder) else {
        console.err(&format!(
            "gate: remote subfolder {} is not a folder inside the work folder",
            request.subfolder
        ));
        git.clear_back_ref(request).map_err(|failed| said(console, &failed))?;
        return Err(u32::from(USAGE_STATUS));
    };
    git.checkout(request).map_err(|failed| said(console, &failed))?;
    if !place.is_dir() {
        console.err(&format!("gate: remote subfolder {} does not exist in the snapshot", request.subfolder));
        return Err(u32::from(USAGE_STATUS));
    }
    let mut status = command(request, &place, console)?;
    if let Err(message) = git.back_commit(request, console) {
        console.err(&format!("gate: remote sync-back failed: {message}"));
        if status == 0 {
            status = FAILED_STATUS;
        }
    }
    Ok(status)
}

/// Says the failed step's line; its status.
fn said(console: &mut Console, failed: &Failed) -> u32 {
    console.err(&failed.line);
    failed.status
}

/// The work folder's lock: `path` opened for reading and writing, created when missing, shared with no one, never
/// through a link, and only as a plain disk file. `None` when another process has it open; else the line saying why it
/// could not be opened.
fn take_lock(path: &str) -> Result<Option<File>, String> {
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    let cannot = |error: io::Error| format!("gate: remote could not open the work folder lock {path}: {error}");
    match opened {
        Ok(lock) => gate_win32::check_plain_file(&lock).map(|()| Some(lock)).map_err(cannot),
        Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => Ok(None),
        Err(error) => Err(cannot(error)),
    }
}

/// The folder the command runs in, as a full path: `sub` below `work`, the work folder itself for an empty `sub`;
/// `None` when `sub` is rooted (a drive, a drive-relative path or a leading separator) or climbs out of `work`. Names
/// are compared as .NET's `OrdinalIgnoreCase` compares them, each character's simple uppercase.
fn subfolder_path(work: &str, sub: &str) -> Option<PathBuf> {
    let work = full_path(Path::new(work))?;
    if sub.is_empty() {
        return Some(work);
    }
    let rooted = sub.starts_with(['\\', '/']) || sub.as_bytes().get(1) == Some(&b':');
    if rooted {
        return None;
    }
    let full = full_path(&work.join(sub))?;
    let (work_text, full_text) = (ordinal_upper(&work.to_string_lossy()), ordinal_upper(&full.to_string_lossy()));
    if full_text == work_text {
        Some(work)
    } else if full_text.starts_with(&format!("{work_text}\\")) {
        Some(full)
    } else {
        None
    }
}

/// `path` made a full path as Windows reads it, `.` and `..` folded and separators written `\`, without a trailing
/// separator; `None` for a path Windows cannot make full, which no folder inside the work folder is.
fn full_path(path: &Path) -> Option<PathBuf> {
    let full = std::path::absolute(path).ok()?;
    let text = full.to_string_lossy();
    let trimmed = text.trim_end_matches('\\');
    // A drive's root keeps its separator.
    Some(if trimmed.ends_with(':') { full } else { PathBuf::from(trimmed) })
}

/// Starts the command in `place` and sees it to its end: `Ok` with its status (127 when it could not be started), or
/// `Err` with the status of a run that ends here, its line said: 2 for a batch word refused, 1 for the box's own
/// failure, 255 for a client that closed the connection.
fn command(request: &Request<'_>, place: &Path, console: &mut Console) -> Result<u32, u32> {
    let variables = Variables::from_process();
    let environment = variables
        .block()
        .map_err(|error| failed(console, &format!("cannot build the command's environment: {error}")))?;
    let Some((word, rest)) = request.command.split_first() else {
        return Err(failed(console, "the run has no command"));
    };
    let rest: Vec<OsString> = rest.iter().map(OsString::from).collect();
    let program = match find_program(word, &variables, place) {
        Ok(program) => program,
        Err(message) => return Ok(not_started(console, word, None, &message)),
    };
    let launch = match written(&program, &rest, &variables) {
        Written::Refused(line) => {
            console.err(&line);
            return Err(u32::from(USAGE_STATUS));
        }
        Written::NoShell(message) => return Ok(not_started(console, word, Some(&program), &message)),
        Written::Line(application, command_line) => PipedLaunch {
            application,
            command_line,
            directory: place.to_owned(),
            environment,
        },
    };
    let job = create_job(request.job, console)?;
    let PipedSuspended { suspended, stdout, stderr } = match launch.start_suspended(&job) {
        Ok(piped) => piped,
        Err(LaunchError::CreateProcess { code, message }) => return start_failed(console, word, &program, code, &message),
        Err(error) => return Err(failed(console, &error.to_string())),
    };
    let running = suspended.assign_and_resume(&job).map_err(|error| failed(console, &error.to_string()))?;
    let mut stopper = Stopper { job: &job, armed: true };
    let watched = watch(&job, &running, Pipes { stdout, stderr }, console);
    stopper.armed = false;
    watched
}

/// A `CreateProcessW` failure: the box's own when its job refuses breakaway (status 1), else the command's start
/// failure (127).
fn start_failed(console: &mut Console, word: &str, program: &Path, code: u32, message: &str) -> Result<u32, u32> {
    if code != ERROR_ACCESS_DENIED {
        return Ok(not_started(console, word, Some(program), message));
    }
    match gate_win32::breakaway_refused() {
        Ok(false) => Ok(not_started(console, word, Some(program), message)),
        Ok(true) => {
            console.err(&format!("gate: the box cannot start a command outside its own job: {message}"));
            Err(FAILED_STATUS)
        }
        Err(error) => Err(failed(
            console,
            &format!("cannot tell whether the box's own job refused the start ({message}): {error}"),
        )),
    }
}

/// Creates the run's job `name`; a name some job already has is refused, so a run never shares a job and a stop meant
/// for an earlier run never reaches it. Else the status 1, its line said.
fn create_job(name: &str, console: &mut Console) -> Result<Job, u32> {
    Job::create(name).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            console.err(&format!("gate: the job {name} already exists; a run's job name must be new"));
            FAILED_STATUS
        } else {
            failed(console, &format!("cannot create the job {name}: {error}"))
        }
    })
}

/// Terminates the job when dropped armed: on a panic after the command was resumed, so the command does not outlive
/// the run.
struct Stopper<'a> {
    job: &'a Job,
    armed: bool,
}

impl Drop for Stopper<'_> {
    fn drop(&mut self) {
        if self.armed {
            // The run is already leaving on a panic, with nowhere left to report a failed terminate to.
            let _ = self.job.terminate(STOPPED);
        }
    }
}

/// The read ends of the command's two pipes.
struct Pipes {
    stdout: File,
    stderr: File,
}

/// Forwards the running command's output, waits for its main process to exit, then lets the readers drain for at most
/// [`DRAIN`]; the command's exit code. A failed write to the box's own output means the client closed the connection,
/// whenever it is noticed: the command's job is stopped then, before the readers drain or after them, and
/// [`CLOSED_STATUS`] answered for it instead. Should the box fail first, the job is terminated, so the command does
/// not outlive a run that no longer watches it.
fn watch(job: &Job, running: &Running, pipes: Pipes, console: &mut Console) -> Result<u32, u32> {
    let lock = OutputLock::default();
    let forwarders = match Forwarders::start(pipes.stdout, pipes.stderr, &lock) {
        Ok(forwarders) => forwarders,
        Err(error) => {
            let status = failed(console, &format!("cannot start reading the command's output: {error}"));
            end_job(job, console);
            return Err(status);
        }
    };
    let code = loop {
        match running.wait(WAIT_STEP) {
            Ok(Some(code)) => break code,
            Ok(None) => {
                if forwarders.pipe_closed() {
                    let ending = terminate_and_wait(job);
                    forwarders.drain(DRAIN);
                    return Err(closed_connection(job, ending, console));
                }
            }
            Err(error) => {
                let status = failed(console, &format!("cannot wait for the command: {error}"));
                end_job(job, console);
                return Err(status);
            }
        }
    };
    let drained = forwarders.drain(DRAIN);
    if drained.pipe_closed {
        return Err(closed_connection(job, terminate_and_wait(job), console));
    }
    if !drained.all_ended {
        console.err("gate: remote command's output is still held open by a leftover process; stopped reading");
    }
    Ok(code)
}

/// The answer to a run whose client closed the connection, its job's `ending` already decided and its readers already
/// drained: says on stderr what became of the job — `stopped` with no process left, `stopping` before why it did not
/// stop — best effort, since the stream that failed may be the one it is said on. [`CLOSED_STATUS`], which the box
/// exits with.
fn closed_connection(job: &Job, ending: io::Result<usize>, console: &mut Console) -> u32 {
    let said = if matches!(&ending, Ok(0)) { "stopped" } else { "stopping" };
    console.err(&format!("gate: remote the client closed the connection; {said} job {}", job.name()));
    match ending {
        Ok(0) => {}
        Ok(left) => console.err(&still_holding(job.name(), left)),
        Err(error) => console.err(&format!("gate: remote could not stop job {}: {error}", job.name())),
    }
    CLOSED_STATUS
}

/// Terminates the job of a command the box no longer watches.
fn end_job(job: &Job, console: &mut Console) {
    if let Err(error) = job.terminate(STOPPED) {
        console.err(&format!("gate: and could not terminate the command's job {}: {error}", job.name()));
    }
}

/// Says the box's own failure; status 1.
fn failed(console: &mut Console, problem: &str) -> u32 {
    console.err(&format!("gate: the gate failed while running the command: {problem}"));
    FAILED_STATUS
}

/// Says the start failure of `word`, found as `resolved`, with the system's `message`, as `gate run` says it; 127.
fn not_started(console: &mut Console, word: &str, resolved: Option<&Path>, message: &str) -> u32 {
    let resolved = resolved.map(|path| path.to_string_lossy().into_owned());
    match resolved.filter(|resolved| resolved != word) {
        Some(resolved) => console.err(&format!("gate: could not start {word} ({resolved}): {message}")),
        None => console.err(&format!("gate: could not start {word}: {message}")),
    }
    NOT_STARTED
}

/// git as the box runs it in the work folder: none of the variables that would point it elsewhere, no standard input,
/// its hooks read from a folder that cannot exist, and no file-system monitor.
struct WorkGit {
    git: Git,
    /// `core.hooksPath=<lock>\hooks`: a folder inside the lock file, which cannot exist while the run holds the lock
    /// open sharing nothing, since a file has no children and cannot be deleted, renamed or replaced meanwhile.
    hooks: String,
}

impl WorkGit {
    /// Finds git; called only once the run holds the lock.
    fn ready(request: &Request<'_>) -> WorkGit {
        WorkGit {
            git: Git::find(),
            hooks: format!(r"core.hooksPath={}\hooks", request.lock),
        }
    }

    /// The safety options, then `args`.
    fn args<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut all = vec!["-c", self.hooks.as_str(), "-c", "core.fsmonitor=false"];
        all.extend_from_slice(args);
        all
    }

    /// Runs `step` in the work folder with `args`, its output passed through.
    fn step(&self, step: &str, request: &Request<'_>, args: &[&str]) -> Result<(), Failed> {
        let mut all = vec!["-C", request.work];
        all.extend_from_slice(args);
        self.git.step(step, &self.args(&all))
    }

    /// The snapshot at `refs/gate/<key>`, fetched from the bare repository, checked out over whatever the last run
    /// left, untracked files removed and ignored ones (build output) kept, and no back commit of an earlier run left
    /// behind.
    fn checkout(&self, request: &Request<'_>) -> Result<(), Failed> {
        let snapshot = format!("refs/gate/{}", request.key);
        self.step("fetch", request, &["fetch", "-q", request.bare, &snapshot])?;
        self.step("checkout", request, &["checkout", "-q", "--force", "--detach", "FETCH_HEAD"])?;
        self.step("clean", request, &["clean", "-fdq"])?;
        self.clear_back_ref(request)
    }

    /// Deletes `refs/gate-back/<key>` in the bare repository, an earlier run's back commit, so the client finds none
    /// to fetch after a run that makes none.
    fn clear_back_ref(&self, request: &Request<'_>) -> Result<(), Failed> {
        let bare = format!("--git-dir={}", request.bare);
        let back = format!("refs/gate-back/{}", request.key);
        self.git.step("back ref reset", &self.args(&[&bare, "update-ref", "-d", &back]))
    }

    /// The tree the command left, committed on `HEAD` and pushed to `refs/gate-back/<key>` in the bare repository;
    /// nothing when the tree is unchanged. It starts from a copy of the work folder's index, which the checkout just
    /// refreshed, so `add -A` skips every unchanged file; a copy that cannot be removed afterwards is said and fails
    /// nothing. Else why it failed.
    fn back_commit(&self, request: &Request<'_>, console: &mut Console) -> Result<(), String> {
        let program = self.git.program.as_ref().map_err(|message| format!("git cannot be started: {message}"))?;
        let back = BackGit {
            program,
            request,
            work: self,
        };
        let index = Path::new(request.work).join(".git").join("index");
        fs::copy(&index, request.index).map_err(|error| format!("cannot copy {} to {}: {error}", index.display(), request.index))?;
        let tree = back
            .status("add", &["add", "-A"], true)
            .and_then(|()| back.line("write-tree", &["write-tree"], true));
        match fs::remove_file(request.index) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                console.err(&format!("gate: remote could not remove {}: {error}", request.index));
            }
            _ => {}
        }
        let tree = tree?;
        if tree == back.line("rev-parse", &["rev-parse", "HEAD^{tree}"], false)? {
            return Ok(());
        }
        let commit = back.line("commit-tree", &["commit-tree", &tree, "-p", "HEAD", "-m", "gate-remote back"], false)?;
        let target = format!("{commit}:refs/gate-back/{}", request.key);
        back.status("push", &["push", "-q", request.bare, &target], false)
    }
}

/// git as the back commit runs it: in the work folder as [`WorkGit`] runs it, with the payload's identity.
struct BackGit<'a> {
    program: &'a Path,
    request: &'a Request<'a>,
    work: &'a WorkGit,
}

impl BackGit<'_> {
    /// git with `args`, using the copied index when `copied_index` is set.
    fn command(&self, args: &[&str], copied_index: bool) -> Command {
        let mut command = Command::new(self.program);
        command.envs(self.request.identity.iter().map(|(name, value)| (name, value)));
        // After the identity, so no identity name can point git elsewhere even if the payload check missed one.
        for name in GIT_REDIRECTS {
            command.env_remove(name);
        }
        if copied_index {
            command.env("GIT_INDEX_FILE", self.request.index);
        }
        command.args(self.work.args(args)).current_dir(self.request.work).stdin(Stdio::null());
        command
    }

    /// Runs `step`, its output passed through; else why it failed.
    fn status(&self, step: &str, args: &[&str], copied_index: bool) -> Result<(), String> {
        // Started under the spawn lock, so it inherits no launch's handles.
        let status = gate_win32::spawn(&mut self.command(args, copied_index))
            .and_then(|mut child| child.wait())
            .map_err(|error| format!("{step} failed: {error}"))?;
        match exit_code(status) {
            0 => Ok(()),
            code => Err(format!("{step} failed (status {code})")),
        }
    }

    /// Runs `step` and returns the first line of its stdout, its stderr passed through; else why it failed.
    fn line(&self, step: &str, args: &[&str], copied_index: bool) -> Result<String, String> {
        // Started under the spawn lock, so it inherits no launch's handles.
        let output = gate_win32::spawn(self.command(args, copied_index).stdout(Stdio::piped()).stderr(Stdio::inherit()))
            .and_then(std::process::Child::wait_with_output)
            .map_err(|error| format!("{step} failed: {error}"))?;
        match exit_code(output.status) {
            0 => String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::to_owned)
                .ok_or_else(|| format!("{step} printed nothing")),
            code => Err(format!("{step} failed (status {code})")),
        }
    }
}
