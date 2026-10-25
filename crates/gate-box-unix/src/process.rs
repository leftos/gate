//! The box's process helpers: the bounded runner every program the box starts runs in (the tool probes, `dotnet
//! --list-sdks`, the git steps and the login shell), and the login shell's `PATH` those programs are found along.
//!
//! The runner starts its program as the leader of a process group of its own, its pipes read by threads of their own
//! ([`crate::forward`]), and watches the leader without reaping it (`waitid` with `WNOWAIT`), so the group's id stays
//! the leader's while anything in it is signalled: once the leader exits, or its bound passes, or its client is gone,
//! what is left of the group is ended, the group's record removed, and only then is the leader reaped and its readers
//! drained. No signal is sent to the group after the reap.

use std::ffi::{CString, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use crate::console::{Forwarding, GAT, Stamp, Stream};
use crate::files::{self, HeldLock};
use crate::forward::{Readers, Take};
use crate::lines::{Login, leader_start_text};
use crate::pid_info;
use crate::process_lines::{ProcessLines, image_of};

/// The most bytes of a program's standard error kept: its tail.
const STDERR_TAIL: usize = 64 * 1024;
/// How often the runner looks whether its program has exited.
const POLL: Duration = Duration::from_millis(10);
/// How often the runner probes whether its client has gone ([`Forwarding::client_gone_probe`]).
const PROBE: Duration = Duration::from_millis(100);

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
    /// The end of its standard error, empty when it was forwarded.
    pub stderr: Vec<u8>,
    /// Whether a process it left, out of its group's reach, still held its output open once the drain gave up on it.
    pub leftover: bool,
    /// Why the runner ended the program's group before its leader exited, when it did: its client gone, or a
    /// [`Watch::check`]. `None` for a leader that exited of itself.
    pub ended_by: Option<EndedBy>,
}

/// Why a [`Watch::check`] ends a program's group: its own word for the ending, for the caller to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Halt(pub String);

/// Why the runner ended a program's group before its leader exited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndedBy {
    /// Its client was gone ([`Watch::ends_on_gone`]).
    Gone,
    /// A [`Watch::check`] ended it, for this.
    Halted(Halt),
}

/// What the runner does with a program's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forward {
    /// Standard output kept up to the cap, the end of standard error kept.
    Kept,
    /// Standard output kept up to the cap, standard error's lines said through the output as standard error, stamped as
    /// this says.
    Err(Stamp),
    /// Both streams' lines said through the output, each as itself, standard output stamped as the first says and
    /// standard error as the second; nothing kept.
    Both(Stamp, Stamp),
}

/// Why a program did not run to an exit the runner could read.
#[derive(Debug)]
pub enum NotRan {
    /// It could not be started.
    Start(io::Error),
    /// It was still running at the deadline; its process group was ended.
    TimedOut {
        /// The process group that was ended.
        group: i32,
    },
    /// It wrote more than the cap to its standard output.
    OverCap,
    /// Its exit or its output could not be read, or its group record not written or removed.
    Failed(String),
}

/// How the runner ends what is left of a program's process group: at once, or asked first.
#[derive(Clone, Copy, Debug)]
pub enum Ending {
    /// `SIGKILL` at once.
    Kill,
    /// `SIGTERM`, then `SIGKILL` once this grace has passed with anything of the group still running.
    TermThenKill(Duration),
}

/// The job record a program's process group is written to as the program starts, so `box stop` finds it: the job's
/// name, the group, then the group's session in decimal, written from the child before its exec, then rewritten once
/// the child started with its start as well ([`crate::lines::job_record`]). It is written and removed only while the
/// run's work lock is held.
#[derive(Debug)]
pub struct GroupRecord<'a> {
    path: PathBuf,
    job: String,
    lock: &'a HeldLock,
}

impl<'a> GroupRecord<'a> {
    /// The record `path` naming the job `job`, changed only while `lock` is held.
    #[must_use]
    pub fn new(path: PathBuf, job: &str, lock: &'a HeldLock) -> GroupRecord<'a> {
        GroupRecord {
            path,
            job: job.to_owned(),
            lock,
        }
    }

    /// Why the record may not be changed now: the work lock no longer held ([`HeldLock::still_held`]), or not looked
    /// at; `None` while it is held.
    fn lock_lost(&self) -> Option<String> {
        lock_lost(self.lock)
    }

    /// Removes the record, a missing one fine, and syncs its folder, while the work lock is held; else why not, the
    /// record left as it is when the lock is lost.
    fn removed(&self) -> Result<(), String> {
        if let Some(why) = self.lock_lost() {
            return Err(why);
        }
        files::remove_if_there(&self.path)
            .and_then(|()| files::folder_synced(&self.path))
            .map_err(|error| format!("its group record could not be removed: {error}"))
    }
}

/// Why a record may not be changed now: the work lock `lock` no longer held ([`HeldLock::still_held`]), or not looked
/// at; `None` while it is held.
fn lock_lost(lock: &HeldLock) -> Option<String> {
    let shown = lock.path().display();
    match lock.still_held() {
        Ok(true) => None,
        Ok(false) => Some(format!("the work folder lock {shown} is no longer held")),
        Err(error) => Some(format!("the work folder lock {shown} could not be looked at: {error}")),
    }
}

/// How the runner watches one program.
pub struct Watch<'a> {
    /// The most bytes of standard output kept; more is [`NotRan::OverCap`].
    pub cap: usize,
    /// When the program's group is ended and the run is [`NotRan::TimedOut`]; `None` for no deadline.
    pub deadline: Option<Instant>,
    /// How the group is ended, at the deadline, for a client gone or a check, or for what the leader left once it
    /// exited.
    pub ending: Ending,
    /// The box's output: the lines `forward` names are said through it, and its client watched when `ends_on_gone`
    /// is set.
    pub output: Option<Arc<dyn Forwarding>>,
    /// What is done with the program's output: kept, or its lines said through `output`; with no `output`, kept.
    pub forward: Forward,
    /// Whether a client gone ends the group: looked at every [`POLL`], and probed every [`PROBE`].
    pub ends_on_gone: bool,
    /// The record the group is written to from the child before its program runs, and removed before the reap.
    pub record: Option<&'a GroupRecord<'a>>,
    /// Called on the waiting thread every [`POLL`] while the leader runs: a [`Halt`] ends the group as `ending` says,
    /// through the runner's own ending, and the run gives [`EndedBy::Halted`].
    pub check: Option<&'a dyn Fn() -> Option<Halt>>,
    /// Called once on the waiting thread with the program's process group, once its readers started and before the
    /// first look at the leader; never when the readers could not start.
    pub on_started: Option<&'a dyn Fn(i32)>,
    /// Called once, when the runner stops watching the leader (its exit seen, or its group's ending decided), before
    /// the group is ended and the leader reaped.
    pub on_leader_exit: Option<&'a dyn Fn()>,
    /// The process lines of the program's group, when it has them: the leader's spawn said as it starts, before its
    /// readers, the group's end told once it has ended or been given up on, and the leader's exit said from its reap,
    /// or its left line when it is not reaped.
    pub processes: Option<&'a ProcessLines>,
}

/// Runs `command` in a process group of its own, standard input null, its standard output kept up to `cap` bytes and
/// the end of its standard error, until `deadline`: past it the whole group is killed (`SIGKILL`). What the program
/// left in its group once it exited is killed too, before the program is reaped.
///
/// # Errors
///
/// [`NotRan`]: it could not start, it ran past the deadline, it wrote more than `cap`, or its exit could not be read.
pub fn run_bounded(command: Command, cap: usize, deadline: Instant) -> Result<Ran, NotRan> {
    run_watched(
        command,
        &Watch {
            cap,
            deadline: Some(deadline),
            ending: Ending::Kill,
            output: None,
            forward: Forward::Kept,
            ends_on_gone: false,
            record: None,
            check: None,
            on_started: None,
            on_leader_exit: None,
            processes: None,
        },
    )
}

/// Runs `command` as `watch` says: started as the leader of a process group of its own, standard input null, its
/// group written to `watch.record` from the child before the program runs; watched until the leader exits, the
/// deadline passes, `watch.check` halts it, or (`ends_on_gone`) the client is gone. Then `watch.on_leader_exit` is
/// called once, what is left of the group is ended as `watch.ending` says while the leader is still unreaped, the
/// record removed, the leader reaped, and the readers drained
/// ([`Readers::drain`]). A program that exited keeps its status, whatever its leftovers did. A group still there after
/// `SIGKILL`, or a leader whose exit could not be read, is not reaped: its readers are stopped and its record kept, so
/// the next run refuses `busy`, unless the leader is no child of the box's and its group has no process left
/// ([`settled`]).
///
/// # Errors
///
/// [`NotRan`], as [`run_bounded`] gives it, or a group record that could not be written or removed.
pub fn run_watched(command: Command, watch: &Watch<'_>) -> Result<Ran, NotRan> {
    let image = image_of(Path::new(command.get_program()));
    let mut child = spawned_recorded(command, watch)?;
    let Ok(group) = i32::try_from(child.id()) else {
        // A pid that is no pid_t cannot be signalled as a group; the child alone is ended.
        let _ = child.kill();
        let _ = child.wait();
        return Err(NotRan::Failed(format!("the process id {} is no pid_t", child.id())));
    };
    // Before its readers start, so no line of its own comes before its spawn.
    if let Some(processes) = watch.processes {
        processes.leader_started(group, image.as_deref());
    }
    let mut readers = Readers::default();
    let started = readers_started(&mut readers, &mut child, watch);
    let leader = Leader { child, group };
    let why = stopped_watching(&leader, group, started, watch);
    let settled = settled(&leader, &why, watch.ending, KILL_WAIT);
    if let Some(processes) = watch.processes {
        processes.group_ended();
        if !matches!(settled, Settled::Ended) {
            processes.leader_ended(None);
        }
    }
    match settled {
        Settled::Ended => {}
        Settled::StillThere => {
            // Neither reaped nor unrecorded: the group still exists, so the next run's busy refusal is right.
            readers.stop();
            return Err(NotRan::Failed(still_there_why(group, KILL_WAIT)));
        }
        Settled::Unread { error, keep } => {
            // Not reaped: the leader's exit was not seen, so its group may still be there.
            readers.stop();
            let unrecorded = if keep {
                Ok(())
            } else {
                watch.record.map_or(Ok(()), GroupRecord::removed)
            };
            let why = format!("its exit could not be read: {error}");
            return Err(NotRan::Failed(match unrecorded {
                Ok(()) => why,
                Err(lost) => format!("{why}; {lost}"),
            }));
        }
    }
    let unrecorded = watch.record.map_or(Ok(()), GroupRecord::removed);
    let status = leader.reap();
    if let Some(processes) = watch.processes {
        processes.leader_ended(status.as_ref().ok());
    }
    let drained = readers.drain();
    let ended_by = match &why {
        Why::Gone => Some(EndedBy::Gone),
        Why::Halted(halt) => Some(EndedBy::Halted(halt.clone())),
        _ => None,
    };
    let ended = Ended { why, unrecorded, status };
    let status = ended.status(group)?;
    let mut kept = drained.kept.into_iter();
    let unread = |error: io::Error| NotRan::Failed(format!("its output could not be read: {error}"));
    let (stdout, over) = kept.next().unwrap_or(Ok((Vec::new(), false))).map_err(unread)?;
    let (stderr, _) = kept.next().unwrap_or(Ok((Vec::new(), false))).map_err(unread)?;
    if over {
        return Err(NotRan::OverCap);
    }
    Ok(Ran {
        status,
        stdout,
        stderr,
        leftover: !drained.all_ended && !drained.stalled,
        ended_by,
    })
}

/// Why a run whose process group `group` was still there `wait` after `SIGKILL` gives [`NotRan::Failed`]
/// ([`Settled::StillThere`]).
pub(crate) fn still_there_why(group: i32, wait: Duration) -> String {
    format!("{STILL_THERE_HEAD}{group}{STILL_THERE_MIDDLE}{}{STILL_THERE_TAIL}", wait.as_secs())
}

/// The start of [`still_there_why`]'s text, before the group.
const STILL_THERE_HEAD: &str = "its process group ";
/// [`still_there_why`]'s text between the group and the wait.
const STILL_THERE_MIDDLE: &str = " was still there ";
/// The end of [`still_there_why`]'s text, after the wait.
const STILL_THERE_TAIL: &str = " s after SIGKILL";

/// Whether `why`, a [`NotRan::Failed`]'s, says the program's group was still there after `SIGKILL`
/// ([`still_there_why`]), rather than any other failure.
#[must_use]
pub fn is_still_there(why: &str) -> bool {
    why.strip_prefix(STILL_THERE_HEAD)
        .and_then(|rest| rest.split_once(STILL_THERE_MIDDLE))
        .is_some_and(|(group, wait)| group.parse::<i32>().is_ok() && wait.ends_with(STILL_THERE_TAIL))
}

/// Watches `leader`, of the process group `group`, until the runner stops watching it ([`watched`]), `watch.on_started`
/// called once first, unless its readers did not start, `started` giving why; then calls `watch.on_leader_exit` once,
/// before the group is ended and the leader reaped. Why it stopped.
fn stopped_watching(leader: &impl Group, group: i32, started: io::Result<()>, watch: &Watch<'_>) -> Why {
    let why = match started {
        Ok(()) => {
            if let Some(hook) = watch.on_started {
                hook(group);
            }
            watched(leader, watch)
        }
        Err(error) => Why::Failed(format!("its output could not be read: {error}")),
    };
    if let Some(hook) = watch.on_leader_exit {
        hook();
    }
    why
}

/// Starts `command` as the leader of a process group of its own, standard input null, its output piped, its group
/// written to `watch.record` from the child before the program runs, the work lock checked first.
///
/// # Errors
///
/// The work lock no longer held, the record's temp file not made, or the start failing ([`NotRan::Start`]), the
/// record and its temp file then removed.
fn spawned_recorded(mut command: Command, watch: &Watch<'_>) -> Result<Child, NotRan> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(why) = watch.record.and_then(GroupRecord::lock_lost) {
        return Err(NotRan::Failed(why));
    }
    let staged = watch
        .record
        .map(Staged::new)
        .transpose()
        .map_err(|error| NotRan::Failed(format!("its group record could not be readied: {error}")))?;
    if let Some(staged) = &staged {
        let child_side = staged.child_side();
        // SAFETY: the closure runs in the child between fork and exec, and calls only `getpid`, `getsid`, `write`,
        // `fsync` and `rename` on a descriptor and names made before the fork; it allocates nothing and takes no lock.
        unsafe {
            command.pre_exec(move || child_side.write());
        }
    }
    let child = spawned(&mut command).map_err(|error| {
        if let Some(staged) = &staged {
            staged.abandon();
        }
        NotRan::Start(error)
    })?;
    if let Some(staged) = &staged
        && let Err(why) = staged.stamped(&child)
    {
        unstamped_said(watch, &staged.path, &why);
    }
    Ok(child)
}

/// Says on the run's standard error, through `watch.output`, that the job record `record` keeps no leader start, and
/// `why`; a run with no output says nothing.
fn unstamped_said(watch: &Watch<'_>, record: &Path, why: &str) {
    if let Some(output) = &watch.output {
        let line = format!("gate: remote job record {} keeps no leader start: {why}", record.display());
        // A client gone takes no line; the run goes on whether or not it was said.
        let _ = output.say_unless(&AtomicBool::new(false), Stream::Err, GAT, &line);
    }
}

/// Starts the readers of `child`'s standard output, kept up to the cap, and of its standard error, forwarded or its
/// tail kept as `watch` says; output first, so the drain gives it first.
fn readers_started(readers: &mut Readers, child: &mut Child, watch: &Watch<'_>) -> io::Result<()> {
    let (out, err) = match (&watch.output, watch.forward) {
        (Some(output), Forward::Both(out, err)) => (
            Take::Lines(Arc::clone(output), Stream::Out, out),
            Take::Lines(Arc::clone(output), Stream::Err, err),
        ),
        (Some(output), Forward::Err(err)) => (Take::Capped(watch.cap), Take::Lines(Arc::clone(output), Stream::Err, err)),
        _ => (Take::Capped(watch.cap), Take::Tail(STDERR_TAIL)),
    };
    match child.stdout.take() {
        Some(pipe) => readers.read(pipe, out)?,
        None => readers.read(io::empty(), out)?,
    }
    match child.stderr.take() {
        Some(pipe) => readers.read(pipe, err),
        None => readers.read(io::empty(), err),
    }
}

/// Why the runner stopped watching the leader.
enum Why {
    /// The leader exited.
    Exited,
    /// The deadline passed.
    Deadline,
    /// The client is gone.
    Gone,
    /// A [`Watch::check`] ended the group.
    Halted(Halt),
    /// The leader's exit could not be read: this.
    Unread(io::Error),
    /// Its readers could not be started: this.
    Failed(String),
}

/// How a run whose group was ended ended, step by step.
struct Ended {
    why: Why,
    unrecorded: Result<(), String>,
    status: io::Result<ExitStatus>,
}

impl Ended {
    /// The leader's status, or why the run gives none: the first step that failed, or the deadline of the group
    /// `group`.
    fn status(self, group: i32) -> Result<ExitStatus, NotRan> {
        match self.why {
            Why::Failed(why) => return Err(NotRan::Failed(why)),
            Why::Unread(error) => return Err(NotRan::Failed(format!("its exit could not be read: {error}"))),
            Why::Exited | Why::Deadline | Why::Gone | Why::Halted(_) => {}
        }
        let status = self
            .status
            .map_err(|error| NotRan::Failed(format!("its exit could not be read: {error}")))?;
        if matches!(self.why, Why::Deadline) {
            return Err(NotRan::TimedOut { group });
        }
        self.unrecorded.map_err(NotRan::Failed)?;
        Ok(status)
    }
}

/// Watches `leader` every [`POLL`] until it exits, the deadline passes, `watch.check` halts it or, when `watch` asks,
/// the client is gone. Takes no output lock and writes nothing.
fn watched(leader: &impl Group, watch: &Watch<'_>) -> Why {
    let gone_watch = watch.output.as_ref().filter(|_| watch.ends_on_gone);
    let mut probed: Option<Instant> = None;
    loop {
        match leader.exited() {
            Ok(true) => return Why::Exited,
            Ok(false) => {}
            Err(error) => return Why::Unread(error),
        }
        if watch.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Why::Deadline;
        }
        if let Some(halt) = watch.check.and_then(|check| check()) {
            return Why::Halted(halt);
        }
        if let Some(output) = gone_watch
            && client_gone(output.as_ref(), &mut probed)
        {
            return Why::Gone;
        }
        thread::sleep(POLL);
    }
}

/// Whether `output`'s client is gone: a write found it, or a probe, run when [`PROBE`] has passed since `probed`.
fn client_gone(output: &dyn Forwarding, probed: &mut Option<Instant>) -> bool {
    if output.client_gone() {
        return true;
    }
    if probed.is_none_or(|at| at.elapsed() >= PROBE) {
        *probed = Some(Instant::now());
        return output.client_gone_probe();
    }
    false
}

/// How long the runner waits for the leader to exit once its group was sent `SIGKILL`.
const KILL_WAIT: Duration = Duration::from_secs(10);

/// How the ending of a group went.
#[derive(Debug, PartialEq, Eq)]
enum GroupEnd {
    /// The leader has exited, unreaped, and the group is ended.
    Ended,
    /// The leader had not exited, or a process besides it was still in the group, when the wait after `SIGKILL`
    /// passed: the group is still there.
    StillThere,
}

/// A program's process group as the runner ends it, its leader unreaped.
trait Group {
    /// Whether the leader has exited, without reaping it.
    ///
    /// # Errors
    ///
    /// Why its exit could not be read.
    fn exited(&self) -> io::Result<bool>;

    /// Whether a process other than the leader is in the group, the group's processes listed into `pids`; a group
    /// that cannot be listed counts as one with a process left.
    fn others_left(&self, pids: &mut Vec<i32>) -> bool;

    /// Sends `signal` to every process of the group.
    fn signal(&self, signal: i32);
}

/// How the end of a watched group went, its leader not yet reaped.
#[derive(Debug)]
enum Settled {
    /// The leader has exited and the group is ended: the record goes, the leader is reaped and the readers drained.
    Ended,
    /// The leader had not exited, or a process besides it was still in the group, when the wait after `SIGKILL`
    /// passed: the record stays, the leader is not reaped.
    StillThere,
    /// The leader's exit could not be read, for `error`: the leader is not reaped and the readers are stopped; the
    /// record stays when `keep`.
    Unread {
        /// Why the exit could not be read.
        error: io::Error,
        /// Whether the record stays: the group may still be there.
        keep: bool,
    },
}

/// Ends what is left of `group` once the runner stopped watching it for `why`: as `ending` says ([`end_group`]) for an
/// exit, the deadline or a gone client; with `SIGKILL` and up to `kill_wait` for the leader's exit and the group to
/// empty when the readers could not start; with `SIGKILL` alone ([`killed_unless_reaped`]) when the leader's exit
/// could not be read. A look at the leader that fails leaves it unreaped ([`Settled::Unread`]).
fn settled(group: &impl Group, why: &Why, ending: Ending, kill_wait: Duration) -> Settled {
    let ended = match why {
        Why::Unread(error) => {
            killed_unless_reaped(group, error);
            Err(copied(error))
        }
        Why::Failed(_) => {
            group.signal(libc::SIGKILL);
            exited_within(group, kill_wait)
        }
        Why::Exited | Why::Deadline | Why::Gone | Why::Halted(_) => end_group(group, ending, kill_wait),
    };
    match ended {
        Ok(GroupEnd::Ended) => Settled::Ended,
        Ok(GroupEnd::StillThere) => Settled::StillThere,
        Err(error) => unread(group, error),
    }
}

/// The look at `group`'s leader failed with `error`: its record is kept unless the failure was `ECHILD`, the leader
/// no child of the box's to look at, and no process is left in the group.
fn unread(group: &impl Group, error: io::Error) -> Settled {
    let keep = error.raw_os_error() != Some(libc::ECHILD) || {
        let mut pids = Vec::new();
        group.others_left(&mut pids) || !pids.is_empty()
    };
    Settled::Unread { error, keep }
}

/// `error` again: its OS error, or its kind and text.
fn copied(error: &io::Error) -> io::Error {
    error
        .raw_os_error()
        .map_or_else(|| io::Error::new(error.kind(), error.to_string()), io::Error::from_raw_os_error)
}

/// Ends what is left of `group` as `ending` says, the leader not reaped: nothing when the leader has exited and no
/// other process is in the group; else `SIGTERM`, with `SIGCONT` right after it, and the grace for the group to empty,
/// when asked first, then `SIGKILL` and up to `kill_wait` for the leader's exit and the group to empty
/// ([`GroupEnd::StillThere`] past it). A look at the leader that fails sends `SIGKILL` before the failure is given, as
/// the leader is unreaped and the group still the runner's, unless it failed with `ECHILD`, the leader no child of the
/// box's to look at.
///
/// # Errors
///
/// The leader's exit could not be read.
fn end_group(group: &impl Group, ending: Ending, kill_wait: Duration) -> io::Result<GroupEnd> {
    let mut pids = Vec::new();
    if quiet_or_killed(group, &mut pids)? {
        return Ok(GroupEnd::Ended);
    }
    if let Ending::TermThenKill(grace) = ending {
        group.signal(libc::SIGTERM);
        // A stopped process holds its SIGTERM pending until it is continued.
        group.signal(libc::SIGCONT);
        let until = Instant::now() + grace;
        while Instant::now() < until {
            if quiet_or_killed(group, &mut pids)? {
                return Ok(GroupEnd::Ended);
            }
            thread::sleep(POLL);
        }
    }
    group.signal(libc::SIGKILL);
    exited_within(group, kill_wait)
}

/// Whether `group`'s leader has exited and no other process is left in it; a look that fails is given after `SIGKILL`
/// is sent ([`killed_unless_reaped`]).
fn quiet_or_killed(group: &impl Group, pids: &mut Vec<i32>) -> io::Result<bool> {
    match group.exited() {
        Ok(exited) => Ok(exited && !group.others_left(pids)),
        Err(error) => {
            killed_unless_reaped(group, &error);
            Err(error)
        }
    }
}

/// Sends `group` `SIGKILL` after the look at its leader failed with `error`, unless with `ECHILD`: then the leader is
/// no child of the box's to look at, and its id may be another's.
fn killed_unless_reaped(group: &impl Group, error: &io::Error) {
    if error.raw_os_error() != Some(libc::ECHILD) {
        group.signal(libc::SIGKILL);
    }
}

/// Waits up to `wait` for `group`'s leader to exit and for no other process to be left in the group, looking every
/// [`POLL`]: the leader's exit alone does not end a group a member is still in.
///
/// # Errors
///
/// The leader's exit could not be read.
fn exited_within(group: &impl Group, wait: Duration) -> io::Result<GroupEnd> {
    let until = Instant::now() + wait;
    let mut pids = Vec::new();
    loop {
        if group.exited()? && !group.others_left(&mut pids) {
            return Ok(GroupEnd::Ended);
        }
        if Instant::now() >= until {
            return Ok(GroupEnd::StillThere);
        }
        thread::sleep(POLL);
    }
}

/// Removes the group record `path`, a missing one fine, best effort, after a spawn that failed.
fn abandoned(path: &Path) {
    // Best effort: the start's own failure is the one said, and the next run removes a record naming no process.
    let _ = files::remove_if_there(path);
}

/// The leader of a program's process group, its group's id its own pid, until it is reaped: a group is signalled only
/// through this, so never once its id may be another's.
struct Leader {
    child: Child,
    group: i32,
}

impl Leader {
    /// Reaps the leader: its status. The group is never signalled again.
    fn reap(mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

impl Group for Leader {
    fn exited(&self) -> io::Result<bool> {
        exit_seen(self.group)
    }

    #[cfg(target_os = "macos")]
    fn others_left(&self, pids: &mut Vec<i32>) -> bool {
        crate::stop::group_pids_into(self.group, pids).map_or(true, |()| pids.iter().any(|pid| *pid != self.group))
    }

    /// A process group is listed only on macOS: elsewhere what is left counts as left, so it is asked and then killed.
    #[cfg(not(target_os = "macos"))]
    fn others_left(&self, _pids: &mut Vec<i32>) -> bool {
        true
    }

    /// A group already gone is no failure.
    fn signal(&self, signal: i32) {
        #[cfg(test)]
        signal_log::note(self.group, signal);
        // SAFETY: `killpg` takes two integers and touches no memory; the group's leader is unreaped, so its id is the
        // group this runner made.
        unsafe {
            libc::killpg(self.group, signal);
        }
    }
}

/// Whether the child `pid` has exited, looked at with `waitid(P_PID, WEXITED | WNOWAIT | WNOHANG)`, which answers at
/// once and leaves it unreaped.
fn exit_seen(pid: i32) -> io::Result<bool> {
    let id = libc::id_t::try_from(pid).map_err(io::Error::other)?;
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid value, which `waitid` fills when a child has changed state.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` lives across the call, which writes only into it.
        let waited = unsafe { libc::waitid(libc::P_PID, id, &raw mut info, libc::WEXITED | libc::WNOWAIT | libc::WNOHANG) };
        if waited == 0 {
            // SAFETY: `waitid` succeeded, so `info` holds a child's state, or zeros when none had changed.
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// A group record readied before the fork: its temp file open, and the names and text the child writes.
struct Staged<'a> {
    file: File,
    temp: PathBuf,
    path: PathBuf,
    job: Vec<u8>,
    lock: &'a HeldLock,
}

impl<'a> Staged<'a> {
    /// Makes the temp file of `record` beside it ([`files::staged_temp`]), `O_CLOEXEC`, so only the child that writes
    /// it before its exec has it.
    fn new(record: &GroupRecord<'a>) -> io::Result<Staged<'a>> {
        let (temp, file) = files::staged_temp(&record.path)?;
        Ok(Staged {
            file,
            temp,
            path: record.path.clone(),
            job: format!("{}\n", record.job).into_bytes(),
            lock: record.lock,
        })
    }

    /// What the child needs to write the record: owned, so the child allocates nothing.
    fn child_side(&self) -> ChildSide {
        ChildSide {
            fd: self.file.as_raw_fd(),
            job: self.job.clone(),
            temp: c_path(&self.temp),
            path: c_path(&self.path),
        }
    }

    /// Removes what a spawn that failed may have left: the temp file, its name this run's own, and the record when the
    /// child renamed it into place before its exec failed, only while the work lock is held, so another run's record at
    /// the name once the lock was lost is left; a missing one is fine. The lock is looked at just before the unlink, not
    /// with it: a lock lost in the gap between the two is accepted.
    fn abandon(&self) {
        abandoned(&self.temp);
        if matches!(self.lock.still_held(), Ok(true)) {
            abandoned(&self.path);
        }
    }

    /// Rewrites the record the child `child` wrote before its exec with the start of `child`, its group's leader, read
    /// now ([`pid_info::start_of`]), through a temp file synced and renamed over it ([`files::replace_plain`]), while
    /// the work lock is held. Else why not: a start that cannot be read (the child already gone), the lock lost, the
    /// session unread, or a rewrite that failed, each leaving the record as the child wrote it, so a stop signals
    /// nothing; the run goes on.
    fn stamped(&self, child: &Child) -> Result<(), String> {
        let pid = child.id();
        let started = i32::try_from(pid)
            .ok()
            .and_then(pid_info::start_of)
            .ok_or_else(|| format!("the start of its leader {pid} could not be read"))?;
        if let Some(why) = lock_lost(self.lock) {
            return Err(why);
        }
        // SAFETY: `getsid(0)` asks for the caller's own session and touches no memory. The child, forked from this
        // process, was in its session when it wrote the record.
        let session = unsafe { libc::getsid(0) };
        if session < 0 {
            return Err(format!("the session could not be read: {}", io::Error::last_os_error()));
        }
        let mut text = self.job.clone();
        text.extend_from_slice(format!("{pid}\n{session}\n{}\n", leader_start_text(started)).as_bytes());
        files::replace_plain(&self.path, &text).map_err(|error| format!("it could not be rewritten: {error}"))
    }
}

/// `path` as a C string; a path holding a NUL, which no file has, is the empty name, which every call refuses.
fn c_path(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap_or_default()
}

/// The child's side of a [`Staged`] record.
struct ChildSide {
    fd: RawFd,
    job: Vec<u8>,
    temp: CString,
    path: CString,
}

impl ChildSide {
    /// Writes the job's name, the child's pid, which is its group's id, on one line, and its session to the temp
    /// file, syncs it and renames it over the record: async-signal-safe calls only, on what was made before the fork.
    fn write(&self) -> io::Result<()> {
        // SAFETY: `getpid` takes nothing and always succeeds.
        let pid = unsafe { libc::getpid() };
        // SAFETY: `getsid(0)` asks for the caller's own session, touches no memory and fails only for another pid.
        // POSIX does not list it as async-signal-safe, but on macOS it is a bare system call stub, which takes no lock
        // and allocates nothing.
        let session = unsafe { libc::getsid(0) };
        if session < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut digits = [0_u8; 12];
        written_all(self.fd, &self.job)?;
        written_all(self.fd, decimal(pid, &mut digits))?;
        written_all(self.fd, b"\n")?;
        written_all(self.fd, decimal(session, &mut digits))?;
        written_all(self.fd, b"\n")?;
        // SAFETY: `fsync` takes a descriptor, the temp file's, open in this child until its exec.
        if unsafe { libc::fsync(self.fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both names are NUL-terminated strings that live across the call, which only reads them.
        if unsafe { libc::rename(self.temp.as_ptr(), self.path.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// `value`, not negative, in decimal, written into the end of `buffer` without allocating: the digits.
fn decimal(value: i32, buffer: &mut [u8; 12]) -> &[u8] {
    let mut left = value.unsigned_abs();
    let mut at = buffer.len();
    loop {
        at -= 1;
        buffer[at] = b"0123456789"[(left % 10) as usize];
        left /= 10;
        if left == 0 {
            return &buffer[at..];
        }
    }
}

/// Writes all of `bytes` to `fd` with `write`, retrying an interrupted one.
fn written_all(fd: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: `bytes` lives across the call, which reads at most its length.
        let wrote = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if let Ok(wrote) = usize::try_from(wrote) {
            bytes = &bytes[wrote..];
        } else {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Starts `command`. Every program the box starts goes through the bounded runner, which owns its process group.
#[expect(
    clippy::disallowed_methods,
    reason = "the spawn lock is the Windows gate's; a Unix box starts its programs here"
)]
fn spawned(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

/// Every group signal the runner sends on this thread, and whether its leader was still unreaped when it was sent, so
/// a test proves no signal reaches a group after its reap.
#[cfg(test)]
pub mod signal_log {
    use std::cell::RefCell;
    use std::io;
    use std::time::Instant;

    /// One signal sent.
    #[derive(Clone, Copy, Debug)]
    pub struct Sent {
        /// The group.
        pub group: i32,
        /// The signal.
        pub signal: i32,
        /// Whether the group's leader was still unreaped, so the group's id still its own.
        pub unreaped: bool,
        /// When it was sent.
        pub at: Instant,
    }

    thread_local! {
        static SENT: RefCell<Vec<Sent>> = const { RefCell::new(Vec::new()) };
    }

    /// Notes `signal` sent to `group`, looking whether its leader is unreaped: `waitid` with `WNOWAIT` still finds it.
    pub(super) fn note(group: i32, signal: i32) {
        let unreaped = match super::exit_seen(group) {
            Ok(_) => true,
            Err(error) => error.raw_os_error() != Some(libc::ECHILD) && error.kind() != io::ErrorKind::InvalidInput,
        };
        let at = Instant::now();
        SENT.with(|sent| {
            sent.borrow_mut().push(Sent { group, signal, unreaped, at });
        });
    }

    /// The signals sent on this thread since the last take.
    #[must_use]
    pub fn take() -> Vec<Sent> {
        SENT.with(|sent| std::mem::take(&mut *sent.borrow_mut()))
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

/// The `PATH` and `GATE_GODOT_ROOT` of the box user's login shell, `$SHELL` (`/bin/zsh` when `SHELL` is unset or empty)
/// as [`login_shell_of`] reads them: an ssh command's own environment lacks what the login profile adds, such as
/// dotnet's folder and the installer's Godot root. Read anew on every call.
///
/// # Errors
///
/// Why it cannot be used, as [`login_shell_of`] says it.
pub fn login_shell() -> Result<Login, String> {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| OsString::from(DEFAULT_SHELL));
    login_shell_of(&shell)
}

/// The `PATH` and `GATE_GODOT_ROOT` the login shell `shell` says when run once as `shell -l -c` with a command that
/// prints a marker of its own ([`crate::lines::login_marker`]) between NULs, then the shell's environment NUL-separated,
/// in the bounded runner within [`LOGIN_PATH_LIMIT`]; only that record is taken ([`crate::lines::marked_login`]), so
/// anything the login profile prints around it is left out. The command is two programs by absolute path with
/// single-quoted words, which every login shell (sh, zsh, bash, fish, tcsh) parses alike, and it expands no variable,
/// so a profile's `set -u` cannot fail it; only an exported `GATE_GODOT_ROOT` is read.
///
/// # Errors
///
/// Why it cannot be used, for [`crate::lines::login_path_line`]: the shell did not start, failed, ran past the limit,
/// or wrote more than [`LOGIN_PATH_CAP`], no marked `PATH`, an empty one, or text that is not UTF-8.
pub fn login_shell_of(shell: &std::ffi::OsStr) -> Result<Login, String> {
    let shown = shell.to_string_lossy().into_owned();
    let mut command = Command::new(shell);
    let marker = crate::lines::login_marker();
    let print = format!("/usr/bin/printf '\\0{marker}\\0'; /usr/bin/env -0");
    command.args(["-l", "-c", &print]);
    let ran = run_bounded(command, LOGIN_PATH_CAP, Instant::now() + LOGIN_PATH_LIMIT).map_err(|not| match not {
        NotRan::Start(error) => format!("cannot run {shown}: {error}"),
        NotRan::TimedOut { .. } => format!("{shown} -l did not answer in {} s", LOGIN_PATH_LIMIT.as_secs()),
        NotRan::OverCap => format!("{shown} -l wrote more than {} KiB", LOGIN_PATH_CAP / 1024),
        NotRan::Failed(why) => format!("{shown} -l failed: {why}"),
    })?;
    if !ran.status.success() {
        return Err(format!("{shown} -l exited with {}", status_text(ran.status)));
    }
    crate::lines::marked_login(&ran.stdout, &marker).map_err(|why| format!("{shown} -l {why}"))
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
    use std::cell::{Cell, RefCell};
    use std::io;
    use std::process::Command;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        EndedBy, Ending, Forward, Group, GroupEnd, GroupRecord, Halt, KILL_WAIT, Login, NotRan, Settled, Staged, Watch, Why, end_group,
        is_still_there, login_shell, login_shell_of, run_bounded, run_watched, settled, signal_log, status_text, still_there_why, stopped_watching,
    };
    use crate::console::{FailingSink, Forwarding, LineSink, Output, RecordingSink, Stamp, Stream};
    use crate::files::held_for_test;
    #[cfg(target_os = "macos")]
    use crate::lines::job_record_text;

    /// How long a program the tests expect to end may take: a hang guard only.
    const HANG_GUARD: Duration = Duration::from_secs(120);
    /// Far past what a run whose leftovers are ended takes, far short of the leftover's own sleep.
    const ENDED_WELL_BEFORE: Duration = Duration::from_secs(20);

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

    /// Whether the process `pid` is still there, a zombie not yet reaped by its new parent included, waited for to go
    /// within the hang guard.
    fn gone_soon(pid: i32) -> bool {
        let guard = Instant::now() + HANG_GUARD;
        loop {
            // SAFETY: signal 0 only asks whether the process exists; it touches no memory.
            let gone = unsafe { libc::kill(pid, 0) } != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone || Instant::now() >= guard {
                return gone;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The pid a script printed as the first line of its standard output.
    fn first_pid(stdout: &[u8]) -> i32 {
        String::from_utf8_lossy(stdout)
            .lines()
            .next()
            .and_then(|line| line.trim().parse().ok())
            .expect("the script printed a pid")
    }

    /// A watch forwarding standard error through `output`, ended on a gone client, TERM first with a 2 s grace.
    fn forwarding(output: Arc<dyn Forwarding>, deadline: Instant) -> Watch<'static> {
        Watch {
            cap: 1024,
            deadline: Some(deadline),
            ending: Ending::TermThenKill(Duration::from_secs(2)),
            output: Some(output),
            forward: Forward::Err(Stamp::Bare),
            ends_on_gone: true,
            record: None,
            check: None,
            on_started: None,
            on_leader_exit: None,
            processes: None,
        }
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
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!group_alive(group), "the background sleep of the group was killed too");
        let sent = signal_log::take();
        assert!(!sent.is_empty(), "the group was signalled");
        assert!(sent.iter().all(|sent| sent.unreaped), "a signal after the reap: {sent:?}");
    }

    #[test]
    fn the_runner_keeps_stdout_and_the_end_of_stderr_and_the_status() {
        let ran = run_bounded(sh("printf out; printf err >&2; exit 3"), 1024, Instant::now() + HANG_GUARD).expect("sh runs");
        assert_eq!(ran.stdout, b"out");
        assert_eq!(ran.stderr, b"err");
        assert_eq!(status_text(ran.status), "status 3");
        assert!(!ran.leftover);
        assert!(signal_log::take().is_empty(), "a program that left nothing is not signalled");
    }

    #[test]
    fn stdout_past_the_cap_is_over_cap() {
        let ran = run_bounded(sh("printf 12345"), 4, Instant::now() + HANG_GUARD);
        assert!(matches!(ran, Err(NotRan::OverCap)), "{ran:?}");
        let fits = run_bounded(sh("printf 1234"), 4, Instant::now() + HANG_GUARD).expect("sh runs");
        assert_eq!(fits.stdout, b"1234", "exactly the cap fits");
    }

    #[test]
    fn a_leader_that_exits_while_its_child_holds_stdout_keeps_its_status_and_the_child_is_ended_before_the_reap() {
        let output: Arc<dyn Forwarding> = Arc::new(Output::new(RecordingSink::default()));
        let started = Instant::now();
        let ran = run_watched(sh("sleep 600 & echo $!; exit 3"), &forwarding(output, started + HANG_GUARD)).expect("sh runs");
        assert!(started.elapsed() < ENDED_WELL_BEFORE, "the step ended within the drain bound");
        assert_eq!(status_text(ran.status), "status 3", "the leader's status is kept");
        assert!(!ran.leftover, "the ended child let its pipe go");
        assert!(gone_soon(first_pid(&ran.stdout)), "the child holding stdout was ended");

        let sent = signal_log::take();
        assert_eq!(sent.first().map(|sent| sent.signal), Some(libc::SIGTERM), "asked first: {sent:?}");
        assert!(
            sent.iter().all(|sent| sent.unreaped),
            "every signal reached the group before the reap: {sent:?}"
        );
    }

    #[test]
    fn a_process_that_leaves_the_group_and_holds_stdout_is_left_behind_and_the_status_kept() {
        // The shell exits only once the perl has left its group, so the perl is out of reach of the leftovers' ending.
        let marker = std::env::temp_dir().join(format!("gate-box-unix-escaped-{}", std::process::id()));
        let started = Instant::now();
        let ran = run_bounded(
            sh(&format!(
                "perl -e 'use POSIX; POSIX::setsid(); open(F, \">\", $ARGV[0]); close F; sleep 60' '{0}' & \
                 while [ ! -e '{0}' ]; do sleep 0.05; done; echo $!; exit 4",
                marker.display()
            )),
            1024,
            started + HANG_GUARD,
        )
        .expect("sh runs");
        // Best effort: the marker is in the temp folder and named for this process.
        let _ = std::fs::remove_file(&marker);
        let escaped = first_pid(&ran.stdout);
        // SAFETY: `escaped` is the perl this case started; the kill only ends it.
        unsafe {
            libc::kill(escaped, libc::SIGKILL);
        }
        assert!(started.elapsed() < ENDED_WELL_BEFORE, "the drain gave up on a reader in its read");
        assert_eq!(status_text(ran.status), "status 4");
        assert!(ran.leftover, "the escaped process held stdout open");
        assert!(signal_log::take().iter().all(|sent| sent.unreaped));
    }

    /// A sink whose every write waits `stall` before it succeeds: a client that is not reading.
    struct StalledSink {
        stall: Duration,
    }

    impl LineSink for StalledSink {
        fn write_line(&self, _stream: Stream, _line: &str) -> io::Result<()> {
            thread::sleep(self.stall);
            Ok(())
        }
    }

    #[test]
    fn a_stalled_client_does_not_hold_back_the_deadline_kill() {
        let output: Arc<dyn Forwarding> = Arc::new(Output::new(StalledSink {
            stall: Duration::from_secs(5),
        }));
        let started = Instant::now();
        let deadline = started + Duration::from_millis(500);
        let ran = run_watched(sh("echo a >&2; echo b >&2; sleep 60"), &forwarding(output, deadline));
        assert!(matches!(ran, Err(NotRan::TimedOut { .. })), "{ran:?}");
        let sent = signal_log::take();
        let first = sent.first().expect("the group was signalled");
        assert!(
            first.at.duration_since(started) < Duration::from_secs(3),
            "the kill came at the deadline, not after the stalled write: {sent:?}"
        );
        assert!(sent.iter().all(|sent| sent.unreaped), "{sent:?}");
    }

    #[test]
    fn a_client_gone_ends_the_group_asked_first_with_sigterm() {
        let output: Arc<dyn Forwarding> = Arc::new(Output::new(FailingSink::new(io::ErrorKind::BrokenPipe)));
        let started = Instant::now();
        let ran = run_watched(
            sh("trap 'echo termed; exit 7' TERM; echo first >&2; sleep 600 & wait"),
            &forwarding(Arc::clone(&output), started + HANG_GUARD),
        )
        .expect("sh runs to its end");
        assert!(started.elapsed() < ENDED_WELL_BEFORE, "ended long before its sleep");
        assert!(output.client_gone());
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "termed\n", "asked with SIGTERM first");
        assert_eq!(status_text(ran.status), "status 7");
        assert!(ran.stderr.is_empty(), "standard error was forwarded");
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
    fn a_program_whose_exec_fails_after_its_group_was_recorded_leaves_no_record() {
        let folder = std::env::temp_dir().join(format!("gate-box-unix-record-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("the case folder is made");
        // Beside the folder, so the folder holds only what the record leaves.
        let lock_path = folder.with_extension("lock");
        let lock = held_for_test(&lock_path);
        let record = GroupRecord::new(folder.join("k.pgid"), "gate-remote-k-0123456789ab", &lock);
        let watch = Watch {
            cap: 4,
            deadline: Some(Instant::now() + HANG_GUARD),
            ending: Ending::Kill,
            output: None,
            forward: Forward::Kept,
            ends_on_gone: false,
            record: Some(&record),
            check: None,
            on_started: None,
            on_leader_exit: None,
            processes: None,
        };
        let ran = run_watched(Command::new("gate-box-no-such-program"), &watch);
        let left: Vec<_> = std::fs::read_dir(&folder).expect("the folder is read").collect();
        // Best effort: a folder left behind is in the temp folder and named for this process.
        let _ = std::fs::remove_dir_all(&folder);
        // Best effort: as above.
        let _ = std::fs::remove_file(&lock_path);
        assert!(matches!(ran, Err(NotRan::Start(_))), "{ran:?}");
        assert!(left.is_empty(), "neither the record nor its temp file is left: {left:?}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_recorded_run_s_record_names_its_leader_s_start_as_read_after_the_spawn() {
        let job = "gate-remote-k-0123456789ab";
        let folder = std::env::temp_dir().join(format!("gate-box-unix-started-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("the case folder is made");
        let lock = held_for_test(&folder.join("k.lock"));
        let path = folder.join("k.pgid");
        let record = GroupRecord::new(path.clone(), job, &lock);
        let seen = RefCell::new(None);
        let on_started = |group: i32| {
            let text = std::fs::read_to_string(&path).ok();
            *seen.borrow_mut() = Some((group, text, crate::pid_info::start_of(group)));
        };
        let mut sleep = Command::new("sleep");
        sleep.arg("600");
        // The deadline has passed once the leader is looked at, so the sleep is killed right after its start is read.
        let watch = Watch {
            cap: 4,
            deadline: Some(Instant::now()),
            ending: Ending::Kill,
            output: None,
            forward: Forward::Kept,
            ends_on_gone: false,
            record: Some(&record),
            check: None,
            on_started: Some(&on_started),
            on_leader_exit: None,
            processes: None,
        };
        let ran = run_watched(sleep, &watch);
        // Best effort: a folder left behind is in the temp folder and named for this process.
        let _ = std::fs::remove_dir_all(&folder);
        assert!(matches!(ran, Err(NotRan::TimedOut { .. })), "{ran:?}");
        let (group, text, started) = seen.take().expect("the run told its start");
        let started = started.expect("the leader's start is read while it runs");
        // SAFETY: `getsid(0)` asks for the caller's own session and touches no memory.
        let session = unsafe { libc::getsid(0) };
        assert_eq!(
            text,
            Some(job_record_text(job, group, session, started)),
            "the record holds four lines, its start the leader's as read after its spawn"
        );
    }

    #[test]
    fn an_abandoned_spawn_once_the_lock_name_was_replaced_leaves_the_other_run_s_record() {
        let folder = std::env::temp_dir().join(format!("gate-box-unix-abandon-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("the case folder is made");
        let lock_path = folder.join("k.lock");
        let lock = held_for_test(&lock_path);
        let path = folder.join("k.pgid");
        let record = GroupRecord::new(path.clone(), "gate-remote-k-0123456789ab", &lock);
        let staged = Staged::new(&record).expect("the record is staged");
        std::fs::remove_file(&lock_path).expect("the lock's name is freed");
        std::fs::write(&lock_path, "").expect("another run's lock takes the name");
        std::fs::write(&path, "other").expect("the other run writes its record");
        staged.abandon();
        let kept = std::fs::read_to_string(&path);
        let temp_left = staged.temp.exists();
        // Best effort: a folder left behind is in the temp folder and named for this process.
        let _ = std::fs::remove_dir_all(&folder);
        assert_eq!(kept.ok().as_deref(), Some("other"), "the other run's record is kept");
        assert!(!temp_left, "this run's own temp file is removed");
    }

    /// What one look at a fake group's leader answers.
    #[derive(Clone, Copy, Debug)]
    enum Look {
        /// The leader has not exited.
        Running,
        /// The leader has exited, unreaped.
        Exited,
        /// The look fails with this `errno`.
        Fails(i32),
    }

    /// The next of `answers`, given in turn, then the last of them again.
    fn next_of<T: Copy>(answers: &RefCell<Vec<T>>) -> Option<T> {
        let mut answers = answers.borrow_mut();
        if answers.len() > 1 { answers.pop() } else { answers.last().copied() }
    }

    /// A group whose leader's looks answer `looks` in turn, then the last of them again, with a process besides the
    /// leader in it unless made [`Fake::emptied`] or listed otherwise ([`Fake::listing`]); it notes each signal sent.
    struct Fake {
        looks: RefCell<Vec<Look>>,
        sent: RefCell<Vec<i32>>,
        others: RefCell<Vec<bool>>,
        /// What happened to the group, in order: each look's answer (`running`, `exited`), each `signal`, and what a
        /// test's hooks note.
        events: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new(looks: &[Look]) -> Fake {
            Fake {
                looks: RefCell::new(looks.iter().rev().copied().collect()),
                sent: RefCell::new(Vec::new()),
                others: RefCell::new(vec![true]),
                events: RefCell::new(Vec::new()),
            }
        }

        /// The group with no process left in it.
        fn emptied(self) -> Fake {
            self.listing(&[false])
        }

        /// The group whose listings find a process besides the leader as `others` says, in turn, then the last again.
        fn listing(self, others: &[bool]) -> Fake {
            Fake {
                others: RefCell::new(others.iter().rev().copied().collect()),
                ..self
            }
        }
    }

    impl Group for Fake {
        fn exited(&self) -> io::Result<bool> {
            match next_of(&self.looks) {
                Some(Look::Fails(errno)) => Err(io::Error::from_raw_os_error(errno)),
                Some(Look::Exited) => {
                    self.events.borrow_mut().push("exited");
                    Ok(true)
                }
                Some(Look::Running) | None => {
                    self.events.borrow_mut().push("running");
                    Ok(false)
                }
            }
        }

        fn others_left(&self, pids: &mut Vec<i32>) -> bool {
            pids.clear();
            next_of(&self.others).unwrap_or(true)
        }

        fn signal(&self, signal: i32) {
            self.events.borrow_mut().push("signal");
            self.sent.borrow_mut().push(signal);
        }
    }

    /// The process group a fake group's hooks are given.
    const FAKE_GROUP: i32 = 4242;

    /// A watch with no deadline, no output and no hooks, ending with `SIGKILL`.
    fn quiet_watch<'a>() -> Watch<'a> {
        Watch {
            cap: 1024,
            deadline: None,
            ending: Ending::Kill,
            output: None,
            forward: Forward::Kept,
            ends_on_gone: false,
            record: None,
            check: None,
            on_started: None,
            on_leader_exit: None,
            processes: None,
        }
    }

    #[test]
    fn a_check_that_halts_ends_the_group_through_the_runner_s_own_ending() {
        let group = Fake::new(&[Look::Running, Look::Running, Look::Running, Look::Running, Look::Exited]).emptied();
        let checks = Cell::new(0);
        let check = || {
            checks.set(checks.get() + 1);
            (checks.get() >= 3).then(|| Halt("ceiling".to_owned()))
        };
        let watch = Watch {
            check: Some(&check),
            ..quiet_watch()
        };
        let why = stopped_watching(&group, FAKE_GROUP, Ok(()), &watch);
        assert!(matches!(&why, Why::Halted(halt) if halt.0 == "ceiling"), "halted at the third check");
        assert_eq!(checks.get(), 3, "the check is made once per poll");
        let settled = settled(&group, &why, Ending::TermThenKill(HANG_GUARD), HANG_GUARD);
        assert!(matches!(settled, Settled::Ended), "{settled:?}");
        assert_eq!(*group.sent.borrow(), [libc::SIGTERM, libc::SIGCONT], "ended as the watch's ending says");
    }

    #[test]
    fn the_leader_exit_hook_is_called_once_after_the_exit_is_seen_and_before_the_group_is_ended() {
        let group = Fake::new(&[Look::Running, Look::Exited]).listing(&[true, false]);
        let hook = || group.events.borrow_mut().push("hook");
        let watch = Watch {
            on_leader_exit: Some(&hook),
            processes: None,
            ..quiet_watch()
        };
        let why = stopped_watching(&group, FAKE_GROUP, Ok(()), &watch);
        let settled = settled(&group, &why, Ending::TermThenKill(HANG_GUARD), HANG_GUARD);
        assert!(matches!(settled, Settled::Ended), "{settled:?}");
        assert_eq!(
            *group.events.borrow(),
            ["running", "exited", "hook", "exited", "signal", "signal", "exited"],
            "once, after the exit is seen, before the leftovers are ended"
        );

        let unread = Fake::new(&[Look::Exited]).emptied();
        let hook = || unread.events.borrow_mut().push("hook");
        let watch = Watch {
            on_leader_exit: Some(&hook),
            processes: None,
            ..quiet_watch()
        };
        let why = stopped_watching(&unread, FAKE_GROUP, Err(io::Error::other("no pipe")), &watch);
        assert!(matches!(why, Why::Failed(_)));
        assert_eq!(*unread.events.borrow(), ["hook"], "called once when the readers did not start too");
    }

    #[test]
    fn the_started_hook_is_called_once_after_the_readers_start_and_before_the_first_wait() {
        let group = Fake::new(&[Look::Running, Look::Exited]).emptied();
        let given = Cell::new(None);
        let hook = |started: i32| {
            given.set(Some(started));
            group.events.borrow_mut().push("started");
        };
        let watch = Watch {
            on_started: Some(&hook),
            ..quiet_watch()
        };
        let why = stopped_watching(&group, FAKE_GROUP, Ok(()), &watch);
        assert!(matches!(why, Why::Exited));
        assert_eq!(*group.events.borrow(), ["started", "running", "exited"], "once, before the first look");
        assert_eq!(given.get(), Some(FAKE_GROUP), "given the program's group");
    }

    #[test]
    fn a_group_still_there_after_sigkill_is_told_from_any_other_failure() {
        assert!(is_still_there(&still_there_why(4242, KILL_WAIT)));
        assert!(!is_still_there("its group record could not be removed: denied"));
        assert!(!is_still_there("its exit could not be read: Input/output error"));
    }

    #[test]
    fn the_started_hook_is_not_called_when_the_readers_could_not_start() {
        let group = Fake::new(&[Look::Exited]).emptied();
        let hook = |_: i32| group.events.borrow_mut().push("started");
        let watch = Watch {
            on_started: Some(&hook),
            ..quiet_watch()
        };
        let why = stopped_watching(&group, FAKE_GROUP, Err(io::Error::other("no pipe")), &watch);
        assert!(matches!(why, Why::Failed(_)));
        assert!(group.events.borrow().is_empty(), "{:?}", group.events.borrow());
    }

    #[test]
    fn the_started_hook_gets_the_group_of_a_real_child() {
        let given = Cell::new(None);
        let hook = |started: i32| given.set(Some((started, group_alive(started))));
        let watch = Watch {
            deadline: Some(Instant::now() + HANG_GUARD),
            on_started: Some(&hook),
            ..quiet_watch()
        };
        let ran = run_watched(sh("echo $$; exit 0"), &watch).expect("sh runs");
        let leader = first_pid(&ran.stdout);
        assert_eq!(given.get(), Some((leader, true)), "the leader's group, there while the hook ran");
    }

    #[test]
    fn a_check_that_halts_a_program_ends_its_group_before_the_reap_and_the_run_says_so() {
        let check = || Some(Halt("ceiling".to_owned()));
        let started = Instant::now();
        let watch = Watch {
            deadline: Some(started + HANG_GUARD),
            ending: Ending::TermThenKill(Duration::from_secs(2)),
            check: Some(&check),
            ..quiet_watch()
        };
        let ran = run_watched(sh("sleep 600"), &watch).expect("sh runs to its end");
        assert!(started.elapsed() < ENDED_WELL_BEFORE, "ended long before its sleep");
        assert_eq!(ran.ended_by, Some(EndedBy::Halted(Halt("ceiling".to_owned()))));
        assert_eq!(status_text(ran.status), "signal 15", "the leader was signalled");
        let sent = signal_log::take();
        assert_eq!(sent.first().map(|sent| sent.signal), Some(libc::SIGTERM), "{sent:?}");
        assert!(sent.iter().all(|sent| sent.unreaped), "{sent:?}");
    }

    #[test]
    fn both_streams_are_forwarded_each_as_itself_and_a_leader_that_exits_of_itself_is_not_ended_by_the_runner() {
        let sink = Arc::new(Output::new(RecordingSink::default()));
        let output: Arc<dyn Forwarding> = Arc::clone(&sink) as Arc<dyn Forwarding>;
        let hooked = Cell::new(0);
        let hook = || hooked.set(hooked.get() + 1);
        let watch = Watch {
            deadline: Some(Instant::now() + HANG_GUARD),
            output: Some(output),
            forward: Forward::Both(Stamp::Bare, Stamp::Bare),
            ends_on_gone: true,
            on_leader_exit: Some(&hook),
            processes: None,
            ..quiet_watch()
        };
        let ran = run_watched(sh("echo out; echo err >&2; exit 5"), &watch).expect("sh runs");
        let mut lines = sink.sink().lines();
        lines.sort_by_key(|(stream, _)| *stream == Stream::Err);
        assert_eq!(lines, [(Stream::Out, "out".to_owned()), (Stream::Err, "err".to_owned())]);
        assert!(ran.stdout.is_empty() && ran.stderr.is_empty(), "nothing kept");
        assert_eq!(status_text(ran.status), "status 5");
        assert_eq!(ran.ended_by, None);
        assert_eq!(hooked.get(), 1, "the hook is called once");
    }

    #[test]
    fn a_client_gone_says_the_runner_ended_the_group_for_it() {
        let output: Arc<dyn Forwarding> = Arc::new(Output::new(FailingSink::new(io::ErrorKind::BrokenPipe)));
        let started = Instant::now();
        let ran = run_watched(sh("echo first >&2; sleep 600"), &forwarding(output, started + HANG_GUARD)).expect("sh runs to its end");
        assert!(started.elapsed() < ENDED_WELL_BEFORE, "ended long before its sleep");
        assert_eq!(ran.ended_by, Some(EndedBy::Gone));
        assert!(signal_log::take().iter().all(|sent| sent.unreaped));
    }

    #[test]
    fn a_look_at_the_leader_that_fails_during_the_grace_still_kills_the_group() {
        let group = Fake::new(&[Look::Running, Look::Fails(libc::EIO)]);
        let ended = end_group(&group, Ending::TermThenKill(Duration::from_secs(60)), Duration::ZERO);
        assert!(ended.is_err(), "{ended:?}");
        assert_eq!(*group.sent.borrow(), [libc::SIGTERM, libc::SIGCONT, libc::SIGKILL]);
    }

    #[test]
    fn a_leader_that_exited_with_a_member_left_past_the_wait_after_sigkill_leaves_its_group_still_there() {
        let group = Fake::new(&[Look::Exited]);
        let started = Instant::now();
        let settled = settled(&group, &Why::Exited, Ending::Kill, Duration::from_millis(200));
        assert!(
            matches!(settled, Settled::StillThere),
            "the member keeps the record and the leader unreaped: {settled:?}"
        );
        assert!(started.elapsed() < ENDED_WELL_BEFORE, "the wait is bounded");
        assert_eq!(*group.sent.borrow(), [libc::SIGKILL]);
    }

    #[test]
    fn a_leader_that_exited_is_ended_once_its_member_is_gone_within_the_wait() {
        let group = Fake::new(&[Look::Exited]).listing(&[true, true, false]);
        let settled = settled(&group, &Why::Exited, Ending::Kill, HANG_GUARD);
        assert!(matches!(settled, Settled::Ended), "{settled:?}");
        assert_eq!(*group.sent.borrow(), [libc::SIGKILL]);
    }

    #[test]
    fn a_group_asked_to_end_with_sigterm_is_continued_at_once() {
        let group = Fake::new(&[Look::Exited]).listing(&[true, false]);
        let settled = settled(&group, &Why::Exited, Ending::TermThenKill(HANG_GUARD), HANG_GUARD);
        assert!(matches!(settled, Settled::Ended), "{settled:?}");
        assert_eq!(*group.sent.borrow(), [libc::SIGTERM, libc::SIGCONT], "a stopped group gets its TERM");
    }

    /// A perl, out of any step's group, whose child joins the process group whose id the step writes to `group_file`,
    /// then writes its pid to `joined` and sleeps; once the child is killed, the perl leaves it a zombie, still listed
    /// in the group, for a second before it reaps it.
    #[cfg(target_os = "macos")]
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this test stands in for a process of the step's"
    )]
    fn joining_member(group_file: &std::path::Path, joined: &std::path::Path) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let script = r#"use POSIX; alarm 120; my ($g_file, $joined) = @ARGV;
            my $c = fork();
            if ($c == 0) {
                sleep 0.01 until -e $g_file;
                open(my $f, '<', $g_file) or die; my $g = <$f>; chomp $g;
                POSIX::setpgid(0, $g) or die "setpgid: $!";
                open(my $j, '>', "$joined.tmp") or die; print $j "$$\n"; close $j; rename("$joined.tmp", $joined) or die;
                sleep 600; exit 0;
            }
            sleep 0.01 until -e $joined;
            sleep 0.05 until `ps -o stat= -p $c` =~ /Z/;
            sleep 1; waitpid($c, 0);"#;
        Command::new("perl")
            .args(["-MTime::HiRes=sleep", "-e", script])
            .arg(group_file)
            .arg(joined)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("perl starts")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_member_left_in_the_group_once_its_leader_exits_is_gone_before_the_run_returns() {
        let folder = std::env::temp_dir().join(format!("gate-box-unix-member-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("the case folder is made");
        let (group_file, joined) = (folder.join("group"), folder.join("joined"));
        let mut helper = joining_member(&group_file, &joined);
        let script = format!(
            "echo $$ > '{0}.tmp' && mv '{0}.tmp' '{0}'; while [ ! -e '{1}' ]; do sleep 0.01; done; echo $$; exit 3",
            group_file.display(),
            joined.display()
        );
        let ran = run_bounded(sh(&script), 1024, Instant::now() + HANG_GUARD).expect("sh runs");
        let group = first_pid(&ran.stdout);
        let left = crate::stop::group_pids(group).expect("the group is listed");
        let member: i32 = std::fs::read_to_string(&joined)
            .expect("the member wrote its pid")
            .trim()
            .parse()
            .expect("the member's pid is a number");
        // Best effort: the perl is this case's own and reaps its child within a second of its kill.
        let _ = helper.wait();
        // Best effort: the folder is in the temp folder and named for this process.
        let _ = std::fs::remove_dir_all(&folder);
        assert!(
            !left.contains(&member),
            "the killed member was still in the group when the run ended: {left:?}"
        );
        assert_eq!(status_text(ran.status), "status 3");
    }

    #[test]
    fn a_leader_that_is_no_child_to_look_at_is_sent_no_signal() {
        let group = Fake::new(&[Look::Fails(libc::ECHILD)]);
        let ended = end_group(&group, Ending::TermThenKill(Duration::from_secs(60)), Duration::ZERO);
        assert!(ended.is_err(), "{ended:?}");
        assert!(group.sent.borrow().is_empty(), "{:?}", group.sent.borrow());
    }

    #[test]
    fn a_leader_that_has_not_exited_once_the_wait_after_sigkill_passed_leaves_its_group_still_there() {
        let group = Fake::new(&[Look::Running]);
        let started = Instant::now();
        let ended = end_group(&group, Ending::Kill, Duration::from_millis(200));
        assert_eq!(ended.ok(), Some(GroupEnd::StillThere));
        assert!(started.elapsed() < ENDED_WELL_BEFORE, "the wait is bounded");
        assert_eq!(*group.sent.borrow(), [libc::SIGKILL]);
    }

    /// Whether `settled` leaves the leader unreaped with its record kept.
    fn kept(settled: &Settled) -> bool {
        matches!(settled, Settled::Unread { keep: true, .. })
    }

    #[test]
    fn a_leader_whose_exit_cannot_be_read_is_killed_left_unreaped_and_its_record_kept() {
        let group = Fake::new(&[Look::Running]);
        let settled = settled(
            &group,
            &Why::Unread(io::Error::from_raw_os_error(libc::EIO)),
            Ending::Kill,
            Duration::ZERO,
        );
        assert!(kept(&settled), "{settled:?}");
        assert_eq!(*group.sent.borrow(), [libc::SIGKILL]);
    }

    #[test]
    fn a_look_that_fails_while_the_leftovers_are_ended_leaves_the_leader_unreaped_and_its_record_kept() {
        let group = Fake::new(&[Look::Running, Look::Fails(libc::EIO)]);
        let ending = Ending::TermThenKill(Duration::from_secs(60));
        let settled = settled(&group, &Why::Exited, ending, Duration::ZERO);
        assert!(kept(&settled), "{settled:?}");
        assert_eq!(*group.sent.borrow(), [libc::SIGTERM, libc::SIGCONT, libc::SIGKILL]);
    }

    #[test]
    fn a_look_that_fails_after_the_sigkill_leaves_the_leader_unreaped_and_its_record_kept() {
        let group = Fake::new(&[Look::Fails(libc::EIO)]);
        let failed = Why::Failed("its output could not be read".to_owned());
        let settled = settled(&group, &failed, Ending::Kill, Duration::from_secs(60));
        assert!(kept(&settled), "{settled:?}");
        assert_eq!(*group.sent.borrow(), [libc::SIGKILL]);
    }

    #[test]
    fn a_leader_that_is_no_child_to_look_at_keeps_its_record_while_its_group_may_have_a_process() {
        let echild = || Why::Unread(io::Error::from_raw_os_error(libc::ECHILD));
        let group = Fake::new(&[Look::Fails(libc::ECHILD)]);
        let settled_with_others = settled(&group, &echild(), Ending::Kill, Duration::ZERO);
        assert!(kept(&settled_with_others), "{settled_with_others:?}");
        assert!(group.sent.borrow().is_empty(), "no signal: {:?}", group.sent.borrow());

        let emptied = Fake::new(&[Look::Fails(libc::ECHILD)]).emptied();
        let settled_empty = settled(&emptied, &echild(), Ending::Kill, Duration::ZERO);
        assert!(
            matches!(settled_empty, Settled::Unread { keep: false, .. }),
            "no process left, so the record goes: {settled_empty:?}"
        );
        assert!(emptied.sent.borrow().is_empty());
    }

    #[test]
    fn the_login_shell_gives_a_path() {
        let path = login_shell().expect("the login shell's PATH is read").path;
        assert!(!path.is_empty(), "{path:?}");
        assert!(path.contains("/usr/bin"), "{path}");
    }

    /// What `login_shell_of` reads of a login shell whose profile prints a banner and runs `profile`: a script standing
    /// in for the shell, `-l -c <command>` running the command after it in the same shell, as a login shell does, then
    /// printing a goodbye as a `.zlogout` would.
    fn login_after(name: &str, profile: &str) -> Result<Login, String> {
        login_in(
            name,
            &format!("#!/bin/sh\necho 'Welcome to the builder'\nprintf 'no newline'\n{profile}\neval \"$3\"\necho 'Goodbye'\n"),
        )
    }

    /// What `login_shell_of` reads of the login shell `script`, a script standing in for the shell.
    fn login_in(name: &str, script: &str) -> Result<Login, String> {
        let folder = std::env::temp_dir().join(format!("gate-box-unix-shell-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("the shell's folder is made");
        let shell = folder.join("noisy-shell");
        std::fs::write(&shell, script).expect("the shell is written");
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the shell is made runnable");
        let login = login_shell_of(shell.as_os_str());
        // Best effort: a folder left behind is in the temp folder and named for this process.
        let _ = std::fs::remove_dir_all(&folder);
        login
    }

    #[test]
    fn a_login_profile_that_prints_noise_leaves_the_path_clean() {
        let login = login_after("noisy", "PATH=/fake/bin:/usr/bin\nunset GATE_GODOT_ROOT");
        assert_eq!(
            login,
            Ok(Login {
                path: "/fake/bin:/usr/bin".to_owned(),
                godot_root: None,
            }),
            "no root is Godot missing"
        );
    }

    #[test]
    fn the_login_shell_s_godot_root_is_read_with_its_path_spaces_kept() {
        let login = login_after(
            "godot",
            "PATH=/fake/bin:/usr/bin\nGATE_GODOT_ROOT='/Users/b/gate dir/godot'\nexport GATE_GODOT_ROOT",
        );
        assert_eq!(
            login,
            Ok(Login {
                path: "/fake/bin:/usr/bin".to_owned(),
                godot_root: Some("/Users/b/gate dir/godot".to_owned()),
            })
        );
        let empty = login_after("godot-empty", "GATE_GODOT_ROOT=\nexport GATE_GODOT_ROOT");
        assert_eq!(empty.map(|login| login.godot_root), Ok(None), "an empty root is none");
    }

    #[test]
    fn a_login_shell_that_is_not_posix_like_still_gives_the_path_and_the_godot_root() {
        // Stands in for fish or tcsh, which cannot parse `${…-}` and fail the whole command.
        let script = "#!/bin/sh\ncase \"$3\" in *'${'*) echo 'fish: Variables cannot be bracketed' >&2; exit 127;; esac\n\
                      PATH=/fake/bin:/usr/bin\nGATE_GODOT_ROOT=/g/godot\nexport GATE_GODOT_ROOT\neval \"$3\"\n";
        assert_eq!(
            login_in("fish", script),
            Ok(Login {
                path: "/fake/bin:/usr/bin".to_owned(),
                godot_root: Some("/g/godot".to_owned()),
            })
        );
    }

    #[test]
    fn a_login_profile_that_sets_nounset_with_no_godot_root_still_gives_the_path() {
        let login = login_after("nounset", "set -u\nunset GATE_GODOT_ROOT\nPATH=/fake/bin:/usr/bin");
        assert_eq!(
            login,
            Ok(Login {
                path: "/fake/bin:/usr/bin".to_owned(),
                godot_root: None,
            })
        );
    }
}
