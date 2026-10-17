//! The helper's run: reach the box's control pipe, say hello, start the command the box sends, report it and wait
//! for the box to say it is done. Each step is bounded, and each way it ends is a [`HelperExit`].

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gate_win32::{DesktopLaunch, EnvironmentBlock, LaunchError, PipeClient, PipeOwner, Released, Suspended};
use windows_sys::Win32::System::SystemServices::SECURITY_MANDATORY_MEDIUM_RID;

use crate::protocol::{DEFAULT_DESKTOP, FRAME_LIMIT, Message, ReadError, Spawn, WriteError, read_message, write_message};
use crate::version::build_version;
use crate::{CONNECT_LIMIT, DONE_LIMIT, HelperExit, MESSAGE_LIMIT, parse_arg0};

/// `ERROR_FILE_NOT_FOUND`: no pipe of the name exists yet.
const ERROR_FILE_NOT_FOUND: i32 = 2;

/// `ERROR_PIPE_BUSY`: the pipe's one instance is held, maybe by a client the box is about to drop.
const ERROR_PIPE_BUSY: i32 = 231;

/// The errors that say the other end of the pipe is gone: `ERROR_BROKEN_PIPE`, `ERROR_NO_DATA` and
/// `ERROR_PIPE_NOT_CONNECTED`.
const PIPE_GONE: [i32; 3] = [109, 232, 233];

/// The pause between two tries to reach a pipe that is not there yet or busy.
const RETRY_PAUSE: Duration = Duration::from_millis(20);

/// The exit code a child the helper ends is given: a command that never ran, as `gate_win32` ends one.
const ENDED_UNSTARTED: u32 = 1;

/// The seam that caps every limit, in milliseconds, in a build with the `test-seams` feature.
#[cfg(feature = "test-seams")]
const TEST_LIMIT_VARIABLE: &str = "GATE_DESKTOP_TEST_LIMIT_MS";

/// The seam that, set to `user`, makes the helper take a control pipe its own user owns in place of Administrators,
/// in a build with the `test-seams` feature.
#[cfg(feature = "test-seams")]
const TEST_PIPE_OWNER_VARIABLE: &str = "GATE_DESKTOP_TEST_PIPE_OWNER";

/// The seam that picks one step of the run to break, in a build with the `test-seams` feature: `exit-before-hello`,
/// `hang`, `wrong-version`, `lie-handle` or `exit-after-create`. An absent, empty or unknown value is no fault.
#[cfg(feature = "test-seams")]
const TEST_FAULT_VARIABLE: &str = "GATE_DESKTOP_TEST_FAULT";

/// The build a `wrong-version` fault names in place of its own, which no [`build_version`] is.
#[cfg(feature = "test-seams")]
const FAULT_VERSION: &str = "0.0.0+fault";

/// One step of the run a build with the `test-seams` feature may break, taken from `GATE_DESKTOP_TEST_FAULT`.
#[cfg(feature = "test-seams")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// Exit right after connecting, before `Hello`, as [`HelperExit::BoxGone`] says a box that went away.
    ExitBeforeHello,
    /// Connect and never say `Hello`, waiting on the pipe as far as the run's own limits reach.
    Hang,
    /// Say `Hello` naming [`FAULT_VERSION`] in place of this build.
    WrongVersion,
    /// Start the child as normal and report its thread's handle value where the box reads its process's: a live
    /// handle of this helper's table, of a kind that cannot stand for the child.
    LieHandle,
    /// Start the child suspended as normal, then exit without reporting it, leaving it suspended.
    ExitAfterCreate,
}

#[cfg(feature = "test-seams")]
impl Fault {
    /// The fault `GATE_DESKTOP_TEST_FAULT` asks for; `None` for none.
    fn asked() -> Option<Fault> {
        match std::env::var_os(TEST_FAULT_VARIABLE)?.to_str()? {
            "exit-before-hello" => Some(Fault::ExitBeforeHello),
            "hang" => Some(Fault::Hang),
            "wrong-version" => Some(Fault::WrongVersion),
            "lie-handle" => Some(Fault::LieHandle),
            "exit-after-create" => Some(Fault::ExitAfterCreate),
            _ => None,
        }
    }
}

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
            connect: Limits::BUILT.connect.min(cap),
            message: Limits::BUILT.message.min(cap),
            done: Limits::BUILT.done.min(cap),
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
    #[cfg(feature = "test-seams")]
    let Some(owner) = pipe_owner_seam() else {
        return HelperExit::BadArgument;
    };
    #[cfg(not(feature = "test-seams"))]
    let owner = PipeOwner::Administrators;
    match serve(box_pid, &name, limits, owner) {
        Ok(exit) | Err(exit) => exit,
    }
}

/// The owner the control pipe must have: Administrators, or its user when `GATE_DESKTOP_TEST_PIPE_OWNER` is `user`.
/// `None` for a seam set to anything else.
#[cfg(feature = "test-seams")]
fn pipe_owner_seam() -> Option<PipeOwner> {
    match std::env::var_os(TEST_PIPE_OWNER_VARIABLE) {
        None => Some(PipeOwner::Administrators),
        Some(value) if value == "user" => Some(PipeOwner::User),
        Some(_) => None,
    }
}

/// The run once `Arg0` is read: every step in order, the first that ends the run giving its exit.
fn serve(box_pid: u32, name: &str, limits: Limits, owner: PipeOwner) -> Result<HelperExit, HelperExit> {
    check_integrity()?;
    let pipe = connect(name, limits.connect, owner)?;
    match pipe.server_pid() {
        Ok(pid) if pid == box_pid => {}
        Ok(_) | Err(_) => return Err(HelperExit::ServerRefused),
    }
    #[cfg(feature = "test-seams")]
    if let Some(exit) = fault_before_hello(&pipe, limits) {
        return Err(exit);
    }
    send(&pipe, &hello(), limits.message)?;
    let spawn = receive_spawn(&pipe, limits.message)?;
    let child = start(&pipe, spawn, limits.message)?;
    let released = child.release().map_err(|_| HelperExit::Internal)?;
    #[cfg(feature = "test-seams")]
    record_child(released.pid)?;
    #[cfg(feature = "test-seams")]
    if Fault::asked() == Some(Fault::ExitAfterCreate) {
        return Err(HelperExit::Internal);
    }
    let report = report_of(&released);
    match write_message(&pipe, &report, Instant::now() + limits.message, &[]) {
        Ok(()) => Ok(await_done(&pipe, &released, limits)),
        Err(WriteError::Closed) => Err(box_gone(&released, limits.message)),
        Err(WriteError::Io(error)) if is_pipe_gone(&error) => Err(box_gone(&released, limits.message)),
        Err(WriteError::TimedOut) => Err(HelperExit::TimedOut),
        Err(WriteError::Io(_) | WriteError::Woken(_) | WriteError::TooLong) => Err(HelperExit::Internal),
    }
}

/// The seam that, set to a file's path, makes the helper add the pid of each child it creates to that file, one line
/// each, in a build with the `test-seams` feature, so a case can see whether the child outlived the run.
#[cfg(feature = "test-seams")]
const TEST_RECORD_VARIABLE: &str = "GATE_DESKTOP_TEST_RECORD";

/// Adds the child's `pid` to the file `GATE_DESKTOP_TEST_RECORD` names, when it names one. A record that cannot be
/// written ends the run as [`HelperExit::Internal`], so a case never reads a record that misses a child.
#[cfg(feature = "test-seams")]
fn record_child(pid: u32) -> Result<(), HelperExit> {
    use std::io::Write;

    let Some(path) = std::env::var_os(TEST_RECORD_VARIABLE) else {
        return Ok(());
    };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{pid}"))
        .map_err(|_| HelperExit::Internal)
}

/// The exit a fault asks for before `Hello` is sent: `exit-before-hello` exits at once, as a helper whose box went
/// away, and `hang` waits on the pipe. `None` for the run to go on.
#[cfg(feature = "test-seams")]
fn fault_before_hello(pipe: &PipeClient, limits: Limits) -> Option<HelperExit> {
    match Fault::asked()? {
        Fault::ExitBeforeHello => Some(HelperExit::BoxGone),
        Fault::Hang => Some(hang(pipe, limits)),
        Fault::WrongVersion | Fault::LieHandle | Fault::ExitAfterCreate => None,
    }
}

/// The exit `hang` ends with: the box gone when its pipe goes away before the done limit, else that the box did not
/// answer in time. It never sends `Hello` and never answers what the box sends: it reads, and goes on waiting, until
/// one of those two ends it. It waits the done limit, the longest the helper keeps, so a box that keeps its own message
/// limit gives up on the hanging helper first.
#[cfg(feature = "test-seams")]
fn hang(pipe: &PipeClient, limits: Limits) -> HelperExit {
    let deadline = Instant::now() + limits.done;
    loop {
        match read_message(pipe, deadline, FRAME_LIMIT, &[]) {
            // Whatever the box said, the hanging helper keeps quiet and waits on. An empty frame and a body that is no
            // message are both read whole, so the pipe still stands at a frame boundary.
            Ok(_) | Err(ReadError::Empty | ReadError::Decode(_)) => {}
            Err(ReadError::Closed) => return HelperExit::BoxGone,
            Err(ReadError::Io(error)) if is_pipe_gone(&error) => return HelperExit::BoxGone,
            // Too long, woken, out of time, or another failure of the pipe: the wait is over, and after the first
            // three the stream stands at no frame boundary any more.
            Err(ReadError::TooLong | ReadError::Woken(_) | ReadError::TimedOut | ReadError::Io(_)) => {
                return HelperExit::TimedOut;
            }
        }
    }
}

/// The `Child` report for `released`: its pid and the values of its process and thread handles, the process value
/// alone replaced, by the thread's, under a `lie-handle` fault.
fn report_of(released: &Released) -> Message {
    #[cfg(feature = "test-seams")]
    let process = match Fault::asked() {
        Some(Fault::LieHandle) => released.thread.addr() as u64,
        _ => released.process.addr() as u64,
    };
    #[cfg(not(feature = "test-seams"))]
    let process = released.process.addr() as u64;
    Message::Child {
        pid: released.pid,
        process,
        thread: released.thread.addr() as u64,
    }
}

/// Refuses to go on above medium integrity or elevated: a helper the Task Scheduler starts with the user's
/// least-privileged token runs at medium, and a higher one must not start commands for the box.
fn check_integrity() -> Result<(), HelperExit> {
    let facts = gate_win32::token_facts().map_err(|_| HelperExit::Internal)?;
    if integrity_refused(facts.elevated, facts.integrity) {
        return Err(HelperExit::WrongIntegrity);
    }
    Ok(())
}

/// Whether a token `elevated` or not, at the integrity level whose relative ID is `integrity`, is refused: an elevated
/// one, or one above medium.
fn integrity_refused(elevated: bool, integrity: u32) -> bool {
    elevated || integrity > SECURITY_MANDATORY_MEDIUM_RID.cast_unsigned()
}

/// The control pipe `name`, opened only when `owner` owns it and its DACL and label are the ones the box gives it for
/// this process's user, tried again while it is not there yet or busy until `limit` has passed.
fn connect(name: &str, limit: Duration, owner: PipeOwner) -> Result<PipeClient, HelperExit> {
    let user = gate_win32::current_user_sid().map_err(|_| HelperExit::Internal)?;
    let deadline = Instant::now() + limit;
    loop {
        match PipeClient::open(name, &user, owner) {
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

/// The seam that, set, names the input desktop in place of the one the helper reads, in a build with the `test-seams`
/// feature: the helper still judges the name it is given against `Default`.
#[cfg(feature = "test-seams")]
const TEST_INPUT_DESKTOP_VARIABLE: &str = "GATE_DESKTOP_TEST_INPUT_DESKTOP";

/// The `Hello` this helper sends: its build and the input desktop's name, empty when it cannot be read. A
/// `wrong-version` fault names [`FAULT_VERSION`] in place of its build, and `GATE_DESKTOP_TEST_INPUT_DESKTOP`, when set,
/// names the desktop.
fn hello() -> Message {
    #[cfg(feature = "test-seams")]
    let desktop = match std::env::var_os(TEST_INPUT_DESKTOP_VARIABLE) {
        Some(named) => named.to_string_lossy().into_owned(),
        None => gate_win32::input_desktop_name().unwrap_or_default(),
    };
    #[cfg(not(feature = "test-seams"))]
    let desktop = gate_win32::input_desktop_name().unwrap_or_default();
    #[cfg(feature = "test-seams")]
    let version = match Fault::asked() {
        Some(Fault::WrongVersion) => FAULT_VERSION.to_owned(),
        _ => build_version(),
    };
    #[cfg(not(feature = "test-seams"))]
    let version = build_version();
    Message::Hello {
        version,
        ok: desktop == DEFAULT_DESKTOP,
        desktop,
    }
}

/// Writes `message` within `limit`, before any child exists; a failure ends the run as [`send_exit`] says.
fn send(pipe: &PipeClient, message: &Message, limit: Duration) -> Result<(), HelperExit> {
    write_message(pipe, message, Instant::now() + limit, &[]).map_err(send_exit)
}

/// The exit for a write that failed before any child exists: the box gone when the pipe is closed or disconnected,
/// else `Internal`, a write the box did not take within the limit among them.
fn send_exit(error: WriteError) -> HelperExit {
    match error {
        WriteError::Closed => HelperExit::BoxGone,
        WriteError::Io(error) if is_pipe_gone(&error) => HelperExit::BoxGone,
        WriteError::TimedOut | WriteError::Io(_) | WriteError::Woken(_) | WriteError::TooLong => HelperExit::Internal,
    }
}

/// The box's `Spawn`, read within `limit`.
fn receive_spawn(pipe: &PipeClient, limit: Duration) -> Result<Spawn, HelperExit> {
    match read_message(pipe, Instant::now() + limit, FRAME_LIMIT, &[]) {
        Ok(Message::Spawn(spawn)) => Ok(spawn),
        Ok(_) | Err(ReadError::Empty | ReadError::TooLong | ReadError::Decode(_)) => Err(HelperExit::BadSpawn),
        Err(ReadError::Closed) => Err(HelperExit::BoxGone),
        Err(ReadError::Io(error)) if is_pipe_gone(&error) => Err(HelperExit::BoxGone),
        Err(ReadError::TimedOut) => Err(HelperExit::TimedOut),
        Err(ReadError::Woken(_) | ReadError::Io(_)) => Err(HelperExit::Internal),
    }
}

/// The command `spawn` asks for, created suspended on the desktop with the two output handles the box placed in this
/// process, once they and the environment pass their checks. A command `CreateProcessW` refuses is reported to the
/// box as `Failed`, written within `limit`; a launch that fails before that call is not.
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
    if gate_win32::same_object(stdout, stderr) {
        return Err(HelperExit::BadSpawn);
    }
    let environment = EnvironmentBlock::from_wide(environment).map_err(|_| HelperExit::BadSpawn)?;
    let launch = DesktopLaunch {
        application: PathBuf::from(OsString::from_wide(&application)),
        command_line,
        directory: PathBuf::from(OsString::from_wide(&directory)),
        environment,
    };
    launch.start_suspended(stdout, stderr).map_err(|error| match error {
        LaunchError::CreateProcess { code, .. } => {
            // The exit says the spawn failed whether or not the box reads why.
            let _reported = send(pipe, &Message::Failed { code }, limit);
            HelperExit::SpawnFailed
        }
        LaunchError::Setup { .. } => HelperExit::SetupFailed,
    })
}

/// The output handle the box placed in this process as `value`, once it passes
/// [`gate_win32::check_received_output`] against the control pipe. A value it refuses is the box's `BadSpawn`; a
/// failure of the system, as reading the control pipe's own name, is `Internal`.
fn received_output<'a>(value: u64, control: BorrowedHandle<'_>) -> Result<BorrowedHandle<'a>, HelperExit> {
    let value = usize::try_from(value).map_err(|_| HelperExit::BadSpawn)?;
    // SAFETY: the box placed the handle in this process and closes it only once it has read the child's report, after
    // the last use of the borrow, which ends when the command holds its own copy.
    unsafe { gate_win32::check_received_output(value, control) }.map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidInput && error.raw_os_error().is_none() {
            HelperExit::BadSpawn
        } else {
            HelperExit::Internal
        }
    })
}

/// Waits up to the done limit for the box's `Done`. The child is ended only when the pipe reads closed, or fails with
/// an error that says its other end is gone: nothing will resume the child. On a deadline passed, any other error, or
/// anything else read, it is left as it is, since the box may have taken it.
fn await_done(pipe: &PipeClient, released: &Released, limits: Limits) -> HelperExit {
    match read_message(pipe, Instant::now() + limits.done, FRAME_LIMIT, &[]) {
        Ok(Message::Done) => HelperExit::Done,
        Err(ReadError::Closed) => box_gone(released, limits.message),
        Err(ReadError::Io(error)) if is_pipe_gone(&error) => box_gone(released, limits.message),
        Err(ReadError::Io(_)) => HelperExit::Internal,
        Err(ReadError::TimedOut) => HelperExit::NoDone,
        Ok(_) | Err(ReadError::Woken(_) | ReadError::Empty | ReadError::TooLong | ReadError::Decode(_)) => HelperExit::BadMessage,
    }
}

/// Whether `error` says the pipe's other end is gone.
fn is_pipe_gone(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| PIPE_GONE.contains(&code))
}

/// The exit once the box is found gone after the child was created: the child is ended within `bound`, and the exit
/// is `BoxGone` when it is known ended, else `ChildNotEnded`.
fn box_gone(released: &Released, bound: Duration) -> HelperExit {
    if end_child(released, bound) {
        HelperExit::BoxGone
    } else {
        HelperExit::ChildNotEnded
    }
}

/// Ends the child this helper created and released, through the copy of its process handle the release kept, which
/// the box's take of the handed-on handle never closes, and waits up to `bound` for its end. True when it is known
/// ended: it had ended already, or it ended within `bound`; false when it could not be ended or waited on, or still ran.
fn end_child(released: &Released, bound: Duration) -> bool {
    matches!(
        gate_win32::end_process(released.kept_process.as_handle(), ENDED_UNSTARTED, bound),
        Ok(true)
    )
}

#[cfg(test)]
mod tests {
    use std::io;

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED};
    use windows_sys::Win32::System::SystemServices::{
        SECURITY_MANDATORY_HIGH_RID, SECURITY_MANDATORY_LOW_RID, SECURITY_MANDATORY_MEDIUM_PLUS_RID, SECURITY_MANDATORY_MEDIUM_RID,
        SECURITY_MANDATORY_SYSTEM_RID,
    };

    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::time::Duration;

    use gate_win32::{EnvironmentBlock, Launch, LaunchOutput, StoppableProcess};

    use super::{ENDED_UNSTARTED, end_child, integrity_refused, send_exit};
    use crate::HelperExit;
    use crate::protocol::WriteError;

    /// The longest the test waits for the child to end: it never runs out unless the code is broken.
    const HANG_GUARD: Duration = Duration::from_secs(600);

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose child cannot be made must fail loudly")]
    fn a_child_is_ended_through_the_kept_handle_after_the_box_took_the_handed_one() {
        let system = gate_win32::system_directory().expect("the system folder is read");
        let child = Launch {
            application: system.join("cmd.exe"),
            command_line: "cmd /d /c exit 0".encode_utf16().collect(),
            directory: system,
            environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
            output: LaunchOutput::Piped,
            priority: None,
        }
        .start_suspended()
        .expect("the command is created");
        let watch = StoppableProcess::open(child.pid()).expect("the child opens").expect("the child exists");
        let released = child.release().expect("the child is released");
        // The box takes the handed-on handles with `DUPLICATE_CLOSE_SOURCE`, which closes them here.
        // SAFETY: `release` gave up both handles, open, and nothing else owns them.
        drop(unsafe {
            (
                OwnedHandle::from_raw_handle(released.process),
                OwnedHandle::from_raw_handle(released.thread),
            )
        });

        let ended = end_child(&released, HANG_GUARD);

        assert!(ended, "the child is known ended");
        assert!(watch.wait(HANG_GUARD).expect("the wait succeeds"), "the child ended");
        assert_eq!(watch.exit_code().expect("the exit code is read"), ENDED_UNSTARTED, "ended by the helper");
    }

    #[test]
    fn only_a_token_not_elevated_and_at_medium_or_below_is_taken() {
        let (low, medium) = (SECURITY_MANDATORY_LOW_RID.cast_unsigned(), SECURITY_MANDATORY_MEDIUM_RID.cast_unsigned());
        let (high, system) = (SECURITY_MANDATORY_HIGH_RID.cast_unsigned(), SECURITY_MANDATORY_SYSTEM_RID.cast_unsigned());

        assert!(!integrity_refused(false, low), "low");
        assert!(!integrity_refused(false, medium), "medium");
        assert!(integrity_refused(false, SECURITY_MANDATORY_MEDIUM_PLUS_RID), "medium plus");
        assert!(integrity_refused(false, high), "high");
        assert!(integrity_refused(false, system), "system");
        assert!(integrity_refused(true, medium), "elevated at medium");
    }

    #[test]
    fn a_failed_write_before_the_child_is_the_box_gone_only_when_the_pipe_is() {
        for code in [ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED] {
            let error = WriteError::Io(io::Error::from_raw_os_error(code.cast_signed()));
            assert_eq!(send_exit(error), HelperExit::BoxGone, "error {code}");
        }
        assert_eq!(send_exit(WriteError::Closed), HelperExit::BoxGone, "closed");

        assert_eq!(send_exit(WriteError::TimedOut), HelperExit::Internal, "timed out");
        let denied = WriteError::Io(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED.cast_signed()));
        assert_eq!(send_exit(denied), HelperExit::Internal, "another error");
        assert_eq!(send_exit(WriteError::Woken(0)), HelperExit::Internal, "woken");
        assert_eq!(send_exit(WriteError::TooLong), HelperExit::Internal, "too long");
    }
}
