//! The box's process helpers: the bounded runner every program the box starts runs in (the tool probes, `dotnet
//! --list-sdks`, the git steps and the login shell), and the login shell's `PATH` those programs are found along.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// The most bytes of a program's standard error kept: its tail.
const STDERR_TAIL: usize = 64 * 1024;
/// How often the runner looks whether its program has exited.
const POLL: Duration = Duration::from_millis(10);
/// How long the runner waits for its readers once the group is killed.
const READER_GRACE: Duration = Duration::from_secs(1);

/// The most bytes the login shell may write for its `PATH`.
pub const LOGIN_PATH_CAP: usize = 64 * 1024;
/// How long the login shell may take to say its `PATH`.
pub const LOGIN_PATH_LIMIT: Duration = Duration::from_secs(10);
/// The shell the login `PATH` is read from when `SHELL` is unset or empty.
const DEFAULT_SHELL: &str = "/bin/zsh";

/// A program that ran to its exit.
#[derive(Debug)]
pub struct Ran {
    /// How it exited.
    pub status: ExitStatus,
    /// Its standard output, at most the cap given.
    pub stdout: Vec<u8>,
    /// The end of its standard error.
    pub stderr: Vec<u8>,
}

/// Why a program did not run to an exit the runner could read.
#[derive(Debug)]
pub enum NotRan {
    /// It could not be started.
    Start(io::Error),
    /// It was still running, or its output still open, at the deadline; its process group was killed.
    TimedOut {
        /// The process group that was killed.
        group: i32,
    },
    /// It wrote more than the cap to its standard output.
    OverCap,
    /// Its exit or its output could not be read.
    Failed(String),
}

/// The most bytes of one line of standard error the runner hands on ([`Stderr::Lines`]): the rest of a longer line is
/// dropped.
pub const LINE_CAP: usize = 64 * 1024;

/// How the runner ends a program's process group: at once, or asked first.
#[derive(Clone, Copy, Debug)]
pub enum Ending {
    /// `SIGKILL` at once.
    Kill,
    /// `SIGTERM`, then `SIGKILL` once this grace has passed or the program has exited, whichever comes first.
    TermThenKill(Duration),
}

/// What takes each line of a program's standard error ([`Stderr::Lines`]): `false` once no more are wanted.
pub type TakeLine<'a> = &'a mut dyn FnMut(&[u8]) -> bool;

/// What the runner does with the program's standard error.
pub enum Stderr<'a> {
    /// Keeps its last bytes in [`Ran::stderr`].
    Tail,
    /// Hands each line, without its end, to `take` on the calling thread as it comes: a line feed, a carriage return
    /// and the two together each end a line, as the Windows box cuts them, and a line keeps at most [`LINE_CAP`] bytes.
    /// [`Ran::stderr`] stays empty. Once `take` answers `false` it is handed no more lines, and the program's group is
    /// ended as the run's [`Ending`] says when `end` is set.
    Lines {
        /// Takes one line; `false` once no more are wanted.
        take: TakeLine<'a>,
        /// Whether a `false` from `take` ends the program.
        end: bool,
    },
}

/// Runs `command` in a process group of its own, standard input null, its standard output kept up to `cap` bytes and
/// the end of its standard error, until `deadline`: past it the whole group is killed (`SIGKILL`) and the program
/// reaped. Output a descendant still holds open once the program has exited is waited for until the deadline too.
///
/// # Errors
///
/// [`NotRan`]: it could not start, it ran past the deadline, it wrote more than `cap`, or its exit could not be read.
pub fn run_bounded(command: Command, cap: usize, deadline: Instant) -> Result<Ran, NotRan> {
    run_watched(command, cap, deadline, Stderr::Tail, Ending::Kill)
}

/// [`run_bounded`], its standard error handled as `stderr` says, and its group ended as `ending` says at the deadline
/// or when `stderr`'s lines ask for it.
///
/// # Errors
///
/// [`NotRan`], as [`run_bounded`] gives it.
pub fn run_watched(mut command: Command, cap: usize, deadline: Instant, stderr: Stderr<'_>, ending: Ending) -> Result<Ran, NotRan> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = spawned(&mut command).map_err(NotRan::Start)?;
    let group = i32::try_from(child.id()).map_err(|error| NotRan::Failed(format!("the process id is no pid_t: {error}")))?;
    let (sender, receiver) = mpsc::channel();
    let split = matches!(stderr, Stderr::Lines { .. });
    let mut watch = Watch::new(group, (deadline, ending), stderr, receiver);
    match child.stdout.take() {
        Some(pipe) => {
            let sender = sender.clone();
            thread::spawn(move || sender.send(Event::Out(capped(pipe, cap))));
        }
        None => watch.stdout = Some(Ok((Vec::new(), false))),
    }
    match child.stderr.take() {
        Some(pipe) => {
            thread::spawn(move || stderr_read(pipe, split, &sender));
        }
        None => watch.stderr = Some(Ok(Vec::new())),
    }
    let status = watch.exited(&mut child)?;
    watch.drained()?;
    watch.ran(status)
}

/// What a reader thread read: all of one stream, or one line of standard error.
enum Event {
    Out(io::Result<(Vec<u8>, bool)>),
    Err(io::Result<Vec<u8>>),
    Line(Vec<u8>),
}

/// The runner's watch of one program: its deadline and ending, what it does with each line of standard error, and
/// what its readers gave.
struct Watch<'a> {
    group: i32,
    deadline: Instant,
    ending: Ending,
    take: Option<TakeLine<'a>>,
    end_on_stop: bool,
    /// Set when a line's `take` answered `false` and the group is to be ended.
    stop_asked: bool,
    receiver: mpsc::Receiver<Event>,
    stdout: Option<io::Result<(Vec<u8>, bool)>>,
    stderr: Option<io::Result<Vec<u8>>>,
}

impl<'a> Watch<'a> {
    fn new(group: i32, (deadline, ending): (Instant, Ending), stderr: Stderr<'a>, receiver: mpsc::Receiver<Event>) -> Watch<'a> {
        let (take, end_on_stop) = match stderr {
            Stderr::Tail => (None, false),
            Stderr::Lines { take, end } => (Some(take), end),
        };
        Watch {
            group,
            deadline,
            ending,
            take,
            end_on_stop,
            stop_asked: false,
            receiver,
            stdout: None,
            stderr: None,
        }
    }

    /// Takes what a reader sent.
    fn handle(&mut self, event: Event) {
        match event {
            Event::Out(read) => self.stdout = Some(read),
            Event::Err(read) => self.stderr = Some(read),
            Event::Line(line) => {
                if let Some(take) = self.take.as_mut()
                    && !take(&line)
                {
                    self.take = None;
                    self.stop_asked = self.end_on_stop;
                }
            }
        }
    }

    /// Takes the next thing a reader sends within `wait`; else waits that long.
    fn next_event(&mut self, wait: Duration) {
        match self.receiver.recv_timeout(wait) {
            Ok(event) => self.handle(event),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(wait),
        }
    }

    /// Waits for `child`, the leader of the group, to exit, taking what the readers send meanwhile; at the deadline
    /// ends the group and reaps it.
    fn exited(&mut self, child: &mut Child) -> Result<ExitStatus, NotRan> {
        loop {
            if std::mem::take(&mut self.stop_asked) {
                end_group(child, self.group, self.ending);
            }
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if Instant::now() >= self.deadline => {
                    end_group(child, self.group, self.ending);
                    return match child.wait() {
                        Ok(_) => Err(NotRan::TimedOut { group: self.group }),
                        Err(error) => Err(NotRan::Failed(format!("it could not be reaped after its kill: {error}"))),
                    };
                }
                Ok(None) => self.next_event(POLL),
                Err(error) => {
                    kill_group(self.group);
                    // Reaped if it can be; the wait's own failure is the one said.
                    let _ = child.wait();
                    return Err(NotRan::Failed(format!("its exit could not be read: {error}")));
                }
            }
        }
    }

    /// Waits until both readers have ended, until the deadline: past it the group is killed, so its holders let the
    /// pipes go, and a reader still held after [`READER_GRACE`] is left behind.
    fn drained(&mut self) -> Result<(), NotRan> {
        while self.stdout.is_none() || self.stderr.is_none() {
            if std::mem::take(&mut self.stop_asked) {
                kill_group(self.group);
            }
            if Instant::now() >= self.deadline {
                kill_group(self.group);
                let grace = Instant::now() + READER_GRACE;
                while (self.stdout.is_none() || self.stderr.is_none()) && Instant::now() < grace {
                    self.next_event(POLL);
                }
                return Err(NotRan::TimedOut { group: self.group });
            }
            self.next_event(POLL);
        }
        Ok(())
    }

    /// What the program gave once it exited with `status` and its readers ended.
    fn ran(self, status: ExitStatus) -> Result<Ran, NotRan> {
        let unread = |error: io::Error| NotRan::Failed(format!("its output could not be read: {error}"));
        let (stdout, over) = self.stdout.unwrap_or(Ok((Vec::new(), false))).map_err(unread)?;
        let stderr = self.stderr.unwrap_or(Ok(Vec::new())).map_err(unread)?;
        if over {
            return Err(NotRan::OverCap);
        }
        Ok(Ran { status, stdout, stderr })
    }
}

/// Reads the standard error `pipe` to its end and sends what it read: each line ([`Stderr::Lines`]) when `split`,
/// then the end of the stream, its tail when not `split`.
fn stderr_read(mut pipe: ChildStderr, split: bool, sender: &mpsc::Sender<Event>) {
    let read = if split {
        lines_sent(&mut pipe, sender).map(|()| Vec::new())
    } else {
        tail(pipe)
    };
    // A runner that stopped receiving has stopped waiting for this reader.
    let _ = sender.send(Event::Err(read));
}

/// Sends each line of `pipe` as it ends ([`LineSplitter`]), and the bytes after the last end.
fn lines_sent(pipe: &mut ChildStderr, sender: &mpsc::Sender<Event>) -> io::Result<()> {
    let mut splitter = LineSplitter::default();
    let mut chunk = [0_u8; 8192];
    let send = |line: &[u8]| {
        // A runner that stopped receiving has stopped waiting for this reader.
        let _ = sender.send(Event::Line(line.to_vec()));
    };
    loop {
        let read = pipe.read(&mut chunk)?;
        if read == 0 {
            splitter.finish(send);
            return Ok(());
        }
        splitter.feed(&chunk[..read], send);
    }
}

/// Cuts bytes into lines as the Windows box's forwarder does (.NET's `ReadLine`): CR LF, LF and a lone CR each end a
/// line, and the bytes after the last end are a line of their own when there are any. A line keeps its first
/// [`LINE_CAP`] bytes, the rest dropped.
#[derive(Default)]
struct LineSplitter {
    line: Vec<u8>,
    after_cr: bool,
}

impl LineSplitter {
    /// Takes `bytes`, giving `emit` each line they end, without its end.
    fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        for &byte in bytes {
            let after_cr = std::mem::take(&mut self.after_cr);
            match byte {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => {
                    emit(&self.line);
                    self.line.clear();
                    self.after_cr = byte == b'\r';
                }
                _ if self.line.len() < LINE_CAP => self.line.push(byte),
                _ => {}
            }
        }
    }

    /// Gives `emit` the bytes after the last line end, when there are any.
    fn finish(self, mut emit: impl FnMut(&[u8])) {
        if !self.line.is_empty() {
            emit(&self.line);
        }
    }
}

/// Starts `command`. Every program the box starts goes through the bounded runner, which owns its process group.
#[expect(
    clippy::disallowed_methods,
    reason = "the spawn lock is the Windows gate's; a Unix box starts its programs here"
)]
fn spawned(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

/// Ends the group `group`, whose leader is `child`, as `ending` says: `SIGTERM` first and a wait for the leader's exit
/// within the grace, then `SIGKILL` for whatever is left.
fn end_group(child: &mut Child, group: i32, ending: Ending) {
    if let Ending::TermThenKill(grace) = ending {
        signal_group(group, libc::SIGTERM);
        let until = Instant::now() + grace;
        while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
            thread::sleep(POLL);
        }
    }
    kill_group(group);
}

/// Kills every process of the group `group` with `SIGKILL`. A group already gone is no failure, and the caller reaps
/// what it started, so nothing is said.
fn kill_group(group: i32) {
    signal_group(group, libc::SIGKILL);
}

/// Sends `signal` to every process of the group `group`; a group already gone is no failure.
fn signal_group(group: i32, signal: i32) {
    // SAFETY: `killpg` takes two integers and touches no memory; `group` is the group the runner made for its own child.
    unsafe {
        libc::killpg(group, signal);
    }
}

/// All of `pipe`, its first `cap` bytes kept, and whether it held more.
fn capped(mut pipe: ChildStdout, cap: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut over = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let read = pipe.read(&mut chunk)?;
        if read == 0 {
            return Ok((kept, over));
        }
        let room = cap.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(room)]);
        over |= read > room;
    }
}

/// The last [`STDERR_TAIL`] bytes of `pipe`.
fn tail(mut pipe: ChildStderr) -> io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = pipe.read(&mut chunk)?;
        if read == 0 {
            return Ok(kept);
        }
        kept.extend_from_slice(&chunk[..read]);
        if kept.len() > STDERR_TAIL {
            kept.drain(..kept.len() - STDERR_TAIL);
        }
    }
}

/// How `status` reads in a failure line: `status <n>`, or `signal <n>` for a program a signal ended.
#[must_use]
pub fn status_text(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("status {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "no status".to_owned(),
    }
}

/// The `PATH` of the box user's login shell, `$SHELL` (`/bin/zsh` when `SHELL` is unset or empty) as
/// [`login_path_of`] reads it: an ssh command's own `PATH` lacks what the login profile adds, such as dotnet's folder.
/// Read anew on every call.
///
/// # Errors
///
/// Why it cannot be used, as [`login_path_of`] says it.
pub fn login_path() -> Result<String, String> {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| OsString::from(DEFAULT_SHELL));
    login_path_of(&shell)
}

/// The `PATH` the login shell `shell` says when run as `shell -l -c` with a command that prints it between two
/// [`crate::lines::PATH_MARKER`]s, in the bounded runner within [`LOGIN_PATH_LIMIT`]; only the text between the markers
/// is taken ([`crate::lines::marked_path`]), so anything the login profile prints around it is left out.
///
/// # Errors
///
/// Why it cannot be used, for [`crate::lines::login_path_line`]: the shell did not start, failed, ran past the limit,
/// or wrote more than [`LOGIN_PATH_CAP`], no marked `PATH`, an empty one, or text that is not UTF-8.
pub fn login_path_of(shell: &std::ffi::OsStr) -> Result<String, String> {
    let shown = shell.to_string_lossy().into_owned();
    let mut command = Command::new(shell);
    let marker = crate::lines::PATH_MARKER;
    command.args(["-l", "-c", &format!("printf '\\n{marker}%s{marker}\\n' \"$PATH\"")]);
    let ran = run_bounded(command, LOGIN_PATH_CAP, Instant::now() + LOGIN_PATH_LIMIT).map_err(|not| match not {
        NotRan::Start(error) => format!("cannot run {shown}: {error}"),
        NotRan::TimedOut { .. } => format!("{shown} -l did not answer in {} s", LOGIN_PATH_LIMIT.as_secs()),
        NotRan::OverCap => format!("{shown} -l wrote more than {} KiB", LOGIN_PATH_CAP / 1024),
        NotRan::Failed(why) => format!("{shown} -l failed: {why}"),
    })?;
    if !ran.status.success() {
        return Err(format!("{shown} -l exited with {}", status_text(ran.status)));
    }
    crate::lines::marked_path(&ran.stdout).map_err(|why| format!("{shown} -l {why}"))
}

/// `command` given `path` as its `PATH`, which it is also found along; `None` leaves the box's own.
pub fn with_path<'c>(command: &'c mut Command, path: Option<&str>) -> &'c mut Command {
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a runner that cannot start sh must fail the test loudly")]
mod tests {
    use std::io;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::{Ending, LINE_CAP, LineSplitter, NotRan, Stderr, login_path, login_path_of, run_bounded, run_watched, status_text};

    /// How long a program the tests expect to end may take: a hang guard only.
    const HANG_GUARD: Duration = Duration::from_secs(120);

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    /// Whether any process of the group `group` is left.
    fn group_alive(group: i32) -> bool {
        // SAFETY: signal 0 only asks whether the group exists; it touches no memory.
        let sent = unsafe { libc::killpg(group, 0) };
        sent == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[test]
    fn the_runner_kills_the_whole_group_at_its_deadline() {
        let ran = run_bounded(sh("sleep 600 & sleep 600"), 1024, Instant::now());
        let group = match &ran {
            Err(NotRan::TimedOut { group }) => Some(*group),
            _ => None,
        };
        assert!(group.is_some(), "not timed out: {ran:?}");
        let group = group.expect("the group was killed");
        // The killed sleeps are reaped by the runner and by launchd; the guard only ends a broken run.
        let guard = Instant::now() + HANG_GUARD;
        while group_alive(group) && Instant::now() < guard {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!group_alive(group), "the background sleep of the group was killed too");
    }

    #[test]
    fn the_runner_keeps_stdout_and_the_end_of_stderr_and_the_status() {
        let ran = run_bounded(sh("printf out; printf err >&2; exit 3"), 1024, Instant::now() + HANG_GUARD).expect("sh runs");
        assert_eq!(ran.stdout, b"out");
        assert_eq!(ran.stderr, b"err");
        assert_eq!(status_text(ran.status), "status 3");
    }

    #[test]
    fn stdout_past_the_cap_is_over_cap() {
        let ran = run_bounded(sh("printf 12345"), 4, Instant::now() + HANG_GUARD);
        assert!(matches!(ran, Err(NotRan::OverCap)), "{ran:?}");
        let fits = run_bounded(sh("printf 1234"), 4, Instant::now() + HANG_GUARD).expect("sh runs");
        assert_eq!(fits.stdout, b"1234", "exactly the cap fits");
    }

    /// The lines `bytes`, fed in `chunks` of the given sizes, are cut into.
    fn split(bytes: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        let mut splitter = LineSplitter::default();
        for part in bytes.chunks(chunk) {
            splitter.feed(part, |line| lines.push(line.to_vec()));
        }
        splitter.finish(|line| lines.push(line.to_vec()));
        lines
    }

    #[test]
    fn lines_end_at_a_line_feed_a_carriage_return_or_both_as_the_windows_box_cuts_them() {
        for chunk in [1, 3, 64] {
            assert_eq!(
                split(b"a\r\nb\nc\rd", chunk),
                [b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
            );
            assert_eq!(split(b"\r\r\n", chunk), [Vec::<u8>::new(), Vec::new()], "a lone CR, then CR LF");
            assert_eq!(split(&[0xff, b'\n'], chunk), [vec![0xff]], "bytes are handed on as they are");
        }
    }

    #[test]
    fn a_line_past_the_line_cap_keeps_its_first_bytes_and_the_next_line_is_whole() {
        let mut bytes = vec![b'x'; LINE_CAP + 10];
        bytes.extend_from_slice(b"\nnext\n");
        assert_eq!(split(&bytes, 8192), [vec![b'x'; LINE_CAP], b"next".to_vec()]);
    }

    #[test]
    fn each_line_of_stderr_is_handed_on_as_it_comes_and_a_refusal_ends_the_group_asked_first() {
        let mut lines = Vec::new();
        let mut take = |line: &[u8]| {
            lines.push(String::from_utf8_lossy(line).into_owned());
            false
        };
        let started = Instant::now();
        let stderr = Stderr::Lines { take: &mut take, end: true };
        let ran = run_watched(
            sh("trap 'echo termed; exit 7' TERM; echo first >&2; echo second >&2; sleep 600 & wait"),
            1024,
            started + HANG_GUARD,
            stderr,
            Ending::TermThenKill(Duration::from_secs(2)),
        )
        .expect("sh runs to its end");
        assert!(started.elapsed() < Duration::from_secs(60), "ended long before its sleep");
        assert_eq!(lines, ["first"], "no line after the refusal");
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "termed\n", "asked with SIGTERM first");
        assert_eq!(status_text(ran.status), "status 7");
        assert!(ran.stderr.is_empty());
    }

    #[test]
    fn a_program_that_is_not_found_does_not_start() {
        let ran = run_bounded(Command::new("gate-box-no-such-program"), 4, Instant::now() + HANG_GUARD);
        assert!(
            matches!(ran, Err(NotRan::Start(ref error)) if error.kind() == io::ErrorKind::NotFound),
            "{ran:?}"
        );
    }

    #[test]
    fn the_login_shell_gives_a_path() {
        let path = login_path().expect("the login shell's PATH is read");
        assert!(!path.is_empty());
        assert!(path.contains("/usr/bin"), "{path}");
    }

    #[test]
    fn a_login_profile_that_prints_noise_leaves_the_path_clean() {
        let folder = std::env::temp_dir().join(format!("gate-box-unix-shell-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("the shell's folder is made");
        let shell = folder.join("noisy-shell");
        // Stands in for a login shell whose profile prints a banner: `-l -c <command>` runs the command after it.
        std::fs::write(
            &shell,
            "#!/bin/sh\necho 'Welcome to the builder'\nprintf 'no newline'\nPATH=/fake/bin:/usr/bin\nexec /bin/sh -c \"$3\"\n",
        )
        .expect("the shell is written");
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the shell is made runnable");

        let path = login_path_of(shell.as_os_str());
        // Best effort: a folder left behind is in the temp folder and named for this process.
        let _ = std::fs::remove_dir_all(&folder);
        assert_eq!(path, Ok("/fake/bin:/usr/bin".to_owned()));
    }
}
