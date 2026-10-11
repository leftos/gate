//! git as the routing decision and the remote client run it: the executable found once along the gate's own `PATH`,
//! asked about one folder alone, started suspended in an anonymous kill-on-close job of its own on this process's
//! console or a hidden one of its own, its output read from pipes by a thread per stream, and ended with its whole job
//! after a time limit or by the console interrupt.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::TestSeams;
use gate_win32::{Interrupt, Job, PipedConsole, PipedSuspended, Running};

use super::launch::{DRAIN, KILL_WAIT, KILLED, call_job, piped_launch};
use crate::boxside::payload::escaped;
use crate::environment::Variables;
use crate::run::{CONTROL_C_EXIT, find_program};

/// How long one git call may take before git is killed.
pub(crate) const GIT_LIMIT: Duration = Duration::from_secs(60);
/// How often a git call is looked at while it runs.
const GIT_POLL: Duration = Duration::from_millis(5);
/// The most of its standard output a call through [`Git::output`] is read for, as the sync-back's calls read the
/// builder's file lists: one that writes more fails.
const OUTPUT_CAP: usize = 16 * 1024 * 1024;
/// The most of the end of its standard error a call through [`Git::output`] keeps for its failure line, which carries
/// at most [`FAILURE_LINES`] and [`FAILURE_BYTES`] of it.
const ERRORS_KEPT: usize = 64 * 1024;
/// How much of a stream a reader takes from its pipe at a time.
const CHUNK: usize = 8 * 1024;
/// The variables that would point git at another repository, index, object store, config or pathspec reading; git is
/// asked about the gate's folder alone, so a gate inside a hook that exports them still routes. The indexed config
/// variables are removed too, by their prefixes. So are the ssh variant and the external diff, which would change how
/// git reaches the builder and what patch the sync-back applies.
const GIT_LOCATION: [&str; 18] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_LITERAL_PATHSPECS",
    "GIT_SSH_VARIANT",
    "GIT_EXTERNAL_DIFF",
];
/// The prefixes of the config variables `GIT_CONFIG_COUNT` indexes: `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>`.
const GIT_CONFIG_INDEXED: [&str; 2] = ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];
/// `FILE_FLAG_DELETE_ON_CLOSE`: the file of git's output is gone once its last handle closes.
const DELETE_ON_CLOSE: u32 = 0x0400_0000;

static NEXT_OUTPUT: AtomicU32 = AtomicU32::new(0);

/// How long each git call of a run may take: `--test-git-limit-ms` when given, else [`GIT_LIMIT`].
pub(crate) fn limit(seams: &TestSeams) -> Duration {
    seams.git_limit_ms.map_or(GIT_LIMIT, Duration::from_millis)
}

/// Why a git call did not finish: the console interrupt ended it, or it failed, with the failure in the words the
/// PowerShell client uses.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GitError {
    /// The console interrupt was raised, and git was ended with it: the run is an interrupt, not a failure.
    Interrupted,
    /// The call failed: what could not be done, or, for a git that ran and refused, `git <args> failed in <root>
    /// (status <n>): <the lines git wrote>`.
    Failed(String),
}

/// A failed call's reason; an interrupt, which has no line of its own, says the run was interrupted.
impl From<GitError> for String {
    fn from(error: GitError) -> String {
        match error {
            GitError::Interrupted => "the run was interrupted".to_owned(),
            GitError::Failed(why) => why,
        }
    }
}

/// The git executable, found along the gate's own `PATH` once: a caller that asks git many times runs the one git it
/// found rather than searching for git again.
pub(crate) struct Git {
    program: PathBuf,
}

impl Git {
    /// git found along the gate's own `PATH`, read against `folder`; else why it cannot be found.
    pub(crate) fn find(folder: &Path) -> Result<Git, String> {
        find_program("git", &Variables::from_process(), folder)
            .map(|program| Git { program })
            .map_err(|why| format!("cannot run git: {why}"))
    }

    /// Runs git as `call` says, its standard output copied whole into a temporary file and its standard error read and
    /// dropped: a caller that reads none of git's errors has none written down.
    pub(crate) fn run(&self, call: &Call<'_>) -> Result<Ran, GitError> {
        let mut file = output_file().map_err(|error| GitError::Failed(format!("cannot make a file for git's output: {error}")))?;
        let into_file = move |mut pipe: File| -> io::Result<File> {
            io::copy(&mut pipe, &mut file)?;
            file.seek(SeekFrom::Start(0))?;
            Ok(file)
        };
        let dropped = |mut pipe: File| io::copy(&mut pipe, &mut io::sink()).map(drop);
        let (status, stdout, ()) = self.spawned(call, into_file, dropped, None)?;
        Ok(Ran { status, stdout })
    }

    /// Runs git as [`Git::run`] does and returns what it wrote on standard output, read up to [`OUTPUT_CAP`]; else why
    /// the call failed, from the last [`ERRORS_KEPT`] of its standard error.
    #[cfg_attr(
        not(any(test, feature = "test-seams")),
        expect(
            dead_code,
            reason = "only the remote client asks git this way, and a gate without the test seams has none"
        )
    )]
    pub(crate) fn output(&self, call: &Call<'_>) -> Result<String, GitError> {
        self.output_within(call, OUTPUT_CAP, ERRORS_KEPT)
    }

    /// [`Git::output`] with standard output read up to `cap` bytes and the last `kept` bytes of standard error kept: a
    /// call that writes more than `cap` is ended as soon as its reader sees it, and fails.
    fn output_within(&self, call: &Call<'_>, cap: usize, kept: usize) -> Result<String, GitError> {
        let over = Over {
            seen: Arc::new(AtomicBool::new(false)),
            line: over_line(call, cap),
        };
        let seen = Arc::clone(&over.seen);
        let read_capped = move |pipe: File| {
            let held = capped(pipe, cap)?;
            if held.is_none() {
                seen.store(true, Ordering::Release);
            }
            Ok(held)
        };
        let (status, output, errors) = self.spawned(call, read_capped, move |pipe: File| tail(pipe, kept), Some(&over))?;
        let Some(output) = output else {
            return Err(GitError::Failed(over.line));
        };
        if !status.success() {
            return Err(GitError::Failed(failed_line(call, status.code(), &errors, &output)));
        }
        String::from_utf8(output).map_err(|_| GitError::Failed("git's output is not valid UTF-8".to_owned()))
    }

    /// git asked about `folder` alone, as every call starts it: the location and config variables removed, `-C
    /// <folder>`, and no file-system monitor. Its arguments, streams and start are the caller's.
    pub(crate) fn command(&self, folder: &Path) -> Command {
        let mut command = Command::new(&self.program);
        for name in GIT_LOCATION {
            command.env_remove(name);
        }
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(is_indexed_config) {
                command.env_remove(name);
            }
        }
        command.arg("-C").arg(folder).args(["-c", "core.fsmonitor=false"]);
        command
    }

    /// Runs git with `call`'s args and variables, created suspended with no standard input on the console `call`
    /// names, and put in an anonymous kill-on-close job of its own before it runs; a thread per stream reads its
    /// standard output with `stdout` and its standard error with `stderr`, and what they read is returned with its
    /// exit status once it has exited. Whatever git left running then is ended with its job. A call made while the
    /// console interrupt is already raised starts no process; one whose wait ends otherwise than in git's exit (it
    /// outlived its limit, was interrupted, wrote more than `over` allows, or could not be waited for) has its whole
    /// job ended.
    fn spawned<O, E>(
        &self,
        call: &Call<'_>,
        stdout: impl FnOnce(File) -> io::Result<O> + Send + 'static,
        stderr: impl FnOnce(File) -> io::Result<E> + Send + 'static,
        over: Option<&Over>,
    ) -> Result<(ExitStatus, O, E), GitError>
    where
        O: Send + 'static,
        E: Send + 'static,
    {
        if call.interrupt.is_some_and(Interrupt::is_raised) {
            return Err(GitError::Interrupted);
        }
        let mut command = self.command(call.folder);
        for name in call.unset {
            command.env_remove(*name);
        }
        command.envs(call.set.iter().copied()).args(call.args);
        let deadline = Instant::now() + call.limit;
        let cannot_run = |error: &dyn std::fmt::Display| GitError::Failed(format!("cannot run git: {error}"));
        let job = call_job(false).map_err(|error| cannot_run(&error))?;
        let launch = piped_launch(&command, call.console).map_err(|error| cannot_run(&error))?;
        let PipedSuspended {
            suspended,
            stdout: out_pipe,
            stderr: err_pipe,
        } = launch.start_suspended_within(&job).map_err(|error| cannot_run(&error))?;
        let running = suspended.assign_and_resume(&job).map_err(|error| cannot_run(&error))?;
        let output = reading(out_pipe, stdout);
        let errors = reading(err_pipe, stderr);
        let code = match waited(&running, &job, call, over, deadline) {
            Ok(code) => code,
            Err(error) => {
                // Whatever ended the wait, the job is ended now, so the readers reach their pipes' ends; what they
                // read is no longer needed, and they are given a short while to end before they are left to it.
                let error = with_job_ended(error, &job);
                let until = Instant::now() + DRAIN;
                drop((received(&output, until), received(&errors, until)));
                return Err(error);
            }
        };
        // git has exited; whatever it left running, which would hold its pipes open, ends with the job, and the
        // readers then reach their pipes' ends within a short while.
        drop(job);
        let until = Instant::now() + DRAIN;
        let unread = || {
            GitError::Failed(format!(
                "cannot read git's output: it did not end within {} s of git's exit",
                DRAIN.as_secs()
            ))
        };
        let output = received(&output, until).ok_or_else(unread)?;
        let errors = received(&errors, until).ok_or_else(unread)?;
        let cannot_read = |error: io::Error| GitError::Failed(format!("cannot read git's output: {error}"));
        Ok((ExitStatus::from_raw(code), output.map_err(cannot_read)?, errors.map_err(cannot_read)?))
    }
}

/// A call whose standard output is read up to a cap: whether its reader has seen more, and the line it then fails with.
struct Over {
    seen: Arc<AtomicBool>,
    line: String,
}

/// Reads `pipe` with `read` on a thread of its own; its result arrives on the channel returned.
fn reading<T: Send + 'static>(pipe: File, read: impl FnOnce(File) -> io::Result<T> + Send + 'static) -> Receiver<io::Result<T>> {
    let (sent, received) = mpsc::channel();
    thread::spawn(move || {
        // The receiver is gone only once the call stopped waiting on this stream, when what it read is not needed.
        drop(sent.send(read(pipe)));
    });
    received
}

/// What a reader read, once it ended before `until`; `None` when it had not. A reader that ended with no result is a
/// read that failed.
fn received<T>(from: &Receiver<io::Result<T>>, until: Instant) -> Option<io::Result<T>> {
    match from.recv_timeout(until.saturating_duration_since(Instant::now())) {
        Ok(result) => Some(result),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => Some(Err(io::Error::other("its reader ended with no result"))),
    }
}

/// What `reader` sends up to `cap` bytes; `None` once it sends more, which is read only to its first byte past the cap.
fn capped(reader: impl Read, cap: usize) -> io::Result<Option<Vec<u8>>> {
    let mut held = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    reader.take(limit).read_to_end(&mut held)?;
    Ok((held.len() <= cap).then_some(held))
}

/// The last `kept` bytes `reader` sends, read to its end while at most twice that is held; when the stream was longer,
/// the cut line at their start is dropped when a line break follows it before their last byte, so they start at a
/// whole line when they hold one, and keep their last line whole when it is all they hold.
fn tail(mut reader: impl Read, kept: usize) -> io::Result<Vec<u8>> {
    let mut held = Vec::new();
    let mut chunk = vec![0; CHUNK];
    let mut cut = false;
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        held.extend_from_slice(chunk.get(..read).unwrap_or_default());
        if held.len() > kept.saturating_mul(2) {
            held.drain(..held.len() - kept);
            cut = true;
        }
    }
    if held.len() > kept {
        held.drain(..held.len() - kept);
        cut = true;
    }
    if cut
        && let Some(end) = held.iter().position(|byte| *byte == b'\n')
        && end + 1 < held.len()
    {
        held.drain(..=end);
    }
    Ok(held)
}

/// The line a call that wrote more than `cap` bytes on standard output fails with: the cap in whole MiB when it is a
/// whole number of them, else in bytes.
fn over_line(call: &Call<'_>, cap: usize) -> String {
    const MIB: usize = 1024 * 1024;
    let size = match cap % MIB {
        0 => format!("{} MiB", cap / MIB),
        _ => format!("{cap} bytes"),
    };
    format!("{}: its output passed {size}", failed_in(call))
}

/// `git <args> failed in <folder>`, how a failed call's line starts, its args and folder escaped, as either may name
/// what the builder sent.
fn failed_in(call: &Call<'_>) -> String {
    format!(
        "git {} failed in {}",
        escaped(&call.args.join(" ")),
        escaped(&call.folder.to_string_lossy())
    )
}

/// One git call: the folder git is asked about, its arguments, the variables it is given and the ones it is not, how
/// long it may take, and the console interrupt that ends it.
pub(crate) struct Call<'a> {
    /// The folder git is asked about with `-C`.
    pub folder: &'a Path,
    pub args: &'a [&'a str],
    /// The variables git is given, beside the ones every call is given.
    pub set: &'a [(&'a str, &'a OsStr)],
    /// The variables git is not given, beside [`GIT_LOCATION`].
    pub unset: &'a [&'a str],
    pub limit: Duration,
    /// The console interrupt a Ctrl+C raises: git is ended with it, and the call is [`GitError::Interrupted`].
    pub interrupt: Option<Interrupt>,
    /// The console git is created on: this process's, so a Ctrl+C reaches git as it reaches the gate, or a hidden one
    /// of its own, which no Ctrl+C reaches.
    pub console: PipedConsole,
}

impl<'a> Call<'a> {
    /// A call on this process's console that sets and removes no variables of its own and answers to no interrupt:
    /// the routing decision asks git this way, before a command is started.
    pub(crate) fn plain(folder: &'a Path, args: &'a [&'a str], limit: Duration) -> Call<'a> {
        Call {
            folder,
            args,
            set: &[],
            unset: &[],
            limit,
            interrupt: None,
            console: PipedConsole::Attached,
        }
    }

    /// A call as [`Call::plain`] makes it, but on a hidden console of its own, which no Ctrl+C reaches: a call that
    /// takes a lock in the git folder and lets it go ends only on its own or at its limit, so no Ctrl+C kills git
    /// while it holds the lock.
    pub(crate) fn locked(folder: &'a Path, args: &'a [&'a str], limit: Duration) -> Call<'a> {
        Call {
            console: PipedConsole::Hidden,
            ..Call::plain(folder, args, limit)
        }
    }
}

/// How a git call ended: its exit status, and its stdout, held in a temporary file read from its start.
pub(crate) struct Ran {
    pub status: ExitStatus,
    pub stdout: File,
}

/// Whether `name` is a variable git is never given: one of [`GIT_LOCATION`] or an indexed config variable, its letter
/// case aside as Windows reads a variable's name.
pub(crate) fn is_location_variable(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| GIT_LOCATION.iter().any(|location| location.eq_ignore_ascii_case(name)) || is_indexed_config(name))
}

/// Whether `name` is one of the config variables `GIT_CONFIG_COUNT` indexes, its letter case aside as Windows reads a
/// variable's name.
fn is_indexed_config(name: &str) -> bool {
    GIT_CONFIG_INDEXED
        .iter()
        .any(|prefix| name.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix)) && name.len() > prefix.len())
}

/// git's exit code once it ends, or [`GitError::Interrupted`] when that is a Ctrl+C's with the console interrupt
/// raised. Past `deadline` its job is ended and the call fails, as it does once its reader saw more of its output than
/// `over` allows; a console interrupt raised while it runs ends its job as [`GitError::Interrupted`].
fn waited(running: &Running, job: &Job, call: &Call<'_>, over: Option<&Over>, deadline: Instant) -> Result<u32, GitError> {
    loop {
        if call.interrupt.is_some_and(Interrupt::is_raised) {
            return Err(match ended(running, job) {
                Ok(()) => GitError::Interrupted,
                Err(error) => GitError::Failed(format!("the run was interrupted, and git could not be stopped: {error}")),
            });
        }
        if let Some(over) = over.filter(|over| over.seen.load(Ordering::Acquire)) {
            return Err(stopped(over.line.clone(), running, job));
        }
        if let Some(code) = running
            .wait(GIT_POLL)
            .map_err(|error| GitError::Failed(format!("cannot wait for git: {error}")))?
        {
            return exited(code, call);
        }
        if Instant::now() >= deadline {
            return Err(stopped(late_git_line(call.limit), running, job));
        }
    }
}

/// git's exit `code`, or [`GitError::Interrupted`] when git exited of a Ctrl+C and the console interrupt is raised by
/// now: a Ctrl+C reaches git as it reaches the gate, so git may exit of it before the wait saw the interrupt.
fn exited(code: u32, call: &Call<'_>) -> Result<u32, GitError> {
    if code == CONTROL_C_EXIT && call.interrupt.is_some_and(Interrupt::is_raised) {
        Err(GitError::Interrupted)
    } else {
        Ok(code)
    }
}

/// `error`, the end of a call whose wait did not end in git's exit, once git's whole job is ended, so nothing of it
/// keeps running or holds its pipes open; with why the job could not be ended when it could not.
fn with_job_ended(error: GitError, job: &Job) -> GitError {
    match job.terminate(KILLED.cast_unsigned()) {
        Ok(()) => error,
        Err(kill) => GitError::Failed(format!("{}, and git could not be stopped: {kill}", String::from(error))),
    }
}

/// The failure `why` of a call whose job is ended now, with why it could not be when it could not.
fn stopped(why: String, running: &Running, job: &Job) -> GitError {
    match ended(running, job) {
        Ok(()) => GitError::Failed(why),
        Err(error) => GitError::Failed(format!("{why}, and could not be stopped: {error}")),
    }
}

/// Ends every process of git's job and waits [`KILL_WAIT`] for git itself to exit; else why it could not.
fn ended(running: &Running, job: &Job) -> Result<(), String> {
    job.terminate(KILLED.cast_unsigned()).map_err(|error| error.to_string())?;
    match running.wait(KILL_WAIT) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!("it did not end within {} s of its kill", KILL_WAIT.as_secs())),
        Err(error) => Err(error.to_string()),
    }
}

/// The line a git that outlived `limit` is killed under: the limit in whole seconds when it is a whole number of
/// seconds, else in whole milliseconds.
fn late_git_line(limit: Duration) -> String {
    match limit.as_millis() % 1000 {
        0 => format!("git did not answer within {} s", limit.as_secs()),
        _ => format!("git did not answer within {} ms", limit.as_millis()),
    }
}

/// Why a git call with `call`'s arguments failed, in the words the PowerShell client uses: the folder git was asked
/// about, its status, and what [`failure_text`] keeps of the lines it wrote, standard output's first and standard
/// error's last, so that its last error lines are the ones kept. The args, the folder and the lines are each escaped
/// once, as any of them may name what the builder sent.
#[cfg_attr(
    not(any(test, feature = "test-seams")),
    expect(
        dead_code,
        reason = "only the remote client reads the lines of a failed git, and a gate without the test seams has none"
    )
)]
fn failed_line(call: &Call<'_>, status: Option<i32>, errors: &[u8], output: &[u8]) -> String {
    let status = status.map_or_else(|| "unknown".to_owned(), |code| code.to_string());
    let errors = String::from_utf8_lossy(errors);
    let output = String::from_utf8_lossy(output);
    let lines: Vec<&str> = output.lines().chain(errors.lines()).collect();
    format!("{} (status {status}): {}", failed_in(call), failure_text(&lines))
}

/// The most lines, the last, and the most bytes of them that a failure line carries of a call's lines.
const FAILURE_LINES: usize = 10;
pub(crate) const FAILURE_BYTES: usize = 4096;

/// The last [`FAILURE_LINES`] of a failed call's `lines`, each as its last carriage-return update leaves it, joined by
/// ` | `, escaped and cut to their last [`FAILURE_BYTES`]: what a failure line carries of them. The characters are
/// counted from the end by the width of their escapes and only those that fit are escaped, so the escaped text stays
/// within the bytes and the cut never splits a character or an escape.
pub(crate) fn failure_text<S: AsRef<str>>(lines: &[S]) -> String {
    let last = lines.get(lines.len().saturating_sub(FAILURE_LINES)..).unwrap_or_default();
    let shown: Vec<&str> = last.iter().map(|line| last_update(line.as_ref())).collect();
    let joined = shown.join(" | ");
    let mut start = joined.len();
    let mut width = 0;
    for (at, character) in joined.char_indices().rev() {
        width += escaped(character.encode_utf8(&mut [0; 4])).len();
        if width > FAILURE_BYTES {
            break;
        }
        start = at;
    }
    escaped(joined.get(start..).unwrap_or_default())
}

/// What a progress line shows once its carriage-return updates are done: the text after its last carriage return.
/// Preflight and failure text are read this way on purpose, each line keeping its last update and dropping what came
/// before it.
pub(crate) fn last_update(line: &str) -> &str {
    line.rsplit('\r').next().unwrap_or(line)
}

/// A new file in the temporary folder for one git call's output, deleted once its last handle closes.
fn output_file() -> io::Result<File> {
    let number = NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("gate-git-{}-{number}.out", std::process::id()));
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(DELETE_ON_CLOSE)
        .open(path)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test whose git cannot be found or whose bytes cannot be read must fail loudly"
)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::thread::{self, ScopedJoinHandle};
    use std::time::{Duration, Instant};

    use gate_win32::Interrupt;
    use jiff::Timestamp;

    use super::{
        Call, ERRORS_KEPT, FAILURE_BYTES, GIT_LIMIT, Git, GitError, OUTPUT_CAP, capped, failed_line, failure_text, is_indexed_config, late_git_line,
        tail,
    };
    use crate::boxside::payload::escaped;
    use crate::remote::snapshot::repository;

    /// How long a case waits for what a passing run always sees: only a broken product reaches it.
    const HANG_GUARD: Duration = Duration::from_secs(600);
    /// How often a case looks again for what it waits for.
    const POLL: Duration = Duration::from_millis(50);
    /// Set for the process of its own the interrupt case runs in.
    const OWN_PROCESS: &str = "GATE_TEST_GIT_INTERRUPT_OWN_PROCESS";

    /// This crate's folder, inside the repository's own checkout, and git found along `PATH` for it.
    fn this_checkout() -> (&'static Path, Git) {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        (here, Git::find(here).expect("git is found along PATH"))
    }

    /// A new repository for the case `name` whose `held.slow` git runs through the clean filter `slow`: its folder, the
    /// file the filter writes its pid to, and the go file the filter waits for, which no case writes. Both files are in
    /// the git folder, where `git add -A` never looks.
    fn slow_repository(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = repository(name);
        fs::write(root.join(".gitattributes"), "*.slow filter=slow\n").expect("the attributes can be written");
        fs::write(root.join("held.slow"), "held\n").expect("a filtered file can be written");
        let pid_file = root.join(".git").join("filter.pid");
        let go = root.join(".git").join("go");
        (root, pid_file, go)
    }

    /// The `-c` value that makes `slow` a clean filter running Windows PowerShell, which writes `pid <its pid>` to
    /// `pid_file` and then waits for `go`: a git call that runs it cannot finish on its own. Not pwsh: a pwsh installed
    /// from the Store starts outside the job of the process that starts it, so no job's kill reaches it.
    fn waiting_filter(pid_file: &Path, go: &Path) -> String {
        let slashed = |path: &Path| path.display().to_string().replace('\\', "/");
        format!(
            "filter.slow.clean=powershell -NoProfile -NonInteractive -Command \"Set-Content -LiteralPath '{}' -Value ('pid ' + \\$PID); \
             while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 100 }}\"",
            slashed(pid_file),
            slashed(go)
        )
    }

    /// Writes the go file when dropped, so a filter a failing case leaves running ends.
    struct LetGo<'a>(&'a Path);

    impl Drop for LetGo<'_> {
        fn drop(&mut self) {
            // A filter that cannot see the file is one the case already saw end, or one left to its folder's cleanup.
            drop(fs::write(self.0, ""));
        }
    }

    /// Removes a case's repository when dropped; a case declares it before its [`LetGo`], so its filter is let go
    /// first, and a pass or a panic leaves no folder behind.
    struct Removed(PathBuf);

    impl Drop for Removed {
        fn drop(&mut self) {
            // A filter a failing case could not end may still hold its folder; that folder is left to the next run of
            // the case, which removes it before making the repository again.
            drop(fs::remove_dir_all(&self.0));
        }
    }

    /// The pid the filter wrote to `pid_file`, once it has written its whole line.
    fn written_pid(pid_file: &Path) -> Option<u32> {
        let text = fs::read_to_string(pid_file).ok()?;
        text.strip_suffix('\n')?.trim_end().strip_prefix("pid ")?.parse().ok()
    }

    /// The pid the filter wrote, waited for while `call` runs: a call that ended before the filter wrote it fails the
    /// case at once, and one whose filter never writes it fails at the hang guard.
    fn pid_while_running<T>(pid_file: &Path, call: &ScopedJoinHandle<'_, T>) -> u32 {
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            if let Some(pid) = written_pid(pid_file) {
                return pid;
            }
            assert!(!call.is_finished(), "git ended before its filter wrote its pid");
            assert!(Instant::now() < deadline, "the filter wrote no pid within the hang guard");
            thread::sleep(POLL);
        }
    }

    /// Waits until the process `pid`, which started at `started` when it was last seen, has ended: gone, or its pid
    /// taken by a later process.
    fn wait_until_gone(pid: u32, started: Option<Timestamp>) {
        let deadline = Instant::now() + HANG_GUARD;
        while started.is_some() && gate_win32::process_start_of(pid).expect("the process can be looked for") == started {
            assert!(Instant::now() < deadline, "the filter's process {pid} did not end within the hang guard");
            thread::sleep(POLL);
        }
    }

    #[test]
    fn an_interrupt_while_git_runs_ends_as_interrupted() {
        // The console interrupt is the process's own and never lowered, and other cases here raise it: this case runs
        // in a process of its own, the test binary run on this case alone, where only it raises the interrupt.
        if std::env::var_os(OWN_PROCESS).is_none() {
            let name = "remote::git::tests::an_interrupt_while_git_runs_ends_as_interrupted";
            let mut own = Command::new(std::env::current_exe().expect("the test binary has a path"));
            own.args(["--exact", name, "--test-threads=1"])
                .env(OWN_PROCESS, "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let ran = gate_win32::spawn(&mut own)
                .and_then(Child::wait_with_output)
                .expect("the test binary runs the case in a process of its own");
            let said = format!("{}{}", String::from_utf8_lossy(&ran.stdout), String::from_utf8_lossy(&ran.stderr));
            assert!(ran.status.success() && said.contains("1 passed"), "{}: {said}", ran.status);
            return;
        }
        let (root, pid_file, go) = slow_repository("git-interrupt");
        let _removed = Removed(root.clone());
        let _let_go = LetGo(&go);
        let clean = waiting_filter(&pid_file, &go);
        let args = ["-c", clean.as_str(), "add", "-A"];
        let git = Git::find(&root).expect("git is found along PATH");
        let interrupt = Interrupt::install().expect("the console handler is installed");
        // The hang guard as limit: in a passing run only the interrupt ends the call.
        let call = Call {
            interrupt: Some(interrupt),
            ..Call::plain(&root, &args, HANG_GUARD)
        };
        let (ended, pid, started) = thread::scope(|scope| {
            let running = scope.spawn(|| git.output(&call));
            let pid = pid_while_running(&pid_file, &running);
            let started = gate_win32::process_start_of(pid).expect("the filter can be looked for");
            interrupt.raise();
            (running.join().expect("the call returns"), pid, started)
        });
        assert_eq!(ended, Err(GitError::Interrupted));
        wait_until_gone(pid, started);
    }

    #[test]
    fn a_git_call_past_its_limit_says_it_did_not_answer() {
        let (root, pid_file, go) = slow_repository("git-limit");
        let _removed = Removed(root.clone());
        let _let_go = LetGo(&go);
        let clean = waiting_filter(&pid_file, &go);
        let args = ["-c", clean.as_str(), "add", "-A"];
        let git = Git::find(&root).expect("git is found along PATH");
        // git cannot finish while its filter waits, so only the limit ends the call, however slow the machine.
        let call = Call::plain(&root, &args, Duration::from_millis(1));
        assert_eq!(git.output(&call), Err(GitError::Failed("git did not answer within 1 ms".to_owned())));
        // A 1 ms limit mostly ends git before its filter starts; a filter that did start ended with git's job.
        if let Some(pid) = written_pid(&pid_file) {
            wait_until_gone(pid, gate_win32::process_start_of(pid).expect("the filter can be looked for"));
        }
    }

    #[test]
    fn a_batch_file_git_says_why_it_cannot_run() {
        let program = std::env::temp_dir().join("gate-git-batch").join("git.cmd");
        let git = Git { program: program.clone() };
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let Err(GitError::Failed(line)) = git.run(&Call::plain(here, &["version"], GIT_LIMIT)) else {
            panic!("a batch file is never run as git");
        };
        assert_eq!(
            line,
            format!(
                "cannot run git: check the launch: the application {} is a batch file, which runs only through cmd.exe",
                program.display()
            )
        );
    }

    #[test]
    fn a_failure_line_cuts_its_escaped_text_to_4_kib() {
        let line = ["\u{1b}".repeat(1000) + "end"];
        let text = failure_text(&line);
        assert!(text.len() <= FAILURE_BYTES, "{} bytes", text.len());
        assert!(text.ends_with("end"), "{text}");
        let start = text.get(..24).unwrap_or(&text);
        assert!(text.starts_with(&escaped("\u{1b}")), "the cut starts at a whole escape: {start}");
    }

    #[test]
    fn output_over_the_cap_fails_instead_of_reading_it_whole() {
        let (here, git) = this_checkout();
        let call = Call::plain(here, &["ls-files"], GIT_LIMIT);
        assert_eq!(
            git.output_within(&call, 64, ERRORS_KEPT),
            Err(GitError::Failed(format!(
                "git ls-files failed in {}: its output passed 64 bytes",
                here.display()
            )))
        );
        assert_eq!(
            capped(io::repeat(b'x'), 64).expect("an endless stream can be read"),
            None,
            "an endless stream is read only to one byte past the cap, so the read ends"
        );
        assert_eq!(
            capped(&b"abcd"[..], 4).expect("bytes can be read"),
            Some(b"abcd".to_vec()),
            "output at the cap is whole"
        );
        assert_eq!(capped(&b"abcde"[..], 4).expect("bytes can be read"), None, "one byte past the cap");
    }

    #[test]
    fn a_failed_git_reads_only_the_tail_of_its_standard_error() {
        let (here, git) = this_checkout();
        let missing: Vec<String> = (1..=40).map(|number| format!("missing-{number:02}")).collect();
        let mut args = vec!["ls-files", "--error-unmatch", "--"];
        args.extend(missing.iter().map(String::as_str));
        let call = Call::plain(here, &args, GIT_LIMIT);
        let Err(GitError::Failed(line)) = git.output_within(&call, OUTPUT_CAP, 512) else {
            panic!("git ls-files --error-unmatch fails on a missing path");
        };
        assert!(line.contains(" (status 1): "), "{line}");
        assert!(line.contains("pathspec 'missing-40'"), "the last error line is kept: {line}");

        let text = (1..=10_000).fold(String::new(), |text, number| text + "line " + &number.to_string() + "\n");
        let kept = tail(text.as_bytes(), 512).expect("bytes can be read");
        assert!(kept.len() <= 512, "only the tail is held: {} bytes", kept.len());
        assert!(kept.starts_with(b"line "), "the tail starts at a whole line");
        assert!(kept.ends_with(b"line 9999\nline 10000\n"), "the tail is the stream's end");
        assert_eq!(
            tail(&b"a\nb\n"[..], 512).expect("bytes can be read"),
            b"a\nb\n",
            "a short stream is kept whole"
        );

        let mut progress = b"x\r".repeat(512);
        progress.extend_from_slice(b"fatal: x\n");
        let kept = tail(progress.as_slice(), 512).expect("bytes can be read");
        assert!(
            kept.ends_with(b"fatal: x\n"),
            "a cut tail whose one line break is its last byte keeps its reason: {}",
            String::from_utf8_lossy(&kept)
        );
    }

    #[test]
    fn a_failure_line_shows_each_kept_line_s_last_progress_update() {
        let lines = [
            "Receiving objects:  1% (1/90)\rReceiving objects: 50% (45/90)",
            "fatal: the remote end hung up unexpectedly",
        ];
        assert_eq!(
            failure_text(&lines),
            "Receiving objects: 50% (45/90) | fatal: the remote end hung up unexpectedly"
        );
    }

    #[test]
    fn a_failure_line_carries_the_last_10_lines_and_4_kib_of_them_at_most() {
        let lines: Vec<String> = (1..=12).map(|number| format!("line {number}")).collect();
        assert_eq!(
            failure_text(&lines),
            "line 3 | line 4 | line 5 | line 6 | line 7 | line 8 | line 9 | line 10 | line 11 | line 12"
        );
        let wide = ["\u{e9}".repeat(3000)];
        let text = failure_text(&wide);
        assert_eq!(text.len(), FAILURE_BYTES, "the last 4 KiB, which start at a character's boundary here");
        let cut = [format!("{}x", "\u{e9}".repeat(3000))];
        let text = failure_text(&cut);
        assert_eq!(text.len(), FAILURE_BYTES - 1, "a character the cut would split is left out");
        assert!(text.starts_with('\u{e9}') && text.ends_with('x'), "{text}");
        assert_eq!(failure_text::<&str>(&[]), "");
    }

    #[test]
    fn a_failed_git_s_standard_error_comes_last_so_its_fatal_line_is_kept() {
        let call = Call::plain(Path::new(r"C:\work\app"), &["fetch"], GIT_LIMIT);
        let output = (1..=12).fold(String::new(), |text, number| text + "out " + &number.to_string() + "\n");
        assert_eq!(
            failed_line(&call, Some(128), b"fatal: the remote end hung up\n", output.as_bytes()),
            r"git fetch failed in C:\work\app (status 128): out 4 | out 5 | out 6 | out 7 | out 8 | out 9 | out 10 | out 11 | out 12 | fatal: the remote end hung up"
        );
    }

    #[test]
    fn a_failed_git_line_escapes_what_git_wrote() {
        let call = Call::plain(Path::new(r"C:\work\app"), &["write-tree"], GIT_LIMIT);
        assert_eq!(
            failed_line(&call, Some(128), b"fatal: \x1b[31mred\x1b]8;;http://x\x07link\n", b""),
            r"git write-tree failed in C:\work\app (status 128): fatal: \u{1b}[31mred\u{1b}]8;;http://x\u{7}link"
        );
    }

    #[test]
    fn git_is_given_none_of_the_config_variables() {
        let command = Git {
            program: PathBuf::from("git.exe"),
        }
        .command(Path::new(r"C:\work\app"));
        let removed: Vec<&OsStr> = command.get_envs().filter(|(_, value)| value.is_none()).map(|(name, _)| name).collect();
        for name in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_PARAMETERS",
            "GIT_DIR",
        ] {
            assert!(removed.contains(&OsStr::new(name)), "{name} is removed: {removed:?}");
        }
        for name in ["GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0", "GIT_CONFIG_KEY_12", "git_config_value_3"] {
            assert!(is_indexed_config(name), "{name}");
        }
        for name in ["GIT_CONFIG_KEYS", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_KEY", "XGIT_CONFIG_KEY_0", "PATH"] {
            assert!(!is_indexed_config(name), "{name}");
        }
    }

    #[test]
    fn a_late_git_names_its_limit_in_seconds_or_milliseconds() {
        assert_eq!(late_git_line(GIT_LIMIT), "git did not answer within 60 s", "the default limit");
        assert_eq!(late_git_line(Duration::from_secs(60)), "git did not answer within 60 s");
        assert_eq!(late_git_line(Duration::from_secs(10)), "git did not answer within 10 s");
        assert_eq!(late_git_line(Duration::from_millis(1)), "git did not answer within 1 ms");
    }

    #[test]
    fn a_failed_git_line_names_the_call_its_folder_its_status_and_git_s_lines() {
        let call = Call::plain(Path::new(r"C:\work\app"), &["write-tree"], GIT_LIMIT);
        assert_eq!(
            failed_line(&call, Some(128), b"warning: first\r\nfatal: last\n", b"then out\n"),
            r"git write-tree failed in C:\work\app (status 128): then out | warning: first | fatal: last"
        );
        assert_eq!(
            failed_line(&call, Some(1), b"", b""),
            r"git write-tree failed in C:\work\app (status 1): ",
            "git wrote nothing"
        );
        assert_eq!(
            failed_line(&call, None, b"fatal: x\n", b""),
            r"git write-tree failed in C:\work\app (status unknown): fatal: x",
            "no exit code"
        );
    }
}
