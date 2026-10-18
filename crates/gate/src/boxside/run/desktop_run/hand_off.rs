//! The hand-off of a desktop run's command to the desktop helper, once the run holds the builder's desktop: the run's
//! kill-on-close job, the control pipe, the helper started through its task in the console's session and checked to be
//! the helper the box started, the command sent to it and reported back, checked and taken into the run's job, the
//! start marker, and the command watched to its end as any run's command is.
//!
//! Before the marker every failure is a refusal. Once the box has sent the command, the helper may hold a child, so a
//! failure drops the pipe without `Done` (the helper then ends its own child), waits once for the helper to end, and
//! only then ends the helper and every process it left that is provably its own. Every wait for the helper's end
//! watches the run's stop signal.

use std::cell::Cell;
use std::fs::OpenOptions;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsHandle, BorrowedHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use gate_args::BoxSeams;
use gate_desktop::protocol::{HELPER_FRAME_LIMIT, Message, ReadError, Spawn, WriteError, read_message, write_message};
use gate_desktop::version::build_version;
use gate_desktop::{PIPE_PREFIX, TASK_PATH, arg0};
use gate_win32::{
    ConnectWait, EndWait, Job, OutputPipes, PipeOwner, PipeServer, QueryOnlyProcess, RemoteProcess, Share, StartedTask, StoppableProcess, Suspended,
    TaskArgument, TaskError, TaskStart,
};
use jiff::Timestamp;
use proc_table::{ProcessInfo, Win32ProcessTable};
use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

use super::{DesktopEnd, desktop_refused};
use crate::boxside::ceiling::{Ceiling, Spec};
use crate::boxside::desktop::lines::{DesktopRefusal, Limits, TaskVerdict, could_not_end_line, end_report, task_verdict};
use crate::boxside::desktop::verify::{
    ExpectedHelper, Facts, FileIdentity, HelperFacts, Reported, WrongFact, WrongHelper, sweep_targets, verify, verify_helper,
};
use crate::boxside::payload::escaped;
use crate::boxside::run::{
    Beat, Launched, Look, One, Pipes, Prepared, Ran, Request, STARTING_COMMAND, Started, Stopper, box_run_job, failed, job_made, not_started,
    stopped_before_start, wait_for_path, watch,
};
use crate::console::Console;
use crate::remote::{Refusal, install_untrusted_line, refused_line, started_line};

/// Why the helper did not start when it never connected its control pipe within the connect limit.
const NO_CONNECT: &str = "it did not connect in time";

/// Why the helper did not start when it sent no whole message within the message limit.
const NO_ANSWER: &str = "it did not answer in time";

/// Why the helper did not start when it did not take the box's message within the message limit.
const NOT_TAKEN: &str = "it did not take the box's message in time";

/// Why the helper did not start when what it sent is no message.
const NOT_A_MESSAGE: &str = "it sent a message the box cannot read";

/// Why the helper did not start when it sent a message the hand-off was not at.
const OUT_OF_TURN: &str = "it sent a message the box did not expect";

/// Why the helper did not start when it closed the control pipe and had not ended by itself within the end limit.
const CLOSED: &str = "it closed the control pipe";

/// Why the run is refused when the program the box sends cannot be opened for its identity.
const PROGRAM_UNREAD: &str = "the program the box sends cannot be read";

/// Why the run is refused when the run's kill-on-close job cannot be created.
const JOB_UNMADE: &str = "the command's job cannot be made";

/// Why the run is refused when the box cannot read its own user, which owns the control pipe.
const USER_UNREAD: &str = "the box's own user cannot be read";

/// Why the run is refused when the helper's own file cannot be opened for its identity.
const HELPER_FILE_UNREAD: &str = "the desktop helper's file cannot be read";

/// Why the helper did not start when it ended by itself and its exit code cannot be read.
const EXIT_CODE_UNREAD: &str = "its exit code cannot be read";

/// `why`, followed by the system's `error` that caused it.
fn with_error(why: &str, error: &io::Error) -> String {
    format!("{why}: {error}")
}

/// The exit code a process the box ends is given: a command that never ran, as `gate_win32` ends one.
const ENDED_UNSTARTED: u32 = 1;

/// What the hand-off reads its run by: the run, its command, prepared, and its output lock and stop signal, the limits
/// of its waits and the console session the box read for it.
pub(super) struct Here<'h, 'r> {
    pub(super) request: &'h Request<'r>,
    pub(super) one: &'h One<'r>,
    pub(super) prepared: &'h Prepared<'r>,
    pub(super) beat: &'h Beat<'r>,
    pub(super) limits: Limits,
    pub(super) session: u32,
}

impl Here<'_, '_> {
    /// Refuses the run for the builder's desktop with `refusal`, its tag and line said under the output lock.
    fn refused(&self, refusal: DesktopRefusal, console: &mut Console) -> DesktopEnd {
        desktop_refused(self.request, &refusal.into(), self.beat, console)
    }

    /// Refuses the run for the box's own machinery, as `why` says.
    fn machinery(&self, why: String, console: &mut Console) -> DesktopEnd {
        self.refused(DesktopRefusal::Machinery(why), console)
    }

    /// Refuses the run, before anything starts, because the desktop helper is not installed where only administrators
    /// can change it, as `why` says: the run's `install` refusal tag, bound to its nonce, then the box's install line,
    /// said together under the output lock, so the PC runs the command itself and journals the fallback `install`.
    fn untrusted(&self, why: &str, console: &mut Console) -> DesktopEnd {
        let _held = self.beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
        console.err(&refused_line(self.request.nonce, Refusal::Install));
        console.err(&install_untrusted_line(why));
        DesktopEnd::Refused
    }

    /// The stop signal's handle, which ends every wait of the hand-off.
    fn stop(&self) -> BorrowedHandle<'_> {
        self.beat.stop.event.as_handle()
    }
}

/// How a step of the hand-off failed.
enum Fault {
    /// Refuse the run with this.
    Refused(DesktopRefusal),
    /// The run's own stop came.
    Stopped,
    /// The helper ended, or closed its end of the control pipe.
    Gone,
}

impl Fault {
    /// A failure of the control pipe itself.
    fn pipe(error: &io::Error) -> Fault {
        Fault::Refused(DesktopRefusal::HelperDidNotStart(format!("the control pipe failed: {error}")))
    }

    /// The helper did not start, as `why` says.
    fn did_not_start(why: &str) -> Fault {
        Fault::Refused(DesktopRefusal::HelperDidNotStart(why.to_owned()))
    }

    /// The box's own machinery failed, as `why` says.
    fn machinery(why: String) -> Fault {
        Fault::Refused(DesktopRefusal::Machinery(why))
    }
}

/// The helper the Task Scheduler started for the run, checked to be the box's: the control pipe it serves, the handle
/// the box opened of it at once, its pid, when it was created, whether the box has sent it the command, from when it
/// may hold a child, and the deadline its connect shares with the task start.
struct Pinned {
    server: PipeServer,
    process: RemoteProcess,
    pid: u32,
    created: Timestamp,
    spawned: Cell<bool>,
    connect_by: Instant,
}

/// How the exchange with the helper ended, where it did not fail.
enum Exchanged {
    /// The command, taken into the run's job and still suspended, and the read ends of its output pipes.
    Taken(Suspended, Pipes),
    /// The helper could not create the command, with the system's error code.
    NotCreated(u32),
}

/// Hands the run's prepared command to the desktop helper in the console session `here` names, and sees it to its end
/// as [`watch`] sees any command: `Ok` with how it ended, `Err` with how the run ended without it, its lines said.
///
/// The job is created kill-on-close, so a box that dies ends the command too, and holds the command alone: no console
/// donor, and no restricted token, as the helper starts the command with the console user's own. The start marker is
/// said once the command is taken into the job, still suspended, and before the stop signal is looked at; a command
/// the helper could not create is said after the marker, as a command that could not be started (127).
pub(super) fn hand_off(here: &Here<'_, '_>, console: &mut Console) -> Result<Ran, DesktopEnd> {
    let program = identity_of(&here.prepared.application).map_err(|error| here.machinery(with_error(PROGRAM_UNREAD, &error), console))?;
    let job = match box_run_job(here.request.job, true) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            return Err(here.machinery(with_error(JOB_UNMADE, &error), console));
        }
        made => job_made(made, here.request.job, console).map_err(DesktopEnd::Ended)?,
    };
    // Kept until the command's wait is over: the seam's stand-in helper's job holds the command too, nested.
    let (pinned, _stand_in) = pinned_helper(here, &job, console)?;
    let (suspended, pipes) = match exchange(here, &pinned, &job, program) {
        Ok(Exchanged::Taken(suspended, pipes)) => (suspended, pipes),
        Ok(Exchanged::NotCreated(code)) => return Ok(not_created(here, pinned, code, console)),
        Err(fault) => return Err(ended_on(here, fault, pinned, &job, console)),
    };
    release_helper(here, pinned, console);
    let beat = here.beat;
    if let Some(park) = beat.seams.park_launch.as_deref() {
        wait_for_path(park, console);
    }
    beat.say(console, &started_line(here.request.nonce));
    match beat.stop.look(STARTING_COMMAND) {
        Look::Stopped => return Err(DesktopEnd::Ended(stopped_before_start(&job, beat.lock, console))),
        Look::Go(Some(unread)) => beat.say(console, &unread),
        Look::Go(None) => {}
    }
    let running = suspended
        .resume()
        .map_err(|error| DesktopEnd::Ended(failed(console, &error.to_string())))?;
    let launched = Launched { running, pipes };
    let mut stopper = Stopper { job: &job, armed: true };
    let spec = Spec {
        timeout: here.one.timeout,
        host: escaped(here.request.host),
        spoken: escaped(&here.one.words.join(" ")),
        nonce: here.request.nonce,
    };
    let ceiling = Ceiling::begin(spec, &job, beat.lock, beat.seams, console);
    let started = Started {
        job: &job,
        launched,
        keep: &[],
        beat,
        ceiling,
    };
    let watched = watch(started, console);
    stopper.armed = false;
    watched.map_err(DesktopEnd::Ended)
}

/// Makes the control pipe, starts the helper's task in the console session with the pipe's name, and checks the
/// process the Task Scheduler gave is the helper the box started ([`verify_helper`]); the helper pinned, and the
/// stand-in helper's job under the desktop seam. A refusal, or a stop, ends the run here; a helper that may have
/// started, or one that is not the box's, has its pipe dropped and is never ended through its pid.
fn pinned_helper(here: &Here<'_, '_>, job: &Job, console: &mut Console) -> Result<(Pinned, Option<Job>), DesktopEnd> {
    let user = gate_win32::current_user_sid().map_err(|error| here.machinery(with_error(USER_UNREAD, &error), console))?;
    let image =
        helper_image(here.beat.seams).map_err(|error| here.machinery(format!("the desktop helper's path cannot be read: {error}"), console))?;
    helper_trusted(here.beat.seams, &image).map_err(|why| here.untrusted(&why, console))?;
    let server = PipeServer::create(PIPE_PREFIX, &user, pipe_owner(here.beat.seams)).map_err(|error| {
        here.refused(
            DesktopRefusal::HelperDidNotStart(format!("the control pipe cannot be made: {error}")),
            console,
        )
    })?;
    let Some(argument) = arg0(std::process::id(), server.name()).and_then(|text| TaskArgument::new(&text)) else {
        return Err(here.machinery("the desktop helper's argument cannot be made".to_owned(), console));
    };
    let image_identity = identity_of(&image).map_err(|error| here.machinery(with_error(HELPER_FILE_UNREAD, &error), console))?;
    let Some(before) = crate::escaped::now() else {
        return Err(here.machinery("the system clock cannot be read".to_owned(), console));
    };
    let task = TaskStart {
        path: TASK_PATH,
        argument: &argument,
        session_id: here.session,
        helper: &image,
        pid_wait: here.limits.connect,
    };
    // One deadline for the task start and the connect, so the whole start lasts at most the connect limit.
    let connect_by = Instant::now() + here.limits.connect;
    let (started, stand_in) = run_helper_task(here.beat.seams, &task, connect_by, &[here.stop()]);
    let pid = match task_verdict(started) {
        TaskVerdict::Started(pid) => pid.get(),
        TaskVerdict::Refused(refusal) => return Err(here.refused(refusal, console)),
        TaskVerdict::MaybeStarted(refusal) => {
            drop(server);
            return Err(here.refused(refusal, console));
        }
        TaskVerdict::Stopped => {
            drop(server);
            return Err(DesktopEnd::Ended(stopped_before_start(job, here.beat.lock, console)));
        }
    };
    let process = match RemoteProcess::open(pid) {
        Ok(process) => process,
        Err(error) => {
            drop(server);
            let why = format!("the process the task started cannot be opened: {error}");
            return Err(here.refused(DesktopRefusal::HelperDidNotStart(why), console));
        }
    };
    let expected = ExpectedHelper {
        before,
        session: here.session,
        image: image_identity,
        user: &user,
    };
    match verify_helper(&PinnedFacts(&process), &expected) {
        Ok(created) => Ok((
            Pinned {
                server,
                process,
                pid,
                created,
                spawned: Cell::new(false),
                connect_by,
            },
            stand_in,
        )),
        Err(refused) => {
            drop(server);
            Err(here.refused(DesktopRefusal::HelperDidNotStart(refused.why()), console))
        }
    }
}

/// The exchange with the pinned helper: its connect, its `Hello`, the command sent with the two write ends of its output
/// pipes, its report, the command it reported checked against the `program` file and taken into `job` ([`taken`]), and
/// `Done`. The helper is marked spawned as the command is sent, from when it may hold a child.
fn exchange(here: &Here<'_, '_>, pinned: &Pinned, job: &Job, program: FileIdentity) -> Result<Exchanged, Fault> {
    let wake = [here.stop(), pinned.process.handle()];
    connected(pinned, &wake)?;
    hello(read(pinned, here.limits, &wake)?)?;
    let OutputPipes {
        stdout_read,
        stdout_write,
        stderr_read,
        stderr_write,
    } = gate_win32::output_pipes().map_err(|error| Fault::machinery(format!("the command's output pipes cannot be made: {error}")))?;
    let given = |handle: OwnedHandle| {
        let value = pinned
            .process
            .push(handle.as_handle())
            .map_err(|error| Fault::machinery(format!("the command's output pipes cannot be given to the desktop helper: {error}")))?;
        // The box's own write end closes here, so the read end reaches its end once the command's copies close.
        drop(handle);
        u64::try_from(value).map_err(|error| Fault::machinery(format!("a handle value does not fit the start request: {error}")))
    };
    let prepared = here.prepared;
    let spawn = Spawn {
        application: wide(&prepared.application),
        command_line: prepared.command_line.clone(),
        directory: wide(here.one.place),
        environment: prepared.environment.as_wide().to_vec(),
        stdout: given(stdout_write)?,
        stderr: given(stderr_write)?,
    };
    let pipes = Pipes {
        stdout: stdout_read,
        stderr: stderr_read,
    };
    pinned.spawned.set(true);
    write(pinned, &Message::Spawn(spawn), here.limits, &wake)?;
    let (pid, process, thread) = match read(pinned, here.limits, &wake)? {
        Message::Child { pid, process, thread } => (pid, process, thread),
        Message::Failed { code } => return Ok(Exchanged::NotCreated(code)),
        Message::Hello { .. } | Message::Spawn(_) | Message::Done => return Err(Fault::did_not_start(OUT_OF_TURN)),
    };
    let reported = Reported {
        pid,
        helper: pinned.pid,
        helper_created: pinned.created,
        session: here.session,
        program,
    };
    let suspended = taken(&pinned.process, (process, thread), job, &reported).map_err(Fault::Refused)?;
    write(pinned, &Message::Done, here.limits, &wake)?;
    Ok(Exchanged::Taken(suspended, pipes))
}

/// Waits for the helper to connect by the deadline it shares with the task start: a client that is not the pinned
/// helper is disconnected and the wait goes on, so only the helper is ever read. The deadline and the stop hold across
/// wrong clients: each is looked at after a wrong client is disconnected, before the wait goes on, so a process that
/// keeps connecting cannot keep the run past its connect limit.
fn connected(pinned: &Pinned, wake: &[BorrowedHandle<'_>]) -> Result<(), Fault> {
    let mut another_client = false;
    loop {
        match pinned.server.wait_for_client(pinned.connect_by, wake) {
            Ok(ConnectWait::Connected) => {
                if pinned.server.client_pid().ok() == Some(pinned.pid) {
                    return Ok(());
                }
                another_client = true;
                pinned.server.disconnect().map_err(|error| Fault::pipe(&error))?;
                still_waiting(pinned, wake)?;
            }
            Ok(ConnectWait::Woken(0)) => return Err(Fault::Stopped),
            Ok(ConnectWait::Woken(_)) => return Err(Fault::Gone),
            Ok(ConnectWait::TimedOut) if another_client => return Err(Fault::did_not_start(WrongHelper::NotTheClient.why())),
            Ok(ConnectWait::TimedOut) => return Err(Fault::did_not_start(NO_CONNECT)),
            Err(error) => return Err(Fault::pipe(&error)),
        }
    }
}

/// Looks, without waiting, at the stop, the helper's end and the connect deadline after a wrong client was dropped: a
/// stop is [`Fault::Stopped`], the helper's end [`Fault::Gone`], a past deadline the refusal for a client that was not
/// the helper; else the wait for the helper goes on.
fn still_waiting(pinned: &Pinned, wake: &[BorrowedHandle<'_>]) -> Result<(), Fault> {
    let looked = pinned
        .process
        .wait_or_woken(Duration::ZERO, wake)
        .map_err(|error| Fault::machinery(format!("the stop and the desktop helper cannot be looked at between connects: {error}")))?;
    match looked {
        EndWait::Woken(0) => Err(Fault::Stopped),
        EndWait::Woken(_) | EndWait::Ended => Err(Fault::Gone),
        EndWait::Running if Instant::now() >= pinned.connect_by => Err(Fault::did_not_start(WrongHelper::NotTheClient.why())),
        EndWait::Running => Ok(()),
    }
}

/// The helper's `Hello` judged: its build must be the box's, and its desktop the console's Default one.
fn hello(message: Message) -> Result<(), Fault> {
    let Message::Hello { version, ok, .. } = message else {
        return Err(Fault::did_not_start(OUT_OF_TURN));
    };
    let ours = build_version();
    if version != ours {
        return Err(Fault::Refused(DesktopRefusal::HelperOtherVersion {
            helper: version,
            box_version: ours,
        }));
    }
    if !ok {
        return Err(Fault::Refused(DesktopRefusal::HelperNotOnDesktop));
    }
    Ok(())
}

/// One message from the helper, within the message limit; a stop or the helper's end ends the read.
fn read(pinned: &Pinned, limits: Limits, wake: &[BorrowedHandle<'_>]) -> Result<Message, Fault> {
    read_message(&pinned.server, Instant::now() + limits.message, HELPER_FRAME_LIMIT, wake).map_err(|error| match error {
        ReadError::Woken(0) => Fault::Stopped,
        ReadError::Woken(_) | ReadError::Closed => Fault::Gone,
        ReadError::TimedOut => Fault::did_not_start(NO_ANSWER),
        ReadError::Empty | ReadError::TooLong | ReadError::Decode(_) => Fault::did_not_start(NOT_A_MESSAGE),
        ReadError::Io(error) => Fault::pipe(&error),
    })
}

/// `message` written to the helper, within the message limit; a stop or the helper's end ends the write.
fn write(pinned: &Pinned, message: &Message, limits: Limits, wake: &[BorrowedHandle<'_>]) -> Result<(), Fault> {
    write_message(&pinned.server, message, Instant::now() + limits.message, wake).map_err(|error| match error {
        WriteError::Woken(0) => Fault::Stopped,
        WriteError::Woken(_) | WriteError::Closed => Fault::Gone,
        WriteError::TimedOut => Fault::did_not_start(NOT_TAKEN),
        WriteError::TooLong => Fault::machinery("the command's start request is too long for the control pipe".to_owned()),
        WriteError::Io(error) => Fault::pipe(&error),
    })
}

/// The command the helper reported as the `values` of its process and thread handles in the helper's table: both
/// handles taken out of the helper, the command checked against `reported` ([`verify`]), adopted, and put in `job`,
/// still suspended. A handle refused, or a command that fails a check, is never adopted, so nothing is ended through
/// it; the handles already taken are closed as they drop. Every refusal carries the system's error.
fn taken(helper: &RemoteProcess, values: (u64, u64), job: &Job, reported: &Reported) -> Result<Suspended, DesktopRefusal> {
    let refused = |error: &dyn std::fmt::Display| DesktopRefusal::WrongCommandFor(WrongFact::NotTheProcess, error.to_string());
    let pulled = |value: u64| match usize::try_from(value) {
        Ok(value) => helper.pull(value).map_err(|error| refused(&error)),
        Err(error) => Err(refused(&error)),
    };
    let process = pulled(values.0)?;
    let thread = pulled(values.1)?;
    let facts = PulledFacts {
        process: process.as_handle(),
        thread: thread.as_handle(),
        job,
    };
    verify(&facts, reported).map_err(DesktopRefusal::from)?;
    let suspended = Suspended::adopt(process, thread, reported.pid, end_report()).map_err(|error| refused(&error.why))?;
    suspended.assign(job).map_err(|error| DesktopRefusal::NotPutInTheJob(error.to_string()))?;
    match suspended.in_job(job) {
        Ok(true) => Ok(suspended),
        Ok(false) => Err(DesktopRefusal::NotPutInTheJob("the job does not hold it after the assign".to_owned())),
        Err(error) => Err(DesktopRefusal::NotPutInTheJob(error.to_string())),
    }
}

/// Lets the helper go once the exchange is over: it is given the end limit to end by itself, the wait ended at once by
/// a stop, then ended if it still runs; its pipe is dropped once that is over.
fn release_helper(here: &Here<'_, '_>, pinned: Pinned, console: &mut Console) {
    let Pinned { server, process, pid, .. } = pinned;
    let ended = end_within(&Watched::new(here, &process), here.limits.end, Instant::now, true);
    drop(server);
    say_unended(here, pid, &ended, console);
}

/// Ends a hand-off whose helper reported that it could not create the command: the same ending as any failure after
/// the command was sent ([`ended_on`]), then the start marker and the command's start failure; 127.
fn not_created(here: &Here<'_, '_>, pinned: Pinned, code: u32, console: &mut Console) -> Ran {
    let Pinned {
        server,
        process,
        pid,
        created,
        ..
    } = pinned;
    drop(server);
    let ended = end_within(&Watched::new(here, &process), here.limits.end, Instant::now, true);
    say_unended(here, pid, &ended, console);
    sweep(here, pid, created, console);
    here.beat.say(console, &started_line(here.request.nonce));
    let message = gate_win32::win32_message(code);
    Ran::Exited(not_started(console, here.prepared.word, Some(&here.prepared.program), &message))
}

/// Ends the hand-off on `fault`: the pipe dropped without `Done`; the helper given one end limit to end by itself
/// (and so its own child) once the command was sent or once it was gone already, else ended at once, as it holds no
/// child; once the command was sent, every process left that is provably the helper's ended ([`sweep`]). Then the
/// refusal is said, or, for a stop, the run's job stopped and its line said.
fn ended_on(here: &Here<'_, '_>, fault: Fault, pinned: Pinned, job: &Job, console: &mut Console) -> DesktopEnd {
    let Pinned {
        server,
        process,
        pid,
        created,
        spawned,
        ..
    } = pinned;
    let spawned = spawned.get();
    drop(server);
    let ended = end_within(
        &Watched::new(here, &process),
        here.limits.end,
        Instant::now,
        spawned || matches!(fault, Fault::Gone),
    );
    let refusal = match fault {
        Fault::Refused(refusal) => Some(refusal),
        Fault::Stopped => None,
        Fault::Gone => Some(gone(&process, &ended)),
    };
    say_unended(here, pid, &ended, console);
    if spawned {
        sweep(here, pid, created, console);
    }
    match refusal {
        Some(refusal) => here.refused(refusal, console),
        None => DesktopEnd::Ended(stopped_before_start(job, here.beat.lock, console)),
    }
}

/// Why a helper that closed its pipe or ended is refused: the exit code it ended with, when it ended by itself; else
/// that it closed the pipe.
fn gone(helper: &RemoteProcess, ended: &HelperEnd) -> DesktopRefusal {
    match ended {
        HelperEnd::Exited => match helper.exit_code() {
            Ok(code) => DesktopRefusal::HelperFailed(code),
            Err(error) => DesktopRefusal::HelperDidNotStart(with_error(EXIT_CODE_UNREAD, &error)),
        },
        HelperEnd::Ended | HelperEnd::NotEnded(_) => DesktopRefusal::HelperDidNotStart(CLOSED.to_owned()),
    }
}

/// How the helper came to its end.
#[derive(Debug)]
enum HelperEnd {
    /// It ended by itself within the wait.
    Exited,
    /// The box ended it.
    Ended,
    /// The box could not end it, for this error.
    NotEnded(io::Error),
}

/// The helper as the box waits for its end and ends it.
trait EndingHelper {
    /// Waits at most `bound` for its end, a stop ending the wait at once.
    fn wait_for_end(&self, bound: Duration) -> io::Result<EndWait>;

    /// Ends it, and waits at most `bound` to see it ended: true once it has.
    fn end(&self, bound: Duration) -> io::Result<bool>;
}

/// The pinned helper's process, and the run's stop signal its wait watches.
struct Watched<'w> {
    process: &'w RemoteProcess,
    stop: BorrowedHandle<'w>,
}

impl<'w> Watched<'w> {
    /// `process` watched with `here`'s stop signal.
    fn new(here: &'w Here<'_, '_>, process: &'w RemoteProcess) -> Watched<'w> {
        Watched { process, stop: here.stop() }
    }
}

impl EndingHelper for Watched<'_> {
    fn wait_for_end(&self, bound: Duration) -> io::Result<EndWait> {
        self.process.wait_or_woken(bound, &[self.stop])
    }

    fn end(&self, bound: Duration) -> io::Result<bool> {
        self.process.terminate(ENDED_UNSTARTED, bound)
    }
}

/// The most of an end limit kept back from the wait for the helper to end by itself, for the wait that sees the box's
/// end of it take effect: an end takes effect some time after the call that asks for it, so a wait of nothing after it
/// almost never sees it.
const END_SLICE: Duration = Duration::from_millis(500);

/// The part of `limit` kept for the wait that sees the box's end take effect: [`END_SLICE`], or a quarter of `limit`
/// when that is less.
fn end_slice(limit: Duration) -> Duration {
    END_SLICE.min(limit / 4)
}

/// Brings `helper` to its end within `limit` in total, on the clock `now`: when `wait_first`, a wait for it to end by
/// itself, which a stop ends at once, given the limit less [`end_slice`], then, if it still runs or the wait failed,
/// its end by the box, given what is left of the limit and never less than that slice. A failed wait is not said: the
/// end that follows it is.
fn end_within(helper: &impl EndingHelper, limit: Duration, now: impl Fn() -> Instant, wait_first: bool) -> HelperEnd {
    let deadline = now() + limit;
    let slice = end_slice(limit);
    if wait_first && matches!(helper.wait_for_end(limit.saturating_sub(slice)), Ok(EndWait::Ended)) {
        return HelperEnd::Exited;
    }
    match helper.end(deadline.saturating_duration_since(now()).max(slice)) {
        Ok(true) => HelperEnd::Ended,
        Ok(false) => HelperEnd::NotEnded(io::Error::other("it had not ended in time")),
        Err(error) => HelperEnd::NotEnded(error),
    }
}

/// Says the one line for a helper of pid `pid` the box could not end, when `ended` says so.
fn say_unended(here: &Here<'_, '_>, pid: u32, ended: &HelperEnd, console: &mut Console) {
    if let HelperEnd::NotEnded(error) = ended {
        here.beat.say(console, &helper_not_ended(pid, error));
    }
}

/// The box's line for a desktop helper it could not end: its pid and the error, escaped into the line.
fn helper_not_ended(pid: u32, error: &io::Error) -> String {
    format!(
        "gate: remote could not end the desktop helper (process {pid}): {}",
        escaped(&error.to_string())
    )
}

/// The box's line for a sweep that could not list the machine's processes, which then ends nothing.
fn unlisted_line(error: &io::Error) -> String {
    format!(
        "gate: remote could not list processes to end after the desktop command: {}",
        escaped(&error.to_string())
    )
}

/// Ends every process a failed hand-off left that is provably the helper's ([`sweep_targets`]): each re-checked by the
/// handle opened to end it, its parent still the helper and its creation time the snapshot's, and one that cannot be
/// opened or ended said. A snapshot that cannot be taken is said, and nothing is ended.
fn sweep(here: &Here<'_, '_>, helper: u32, created: Timestamp, console: &mut Console) {
    sweep_on(&Machine, helper, created, here.session, &mut |line| here.beat.say(console, line));
}

/// [`sweep`] over the processes of `machine`, of the helper `helper` created at `created` in the session `session`,
/// each line it says given to `say`.
fn sweep_on(machine: &impl SweptMachine, helper: u32, created: Timestamp, session: u32, say: &mut impl FnMut(&str)) {
    let snapshot = match machine.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            say(&unlisted_line(&error));
            return;
        }
    };
    for (pid, at) in sweep_targets(&snapshot, helper, created, session, |pid| machine.session_of(pid)) {
        let process = match machine.open(pid) {
            Ok(Some(process)) => process,
            Ok(None) => continue,
            Err(error) => {
                say(&could_not_end_line(pid, &error));
                continue;
            }
        };
        let still_the_helpers = process.parent_pid().ok() == Some(helper) && process.created().ok() == Some(at);
        if !still_the_helpers {
            continue;
        }
        if let Err(error) = process.terminate(ENDED_UNSTARTED) {
            say(&could_not_end_line(pid, &error));
        }
    }
}

/// The machine's processes as a sweep reads and ends them.
trait SweptMachine {
    /// A process opened to be re-checked and ended.
    type Process: SweptProcess;

    /// Every process on the machine, without its command line.
    fn snapshot(&self) -> io::Result<Vec<ProcessInfo>>;

    /// The session the process `pid` runs in; `None` when it cannot be read.
    fn session_of(&self, pid: u32) -> Option<u32>;

    /// The process `pid`, opened to be re-checked and ended; `None` when no process has that pid any more.
    fn open(&self, pid: u32) -> io::Result<Option<Self::Process>>;
}

/// A process a sweep opened: its parent and creation time read through the handle it is ended by.
trait SweptProcess {
    /// The pid of the process that started it.
    fn parent_pid(&self) -> io::Result<u32>;

    /// When it was created.
    fn created(&self) -> io::Result<Timestamp>;

    /// Ends it with the exit code `exit_code`.
    fn terminate(&self, exit_code: u32) -> io::Result<()>;
}

/// This machine's own processes.
struct Machine;

impl SweptMachine for Machine {
    type Process = StoppableProcess;

    fn snapshot(&self) -> io::Result<Vec<ProcessInfo>> {
        Win32ProcessTable.try_snapshot_without_command_lines()
    }

    fn session_of(&self, pid: u32) -> Option<u32> {
        QueryOnlyProcess::open(pid).ok().and_then(|process| process.session().ok())
    }

    fn open(&self, pid: u32) -> io::Result<Option<StoppableProcess>> {
        StoppableProcess::open(pid)
    }
}

impl SweptProcess for StoppableProcess {
    fn parent_pid(&self) -> io::Result<u32> {
        StoppableProcess::parent_pid(self)
    }

    fn created(&self) -> io::Result<Timestamp> {
        StoppableProcess::created(self)
    }

    fn terminate(&self, exit_code: u32) -> io::Result<()> {
        StoppableProcess::terminate(self, exit_code)
    }
}

/// The identity of the file at `path`, as the file the path reaches, through any junction, link or short name: opened
/// for its attributes alone, sharing everything, so no other holder of the file is in the way.
///
/// # Errors
///
/// The system's error when the file cannot be opened or its identity read, and [`gate_win32::file_id`]'s error when
/// its id is zero.
fn identity_of(path: &Path) -> io::Result<FileIdentity> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(Share::All.mode())
        .open(path)?;
    let (volume, file) = gate_win32::file_id(&file)?;
    Ok(FileIdentity { volume, file })
}

/// The identity of the image the process behind `process` runs, read from the path the system gives for it.
fn image_identity_of(process: BorrowedHandle<'_>) -> io::Result<FileIdentity> {
    identity_of(&gate_win32::image_path_of(process)?)
}

/// The facts of the pinned helper, read through the handle the box holds of it.
struct PinnedFacts<'p>(&'p RemoteProcess);

impl HelperFacts for PinnedFacts<'_> {
    fn created(&self) -> io::Result<Timestamp> {
        gate_win32::creation_time_of(self.0.handle())
    }

    fn session(&self) -> io::Result<u32> {
        gate_win32::session_of(self.0.handle())
    }

    fn image(&self) -> io::Result<FileIdentity> {
        image_identity_of(self.0.handle())
    }

    fn user(&self) -> io::Result<String> {
        gate_win32::user_of(self.0.handle())
    }
}

/// The facts of the command the helper reported, read through the handles the box took out of the helper, and the
/// run's job.
struct PulledFacts<'h> {
    process: BorrowedHandle<'h>,
    thread: BorrowedHandle<'h>,
    job: &'h Job,
}

impl Facts for PulledFacts<'_> {
    fn process_pid(&self) -> io::Result<u32> {
        gate_win32::pid_of_process(self.process)
    }

    fn thread_pid(&self) -> io::Result<u32> {
        gate_win32::pid_of_thread(self.thread)
    }

    fn parent_pid(&self) -> io::Result<u32> {
        gate_win32::parent_pid_of(self.process)
    }

    fn created(&self) -> io::Result<Timestamp> {
        gate_win32::creation_time_of(self.process)
    }

    fn session(&self) -> io::Result<u32> {
        gate_win32::session_of(self.process)
    }

    fn image(&self) -> io::Result<FileIdentity> {
        image_identity_of(self.process)
    }

    fn suspended_once(&self) -> io::Result<bool> {
        gate_win32::thread_is_suspended(self.thread)
    }

    fn in_job(&self) -> io::Result<bool> {
        self.job.holds_process(self.process)
    }
}

/// `path` as UTF-16, as the start request carries it.
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}

/// The file name of the seam's stand-in helper, beside the box's own program.
#[cfg(feature = "test-seams")]
const STAND_IN: &str = "fake-desktop-helper.exe";

/// The stand-in helper's seam that, set to a file's path, has it record each child it creates there, and the box record
/// the stand-in's own pid first; a run whose stand-in records its children starts it in no job of the box's, so a
/// stand-in or a child the box failed to end outlives the run and the case that reads the record sees it.
#[cfg(feature = "test-seams")]
const RECORD_VARIABLE: &str = "GATE_DESKTOP_TEST_RECORD";

/// The owner of the control pipe: Administrators, or, under the desktop seam, the box's own user, as the stand-in
/// helper, unelevated, is told to take.
#[cfg(feature = "test-seams")]
fn pipe_owner(seams: &BoxSeams) -> PipeOwner {
    if seams.desktop.is_some() {
        PipeOwner::User
    } else {
        PipeOwner::Administrators
    }
}

/// The owner of the control pipe: Administrators.
#[cfg(not(feature = "test-seams"))]
fn pipe_owner(_seams: &BoxSeams) -> PipeOwner {
    PipeOwner::Administrators
}

/// The helper's image: the gate's desktop helper beside the box's own program, or, under the desktop seam, the
/// stand-in helper there.
#[cfg(feature = "test-seams")]
fn helper_image(seams: &BoxSeams) -> io::Result<PathBuf> {
    match seams.desktop {
        Some(_) => Ok(std::env::current_exe()?.with_file_name(STAND_IN)),
        None => gate_desktop::helper_path(),
    }
}

/// The helper's image: the gate's desktop helper beside the box's own program.
#[cfg(not(feature = "test-seams"))]
fn helper_image(_seams: &BoxSeams) -> io::Result<PathBuf> {
    gate_desktop::helper_path()
}

/// `Ok` when the helper's image `image` is installed where only an administrator can change it, or is missing, which
/// the hand-off refuses below as a desktop refusal ([`crate::boxside::helper_trusted`]), or is the stand-in helper of
/// the desktop seam, which is not judged; else why not.
#[cfg(feature = "test-seams")]
fn helper_trusted(seams: &BoxSeams, image: &Path) -> Result<(), String> {
    match seams.desktop {
        Some(_) => Ok(()),
        None => crate::boxside::helper_trusted(image),
    }
}

/// `Ok` when the helper's image `image` is installed where only an administrator can change it, or is missing, which
/// the hand-off refuses below as a desktop refusal ([`crate::boxside::helper_trusted`]); else why not.
#[cfg(not(feature = "test-seams"))]
fn helper_trusted(_seams: &BoxSeams, image: &Path) -> Result<(), String> {
    crate::boxside::helper_trusted(image)
}

/// The helper's task started as `task` says ([`gate_win32::run_task`]); under the desktop seam, the answer the seam
/// names in its place, or the stand-in helper started with the seam's fault and the job that holds it.
#[cfg(feature = "test-seams")]
fn run_helper_task(
    seams: &BoxSeams,
    task: &TaskStart<'_>,
    deadline: Instant,
    wake: &[BorrowedHandle<'_>],
) -> (Result<StartedTask, TaskError>, Option<Job>) {
    use gate_args::DesktopMode;

    let Some(mode) = seams.desktop else {
        return (gate_win32::run_task(task, deadline, wake), None);
    };
    let fault = match mode {
        DesktopMode::TaskNotRegistered => return (Err(TaskError::NotRegistered), None),
        DesktopMode::TaskSetUpWrong => return (Err(TaskError::SetUpWrong("the desktop seam's definition".to_owned())), None),
        DesktopMode::TaskFailed => return (Err(TaskError::Failed("the desktop seam's start failed".to_owned())), None),
        DesktopMode::TaskNoPid => return (Ok(StartedTask::NoPid), None),
        DesktopMode::Ok => "",
        DesktopMode::ExitBeforeHello => "exit-before-hello",
        DesktopMode::Hang => "hang",
        DesktopMode::WrongVersion => "wrong-version",
        DesktopMode::LieHandle => "lie-handle",
        DesktopMode::ExitAfterCreate => "exit-after-create",
    };
    match stand_in(fault, task.argument, seams.desktop_ms) {
        Ok((pid, job)) => (Ok(StartedTask::Process(pid)), job),
        Err(error) => (Err(TaskError::Failed(format!("the stand-in helper cannot be started: {error}"))), None),
    }
}

/// The helper's task started as `task` says ([`gate_win32::run_task`]).
#[cfg(not(feature = "test-seams"))]
fn run_helper_task(
    _seams: &BoxSeams,
    task: &TaskStart<'_>,
    deadline: Instant,
    wake: &[BorrowedHandle<'_>],
) -> (Result<StartedTask, TaskError>, Option<Job>) {
    (gate_win32::run_task(task, deadline, wake), None)
}

/// The stand-in helper started in the box's own session, as the task would start the helper: below normal priority,
/// with `argument` as its one argument, its environment the box's with the helper's seams set: the fault `fault` (none
/// when empty), a cap on its limits of twice the box's cap `cap_ms`, and a pipe its user owns. It runs in a
/// kill-on-close job that allows no breakaway, unless the box's environment asks it to record its children
/// ([`RECORD_VARIABLE`]), when it runs in none of the box's. Its pid, and the job, which the caller keeps for as long as
/// the command runs.
#[cfg(feature = "test-seams")]
fn stand_in(fault: &str, argument: &TaskArgument, cap_ms: Option<u64>) -> io::Result<(std::num::NonZeroU32, Option<Job>)> {
    let application = std::env::current_exe()?.with_file_name(STAND_IN);
    let directory = application
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("the stand-in helper is in no folder"))?;
    let mut command_line = std::ffi::OsString::from("\"");
    command_line.push(application.as_os_str());
    command_line.push("\" ");
    command_line.push(argument.as_str());
    let cap = cap_ms.map(|ms| ms.saturating_mul(2).to_string());
    let seams = [
        ("GATE_DESKTOP_TEST_FAULT", Some(fault)),
        ("GATE_DESKTOP_TEST_LIMIT_MS", cap.as_deref()),
        ("GATE_DESKTOP_TEST_PIPE_OWNER", Some("user")),
    ];
    let inherited = std::env::vars_os().filter(|(name, _)| !seams.iter().any(|(seam, _)| name.eq_ignore_ascii_case(seam)));
    let set = seams
        .iter()
        .filter_map(|(name, value)| value.map(|value| (std::ffi::OsString::from(name), std::ffi::OsString::from(value))));
    let launch = gate_win32::Launch {
        application,
        command_line: command_line.encode_wide().collect(),
        directory,
        environment: gate_win32::EnvironmentBlock::new(inherited.chain(set))?,
        output: gate_win32::LaunchOutput::Piped,
        priority: Some(gate_win32::PriorityClass::BelowNormal),
    };
    let suspended = launch.start_suspended().map_err(|error| io::Error::other(error.to_string()))?;
    let (running, job) = if let Some(record) = std::env::var_os(RECORD_VARIABLE) {
        // Still suspended, so a record that cannot be written ends the stand-in with the drop of `suspended`.
        record_stand_in(Path::new(&record), suspended.pid())?;
        (suspended.resume(), None)
    } else {
        let job = Job::anonymous_kill_on_close()?;
        (suspended.assign_and_resume(&job), Some(job))
    };
    let running = running.map_err(|error| io::Error::other(error.to_string()))?;
    let pid = std::num::NonZeroU32::new(running.pid()).ok_or_else(|| io::Error::other("the stand-in helper has no pid"))?;
    Ok((pid, job))
}

/// Adds the stand-in helper's own `pid` and creation time to the record at `record` as `stand-in <pid> <created>`,
/// which no other process at a reused pid matches: before any child it records, as it is written while the stand-in
/// is still suspended.
#[cfg(feature = "test-seams")]
fn record_stand_in(record: &Path, pid: u32) -> io::Result<()> {
    use std::io::Write;

    let created = QueryOnlyProcess::open(pid)?.created()?;
    let mut file = OpenOptions::new().create(true).append(true).open(record)?;
    writeln!(file, "{STAND_IN_RECORD}{pid} {created}")
}

/// The start of the record's line holding the stand-in helper's own pid.
#[cfg(feature = "test-seams")]
const STAND_IN_RECORD: &str = "stand-in ";

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a case whose files cannot be made must fail loudly")]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use gate_win32::EndWait;
    use jiff::Timestamp;
    use proc_table::ProcessInfo;

    use crate::boxside::desktop::lines::DesktopRefusal;

    use super::{EndingHelper, HelperEnd, SweptMachine, SweptProcess, end_within, helper_not_ended, identity_of, sweep_on, unlisted_line};

    /// A case's own temp folder, removed with everything in it when the case ends, also when it fails.
    struct Removed(PathBuf);

    impl Drop for Removed {
        fn drop(&mut self) {
            // Nothing here may fail a case that is already ending; a folder left behind is only temp clutter.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_file_reached_through_a_junction_has_its_targets_identity() {
        let folder = crate::test_support::temp_dir().join(format!("gate-identity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        let removed = Removed(folder);
        let folder = &removed.0;
        let real = folder.join("real");
        fs::create_dir_all(&real).expect("the target folder is made");
        fs::write(real.join("tool.exe"), "tool").expect("the file is made");
        fs::write(real.join("other.exe"), "other").expect("the other file is made");
        let current = folder.join("current");
        crate::test_support::junction(&current, &real);

        let through = identity_of(&current.join("tool.exe")).expect("the file is read through the junction");

        assert_eq!(
            through,
            identity_of(&real.join("tool.exe")).expect("the target is read"),
            "one file, two paths"
        );
        assert_ne!(
            through,
            identity_of(&real.join("other.exe")).expect("the other file is read"),
            "another file in the same folder is another identity"
        );
        assert!(
            identity_of(&real.join("missing.exe")).is_err(),
            "a file that is not there has no identity"
        );
    }

    /// A helper that never ends by itself and is never seen ended, on a clock that every wait and end runs to the
    /// bound it was given, as a helper on a busy machine would, and its wait `late` past that: the bounds it was given
    /// are kept.
    struct Stubborn {
        clock: Cell<Instant>,
        bounds: RefCell<Vec<Duration>>,
        wait: io::Result<EndWait>,
        late: Duration,
    }

    impl Stubborn {
        fn spend(&self, bound: Duration) {
            self.bounds.borrow_mut().push(bound);
            self.clock.set(self.clock.get() + bound);
        }
    }

    impl EndingHelper for Stubborn {
        fn wait_for_end(&self, bound: Duration) -> io::Result<EndWait> {
            self.spend(bound);
            self.clock.set(self.clock.get() + self.late);
            match &self.wait {
                Ok(waited) => Ok(*waited),
                Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
            }
        }

        fn end(&self, bound: Duration) -> io::Result<bool> {
            self.spend(bound);
            Ok(false)
        }
    }

    /// A [`Stubborn`] helper whose wait answers `wait`, on time.
    fn stubborn(wait: io::Result<EndWait>) -> Stubborn {
        Stubborn {
            clock: Cell::new(Instant::now()),
            bounds: RefCell::new(Vec::new()),
            wait,
            late: Duration::ZERO,
        }
    }

    #[test]
    fn the_end_keeps_its_slice_after_a_late_wait() {
        let limit = Duration::from_secs(10);
        let helper = Stubborn {
            late: Duration::from_secs(1),
            ..stubborn(Ok(EndWait::Running))
        };

        end_within(&helper, limit, || helper.clock.get(), true);

        assert_eq!(
            helper.bounds.borrow().as_slice(),
            [Duration::from_millis(9_500), super::END_SLICE],
            "a wait that returned past its bound still leaves the end its slice"
        );
    }

    #[test]
    fn a_limit_under_two_seconds_keeps_a_quarter_of_it_for_the_end() {
        let limit = Duration::from_secs(1);
        let helper = stubborn(Ok(EndWait::Running));

        end_within(&helper, limit, || helper.clock.get(), true);

        assert_eq!(
            helper.bounds.borrow().as_slice(),
            [Duration::from_millis(750), Duration::from_millis(250)],
            "the wait, then the end's quarter"
        );
    }

    #[test]
    fn the_helpers_end_takes_one_end_limit_in_all() {
        let limit = Duration::from_secs(10);
        for wait in [Ok(EndWait::Running), Ok(EndWait::Woken(0)), Err(io::Error::other("the wait failed"))] {
            let helper = stubborn(wait);

            let ended = end_within(&helper, limit, || helper.clock.get(), true);

            let bounds = helper.bounds.borrow();
            assert_eq!(bounds.len(), 2, "one wait, then one end: {bounds:?}");
            assert!(bounds.iter().sum::<Duration>() <= limit, "within one end limit in all: {bounds:?}");
            assert!(
                bounds[1] > Duration::ZERO,
                "the end keeps a part of the limit for its own wait: {bounds:?}"
            );
            assert!(matches!(ended, HelperEnd::NotEnded(_)), "{ended:?}");
        }

        let helper = stubborn(Ok(EndWait::Running));
        let ended = end_within(&helper, limit, || helper.clock.get(), false);
        assert_eq!(
            helper.bounds.borrow().as_slice(),
            [limit],
            "no wait first: the end alone, given the limit"
        );
        assert!(matches!(ended, HelperEnd::NotEnded(_)), "{ended:?}");

        let helper = stubborn(Ok(EndWait::Ended));
        let ended = end_within(&helper, limit, || helper.clock.get(), true);
        assert_eq!(helper.bounds.borrow().len(), 1, "a helper that ended by itself is not ended again");
        assert!(matches!(ended, HelperEnd::Exited), "{ended:?}");
    }

    #[test]
    fn each_helper_that_did_not_start_has_its_own_line() {
        let start = "gate: not routed: the builder has no desktop session: the desktop helper did not start:";
        let whys = [
            (super::NO_CONNECT, "it did not connect in time"),
            (super::NO_ANSWER, "it did not answer in time"),
            (super::NOT_TAKEN, "it did not take the box's message in time"),
            (super::NOT_A_MESSAGE, "it sent a message the box cannot read"),
            (super::OUT_OF_TURN, "it sent a message the box did not expect"),
            (super::CLOSED, "it closed the control pipe"),
        ];
        for (why, said) in whys {
            let line = crate::boxside::desktop::lines::DesktopRefusal::HelperDidNotStart(why.to_owned()).line();
            assert_eq!(line, format!("{start} {said}"));
        }
        let pipe = match super::Fault::pipe(&io::Error::other("broken")) {
            super::Fault::Refused(refusal) => Some(refusal.line()),
            super::Fault::Stopped | super::Fault::Gone => None,
        };
        assert_eq!(
            pipe,
            Some(format!("{start} the control pipe failed: broken")),
            "a pipe failure is a refusal"
        );
    }

    /// A builder with no desktop helper passes the preflight, and the hand-off then refuses it as a desktop run, as it
    /// did before the install tripwire: only a helper that is there and not trusted is an `install` refusal.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose file cannot be read must fail loudly")]
    fn a_missing_helper_is_a_desktop_refusal_not_an_install_one() {
        let missing = crate::test_support::temp_dir().join(format!("gate-hand-off-no-helper-{}.exe", std::process::id()));
        let _ = fs::remove_file(&missing);
        let seams = gate_args::BoxSeams::default();

        assert_eq!(
            super::helper_trusted(&seams, &missing),
            Ok(()),
            "a missing helper is not a helper that is there and not trusted"
        );

        let error = identity_of(&missing).expect_err("the missing helper has no identity");
        let line = DesktopRefusal::Machinery(super::with_error(super::HELPER_FILE_UNREAD, &error)).line();
        assert!(line.starts_with("gate: not routed: the builder has no desktop session:"), "{line}");
        assert!(!line.contains(crate::remote::INSTALL_UNTRUSTED), "{line}");
    }

    #[test]
    fn each_machinery_why_of_the_hand_off_has_its_line() {
        let refused = "gate: not routed: the builder has no desktop session:";
        let denied = io::Error::other("access denied");
        let whys = [
            (super::PROGRAM_UNREAD, "the program the box sends cannot be read"),
            (super::JOB_UNMADE, "the command's job cannot be made"),
            (super::HELPER_FILE_UNREAD, "the desktop helper's file cannot be read"),
            (super::USER_UNREAD, "the box's own user cannot be read"),
        ];
        for (why, said) in whys {
            let line = DesktopRefusal::Machinery(super::with_error(why, &denied)).line();
            assert_eq!(line, format!("{refused} {said}: access denied"));
        }
        assert_eq!(
            DesktopRefusal::HelperDidNotStart(super::with_error(super::EXIT_CODE_UNREAD, &denied)).line(),
            format!("{refused} the desktop helper did not start: its exit code cannot be read: access denied")
        );
    }

    #[test]
    fn the_hand_offs_own_lines_name_their_process_and_error() {
        assert_eq!(
            helper_not_ended(4321, &io::Error::other("access denied")),
            "gate: remote could not end the desktop helper (process 4321): access denied"
        );
        assert_eq!(
            unlisted_line(&io::Error::other("no snapshot")),
            "gate: remote could not list processes to end after the desktop command: no snapshot"
        );
    }

    /// The helper's pid in every sweep case.
    const HELPER: u32 = 4000;
    /// The session the builder's desktop runs in, in every sweep case.
    const SESSION: u32 = 3;
    /// The pid of the one process every sweep case's snapshot holds under the helper.
    const TARGET: u32 = 4100;

    /// The time `second` seconds after the epoch.
    fn time(second: i64) -> Timestamp {
        Timestamp::from_second(second).expect("a time in range")
    }

    /// When the helper of every sweep case was created.
    fn helper_created() -> Timestamp {
        time(100)
    }

    /// When the target of every sweep case was created, as the snapshot holds it.
    fn target_created() -> Timestamp {
        time(200)
    }

    /// What opening the target of a sweep case gives.
    enum Opened {
        /// No process has its pid any more.
        Gone,
        /// It cannot be opened, for this error.
        Refused(&'static str),
        /// A process with this parent and creation time, whose end fails with this error when there is one.
        Process {
            parent: u32,
            created: Timestamp,
            end_error: Option<&'static str>,
        },
    }

    /// A machine of a sweep case: a snapshot holding the target, or the error taking it fails with, and the target as
    /// opening it gives it; every pid it ended is kept.
    struct Scripted {
        snapshot: Result<(), &'static str>,
        opened: Opened,
        ended: Rc<RefCell<Vec<u32>>>,
    }

    /// The target as a sweep case opened it.
    struct ScriptedProcess {
        pid: u32,
        parent: u32,
        created: Timestamp,
        end_error: Option<&'static str>,
        ended: Rc<RefCell<Vec<u32>>>,
    }

    impl SweptMachine for Scripted {
        type Process = ScriptedProcess;

        fn snapshot(&self) -> io::Result<Vec<ProcessInfo>> {
            self.snapshot.map_err(io::Error::other)?;
            Ok(vec![ProcessInfo {
                pid: TARGET,
                parent_pid: HELPER,
                name: "fake-tool.exe".to_owned(),
                command_line: None,
                created: Some(target_created()),
            }])
        }

        fn session_of(&self, _pid: u32) -> Option<u32> {
            Some(SESSION)
        }

        fn open(&self, pid: u32) -> io::Result<Option<ScriptedProcess>> {
            match self.opened {
                Opened::Gone => Ok(None),
                Opened::Refused(error) => Err(io::Error::other(error)),
                Opened::Process { parent, created, end_error } => Ok(Some(ScriptedProcess {
                    pid,
                    parent,
                    created,
                    end_error,
                    ended: Rc::clone(&self.ended),
                })),
            }
        }
    }

    impl SweptProcess for ScriptedProcess {
        fn parent_pid(&self) -> io::Result<u32> {
            Ok(self.parent)
        }

        fn created(&self) -> io::Result<Timestamp> {
            Ok(self.created)
        }

        fn terminate(&self, _exit_code: u32) -> io::Result<()> {
            if let Some(error) = self.end_error {
                return Err(io::Error::other(error));
            }
            self.ended.borrow_mut().push(self.pid);
            Ok(())
        }
    }

    /// A sweep case's machine: its snapshot taken as `snapshot` says, and its target opened as `opened`.
    fn scripted(snapshot: Result<(), &'static str>, opened: Opened) -> Scripted {
        Scripted {
            snapshot,
            opened,
            ended: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// The target opened still the helper's: its parent the helper and its creation time the snapshot's.
    fn still_the_helpers(end_error: Option<&'static str>) -> Opened {
        Opened::Process {
            parent: HELPER,
            created: target_created(),
            end_error,
        }
    }

    /// The lines a sweep of `machine` says, and the pids it ended.
    fn swept(machine: &Scripted) -> (Vec<String>, Vec<u32>) {
        let mut said = Vec::new();
        sweep_on(machine, HELPER, helper_created(), SESSION, &mut |line| said.push(line.to_owned()));
        (said, machine.ended.borrow().clone())
    }

    #[test]
    fn a_sweep_ends_a_target_still_the_helpers() {
        let machine = scripted(Ok(()), still_the_helpers(None));

        assert_eq!(swept(&machine), (Vec::new(), vec![TARGET]));
    }

    #[test]
    fn a_sweep_leaves_a_target_whose_parent_changed() {
        let machine = scripted(
            Ok(()),
            Opened::Process {
                parent: 4200,
                created: target_created(),
                end_error: None,
            },
        );

        assert_eq!(swept(&machine), (Vec::new(), Vec::new()), "another process took the pid");
    }

    #[test]
    fn a_sweep_leaves_a_target_whose_creation_time_changed() {
        let machine = scripted(
            Ok(()),
            Opened::Process {
                parent: HELPER,
                created: time(300),
                end_error: None,
            },
        );

        assert_eq!(swept(&machine), (Vec::new(), Vec::new()), "another process took the pid");
    }

    #[test]
    fn a_sweep_skips_a_target_that_is_gone_without_a_line() {
        let machine = scripted(Ok(()), Opened::Gone);

        assert_eq!(swept(&machine), (Vec::new(), Vec::new()));
    }

    #[test]
    fn a_sweep_says_a_target_it_cannot_open() {
        let machine = scripted(Ok(()), Opened::Refused("access denied"));

        assert_eq!(
            swept(&machine),
            (
                vec!["gate: remote could not end the desktop command (process 4100): access denied".to_owned()],
                Vec::new()
            )
        );
    }

    #[test]
    fn a_sweep_says_a_target_it_cannot_end() {
        let machine = scripted(Ok(()), still_the_helpers(Some("access denied")));

        assert_eq!(
            swept(&machine),
            (
                vec!["gate: remote could not end the desktop command (process 4100): access denied".to_owned()],
                Vec::new()
            )
        );
    }

    #[test]
    fn a_sweep_whose_snapshot_fails_says_so_and_ends_nothing() {
        let machine = scripted(Err("no snapshot"), still_the_helpers(None));

        assert_eq!(
            swept(&machine),
            (
                vec!["gate: remote could not list processes to end after the desktop command: no snapshot".to_owned()],
                Vec::new()
            )
        );
    }
}
