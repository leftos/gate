//! The desktop helper and the protocol the box speaks with it. The box starts the helper in the logged-on session
//! with an `Arg0` naming the box's pid and its control pipe; the helper connects, says which desktop takes input
//! ([`protocol::Message::Hello`]), starts the command the box sends ([`protocol::Message::Spawn`]) suspended on that
//! desktop with the box's two output pipes, reports it ([`protocol::Message::Child`] or
//! [`protocol::Message::Failed`]), and waits for [`protocol::Message::Done`]. Its exit code is a [`HelperExit`].

mod helper;
pub mod protocol;
pub mod version;

use std::time::Duration;

pub use helper::run;

/// The prefix of the control pipe's name, `\\.\pipe\gate-desktop-<32 hex digits>`, as the box creates it with
/// `gate_win32::PipeServer::create`.
pub const PIPE_PREFIX: &str = "gate-desktop";

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
    /// The pipe could not be opened, its owner, DACL or label was not the box's, or its server was not the box's pid.
    ServerRefused = 12,
    /// The box closed the pipe, or a write to it failed.
    BoxGone = 13,
    /// The box's `Spawn` was malformed, or one of its values failed a check.
    BadSpawn = 14,
    /// The command could not be created; the box was sent `Failed` with the system's code.
    SpawnFailed = 15,
    /// The box never said `Done`; the child was left as it was.
    NoDone = 16,
    /// The box sent something other than `Done` after the child; the child was left as it was.
    BadMessage = 17,
    /// The box sent no `Spawn` before the message limit.
    TimedOut = 18,
    /// Any other failure of the system.
    Internal = 19,
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

/// The box's pid and the pipe's full name from `Arg0`; `None` unless it is `<pid>:<hex>`, the pid all ASCII digits
/// and not 0, and the hex exactly 32 lowercase hex digits. The name is always built here, never taken from `Arg0`.
fn parse_arg0(text: &str) -> Option<(u32, String)> {
    let (pid, hex) = text.split_once(':')?;
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) || !is_pipe_hex(hex) {
        return None;
    }
    let pid = pid.parse::<u32>().ok().filter(|&pid| pid != 0)?;
    Some((pid, format!(r"\\.\pipe\{PIPE_PREFIX}-{hex}")))
}

/// Whether `hex` is a pipe name's random part: exactly 32 digits of `0-9a-f`.
fn is_pipe_hex(hex: &str) -> bool {
    hex.len() == HEX_DIGITS && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}
