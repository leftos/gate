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

/// Runs `command` in a process group of its own, standard input null, its standard output kept up to `cap` bytes and
/// the end of its standard error, until `deadline`: past it the whole group is killed (`SIGKILL`) and the program
/// reaped. Output a descendant still holds open once the program has exited is waited for until the deadline too.
///
/// # Errors
///
/// [`NotRan`]: it could not start, it ran past the deadline, it wrote more than `cap`, or its exit could not be read.
pub fn run_bounded(mut command: Command, cap: usize, deadline: Instant) -> Result<Ran, NotRan> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = spawned(&mut command).map_err(NotRan::Start)?;
    let group = i32::try_from(child.id()).map_err(|error| NotRan::Failed(format!("the process id is no pid_t: {error}")))?;
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let (sender, receiver) = mpsc::channel();
    let out_sender = sender.clone();
    thread::spawn(move || out_sender.send(Stream::Out(stdout.map(|pipe| capped(pipe, cap)))));
    thread::spawn(move || sender.send(Stream::Err(stderr.map(tail))));
    let status = exited(&mut child, group, deadline)?;
    let mut ran = Ran {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    let mut over = false;
    for _ in 0..2 {
        let Ok(stream) = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) else {
            kill_group(group);
            // The readers end once the group's holders are gone; one held past that is left behind.
            let _ = receiver.recv_timeout(READER_GRACE);
            return Err(NotRan::TimedOut { group });
        };
        match stream {
            Stream::Out(Some(Ok((bytes, more)))) => (ran.stdout, over) = (bytes, more),
            Stream::Err(Some(Ok(bytes))) => ran.stderr = bytes,
            Stream::Out(Some(Err(error))) | Stream::Err(Some(Err(error))) => {
                return Err(NotRan::Failed(format!("its output could not be read: {error}")));
            }
            Stream::Out(None) | Stream::Err(None) => {}
        }
    }
    if over { Err(NotRan::OverCap) } else { Ok(ran) }
}

/// What a reader thread read of one stream; `None` for a stream that was not piped.
enum Stream {
    Out(Option<io::Result<(Vec<u8>, bool)>>),
    Err(Option<io::Result<Vec<u8>>>),
}

/// Starts `command`. Every program the box starts goes through the bounded runner, which owns its process group.
#[expect(
    clippy::disallowed_methods,
    reason = "the spawn lock is the Windows gate's; a Unix box starts its programs here"
)]
fn spawned(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

/// Waits for `child`, the leader of the process group `group`, to exit, looking every [`POLL`]; at `deadline` kills
/// the group and reaps it.
fn exited(child: &mut Child, group: i32, deadline: Instant) -> Result<ExitStatus, NotRan> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_group(group);
                return match child.wait() {
                    Ok(_) => Err(NotRan::TimedOut { group }),
                    Err(error) => Err(NotRan::Failed(format!("it could not be reaped after its kill: {error}"))),
                };
            }
            Ok(None) => thread::sleep(POLL),
            Err(error) => {
                kill_group(group);
                // Reaped if it can be; the wait's own failure is the one said.
                let _ = child.wait();
                return Err(NotRan::Failed(format!("its exit could not be read: {error}")));
            }
        }
    }
}

/// Kills every process of the group `group` with `SIGKILL`. A group already gone is no failure, and the caller reaps
/// what it started, so nothing is said.
fn kill_group(group: i32) {
    // SAFETY: `killpg` takes two integers and touches no memory; `group` is the group the runner made for its own child.
    unsafe {
        libc::killpg(group, libc::SIGKILL);
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

    use super::{NotRan, login_path, login_path_of, run_bounded, status_text};

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
