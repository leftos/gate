//! What the box reads of one process by its pid: its user, its parent and when it started, from macOS's
//! `proc_pidinfo(PROC_PIDTBSDINFO)`. A job record names its group's leader by this start, and the process lines tell a
//! process from a later one that took its pid by it.

/// When a process started: seconds and microseconds after the Unix epoch, as the kernel keeps it (`pbi_start_tvsec`,
/// `pbi_start_tvusec`), the microseconds below a million.
pub(crate) type Start = (u64, u32);

/// What the box sees of the process with one pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Process {
    /// No process has that pid.
    Absent,
    /// A process has it.
    Present {
        /// Its effective user id.
        uid: u32,
        /// Its parent's pid.
        parent: u32,
        /// When it started.
        started: Start,
    },
    /// It could not be looked at.
    Unknown,
}

/// The process whose pid is `pid`, from `proc_pidinfo(PROC_PIDTBSDINFO)`, one that exited and is not yet reaped
/// included: a group's leader that ended on a signal is still its group's leader, with its start, until its run reaps
/// it.
#[cfg(target_os = "macos")]
pub(crate) fn process(pid: i32) -> Process {
    bsd_info(pid, 1)
}

/// The running process whose pid is `pid`, from `proc_pidinfo(PROC_PIDTBSDINFO)`: one that exited and is not yet
/// reaped is [`Process::Absent`].
#[cfg(target_os = "macos")]
pub(crate) fn running(pid: i32) -> Process {
    bsd_info(pid, 0)
}

/// The process whose pid is `pid`, from `proc_pidinfo(PROC_PIDTBSDINFO)` with `arg`: 1 to find one that exited and is
/// not yet reaped as well, 0 for a running one only.
#[cfg(target_os = "macos")]
fn bsd_info(pid: i32, arg: u64) -> Process {
    // SAFETY: an all-zero `proc_bsdinfo` is a valid value of a plain C struct.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let Ok(size) = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return Process::Unknown;
    };
    // SAFETY: `info` is `size` bytes and lives across the call, which writes at most that much into it.
    let filled = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, arg, (&raw mut info).cast(), size) };
    if filled != size {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) if filled <= 0 => Process::Absent,
            _ => Process::Unknown,
        };
    }
    match u32::try_from(info.pbi_start_tvusec) {
        Ok(micros) if micros < 1_000_000 => Process::Present {
            uid: info.pbi_uid,
            parent: info.pbi_ppid,
            started: (info.pbi_start_tvsec, micros),
        },
        _ => Process::Unknown,
    }
}

/// A process is looked at only on macOS.
#[cfg(not(target_os = "macos"))]
pub(crate) fn process(_pid: i32) -> Process {
    Process::Unknown
}

/// When the process whose pid is `pid` started ([`process`]); `None` when no process has it or it could not be looked
/// at.
pub(crate) fn start_of(pid: i32) -> Option<Start> {
    match process(pid) {
        Process::Present { started, .. } => Some(started),
        Process::Absent | Process::Unknown => None,
    }
}

#[cfg(all(test, target_os = "macos"))]
#[expect(clippy::expect_used, reason = "a test whose shell cannot start or be waited on must fail loudly")]
mod tests {
    use std::io;
    use std::process::{Command, Stdio};

    use super::start_of;

    /// Waits for the child `pid` to exit, leaving it unreaped: `waitid(P_PID, WEXITED | WNOWAIT)`.
    fn exited_unreaped(pid: i32) {
        let id = libc::id_t::try_from(pid).expect("a pid is an id_t");
        loop {
            // SAFETY: an all-zero `siginfo_t` is a valid value, which `waitid` fills.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: `info` lives across the call, which writes only into it.
            let waited = unsafe { libc::waitid(libc::P_PID, id, &raw mut info, libc::WEXITED | libc::WNOWAIT) };
            if waited == 0 {
                return;
            }
            let error = io::Error::last_os_error();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted, "the shell's exit is waited on: {error}");
        }
    }

    #[test]
    fn a_process_that_exited_and_is_not_yet_reaped_keeps_its_start() {
        #[expect(
            clippy::disallowed_methods,
            reason = "the spawn lock is the Windows gate's; this test stands in for a run"
        )]
        let mut shell = Command::new("/bin/sh")
            .args(["-c", "read line; exit 0"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("sh starts");
        let pid = i32::try_from(shell.id()).expect("a pid is a pid_t");
        // The shell waits on its input, so it is running here.
        let running = start_of(pid);
        drop(shell.stdin.take());
        exited_unreaped(pid);
        let zombie = start_of(pid);
        shell.wait().expect("the shell is reaped");
        let running = running.expect("the running shell's start is read");
        assert_eq!(
            zombie,
            Some(running),
            "an exited leader not yet reaped is read with the start it ran with"
        );
    }
}
