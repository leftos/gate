//! `gate-box box run`'s command: the payload's words started directly, with no shell, in the run's place, found along
//! the login shell's `PATH`, as the leader of a process group of its own that the bounded runner watches
//! ([`run_watched`]), both its streams forwarded line by line. The start marker is said first; the interrupted-command
//! record `<Work>.command` is written just before the start and removed only once the command ended clean; the status
//! the run ends on is the command's own, or the box's ([`command_status`]).

use std::cell::{Cell, RefCell};
#[cfg(target_os = "macos")]
use std::ffi::c_int;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gate_watchdog::{Config, CpuSource, Kill, Machine, Pump, SystemTimes, Ticks};
use gate_wire::{BoxKill, escaped, killed_line, started_line};

use crate::console::{Forwarding, LineSink, Output, Stream};
use crate::cpu::GroupCpu;
use crate::files::{self, HeldLock};
use crate::lines::{CommandEnd, FAILED_STATUS, command_environment, command_status, round_trip, write_failure};
use crate::process::{EndedBy, Ending, Forward, GroupRecord, Halt, NotRan, Ran, Watch, is_still_there, run_watched};
use crate::run::group_record;

/// How long the command's group has, once asked to end with `SIGTERM`, before `SIGKILL`: the git steps' grace.
const END_GRACE: Duration = Duration::from_secs(2);

/// What the command of a run is started with.
pub struct Launch<'a> {
    /// The payload's words: the program, found along the command's `PATH`, then its arguments.
    pub words: &'a [String],
    /// The folder the command runs in.
    pub place: &'a Path,
    /// The work folder, `<root>/work/<key>`, beside which the run's records are.
    pub work: &'a str,
    /// The run's job name, written to the records and given to the command.
    pub job: &'a str,
    /// The run's nonce, which the start marker and the build-server run id name.
    pub nonce: &'a str,
    /// The payload's `Env` pairs.
    pub env: &'a [(String, String)],
    /// The login shell's `PATH`, `None` to keep the box's own.
    pub login_path: Option<&'a str>,
    /// The work lock, held: no record is written or removed once it is lost.
    pub lock: &'a HeldLock,
    /// The run's ceiling in whole seconds of load-adjusted time, `None` for none.
    pub timeout: Option<u64>,
    /// The PC's name for the builder, empty when the payload gave none.
    pub host: &'a str,
    /// How often the box's ceiling samples the command.
    pub sample: Duration,
    /// The heartbeat, which watches the command's group while it runs; `None` when it could not start.
    pub pump: Option<&'a Pump>,
}

/// The box's ceiling on the command, looked at on the runner's waiting thread every poll, without the output lock:
/// gate-watchdog's watch, begun when the command started, sampled every period and asked for its kill on every look.
/// The watch reads no system times: it goes by wall time alone from its start, so only the backstop, five ceilings of
/// wall time, can fire. The kill it decides is kept for the run's end.
struct Ceiling {
    timeout: Option<u64>,
    sample: Duration,
    watching: RefCell<Option<Watching>>,
    kill: Cell<Option<Kill>>,
}

/// A begun watch: since when, and when it samples next.
struct Watching {
    watch: gate_watchdog::Watch,
    started: Instant,
    next: Duration,
}

impl Ceiling {
    /// The ceiling of `timeout` seconds, `None` for none, sampled every `sample`; not begun.
    fn new(timeout: Option<u64>, sample: Duration) -> Ceiling {
        Ceiling {
            timeout,
            sample,
            watching: RefCell::new(None),
            kill: Cell::new(None),
        }
    }

    /// Begins the watch now, the command started, on every core of the builder, by wall time alone; nothing for a
    /// run with no ceiling.
    fn begin(&self) {
        let Some(timeout) = self.timeout else {
            return;
        };
        let cores = thread::available_parallelism().map_or(1, |cores| u32::try_from(cores.get()).unwrap_or(u32::MAX));
        let config = Config {
            timeout,
            stall: Duration::MAX,
            stall_from_output: false,
            wsl: false,
        };
        let none = SystemTimes { idle: 0, kernel: 0, user: 0 };
        let mut watch = gate_watchdog::Watch::new(config, Machine::new(cores, cores, false), none, Ticks(0), None);
        // Nothing failed, so its line is not said: the watch only goes by wall time from its start.
        let _ = watch.sampler_failed("no system times are read");
        *self.watching.borrow_mut() = Some(Watching {
            watch,
            started: Instant::now(),
            next: self.sample,
        });
    }

    /// One look: a wall-time sample once its period has passed, then the kill that holds now, kept and given as the
    /// halt that ends the group; `None` before the watch began, for no ceiling, and while no kill holds.
    fn check(&self) -> Option<Halt> {
        let mut watching = self.watching.borrow_mut();
        let watching = watching.as_mut()?;
        let now = watching.started.elapsed();
        if now >= watching.next {
            watching.next = now + self.sample;
            watching.watch.wall_only(now, false);
        }
        let kill = watching.watch.kill_check(now)?;
        self.kill.set(Some(kill));
        Some(Halt(tag_of(kill).1.to_owned()))
    }
}

/// The kill tag of `kill` and the word its note names it by.
fn tag_of(kill: Kill) -> (BoxKill, &'static str) {
    match kill {
        Kill::Backstop { .. } => (BoxKill::Backstop, "backstop"),
        // Not reached while the watch reads no system times, as only the backstop fires; the box's watch has no stall
        // window either.
        Kill::TimedOut { .. } | Kill::Stalled { .. } => (BoxKill::TimedOut, "ceiling"),
    }
}

/// The interrupted-command record of the work folder `work`: `<work>.command`.
#[must_use]
pub fn command_record(work: &str) -> String {
    format!("{work}.command")
}

/// Runs the run's command as `launch` says, saying its lines through `output`, and gives the status the run ends on.
///
/// The start marker is said first; the client is probed, the interrupted-command record written
/// (`{UTC start} {job}`, [`files::replace_plain`]), and the client probed again, so a client gone at either probe
/// ends the run with [`crate::lines::GONE_STATUS`] and nothing started; then the command runs ([`run_watched`]) with standard input
/// null, its group written to `<work>.pgid` from the child, ended `SIGTERM` first on a gone client. The record is kept
/// unless the command exited of itself, the client still there and no leftover holding its output; it is removed for a
/// command that never started. Every removal looks first that the work lock is still held.
///
/// Once the command's readers start, the heartbeat watches its group's CPU ([`GroupCpu`]) and the box's ceiling begins
/// ([`Ceiling`]); the heartbeat stops watching once the runner stops watching the leader, before the group is ended.
/// A command the ceiling ends gives [`crate::lines::KILLED_STATUS`] after the kill line and the kill tag, its records
/// kept ([`killed`]).
pub fn run_command<S: LineSink + 'static>(launch: &Launch<'_>, output: &Arc<Output<S>>) -> u8 {
    let beat = Beat {
        cpu_of: &|group| Arc::new(GroupCpu::new(group)) as Arc<dyn CpuSource>,
        unwatched: &|| {},
    };
    run_with(launch, output, &beat)
}

/// How [`run_with`] has the heartbeat watch the command.
struct Beat<'a> {
    /// The CPU source of the command's group.
    cpu_of: &'a dyn Fn(i32) -> Arc<dyn CpuSource>,
    /// Called once the heartbeat stopped watching the group, in the runner's leader-exit hook.
    unwatched: &'a dyn Fn(),
}

/// [`run_command`], the heartbeat watching the command as `beat` says.
fn run_with<S: LineSink + 'static>(launch: &Launch<'_>, output: &Arc<Output<S>>, beat: &Beat<'_>) -> u8 {
    let Some((program, rest)) = launch.words.split_first() else {
        let _ = output.say(Stream::Err, &failed_line("the run has no command"));
        return FAILED_STATUS;
    };
    // A client that has gone has no one left to tell; the probe below ends the run.
    let _ = output.say(Stream::Out, &started_line(launch.nonce));
    if output.client_gone_probe() {
        return command_status(CommandEnd::Gone);
    }
    let record = command_record(launch.work);
    if let Err(failure) = files::replace_plain(Path::new(&record), record_text(launch.job).as_bytes()) {
        // The plain write failure, for an unsynced record too: it is removed below.
        let (files::ReplaceFailure::NotWritten(error) | files::ReplaceFailure::Unsynced(error)) = &failure;
        let _ = output.say(Stream::Err, &write_failure(&record, &error.to_string()));
        // A record put in place but not synced is still at the name: the command never ran.
        removed_if_held(&record, launch.lock, output);
        return FAILED_STATUS;
    }
    if output.client_gone_probe() {
        removed_if_held(&record, launch.lock, output);
        return command_status(CommandEnd::Gone);
    }
    let group = GroupRecord::new(PathBuf::from(group_record(launch.work)), launch.job, launch.lock);
    let forwarding: Arc<dyn Forwarding> = Arc::clone(output) as Arc<dyn Forwarding>;
    let ceiling = Ceiling::new(launch.timeout, launch.sample);
    let started = |group: i32| {
        ceiling.begin();
        if let Some(pump) = launch.pump {
            pump.watch((beat.cpu_of)(group));
        }
    };
    let leader_exited = || {
        if let Some(pump) = launch.pump {
            pump.unwatch();
        }
        (beat.unwatched)();
    };
    let check = || ceiling.check();
    let watch = Watch {
        cap: 0,
        deadline: None,
        ending: Ending::TermThenKill(END_GRACE),
        output: Some(forwarding),
        forward: Forward::Both,
        ends_on_gone: true,
        record: Some(&group),
        check: Some(&check),
        on_started: Some(&started),
        on_leader_exit: Some(&leader_exited),
    };
    let ran = run_watched(command_of(launch, program, rest), &watch);
    ended(launch, &record, ran, ceiling.kill.get(), output)
}

/// The status of the run whose command, `launch`'s words, ran as `ran`, its record `record` removed when the command
/// ended clean or never started, and the line of a start that failed or of the box's own failure said; for a command
/// the box killed for `kill`, what [`killed`] gives.
fn ended<S: LineSink>(launch: &Launch<'_>, record: &str, ran: Result<Ran, NotRan>, kill: Option<Kill>, output: &Output<S>) -> u8 {
    if let Some(kill) = kill {
        return killed(launch, kill, ran, output);
    }
    let ran = match ran {
        Ok(ran) => ran,
        Err(NotRan::Start(error)) => {
            removed_if_held(record, launch.lock, output);
            let program = launch.words.first().map_or("", String::as_str);
            let line = format!("gate: remote could not start {}: {}", escaped(program), escaped(&error.to_string()));
            let _ = output.say(Stream::Err, &line);
            return command_status(CommandEnd::NotStarted);
        }
        Err(not) => {
            // The records stay: the group may still be there, or the lock is lost.
            if output.client_gone() {
                return command_status(CommandEnd::Gone);
            }
            let _ = output.say(Stream::Err, &failed_line(&unran(not)));
            return command_status(CommandEnd::Failed);
        }
    };
    let end = end_of(&ran, output.client_gone());
    if matches!(end, CommandEnd::Exited(_)) && ran.ended_by.is_none() && !ran.leftover {
        removed_if_held(record, launch.lock, output);
    }
    command_status(end)
}

/// The status of the run whose command the box killed for `kill`, its group's ending as `ran` says, every record kept:
/// [`crate::lines::GONE_STATUS`] and nothing said once the client is gone; else, said together, a note when the
/// ending failed (that the group could not be ended when it was still there after `SIGKILL`, else what failed after
/// the kill), the kill line and the kill tag, then [`crate::lines::KILLED_STATUS`], as the run is a kill
/// whatever its ending left.
fn killed<S: LineSink>(launch: &Launch<'_>, kill: Kill, ran: Result<Ran, NotRan>, output: &Output<S>) -> u8 {
    if output.client_gone() {
        return command_status(CommandEnd::Gone);
    }
    let (tag, what) = tag_of(kill);
    let note = ran.err().map(|not| match not {
        NotRan::Failed(why) if is_still_there(&why) => format!("gate: note: the box's {what} kill could not end job {}: {why}", launch.job),
        not => format!("gate: note: after the box's {what} kill: {}", unran(not)),
    });
    let spoken = escaped(&launch.words.join(" "));
    let line = if launch.host.is_empty() {
        kill.line(&spoken)
    } else {
        kill.line_on(&spoken, &escaped(launch.host))
    };
    let tagged = killed_line(launch.nonce, tag);
    let mut lines: Vec<(Stream, &str)> = note.iter().map(|note| (Stream::Err, note.as_str())).collect();
    lines.extend([(Stream::Err, line.as_str()), (Stream::Err, tagged.as_str())]);
    // A client gone meanwhile hears nothing, and the status stands.
    let _ = output.say_together(&lines);
    command_status(CommandEnd::Killed(tag))
}

/// How the command that ran as `ran` ended, its client gone when `gone`.
fn end_of(ran: &Ran, gone: bool) -> CommandEnd {
    if gone || ran.ended_by == Some(EndedBy::Gone) {
        return CommandEnd::Gone;
    }
    match (ran.status.code(), ran.status.signal()) {
        (Some(code), _) => CommandEnd::Exited(code),
        (None, Some(signal)) => CommandEnd::Signalled(signal),
        (None, None) => CommandEnd::Failed,
    }
}

/// Why the runner gave no exit for the command, `not`.
fn unran(not: NotRan) -> String {
    match not {
        NotRan::Failed(why) => why,
        NotRan::Start(error) => error.to_string(),
        NotRan::TimedOut { group } => format!("its process group {group} ran past its deadline"),
        NotRan::OverCap => "it printed more than the box keeps".to_owned(),
    }
}

/// The line of the box's own failure while it ran the command, as the Windows box says it.
fn failed_line(problem: &str) -> String {
    format!("gate: the gate failed while running the command: {problem}")
}

/// The interrupted-command record's text for a command of the job `job` starting now: `{UTC start} {job}`, the start in
/// .NET's round-trip format.
fn record_text(job: &str) -> String {
    // A clock before 1970 writes 1970: a later run reads any start as the command's own, as the clear only needs the job.
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
    format!("{} {job}", round_trip(seconds, i64::from(since.subsec_nanos() / 100)))
}

/// Removes the record `record` and syncs its folder while the work lock `lock` is still held, so a record another run
/// put at the name once this one's lock was lost is left; a removal that fails is said and changes nothing else. The
/// lock is looked at just before the unlink, not with it: a lock lost in the gap between the two is accepted.
fn removed_if_held<S: LineSink>(record: &str, lock: &HeldLock, output: &Output<S>) {
    if !matches!(lock.still_held(), Ok(true)) {
        return;
    }
    let path = Path::new(record);
    if let Err(error) = files::remove_if_there(path).and_then(|()| files::folder_synced(path)) {
        let line = format!("gate: remote could not remove {}: {}", escaped(record), escaped(&error.to_string()));
        // Said; the run's status stands.
        let _ = output.say(Stream::Err, &line);
    }
}

/// The command `program` with the arguments `rest`, run in `launch`'s place with the environment
/// [`command_environment`] composes, the build servers named by the run id `gate-box-<nonce>`.
fn command_of(launch: &Launch<'_>, program: &str, rest: &[String]) -> Command {
    let run = format!("gate-box-{}", launch.nonce);
    let environment = command_environment(std::env::vars_os().collect(), launch.login_path, launch.env, &run, launch.job);
    let mut command = Command::new(program);
    command.args(rest).current_dir(launch.place).env_clear().envs(environment);
    // SAFETY: the closure runs in the child between fork and exec, and calls only `sigaction`, `sigemptyset` and
    // `sigprocmask`, which are async-signal-safe, on values on its own stack; it allocates nothing and takes no lock.
    unsafe {
        command.pre_exec(signals_reset);
    }
    command
}

/// Puts `SIGTERM`, `SIGINT`, `SIGHUP` and `SIGQUIT` back to their default action and clears the signal mask, in the
/// child before its exec: an ignored signal stays ignored across `exec`, so a command whose ssh session ignored
/// `SIGTERM` would wait out every ending's grace; std resets only `SIGPIPE` and keeps the parent's mask.
fn signals_reset() -> io::Result<()> {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: an all-zero `sigaction` is a valid value: no flags and an empty mask; its handler is set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = libc::SIG_DFL;
        // SAFETY: `action` lives across the call, which only reads it; no old action is asked for.
        if unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: an all-zero `sigset_t` is a valid value, emptied by `sigemptyset` below.
    let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `empty` lives across both calls; `sigemptyset` writes only into it and `sigprocmask` only reads it.
    let cleared = unsafe {
        libc::sigemptyset(&raw mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &raw const empty, std::ptr::null_mut())
    };
    if cleared != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The box's `sysctl.proc_translated`: 1 under Rosetta, 0 native; no such name on an Intel Mac.
///
/// # Errors
///
/// Why it could not be read.
#[cfg(target_os = "macos")]
pub fn proc_translated() -> io::Result<i32> {
    let mut value: c_int = 0;
    let mut size = std::mem::size_of::<c_int>();
    // SAFETY: the name is a NUL-terminated string, and `value` and `size` live across the call, which fills `value` no
    // further than `size` bytes; no new value is given.
    let read = unsafe {
        libc::sysctlbyname(
            c"sysctl.proc_translated".as_ptr(),
            (&raw mut value).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read == 0 { Ok(value) } else { Err(io::Error::last_os_error()) }
}

/// Rosetta is macOS's alone: elsewhere the name is missing, which reads as native.
///
/// # Errors
///
/// Always [`io::ErrorKind::NotFound`].
#[cfg(not(target_os = "macos"))]
pub fn proc_translated() -> io::Result<i32> {
    Err(io::Error::from(io::ErrorKind::NotFound))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose folder or lock cannot be made must fail loudly")]
mod tests {
    use std::fs;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::ExitStatus;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread;
    use std::time::{Duration, Instant};

    use gate_watchdog::{CpuSource, Kill, Pump};
    use gate_wire::{BoxKill, killed_line, started_line};

    use super::{Beat, Ceiling, Launch, command_record, ended, removed_if_held, run_command, run_with};
    use crate::console::{Forwarding, HungUpSink, LineSink, Output, RecordingSink, Stream};
    use crate::files::{HeldLock, held_for_test};
    use crate::heartbeat;
    use crate::process::{EndedBy, Halt, NotRan, Ran, still_there_why};

    const NONCE: &str = "0123456789ab";
    const JOB: &str = "gate-remote-k-0123456789ab";

    /// A work folder `k` in a temp folder private to this process, its lock held, removed when the case ends.
    struct Case {
        base: PathBuf,
        place: PathBuf,
        work: String,
        lock_path: PathBuf,
        lock: HeldLock,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-command-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            let place = base.join("k");
            fs::create_dir_all(&place).expect("the work folder is made");
            let lock_path = base.join("k.lock");
            let lock = held_for_test(&lock_path);
            Case {
                work: place.display().to_string(),
                base,
                place,
                lock_path,
                lock,
            }
        }

        fn record(&self) -> PathBuf {
            PathBuf::from(command_record(&self.work))
        }

        fn run<S: LineSink + 'static>(&self, words: &[&str], env: &[(String, String)], login_path: Option<&str>, output: &Arc<Output<S>>) -> u8 {
            let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
            let launch = Launch {
                env,
                login_path,
                ..self.launch(&words, &unwatched())
            };
            run_command(&launch, output)
        }

        /// Runs `words` watched as `watched` says, the heartbeat watching as `beat` says.
        fn watched<S: LineSink + 'static>(&self, words: &[&str], watched: &Watched<'_>, beat: &Beat<'_>, output: &Arc<Output<S>>) -> u8 {
            let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
            run_with(&self.launch(&words, watched), output, beat)
        }

        /// The launch of `words` in this case's work folder, watched as `watched` says.
        fn launch<'a>(&'a self, words: &'a [String], watched: &Watched<'a>) -> Launch<'a> {
            Launch {
                words,
                place: &self.place,
                work: &self.work,
                job: JOB,
                nonce: NONCE,
                env: &[],
                login_path: None,
                lock: &self.lock,
                timeout: watched.timeout,
                host: watched.host,
                sample: watched.sample,
                pump: watched.pump,
            }
        }

        fn group_record(&self) -> PathBuf {
            PathBuf::from(format!("{}.pgid", self.work))
        }
    }

    /// How a case's command is watched.
    struct Watched<'a> {
        timeout: Option<u64>,
        host: &'a str,
        sample: Duration,
        pump: Option<&'a Pump>,
    }

    /// No ceiling and no heartbeat.
    fn unwatched() -> Watched<'static> {
        Watched {
            timeout: None,
            host: "",
            sample: Duration::from_secs(5),
            pump: None,
        }
    }

    /// A CPU source that has used nothing.
    fn idle_cpu(_group: i32) -> Arc<dyn CpuSource> {
        Arc::new(CountingCpu::default())
    }

    /// The heartbeat watching an idle source, nothing told of the unwatch.
    fn idle_beat() -> Beat<'static> {
        Beat {
            cpu_of: &idle_cpu,
            unwatched: &|| {},
        }
    }

    /// A CPU source that counts its reads, and those made once `unwatched` was set; each read gives `step` more ticks
    /// than the last.
    #[derive(Default)]
    struct CountingCpu {
        unwatched: AtomicBool,
        step: u64,
        reads: AtomicUsize,
        after_unwatch: AtomicUsize,
    }

    impl CpuSource for CountingCpu {
        fn job_cpu_ticks(&self) -> Result<u64, String> {
            let reads = self.reads.fetch_add(1, Ordering::SeqCst);
            if self.unwatched.load(Ordering::SeqCst) {
                self.after_unwatch.fetch_add(1, Ordering::SeqCst);
            }
            Ok(self.step.saturating_mul(u64::try_from(reads).unwrap_or(u64::MAX)))
        }

        fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
            Ok(None)
        }
    }

    /// When a [`BeatSink`] with a hold blocks its first heartbeat.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum HoldFrom {
        /// The first heartbeat it is given.
        First,
        /// The first heartbeat after the start marker.
        Started,
    }

    /// A sink that keeps every line and reports each heartbeat on a channel; with a hold, the first heartbeat its
    /// `from` names makes `marker`, when given, and blocks until the case lets it go.
    struct BeatSink {
        lines: Mutex<Vec<(Stream, String)>>,
        beats: Mutex<Sender<()>>,
        hold: Option<(Mutex<Receiver<()>>, HoldFrom, Option<PathBuf>)>,
        armed: AtomicBool,
        held: AtomicBool,
    }

    impl BeatSink {
        fn new(beats: Sender<()>) -> BeatSink {
            BeatSink {
                lines: Mutex::new(Vec::new()),
                beats: Mutex::new(beats),
                hold: None,
                armed: AtomicBool::new(false),
                held: AtomicBool::new(false),
            }
        }

        fn holding(beats: Sender<()>, hold: Receiver<()>, from: HoldFrom, marker: Option<PathBuf>) -> BeatSink {
            BeatSink {
                hold: Some((Mutex::new(hold), from, marker)),
                armed: AtomicBool::new(from == HoldFrom::First),
                ..BeatSink::new(beats)
            }
        }

        fn lines(&self) -> Vec<(Stream, String)> {
            self.lines.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }
    }

    impl LineSink for BeatSink {
        fn write_line(&self, stream: Stream, line: &str) -> io::Result<()> {
            if line.starts_with("gate-remote-started:") {
                self.armed.store(true, Ordering::SeqCst);
            }
            if line.starts_with("gate-remote-heartbeat:") {
                let _ = self.beats.lock().unwrap_or_else(PoisonError::into_inner).send(());
                if let Some((hold, _, marker)) = &self.hold
                    && self.armed.load(Ordering::SeqCst)
                    && !self.held.swap(true, Ordering::SeqCst)
                {
                    if let Some(marker) = marker {
                        fs::write(marker, "")?;
                    }
                    let _ = hold.lock().unwrap_or_else(PoisonError::into_inner).recv();
                }
            }
            self.lines.lock().unwrap_or_else(PoisonError::into_inner).push((stream, line.to_owned()));
            Ok(())
        }
    }

    /// How long a case waits for an event before it calls the case hung: a hang guard only.
    const GUARD: Duration = Duration::from_secs(120);

    /// Whether any process of the group `group` is left, waited for to go within the hang guard.
    fn group_gone_soon(group: i32) -> bool {
        let guard = Instant::now() + GUARD;
        loop {
            // SAFETY: signal 0 only asks whether the group exists; it touches no memory.
            let gone = unsafe { libc::killpg(group, 0) } != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone || Instant::now() >= guard {
                return gone;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// A backstop kill of a 1 s ceiling.
    fn backstop() -> Kill {
        Kill::Backstop {
            timeout: 1,
            wall: Duration::from_secs(5),
            adjusted: 0.0,
            free_percent: None,
        }
    }

    #[test]
    fn the_pump_is_unwatched_before_the_group_is_ended() {
        let case = Case::new("unwatched");
        let (beats, _beaten) = channel();
        let output = Arc::new(Output::new(BeatSink::new(beats)));
        let pump = heartbeat::start(Arc::clone(&output) as Arc<dyn Forwarding>, Duration::from_millis(1)).expect("the pump starts");
        let source = Arc::new(CountingCpu::default());
        let cpu_of = |_: i32| Arc::clone(&source) as Arc<dyn CpuSource>;
        let unwatched_seen = || source.unwatched.store(true, Ordering::SeqCst);
        let beat = Beat {
            cpu_of: &cpu_of,
            unwatched: &unwatched_seen,
        };
        let watched = Watched {
            pump: Some(&pump),
            ..unwatched()
        };
        // The subshell ignores TERM, so the runner's ending waits its whole grace before SIGKILL: a pump still watching
        // through it reads the source many times over.
        let script = "(trap '' TERM; while :; do sleep 0.05; done) & sleep 0.2; exit 0";
        let status = case.watched(&["/bin/sh", "-c", script], &watched, &beat, &output);
        heartbeat::stop(pump);
        assert_eq!(status, 0, "{:?}", output.sink().lines());
        assert!(source.reads.load(Ordering::SeqCst) >= 1, "the watch read the group once at least");
        assert_eq!(
            source.after_unwatch.load(Ordering::SeqCst),
            0,
            "no read once the leader-exit hook unwatched the pump"
        );
    }

    #[test]
    fn the_main_thread_is_not_held_by_a_pump_stuck_in_a_write() {
        let case = Case::new("stuck-pump");
        let (beats, _beaten) = channel();
        let (release, hold) = channel();
        let (marker, pid_file) = (case.base.join("held"), case.base.join("pid"));
        let sink = BeatSink::holding(beats, hold, HoldFrom::Started, Some(marker.clone()));
        let output = Arc::new(Output::bounded(sink, Duration::from_millis(200)));
        let pump = heartbeat::start(Arc::clone(&output) as Arc<dyn Forwarding>, Duration::from_millis(1)).expect("the pump starts");
        // Each read a second more: the watched pump beats on every tick.
        let source = Arc::new(CountingCpu {
            step: 10_000_000,
            ..CountingCpu::default()
        });
        let cpu_of = |_: i32| Arc::clone(&source) as Arc<dyn CpuSource>;
        let beat = Beat {
            cpu_of: &cpu_of,
            unwatched: &|| {},
        };
        let watched = Watched {
            timeout: Some(1),
            sample: Duration::from_millis(50),
            pump: Some(&pump),
            ..unwatched()
        };
        // The command runs once the pump is stuck in a beat after the start marker, and the box's backstop ends it; its
        // end-of-run lines are the says that wait on the stuck pump. Nothing is forwarded, so no reader waits on it.
        let script = format!(
            "echo $$ > '{pid}'; i=0; while [ ! -e '{held}' ] && [ $i -lt 1200 ]; do sleep 0.1; i=$((i+1)); done; sleep 600",
            pid = pid_file.display(),
            held = marker.display()
        );
        // A main thread that waited for the stuck write would never return: the write is let go only after this.
        let status = case.watched(&["/bin/sh", "-c", &script], &watched, &beat, &output);
        let gone = output.client_gone();
        let _ = release.send(());
        heartbeat::stop(pump);
        assert!(marker.exists(), "the pump was stuck in its write while the command ran");
        assert!(gone, "the client is marked gone");
        assert_eq!(status, 124, "{:?}", output.sink().lines());
        let group: i32 = fs::read_to_string(&pid_file)
            .expect("the command wrote its pid")
            .trim()
            .parse()
            .expect("a pid");
        assert!(group_gone_soon(group), "the command's group is ended");
    }

    #[test]
    fn a_backstop_kill_gives_124_the_kill_line_and_the_tag_and_keeps_the_record() {
        let case = Case::new("backstop");
        let output = recording();
        let watched = Watched {
            timeout: Some(1),
            host: "mac-mini",
            sample: Duration::from_millis(50),
            ..unwatched()
        };
        let status = case.watched(&["/bin/sh", "-c", "echo $$; sleep 600"], &watched, &idle_beat(), &output);
        assert_eq!(status, 124, "{:?}", output.sink().lines());
        let out = said(&output, Stream::Out);
        let group: i32 = out.get(1).and_then(|line| line.parse().ok()).expect("the command said its pid");
        let err = said(&output, Stream::Err);
        assert!(err.len() >= 2, "the kill line and the tag: {err:?}");
        let (line, tag) = (&err[err.len() - 2], &err[err.len() - 1]);
        assert!(
            line.starts_with("gate: BACKSTOP on mac-mini: ")
                && line.contains("(5 x 1 s ceiling, machine load unknown: the sampler failed)")
                && line.ends_with("and its children"),
            "{line}"
        );
        assert_eq!(*tag, killed_line(NONCE, BoxKill::Backstop), "the tag right after the kill line");
        assert!(case.record().exists(), "the command's record is kept");
        assert!(!case.group_record().exists(), "the group record is removed");
        assert!(group_gone_soon(group), "the command's group is ended");
    }

    #[test]
    fn a_kill_with_the_client_gone_gives_255_and_says_no_kill_line() {
        let case = Case::new("kill-gone");
        let output = recording();
        output.mark_gone();
        fs::write(case.record(), "ours").expect("the record is written");
        let words = ["/bin/sh".to_owned()];
        let ran = Ran {
            status: ExitStatus::from_raw(libc::SIGTERM),
            stdout: Vec::new(),
            stderr: Vec::new(),
            leftover: false,
            ended_by: Some(EndedBy::Halted(Halt("backstop".to_owned()))),
        };
        let record = command_record(&case.work);
        let status = ended(&case.launch(&words, &unwatched()), &record, Ok(ran), Some(backstop()), &output);
        assert_eq!(status, 255);
        assert!(output.sink().lines().is_empty(), "{:?}", output.sink().lines());
        assert!(case.record().exists(), "the record is kept");
    }

    #[test]
    fn a_kill_whose_ending_failed_says_a_note_the_kill_line_and_the_tag_and_gives_124() {
        let still_there = still_there_why(77, Duration::from_secs(10));
        let unrecorded = "its group record could not be removed: denied";
        for (why, note) in [
            (
                still_there.as_str(),
                format!("gate: note: the box's backstop kill could not end job {JOB}: {still_there}"),
            ),
            (unrecorded, format!("gate: note: after the box's backstop kill: {unrecorded}")),
        ] {
            let case = Case::new("kill-unended");
            let output = recording();
            fs::write(case.record(), "ours").expect("the record is written");
            let words = ["/bin/sh".to_owned()];
            let record = command_record(&case.work);
            let status = ended(
                &case.launch(&words, &unwatched()),
                &record,
                Err(NotRan::Failed(why.to_owned())),
                Some(backstop()),
                &output,
            );
            assert_eq!(status, 124, "{why}");
            assert_eq!(
                said(&output, Stream::Err),
                [note, backstop().line("/bin/sh"), killed_line(NONCE, BoxKill::Backstop)]
            );
            assert!(case.record().exists(), "the record is kept: {why}");
        }
    }

    #[test]
    fn a_ceiling_check_never_takes_the_output_lock() {
        let (beats, beaten) = channel();
        let (release, hold) = channel();
        let output = Output::new(BeatSink::holding(beats, hold, HoldFrom::First, None));
        thread::scope(|scope| {
            let stuck = scope.spawn(|| output.say(Stream::Err, "gate-remote-heartbeat: 0"));
            assert!(beaten.recv_timeout(GUARD).is_ok(), "the other thread holds the output lock in its write");
            // A check that took the lock would wait for the stuck write, which is let go only after it.
            let ceiling = Ceiling::new(Some(0), Duration::from_millis(1));
            ceiling.begin();
            let halt = ceiling.check();
            let _ = release.send(());
            assert!(stuck.join().is_ok_and(|said| said.is_ok()));
            assert_eq!(halt, Some(Halt("backstop".to_owned())), "a 0 s ceiling's backstop holds at once");
            assert!(matches!(ceiling.kill.get(), Some(Kill::Backstop { .. })), "the kill is kept");
        });
        let none = Ceiling::new(None, Duration::from_millis(1));
        none.begin();
        assert_eq!(none.check(), None, "no ceiling, no kill");
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn recording() -> Arc<Output<RecordingSink>> {
        Arc::new(Output::new(RecordingSink::default()))
    }

    /// The lines `output` said on `stream`.
    fn said(output: &Output<RecordingSink>, stream: Stream) -> Vec<String> {
        output
            .sink()
            .lines()
            .into_iter()
            .filter(|(said, _)| *said == stream)
            .map(|(_, line)| line)
            .collect()
    }

    #[test]
    fn a_command_that_exits_3_gives_3_after_the_start_marker_and_leaves_no_record() {
        let case = Case::new("exit-3");
        let output = recording();
        let status = case.run(&["/bin/sh", "-c", "echo hi; echo there >&2; exit 3"], &[], None, &output);
        assert_eq!(status, 3);
        assert_eq!(said(&output, Stream::Out), [started_line(NONCE), "hi".to_owned()]);
        assert_eq!(said(&output, Stream::Err), ["there"]);
        assert!(!case.record().exists(), "a clean end removes the record");
        assert!(!PathBuf::from(format!("{}.pgid", case.work)).exists(), "the group record is removed");
    }

    #[test]
    fn a_command_killed_by_sigkill_gives_137_and_keeps_its_record() {
        let case = Case::new("sigkill");
        let output = recording();
        let status = case.run(&["/bin/sh", "-c", "kill -KILL $$"], &[], None, &output);
        assert_eq!(status, 137);
        let text = fs::read_to_string(case.record()).expect("the record is kept");
        let (start, job) = text.split_once(' ').expect("the record holds a start and a job");
        assert_eq!(job, JOB);
        assert_eq!(start.len(), "2026-10-21T12:34:56.1234567Z".len(), "{start}");
        assert!(start.ends_with('Z') && start.as_bytes().get(10) == Some(&b'T'), "{start}");
    }

    #[test]
    fn a_program_that_does_not_exist_gives_127_and_leaves_no_record() {
        let case = Case::new("missing");
        let output = recording();
        let status = case.run(&["gate-box-no-such-program", "an-argument"], &[], None, &output);
        assert_eq!(status, 127);
        assert!(!case.record().exists(), "a command that never started leaves no record");
        let lines = said(&output, Stream::Err);
        assert!(
            lines.len() == 1 && lines[0].starts_with("gate: remote could not start gate-box-no-such-program: "),
            "{lines:?}"
        );
    }

    #[test]
    fn a_client_gone_before_the_spawn_gives_255_and_starts_nothing() {
        let case = Case::new("gone");
        let marker = case.base.join("ran");
        let output = Arc::new(Output::new(HungUpSink));
        let status = case.run(&["/usr/bin/touch", &marker.display().to_string()], &[], None, &output);
        assert_eq!(status, 255);
        assert!(!marker.exists(), "nothing started");
        assert!(!case.record().exists(), "no record");
    }

    #[test]
    fn a_removal_once_the_lock_name_was_replaced_leaves_the_other_run_s_record() {
        let case = Case::new("lock-replaced");
        fs::remove_file(&case.lock_path).expect("the lock's name is freed");
        fs::write(&case.lock_path, "").expect("another run's lock takes the name");
        fs::write(case.record(), "other").expect("the other run writes its record");
        let output = recording();
        removed_if_held(&command_record(&case.work), &case.lock, &output);
        assert_eq!(fs::read_to_string(case.record()).expect("the other run's record is kept"), "other");
        assert!(output.sink().lines().is_empty(), "nothing is said: {:?}", output.sink().lines());
    }

    #[test]
    fn a_removal_while_the_lock_is_held_removes_the_record() {
        let case = Case::new("lock-held");
        fs::write(case.record(), "ours").expect("the record is written");
        let output = recording();
        removed_if_held(&command_record(&case.work), &case.lock, &output);
        assert!(!case.record().exists(), "the record is removed");
        assert!(output.sink().lines().is_empty(), "nothing is said: {:?}", output.sink().lines());
    }

    #[test]
    fn a_command_whose_parent_ignores_term_still_dies_on_term_and_keeps_its_record() {
        let case = Case::new("term-ignored");
        let output = recording();
        // SAFETY: setting a disposition touches no memory; nextest runs this case in a process of its own, and the
        // default is put back before any assertion.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        let status = case.run(&["/bin/sh", "-c", "kill -TERM $$; exit 0"], &[], None, &output);
        // SAFETY: as above.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
        }
        assert_eq!(status, 143, "the command took TERM with its default action");
        assert!(case.record().exists(), "a signalled command keeps its record");
    }

    #[test]
    fn the_command_s_environment_has_the_login_path_the_payload_s_env_and_the_box_s_names_once() {
        let case = Case::new("environment");
        let output = recording();
        let env = [
            ("sharedcompilationid".to_owned(), "theirs".to_owned()),
            ("MY_VAR".to_owned(), "v".to_owned()),
        ];
        let status = case.run(&["/usr/bin/env"], &env, Some("/login/first:/usr/bin:/bin"), &output);
        assert_eq!(status, 0);
        let lines = said(&output, Stream::Out);
        for wanted in [
            "PATH=/login/first:/usr/bin:/bin",
            "MY_VAR=v",
            "GATE_BOX_JOB=gate-remote-k-0123456789ab",
            "SharedCompilationId=gate-box-0123456789ab",
        ] {
            assert!(lines.iter().any(|line| line == wanted), "{wanted}: {lines:?}");
        }
        let named = lines
            .iter()
            .filter(|line| {
                line.split('=')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case("SharedCompilationId"))
            })
            .count();
        assert_eq!(named, 1, "one name: {lines:?}");
    }

    #[test]
    fn a_program_on_the_login_path_is_found_there() {
        let case = Case::new("login-path");
        let bin = case.base.join("bin");
        fs::create_dir_all(&bin).expect("the tool folder is made");
        let tool = bin.join("gate-box-test-tool");
        fs::write(&tool, "#!/bin/sh\nexit 4\n").expect("the tool is written");
        fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the tool is made runnable");
        let output = recording();
        let login = format!("{}:/usr/bin:/bin", bin.display());
        let status = case.run(&["gate-box-test-tool"], &[], Some(&login), &output);
        assert_eq!(status, 4, "{:?}", output.sink().lines());
    }
}
