//! The helper's run: reach the box's control pipe, say hello, start the command the box sends, report it and wait
//! for the box to say it is done. Each step is bounded, and each way it ends is a [`HelperExit`].

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gate_win32::{DesktopLaunch, EnvironmentBlock, LaunchError, PipeClient, Released, Suspended};

use crate::protocol::{Message, ReadError, Spawn, read_message, write_message};
use crate::version::build_version;
use crate::{CONNECT_LIMIT, DONE_LIMIT, HelperExit, MESSAGE_LIMIT, parse_arg0};

/// `ERROR_FILE_NOT_FOUND`: no pipe of the name exists yet.
const ERROR_FILE_NOT_FOUND: i32 = 2;

/// `ERROR_PIPE_BUSY`: the pipe's one instance is held, maybe by a client the box is about to drop.
const ERROR_PIPE_BUSY: i32 = 231;

/// `ERROR_INVALID_PARAMETER`: the code `Failed` carries for a launch refused without a system error.
const ERROR_INVALID_PARAMETER: u32 = 87;

/// The pause between two tries to reach a pipe that is not there yet or busy.
const RETRY_PAUSE: Duration = Duration::from_millis(20);

/// The input desktop's name when it is the user's own.
const DEFAULT_DESKTOP: &str = "Default";

/// The exit code a child the helper ends is given: a command that never ran, as `gate_win32` ends one.
const ENDED_UNSTARTED: u32 = 1;

/// The seam that caps every limit, in milliseconds, in a build with the `test-seams` feature.
#[cfg(feature = "test-seams")]
const TEST_LIMIT_VARIABLE: &str = "GATE_DESKTOP_TEST_LIMIT_MS";

/// How long each step may take.
#[derive(Clone, Copy, Debug)]
struct Limits {
    connect: Duration,
    message: Duration,
    done: Duration,
}

impl Limits {
    /// The limits an installed helper keeps: [`CONNECT_LIMIT`], [`MESSAGE_LIMIT`] and [`DONE_LIMIT`].
    const BUILT: Limits = Limits {
        connect: CONNECT_LIMIT,
        message: MESSAGE_LIMIT,
        done: DONE_LIMIT,
    };

    /// The built limits, each capped by `GATE_DESKTOP_TEST_LIMIT_MS` when it is set. `None` for a seam that is set but
    /// is not a whole number of milliseconds.
    #[cfg(feature = "test-seams")]
    fn with_seam() -> Option<Limits> {
        let Some(cap) = std::env::var_os(TEST_LIMIT_VARIABLE) else {
            return Some(Limits::BUILT);
        };
        let cap = Duration::from_millis(cap.to_str()?.parse::<u64>().ok()?);
        Some(Limits {
            connect: CONNECT_LIMIT.min(cap),
            message: MESSAGE_LIMIT.min(cap),
            done: DONE_LIMIT.min(cap),
        })
    }
}

/// Runs the helper with its arguments, `args`, the program's name left out: exactly one, the `Arg0`
/// `<box pid>:<the pipe's random part>`.
#[must_use]
pub fn run(args: impl IntoIterator<Item = OsString>) -> HelperExit {
    let mut args = args.into_iter();
    let (Some(arg0), None) = (args.next(), args.next()) else {
        return HelperExit::BadArgument;
    };
    let Some((box_pid, name)) = arg0.to_str().and_then(parse_arg0) else {
        return HelperExit::BadArgument;
    };
    #[cfg(feature = "test-seams")]
    let Some(limits) = Limits::with_seam() else {
        return HelperExit::BadArgument;
    };
    #[cfg(not(feature = "test-seams"))]
    let limits = Limits::BUILT;
    match serve(box_pid, &name, limits) {
        Ok(exit) | Err(exit) => exit,
    }
}

/// The run once `Arg0` is read: every step in order, the first that ends the run giving its exit.
fn serve(box_pid: u32, name: &str, limits: Limits) -> Result<HelperExit, HelperExit> {
    let pipe = connect(name, limits.connect)?;
    match pipe.server_pid() {
        Ok(pid) if pid == box_pid => {}
        Ok(_) => return Err(HelperExit::ServerRefused),
        Err(_) => return Err(HelperExit::Internal),
    }
    send(&pipe, &hello(), limits.message)?;
    let spawn = receive_spawn(&pipe, limits.message)?;
    let child = start(&pipe, spawn, limits.message)?;
    let released = child.release().map_err(|_| HelperExit::Internal)?;
    let report = Message::Child {
        pid: released.pid,
        process: released.process.addr() as u64,
        thread: released.thread.addr() as u64,
    };
    if send(&pipe, &report, limits.message).is_err() {
        end_child(&released, limits.message);
        return Err(HelperExit::BoxGone);
    }
    Ok(await_done(&pipe, &released, limits))
}

/// The control pipe `name`, opened as its owner, this process's user, made it, tried again while it is not there yet
/// or busy until `limit` has passed.
fn connect(name: &str, limit: Duration) -> Result<PipeClient, HelperExit> {
    let user = gate_win32::current_user_sid().map_err(|_| HelperExit::Internal)?;
    let deadline = Instant::now() + limit;
    loop {
        match PipeClient::open(name, &user) {
            Ok(pipe) => return Ok(pipe),
            Err(error) if matches!(error.raw_os_error(), Some(ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY)) => {}
            Err(_) => return Err(HelperExit::ServerRefused),
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(HelperExit::NoPipe);
        }
        std::thread::sleep(RETRY_PAUSE.min(left));
    }
}

/// The `Hello` this helper sends: its build and the input desktop's name, empty when it cannot be read.
fn hello() -> Message {
    let desktop = gate_win32::input_desktop_name().unwrap_or_default();
    Message::Hello {
        version: build_version(),
        ok: desktop == DEFAULT_DESKTOP,
        desktop,
    }
}

/// Writes `message` within `limit`; any failure is the box gone, the stream being out of step.
fn send(pipe: &PipeClient, message: &Message, limit: Duration) -> Result<(), HelperExit> {
    write_message(pipe, message, Instant::now() + limit).map_err(|_| HelperExit::BoxGone)
}

/// The box's `Spawn`, read within `limit`.
fn receive_spawn(pipe: &PipeClient, limit: Duration) -> Result<Spawn, HelperExit> {
    match read_message(pipe, Instant::now() + limit) {
        Ok(Message::Spawn(spawn)) => Ok(spawn),
        Ok(_) | Err(ReadError::Empty | ReadError::TooLong | ReadError::Decode(_)) => Err(HelperExit::BadSpawn),
        Err(ReadError::Closed) => Err(HelperExit::BoxGone),
        Err(ReadError::TimedOut) => Err(HelperExit::TimedOut),
        Err(ReadError::Woken(_) | ReadError::Io(_)) => Err(HelperExit::Internal),
    }
}

/// The command `spawn` asks for, created suspended on the desktop with the two output handles the box placed in this
/// process, once they and the environment pass their checks. A command that cannot be created is reported to the box
/// as `Failed`, written within `limit`.
fn start(pipe: &PipeClient, spawn: Spawn, limit: Duration) -> Result<Suspended, HelperExit> {
    let Spawn {
        application,
        command_line,
        directory,
        environment,
        stdout,
        stderr,
    } = spawn;
    if stdout == stderr {
        return Err(HelperExit::BadSpawn);
    }
    let stdout = received_output(stdout, pipe.handle())?;
    let stderr = received_output(stderr, pipe.handle())?;
    let environment = EnvironmentBlock::from_wide(environment).map_err(|_| HelperExit::BadSpawn)?;
    let launch = DesktopLaunch {
        application: PathBuf::from(OsString::from_wide(&application)),
        command_line,
        directory: PathBuf::from(OsString::from_wide(&directory)),
        environment,
    };
    launch.start_suspended(stdout, stderr).map_err(|error| {
        let code = match error {
            LaunchError::CreateProcess { code, .. } => code,
            LaunchError::Setup { error, .. } => error.raw_os_error().map_or(ERROR_INVALID_PARAMETER, i32::cast_unsigned),
        };
        // The exit says the spawn failed whether or not the box reads why.
        let _reported = send(pipe, &Message::Failed { code }, limit);
        HelperExit::SpawnFailed
    })
}

/// The output handle the box placed in this process as `value`, once it passes
/// [`gate_win32::check_received_output`] against the control pipe.
fn received_output<'a>(value: u64, control: BorrowedHandle<'_>) -> Result<BorrowedHandle<'a>, HelperExit> {
    let value = usize::try_from(value).map_err(|_| HelperExit::BadSpawn)?;
    // SAFETY: the box placed the handle in this process and closes it only once it has read the child's report, after
    // the last use of the borrow, which ends when the command holds its own copy.
    unsafe { gate_win32::check_received_output(value, control) }.map_err(|_| HelperExit::BadSpawn)
}

/// Waits up to the done limit for the box's `Done`. The child is ended only when the pipe reads closed or fails first: the
/// box is gone and nothing will resume it. On a deadline passed, or anything else read, it is left as it is, since the
/// box may have taken it.
fn await_done(pipe: &PipeClient, released: &Released, limits: Limits) -> HelperExit {
    match read_message(pipe, Instant::now() + limits.done) {
        Ok(Message::Done) => HelperExit::Done,
        Err(ReadError::Closed | ReadError::Io(_)) => {
            end_child(released, limits.message);
            HelperExit::BoxGone
        }
        Err(ReadError::TimedOut) => HelperExit::NoDone,
        Ok(_) | Err(ReadError::Woken(_) | ReadError::Empty | ReadError::TooLong | ReadError::Decode(_)) => HelperExit::BadMessage,
    }
}

/// Ends the child this helper created and released, found again by its pid, this process as its parent and its
/// creation time, and waits up to `bound` for its end. Nothing is reported: the helper exits `BoxGone` either way.
fn end_child(released: &Released, bound: Duration) {
    if let Ok(Some(process)) = gate_win32::process_of_child(released.pid, std::process::id(), released.created) {
        let _ended = gate_win32::end_process(process.as_handle(), ENDED_UNSTARTED, bound);
    }
}
