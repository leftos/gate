//! The desktop helper and the protocol the box speaks with it. The box starts the helper in the logged-on session
//! with an `Arg0` naming the box's pid and its control pipe; the helper connects, says which desktop takes input
//! ([`protocol::Message::Hello`]), starts the command the box sends ([`protocol::Message::Spawn`]) suspended on that
//! desktop with the box's two output pipes, reports it ([`protocol::Message::Child`] or
//! [`protocol::Message::Failed`]), and waits for [`protocol::Message::Done`]. Its exit code is a [`HelperExit`].

mod helper;
pub mod protocol;
pub mod version;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

pub use helper::run;

/// The prefix of the control pipe's name, `\\.\pipe\gate-desktop-<32 hex digits>`, as the box creates it with
/// `gate_win32::PipeServer::create`.
pub const PIPE_PREFIX: &str = "gate-desktop";

/// The Task Scheduler path of the desktop helper's task, which the box runs to start the helper in the logged-on
/// user's session.
pub const TASK_PATH: &str = "\\gate\\desktop-helper";

/// The file name of the desktop helper, the `gate-desktop-helper` binary of this crate.
pub const HELPER_FILE: &str = "gate-desktop-helper.exe";

/// Where the desktop helper must be: [`HELPER_FILE`] in the folder of the running program, `gate.exe`. The task's
/// action and the image of the helper that connects are both checked against it.
///
/// # Errors
///
/// The system's error when the running program's path cannot be read.
pub fn helper_path() -> io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name(HELPER_FILE))
}

/// The longest the helper tries to reach the control pipe.
pub const CONNECT_LIMIT: Duration = Duration::from_secs(10);

/// The longest the helper takes to write one message, or waits for `Spawn`.
pub const MESSAGE_LIMIT: Duration = Duration::from_secs(5);

/// The longest the helper waits for `Done` once it has reported its child.
pub const DONE_LIMIT: Duration = Duration::from_secs(30);

/// The digits of the pipe name's random part: 128 bits in lowercase hex.
const HEX_DIGITS: usize = 32;

/// How the helper ended: its process exit code.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperExit {
    /// The box read the child and said `Done`.
    Done = 0,
    /// `Arg0` was missing or not `<box pid>:<32 lowercase hex digits>`, or a test seam was malformed.
    BadArgument = 10,
    /// No pipe of the name appeared before the connect limit.
    NoPipe = 11,
    /// The pipe could not be opened, its owner, DACL or label was not the box's, or its server's pid was not the box's
    /// or could not be read.
    ServerRefused = 12,
    /// The box closed or disconnected the pipe: the write of `Hello` or `Child`, or the read of `Spawn` or `Done`,
    /// found it closed, or failed with `ERROR_BROKEN_PIPE`, `ERROR_PIPE_NOT_CONNECTED` or `ERROR_NO_DATA`. A child
    /// already created was ended.
    BoxGone = 13,
    /// The box's `Spawn` was malformed, another message came in its place, or one of its values failed a check.
    BadSpawn = 14,
    /// `CreateProcessW` refused the command; the box was sent `Failed` with the system's code.
    SpawnFailed = 15,
    /// The box never said `Done`; the child was left as it was.
    NoDone = 16,
    /// The box sent something other than `Done` after the child; the child was left as it was.
    BadMessage = 17,
    /// The box sent no `Spawn` before the message limit, or did not take the child's report within it; a child already
    /// created was left as it was.
    TimedOut = 18,
    /// Any other failure of the system, a `Hello` the box did not take within the message limit among them; a child
    /// already created was left as it was.
    Internal = 19,
    /// The launch failed before `CreateProcessW` was called (a refused application or folder, or a handle or attribute
    /// list that could not be made); no `Failed` was sent, since this is no failure of the command itself.
    SetupFailed = 20,
    /// The box's pipe closed or failed after the child was created (it read closed, or a read or write of it failed
    /// with `ERROR_BROKEN_PIPE`, `ERROR_PIPE_NOT_CONNECTED` or `ERROR_NO_DATA`), and the helper could not show the
    /// child ended: ending it or waiting for its end failed, or it had not ended within the message limit.
    ChildNotEnded = 21,
    /// The helper runs above medium integrity or elevated, which a helper the Task Scheduler starts with the user's
    /// least-privileged token never does; it touched no pipe.
    WrongIntegrity = 22,
}

impl HelperExit {
    /// The exit code.
    #[must_use]
    pub fn code(self) -> u32 {
        self as u32
    }

    /// The exit code as a process's `main` returns it.
    #[must_use]
    pub fn exit_code(self) -> std::process::ExitCode {
        // Every code is below 256.
        std::process::ExitCode::from(u8::try_from(self.code()).unwrap_or(u8::MAX))
    }
}

/// The `Arg0` the box gives the helper: `<box_pid>:<the random part of pipe>`, for `pipe`, the full name of a pipe
/// the box made with [`PIPE_PREFIX`]. `None` for a pid of 0 or a name not of that form.
#[must_use]
pub fn arg0(box_pid: u32, pipe: &str) -> Option<String> {
    let hex = pipe.strip_prefix(r"\\.\pipe\")?.strip_prefix(PIPE_PREFIX)?.strip_prefix('-')?;
    (box_pid != 0 && is_pipe_hex(hex)).then(|| format!("{box_pid}:{hex}"))
}

/// The box's pid and the pipe's full name from `Arg0`; `None` unless it is `<pid>:<hex>`, the pid all ASCII digits,
/// with no leading 0, and not 0, and the hex exactly 32 lowercase hex digits: the form [`arg0`] writes. The name is
/// always built here, never taken from `Arg0`.
fn parse_arg0(text: &str) -> Option<(u32, String)> {
    let (pid, hex) = text.split_once(':')?;
    if pid.is_empty() || pid.starts_with('0') || !pid.bytes().all(|byte| byte.is_ascii_digit()) || !is_pipe_hex(hex) {
        return None;
    }
    let pid = pid.parse::<u32>().ok().filter(|&pid| pid != 0)?;
    Some((pid, format!(r"\\.\pipe\{PIPE_PREFIX}-{hex}")))
}

/// Whether `hex` is a pipe name's random part: exactly 32 digits of `0-9a-f`.
fn is_pipe_hex(hex: &str) -> bool {
    hex.len() == HEX_DIGITS && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose program path cannot be read must fail loudly")]
mod tests {
    use gate_win32::TaskArgument;

    use super::{HELPER_FILE, PIPE_PREFIX, arg0, helper_path};

    #[test]
    fn every_arg0_is_a_task_argument() {
        for (pid, digit) in [(1, "0"), (u32::MAX, "f")] {
            let pipe = format!(r"\\.\pipe\{PIPE_PREFIX}-{}", digit.repeat(32));
            let written = arg0(pid, &pipe).expect("the pipe's name makes an Arg0");

            assert_eq!(TaskArgument::new(&written).map(|argument| argument.as_str().to_owned()), Some(written));
        }
    }

    #[test]
    fn the_helper_is_beside_the_running_program() {
        let program = std::env::current_exe().expect("the program's path is read");

        let helper = helper_path().expect("the helper's path is made");

        assert_eq!(helper.parent(), program.parent());
        assert_eq!(helper.file_name().and_then(|name| name.to_str()), Some(HELPER_FILE));
    }
}
