//! The Rust remote client, in the phases the PowerShell client runs: the snapshot; the run call built and checked
//! against the most cmd takes before anything is sent; the preflight and the builder's version; the push and the saved
//! ref; and the run call, its output passed through to the log and the console. A call that failed before the ssh
//! banner is made once more. A Ctrl+C ends the attempt between its calls, during the retry pause, and during any call;
//! one during the run call stops the command on the builder. Each call goes through the [`Transport`]; every process
//! is created suspended, put in an anonymous job of its own that ends its whole tree when it is killed, and only then
//! let run. The preflight, the push and the run call run under the run's watchdog, whose clock starts with the
//! attempt: a call that sends nothing for the stall window, or outlives the run's ceiling, is killed. A kill before the
//! run call falls back `unreachable`, as the command never started; a kill of the run call stops the command and ends
//! the run 124. A run call that exits 255 other than a failure before the banner lost its connection: nothing is synced
//! back and the command is stopped; then, when the preflight said the builder's boot time, the builder is asked for it
//! again. One that booted more than 30 s later restarted, and the run falls back `rebooted`; one that gives no boot
//! time, or when either value is no timestamp, falls back `unreachable`; else the run ends 255. No call holds more than
//! 64 KiB of a line in memory: a longer one ends any call but the run call, whose output is passed on whole. The run
//! takes the work-folder key's lock, [`hold_key`], before its remote slot, so a second routed run of one worktree falls
//! back `busy` at once.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::ops::Range;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::{FallbackReason, KernelObject, key_hash};
use gate_win32::{
    AcquireError, EnvironmentBlock, Interrupt, Job, NamedMutex, OwnedMutexGuard, PipedConsole, PipedLaunch, PipedSuspended, Running,
    same_variable_name,
};
use jiff::{SignedDuration, Timestamp};
use regex::Regex;

use super::git::{self, Git, GitError, last_update};
use super::payload::{self, Location, RunCall};
use super::retry::{self, Ended, GIT_SSH_FAILED, SSH_FAILED};
use super::snapshot::{self, Snapshot};
use super::syncback;
use super::transport::{Local, Program, Transport};
use super::watchdog::{self, Cut, Kept, LINE_CAP};
use super::{FAILED, FAILED_RUN, REFUSED, RemoteCall, RemoteClient, RemoteOutcome, busy_line};
use crate::boxside::payload::escaped;
use crate::run::CONTROL_C_EXIT;

/// The statuses of a preflight whose builder has no `gate box` that takes the run: cmd's for a program it cannot find,
/// and a gate's usage error for a payload it does not read.
const NOT_RECOGNIZED: i32 = 9009;
const USAGE: i32 = 2;
/// How often the retry pause looks at the console interrupt.
const PAUSE_STEP: Duration = Duration::from_millis(20);
/// The box's heartbeat, `gate-remote-heartbeat: <digits>` on standard error: progress, kept out of the log.
const HEARTBEAT: &str = "gate-remote-heartbeat: ";
/// The preflight's lines the client reads.
const VERSION: &str = "gate: remote version ";
const CREATED: &str = "gate: remote created ";
/// The builder's boot time, which the preflight and the `boot` call say.
const BOOT: &str = "gate: remote boot ";
/// How far apart two boot times of one builder may be and still be one boot.
const BOOT_SLACK: SignedDuration = SignedDuration::from_secs(30);
/// This gate's version, which the builder's must be.
const OURS: &str = env!("CARGO_PKG_VERSION");
/// The status of a call the client killed, and of a run whose run call the watchdog killed.
const KILLED: i32 = 124;
/// How long the client waits for a killed call's output to end before it stops waiting on it.
const DRAIN: Duration = Duration::from_secs(2);
/// How long the client waits for a call it killed to exit.
const KILL_WAIT: Duration = Duration::from_secs(30);
/// The line said when `--test-remote-interrupt-at` is given but the console handler that would carry the interrupt
/// could not be installed.
const NO_HANDLER: &str = "gate: --test-remote-interrupt-at needs the console handler; it could not be installed";

/// The client of `--test-remote-transport`.
pub(crate) struct RustClient {
    transport: Box<dyn Transport>,
    /// How long each git call of the snapshot and the saved ref may take.
    limit: Duration,
    /// The pause before a retry.
    pause: Duration,
    /// How long a bounded call, the `stop` or the `boot`, may take.
    bound: Duration,
    /// `--test-stall-ms`: the stall window, given as it is, with no floor.
    stall_ms: Option<u64>,
    /// `--test-remote-timeout-ms`: the ceiling, in milliseconds.
    timeout_ms: Option<u64>,
    /// `--test-remote-watch-call`: the one call the two seams above apply to, when they are not the whole run's.
    watch_call: Option<String>,
    /// `--test-remote-interrupt-at`: the call after whose first output byte the client raises the console interrupt.
    interrupt_at: Option<String>,
}

impl RustClient {
    /// The client of the local transport `transport` names, with the request's seams; else the line saying why the
    /// transport cannot be used.
    pub(crate) fn local(request: &RunRequest, transport: &str) -> Result<RustClient, String> {
        let seams = &request.test_seams;
        let local = Local::new(transport, seams.remote_ssh.as_deref())?;
        Ok(RustClient {
            transport: Box::new(local),
            limit: git::limit(seams),
            pause: retry::pause(seams),
            bound: retry::bound(seams),
            stall_ms: seams.stall_ms,
            timeout_ms: seams.remote_timeout_ms,
            watch_call: seams.remote_watch_call.clone(),
            interrupt_at: seams.remote_interrupt_at.clone(),
        })
    }
}

/// The lock of `call`'s work-folder key: a root-hashed kernel mutex taken without waiting, which a routed run holds from
/// before it takes a remote slot until its remote attempt ends. A lock its last holder left without releasing is taken.
/// Else `None` when another routed run holds it, or the line saying why it could not be opened or taken.
pub(crate) fn hold_key(call: &RemoteCall<'_>) -> Result<OwnedMutexGuard, Option<String>> {
    let name = call.state.object_name(KernelObject::Routed { hash: key_hash(call.key) });
    let mutex = NamedMutex::open(&name).map_err(|error| Some(format!("gate: cannot open the mutex {name}: {error}")))?;
    match mutex.acquire_owned(Duration::ZERO) {
        Ok(held) => Ok(held),
        Err(AcquireError::Timeout) => Err(None),
        Err(AcquireError::Failed(error)) => Err(Some(format!("gate: cannot take the mutex {name}: {error}"))),
    }
}

impl RemoteClient for RustClient {
    fn hold_key(&self, call: &RemoteCall<'_>) -> Result<Option<OwnedMutexGuard>, Option<String>> {
        hold_key(call).map(Some)
    }

    fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
        let started = Instant::now();
        let one = self.watch_call.as_deref();
        // With `--test-remote-watch-call`, the stall and ceiling seams are that one call's, and the run keeps its own.
        let (stall_ms, timeout_ms) = if one.is_some() { (None, None) } else { (self.stall_ms, self.timeout_ms) };
        let (stall, raised) = watchdog::stall_window(call.stall, stall_ms);
        let mut attempt = Attempt {
            call,
            transport: self.transport.as_ref(),
            limit: self.limit,
            pause: self.pause,
            limits: Limits {
                started,
                stall,
                ceiling: timeout_ms.map_or(Duration::from_secs(call.timeout), Duration::from_millis),
                bound: self.bound,
            },
            one_call: one.map(|name| OneCall {
                name,
                stall: self.stall_ms.map(Duration::from_millis),
                ceiling: self.timeout_ms.map(Duration::from_millis),
            }),
            interrupt_at: self.interrupt_at.as_deref(),
            lines: Vec::new(),
            problem: None,
        };
        if let Some(line) = raised {
            attempt.say(line);
        }
        if attempt.interrupt_at.is_some() && call.interrupt.is_none() {
            attempt.told(NO_HANDLER);
        }
        let end = match attempt.phases() {
            Ok(end) | Err(end) => end,
        };
        attempt.outcome(end)
    }
}

/// How the attempt ended.
#[derive(Debug)]
pub(super) enum End {
    /// The run call ended with this status, the command's own.
    Status(i64),
    /// The builder cannot take the run, for this reason: status 75, and the run falls back here.
    Refused(FallbackReason),
    /// The client failed, for this reason: status 1, with no fallback.
    Failed(String),
    /// The console interrupt ended it.
    Interrupted,
    /// The watchdog killed the run call, and the command was stopped: status 124.
    Killed,
    /// The run call's connection was lost: it exited 255, ssh's own failure status, other than before the banner. The
    /// run call returns it before anything is stopped, with where the run call's own lines are among the attempt's, so
    /// a fallback can leave them out. Nothing is synced back: status 255.
    Lost(Range<usize>),
}

/// What the preflight found: whether it created the bare repository just now, and the builder's boot time, `None`
/// when it said none.
struct Prepared {
    created: bool,
    boot: Option<String>,
}

/// The run's clock, which starts with the attempt, and the limits the client's calls are watched by: the stall window,
/// the ceiling, and the bound of the bounded calls, the `stop` and the `boot`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    started: Instant,
    stall: Duration,
    ceiling: Duration,
    bound: Duration,
}

/// `--test-remote-watch-call`: the one call, `preflight`, `push`, `run`, `ls-remote` or `fetch`, that the stall window
/// and the ceiling of `--test-stall-ms` and `--test-remote-timeout-ms` apply to, its ceiling counted from its own start.
/// For `ls-remote`, which has a deadline rather than a stall window, the ceiling is that deadline.
#[derive(Clone, Copy)]
struct OneCall<'c> {
    name: &'c str,
    stall: Option<Duration>,
    ceiling: Option<Duration>,
}

/// How a call is watched while it runs.
#[derive(Clone, Copy, Debug)]
pub(super) enum Watch {
    /// By the run's watchdog under these limits: killed once it sent nothing for the stall window, or the run outlived
    /// its ceiling.
    Run(Limits),
    /// Killed once it sent nothing for the stall window of these limits, or at the run's ceiling: the sync-back's fetch.
    Fetch(Limits),
    /// Killed once it ran for this long.
    Deadline(Duration),
    /// As [`Watch::Deadline`], but a Ctrl+C does not end it: the bounded calls, the `stop`, made after one, and the
    /// `boot`.
    Bounded(Duration),
}

impl Watch {
    /// Why a call watched so, started at `begun`, is killed at `now`, nothing having come from it for `idle`, and
    /// whether a capped call sent a line `too_long`; `None` while it may run on.
    fn cut(self, now: Instant, begun: Instant, idle: Duration, too_long: bool) -> Option<Cut> {
        if too_long {
            return Some(Cut::TooLong);
        }
        match self {
            Watch::Run(limits) => {
                let elapsed = now.saturating_duration_since(limits.started);
                watchdog::kill(idle, elapsed, limits.stall, limits.ceiling).map(Cut::Watchdog)
            }
            Watch::Fetch(limits) if idle >= limits.stall => Some(Cut::Silent(limits.stall)),
            Watch::Fetch(limits) => (now.saturating_duration_since(limits.started) >= limits.ceiling).then_some(Cut::Ceiling),
            Watch::Deadline(limit) | Watch::Bounded(limit) => (now.saturating_duration_since(begun) >= limit).then_some(Cut::Late(limit)),
        }
    }

    /// How long the client waits between two looks at a call watched so.
    fn tick(self) -> Duration {
        match self {
            Watch::Run(limits) | Watch::Fetch(limits) => watchdog::tick(limits.stall),
            Watch::Deadline(limit) | Watch::Bounded(limit) => watchdog::tick(limit),
        }
    }

    /// Whether a Ctrl+C ends the call: every call's but a bounded one's.
    fn interruptible(self) -> bool {
        !matches!(self, Watch::Bounded(_))
    }
}

/// Where a call's lines go besides the log.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Echo {
    /// To the console stream each came on, as the command's output is.
    Console,
    /// Nowhere else: a preflight's lines.
    LogOnly,
    /// Nowhere else, each line as its carriage-return updates leave it: a push's progress.
    Progress,
    /// Not even to the log: the lines are only kept for the call's failure line, as the sync-back's calls to the
    /// builder keep theirs.
    Unlogged,
}

/// One call's process as it ended: its exit status, 124 when the client killed it, and why it did; whether it wrote on
/// standard output, its first standard error line (a heartbeat included) and its last one (none), and the last lines
/// it wrote but heartbeats, each cut to the cap; and, for a call the client killed, what else went wrong meanwhile.
pub(super) struct Called {
    pub(super) status: i32,
    pub(super) cut: Option<Cut>,
    /// A reader's problem, or a failure to end the call, which a kill keeps as a note rather than as the run's failure.
    pub(super) problem: Option<String>,
    output_seen: bool,
    first_error: Option<String>,
    last_error: Option<String>,
    pub(super) lines: Vec<String>,
}

impl Called {
    /// Whether the call failed before the banner, for a program that exits `fail_status` when ssh failed.
    fn before_the_banner(&self, fail_status: i32) -> bool {
        retry::before_the_banner(&Ended {
            status: self.status,
            fail_status,
            output_seen: self.output_seen,
            first_error: self.first_error.as_deref(),
        })
    }
}

/// One run of the client: the call, the way to the builder, the limits, every line the attempt wrote to the log, and
/// the first line that could not be written.
pub(super) struct Attempt<'c, 'a> {
    pub(super) call: &'c RemoteCall<'a>,
    pub(super) transport: &'c dyn Transport,
    pub(super) limit: Duration,
    pause: Duration,
    pub(super) limits: Limits,
    one_call: Option<OneCall<'c>>,
    /// The call, `preflight` or `run`, after whose first output byte the console interrupt is raised, as a Ctrl+C would.
    interrupt_at: Option<&'c str>,
    lines: Vec<String>,
    problem: Option<String>,
}

impl Attempt<'_, '_> {
    /// The phases in order; `Err` for an end before the run call.
    fn phases(&mut self) -> Result<End, End> {
        let snapshot = self.snapshot()?;
        let location = Location::under(self.transport.root(), self.call.repo, self.call.key, &payload::job_nonce());
        let nonce = payload::run_nonce();
        let run = RunCall {
            words: self.call.words,
            subfolder: self.call.subfolder,
            nonce: &nonce,
            snapshot: &snapshot.commit,
        };
        let run_line = payload::capped("run", payload::line("run", &payload::run_word(&location, &run))).map_err(End::Failed)?;
        let prepared = self.preflight(&location)?;
        self.push(&snapshot, prepared.created)?;
        let end = self.run_call(&run_line, &location)?;
        // The sync-back runs after a run call that ended with the command's own status; a refusal, an interrupt, a
        // kill, a lost connection or a failed call ended the attempt before it, so a box refusal's 75 never syncs back.
        match end {
            End::Status(status) => syncback::sync_back(self, &snapshot, status),
            End::Lost(output) => Ok(self.connection_lost(&location, prepared.boot.as_deref(), output)),
            end => Ok(end),
        }
    }

    /// Goes on unless the console interrupt is raised, which ends the attempt as an interrupt.
    pub(super) fn go_on(&self) -> Result<(), End> {
        if self.call.interrupt.is_some_and(Interrupt::is_raised) {
            return Err(End::Interrupted);
        }
        Ok(())
    }

    /// The snapshot of the call's worktree.
    fn snapshot(&self) -> Result<Snapshot, End> {
        let roots = [PathBuf::from(self.call.root)];
        snapshot::take(&roots, self.call.key, self.limit, self.call.interrupt)
            .map_err(git_end)?
            .pop()
            .ok_or_else(|| End::Failed("the snapshot made no commit".to_owned()))
    }

    /// The preflight: readies the work folder, and checks the builder runs this gate's version. Whether it created the
    /// bare repository just now, and the builder's boot time. A builder with no `gate box` that takes the run is not
    /// routed, as one of another version is not.
    fn preflight(&mut self, location: &Location) -> Result<Prepared, End> {
        self.go_on()?;
        let line = payload::capped("preflight", payload::line("preflight", &payload::location_word(location))).map_err(End::Failed)?;
        let watch = Watch::Run(self.limits_for("preflight"));
        let raise = self.raises_at("preflight");
        let (mut called, _) = self.with_retry(SSH_FAILED, |attempt| attempt.ssh(&line, Echo::LogOnly, watch, raise))?;
        let theirs = their_version(&called.lines).map(str::to_owned);
        let created = called.lines.iter().any(|line| line.starts_with(CREATED));
        let boot = boot_time(&called.lines).map(str::to_owned);
        self.lines.append(&mut called.lines);
        self.not_started_if_killed("the preflight", &called)?;
        let host = self.call.host;
        match called.status {
            0 => {}
            SSH_FAILED => {
                let last = escaped(&git::failure_text(&[called.last_error.as_deref().unwrap_or_default()]));
                self.say(format!("gate: remote {host} unreachable: {last}"));
                return Err(End::Refused(FallbackReason::Unreachable));
            }
            status @ (NOT_RECOGNIZED | USAGE) => {
                self.say(format!(
                    "gate: not routed: {host} has no gate box that takes this run (preflight status {status})"
                ));
                return Err(End::Refused(FallbackReason::Version));
            }
            status => {
                self.say(format!("gate: preparing {} on {host} failed (status {status})", self.transport.root()));
                return Err(End::Status(i64::from(status)));
            }
        }
        if let Some(line) = version_refusal(host, theirs.as_deref(), OURS) {
            self.say(line);
            return Err(End::Refused(FallbackReason::Version));
        }
        Ok(Prepared { created, boot })
    }

    /// Pushes the snapshot, and its bridge unless the bare repository was `created` just now, with no hooks and no
    /// submodules, then saves the local ref and says the push. A push that fails is `unreachable`.
    fn push(&mut self, snapshot: &Snapshot, created: bool) -> Result<(), End> {
        self.go_on()?;
        let (root, key) = (Path::new(self.call.root), self.call.key);
        let target = self.transport.push_target(self.call.host, self.call.repo);
        let mut args: Vec<String> = ["push", "--no-verify", "--no-recurse-submodules", "--progress"].map(str::to_owned).into();
        args.extend(target.receive_pack.iter().map(|pack| format!("--receive-pack={pack}")));
        args.extend(["--".to_owned(), target.url.clone(), format!("+{}:refs/gate/{key}", snapshot.commit)]);
        let bridge = snapshot.bridge.as_deref().filter(|_| !created);
        args.extend(bridge.map(|bridge| format!("+{bridge}:refs/gate-bridge/{key}")));
        let git = Git::find(root).map_err(End::Failed)?;
        let started = Instant::now();
        let ssh_command = target.ssh_command.as_deref();
        let watch = Watch::Run(self.limits_for("push"));
        let (mut called, _) = self.with_retry(GIT_SSH_FAILED, |attempt| attempt.git(&git, &args, ssh_command, Echo::Progress, watch))?;
        let size = push_size(&called.lines);
        self.lines.append(&mut called.lines);
        self.not_started_if_killed("the push", &called)?;
        if called.status != 0 {
            self.say(format!("gate: pushing the snapshot to {} failed (status {})", target.url, called.status));
            return Err(End::Refused(FallbackReason::Unreachable));
        }
        snapshot::save(root, key, &snapshot.commit, self.limit, self.call.interrupt).map_err(git_end)?;
        let short = snapshot.commit.get(..8).unwrap_or(&snapshot.commit);
        self.say(format!("gate: pushed snapshot {short} in {} s ({size})", seconds(started.elapsed())));
        Ok(())
    }

    /// Ends the call `what`, a preflight or a push, that the client killed as the fallback `unreachable`: the command
    /// never started, so nothing is stopped. Its line names the builder unreachable, not the kill: the run goes on here
    /// with the same log, whose verdict would read a kill line as its own.
    fn not_started_if_killed(&mut self, what: &str, called: &Called) -> Result<(), End> {
        let Some(cut) = called.cut else {
            return Ok(());
        };
        self.say(format!("gate: remote {} unreachable: {}", self.call.host, cut.reason(what)));
        self.noted(called);
        Err(End::Refused(FallbackReason::Unreachable))
    }

    /// Says what else went wrong around a call the client killed, which the kill's own end keeps: a reader's problem,
    /// or a failure to end the call.
    pub(super) fn noted(&mut self, called: &Called) {
        if let Some(problem) = &called.problem {
            self.say(format!("gate: {problem}"));
        }
    }

    /// The limits the call `name` is watched by: the run's own, or for the one call `--test-remote-watch-call` names,
    /// the seams' stall window and ceiling, the ceiling counted from now.
    pub(super) fn limits_for(&self, name: &str) -> Limits {
        match self.one_call {
            Some(one) if one.name == name => Limits {
                started: if one.ceiling.is_some() { Instant::now() } else { self.limits.started },
                stall: one.stall.unwrap_or(self.limits.stall),
                ceiling: one.ceiling.unwrap_or(self.limits.ceiling),
                bound: self.limits.bound,
            },
            _ => self.limits,
        }
    }

    /// The deadline of the call `name`: `limit`, or the ceiling of `--test-remote-timeout-ms` when
    /// `--test-remote-watch-call` names the call.
    pub(super) fn deadline_for(&self, name: &str, limit: Duration) -> Duration {
        match self.one_call {
            Some(OneCall {
                name: one,
                ceiling: Some(ceiling),
                ..
            }) if one == name => ceiling,
            _ => limit,
        }
    }

    /// The run call to the builder at `location`: the command's own status; `unreachable` when it failed before the
    /// banner twice, as the command never started; a refusal when its 75 comes with the box's own refusal line; a lost
    /// connection when it exited 255 otherwise. A watchdog kill says its line, stops the command and ends the run
    /// killed; a Ctrl+C stops the command too.
    fn run_call(&mut self, line: &str, location: &Location) -> Result<End, End> {
        self.go_on()?;
        let limits = self.limits_for("run");
        let raise = self.raises_at("run");
        self.run_call_made(location, limits, |attempt| attempt.ssh(line, Echo::Console, Watch::Run(limits), raise))
    }

    /// The run call as `make` makes it, watched by `limits`, as [`Attempt::run_call`] says. The command is stopped only
    /// when it may have started: after a kill, after a Ctrl+C that ended a call still running, and after one raised by
    /// the time a call ended with a status; never after a Ctrl+C in the retry pause or after a call that failed before
    /// the banner.
    fn run_call_made(&mut self, location: &Location, limits: Limits, mut make: impl FnMut(&mut Self) -> Result<Called, End>) -> Result<End, End> {
        let mut running = false;
        let made = self.with_retry(SSH_FAILED, |attempt| {
            let made = make(attempt);
            running = matches!(made, Err(End::Interrupted));
            made
        });
        let (mut called, again) = match made {
            Ok(made) => made,
            Err(End::Interrupted) if running => {
                self.stop(location);
                return Err(End::Interrupted);
            }
            Err(end) => return Err(end),
        };
        let reason = refusal(self.call.key, &called.lines);
        let kept = self.lines.len();
        self.lines.append(&mut called.lines);
        let output = kept..self.lines.len();
        if let Some(Cut::Watchdog(kill)) = called.cut {
            self.say(watchdog::kill_line(kill, limits.stall, limits.ceiling));
            self.noted(&called);
            self.stop(location);
            return Ok(End::Killed);
        }
        if again {
            return Ok(End::Refused(FallbackReason::Unreachable));
        }
        if self.go_on().is_err() {
            self.stop(location);
            return Err(End::Interrupted);
        }
        if called.status == SSH_FAILED {
            return Ok(End::Lost(output));
        }
        let status = i64::from(called.status);
        match reason {
            Some(reason) if status == REFUSED => Ok(End::Refused(reason)),
            _ => Ok(End::Status(status)),
        }
    }

    /// Makes a call with `make` and, when it failed before the banner, says so, pauses and makes it once more; a Ctrl+C
    /// before the retry or during its pause ends the attempt instead. The call that counts, its lines not yet kept, and
    /// whether the retry failed before the banner too.
    pub(super) fn with_retry(&mut self, fail_status: i32, mut make: impl FnMut(&mut Self) -> Result<Called, End>) -> Result<(Called, bool), End> {
        let mut first = make(self)?;
        if !first.before_the_banner(fail_status) {
            return Ok((first, false));
        }
        self.lines.append(&mut first.lines);
        self.go_on()?;
        self.say(retry::retry_line(self.call.host));
        paused(self.pause, self.call.interrupt)?;
        self.go_on()?;
        let second = make(self)?;
        let again = second.before_the_banner(fail_status);
        Ok((second, again))
    }

    /// After the run call's connection to the builder at `location` was lost: stops the command, then, when the
    /// preflight said the builder's boot time, `before`, asks for it again, as [`after_loss`] reads the two. A restart
    /// falls back `rebooted`, and no boot time to compare by `unreachable`; either leaves the run call's own lines,
    /// `output` among the attempt's, out of the run that falls back. Else the connection was lost, and a Ctrl+C that
    /// came meanwhile ends the run as an interrupt; one that came by the end of the stop ends it there, with no boot
    /// call.
    fn connection_lost(&mut self, location: &Location, before: Option<&str>, output: Range<usize>) -> End {
        let host = self.call.host;
        self.stop(location);
        if self.go_on().is_err() {
            self.say(format!("gate: lost the connection to {host}"));
            return End::Interrupted;
        }
        let now = before.and_then(|_| self.boot_now(location));
        match after_loss(before, now.as_deref()) {
            Loss::Lost => {
                self.say(format!("gate: lost the connection to {host}"));
                if self.go_on().is_err() { End::Interrupted } else { End::Lost(output) }
            }
            Loss::Restarted(booted) => {
                let shown = escaped(first_bytes(booted, git::FAILURE_BYTES));
                let line = format!("gate: {host} restarted during the run (booted {shown}); the run did not finish there");
                self.falls_back_after_loss(output, line, FallbackReason::Rebooted)
            }
            Loss::Unreachable => {
                let line = format!("gate: {host} unreachable after the connection was lost; the run did not finish there");
                self.falls_back_after_loss(output, line, FallbackReason::Unreachable)
            }
        }
    }

    /// Falls back for `reason` after a lost connection, saying `line`: the run call's own lines, `output` among the
    /// attempt's, are left out, so the run that falls back reads none of the builder's output as its own.
    fn falls_back_after_loss(&mut self, output: Range<usize>, line: String, reason: FallbackReason) -> End {
        self.lines.drain(output);
        self.say(line);
        End::Refused(reason)
    }

    /// The builder's boot time now, from the `boot` call to `location`, a bounded call; `None` when the call failed,
    /// did not finish or said none.
    fn boot_now(&mut self, location: &Location) -> Option<String> {
        let what = format!("asking {} for its boot time", self.call.host);
        let called = self.bounded("boot", location, &what)?;
        (called.status == 0 && called.cut.is_none())
            .then(|| boot_time(&called.lines).map(str::to_owned))
            .flatten()
    }

    /// Stops the command the run call started on the builder at `location`, with the `stop` call, a bounded call; the
    /// run's end is the caller's however it went.
    fn stop(&mut self, location: &Location) {
        let what = format!("stopping the command on {}", self.call.host);
        self.bounded("stop", location, &what);
    }

    /// The bounded call `verb` to the builder at `location`, which the bound limits and a Ctrl+C does not end; it is
    /// made once more after a failure before the banner while 5 s of the bound would be left. Keeps the lines of the
    /// call that counts, says when it did not finish or failed, `what` it does named, and gives it back; `None` when it
    /// could not be made.
    fn bounded(&mut self, verb: &str, location: &Location, what: &str) -> Option<Called> {
        let line = match payload::capped(verb, payload::line(verb, &payload::location_word(location))) {
            Ok(line) => line,
            Err(why) => {
                self.say(format!("gate: {what} failed: {why}"));
                return None;
            }
        };
        let bound = self.limits.bound;
        let begun = Instant::now();
        let mut made = self.ssh(&line, Echo::LogOnly, Watch::Bounded(bound), false);
        let again = matches!(&made, Ok(first) if first.before_the_banner(SSH_FAILED)) && retry::retry_allowed(bound, begun.elapsed(), self.pause);
        if again {
            if let Ok(first) = &mut made {
                self.lines.append(&mut first.lines);
            }
            self.say(retry::retry_line(self.call.host));
            thread::sleep(self.pause);
            made = self.ssh(&line, Echo::LogOnly, Watch::Bounded(bound.saturating_sub(begun.elapsed())), false);
        }
        match made {
            Ok(called) => {
                self.lines.extend_from_slice(&called.lines);
                if let Some(said) = bounded_line(what, &called, bound) {
                    self.say(said);
                }
                self.noted(&called);
                Some(called)
            }
            Err(End::Failed(why)) => {
                self.say(format!("gate: {what} failed: {why}"));
                None
            }
            Err(end) => {
                self.say(format!("gate: {what} failed: {end:?}"));
                None
            }
        }
    }

    /// Whether the console interrupt is raised after the first output byte of the call `verb`:
    /// `--test-remote-interrupt-at`.
    fn raises_at(&self, verb: &str) -> bool {
        self.interrupt_at == Some(verb)
    }

    /// Says `line` on stderr alone; the first failure to is kept.
    fn told(&mut self, line: &str) {
        if let Err(error) = say_on(io::stderr().lock(), &format!("{line}\n")) {
            self.problem.get_or_insert_with(|| format!("cannot say a line: {error}"));
        }
    }

    /// One call of `gate box` on the builder carrying `line`, watched as `watch` says; the console interrupt raised
    /// after its first output byte when `raise`.
    fn ssh(&mut self, line: &str, echo: Echo, watch: Watch, raise: bool) -> Result<Called, End> {
        let Program { program, args } = self.transport.call(self.call.host, line);
        let mut command = Command::new(program);
        command.args(args);
        let starts = Starts {
            breakaway: self.transport.box_runs_here(),
            raise,
        };
        self.started(&command, echo, watch, starts)
    }

    /// git with `args` in the worktree, given `ssh_command` as the ssh it runs when there is one, its lines going where
    /// `echo` says, watched as `watch` says.
    pub(super) fn git(&mut self, git: &Git, args: &[String], ssh_command: Option<&str>, echo: Echo, watch: Watch) -> Result<Called, End> {
        let mut command = git.command(Path::new(self.call.root));
        command.args(args);
        if let Some(ssh) = ssh_command {
            command.env("GIT_SSH_COMMAND", ssh);
        }
        let starts = Starts {
            breakaway: false,
            raise: false,
        };
        self.started(&command, echo, watch, starts)
    }

    /// Starts `command` with no standard input, created suspended and put in an anonymous job of its own before it
    /// runs, and reads its two streams line by line into the log, and onto the console as `echo` says, until both end
    /// and it has exited, watched as `watch` says meanwhile. Every call's line is capped at 64 KiB but the run call's,
    /// whose longer lines are passed on in pieces. A call the watch cuts has its whole job ended, and its readers are
    /// given [`DRAIN`] to end; a Ctrl+C ends a call a bound does not limit the same way, as the attempt's interrupt.
    fn started(&mut self, command: &Command, echo: Echo, watch: Watch, starts: Starts) -> Result<Called, End> {
        let shown = Path::new(command.get_program()).display().to_string();
        let shown_log = self.call.log.display();
        let cannot_log = |error: io::Error| End::Failed(format!("cannot write to {shown_log}: {error}"));
        let log = OpenOptions::new().append(true).create(true).open(self.call.log).map_err(cannot_log)?;
        let (out_log, err_log) = if echo == Echo::Unlogged {
            (None, None)
        } else {
            (Some(log.try_clone().map_err(cannot_log)?), Some(log))
        };
        let job = call_job(starts.breakaway).map_err(|error| End::Failed(format!("cannot make a job for {shown}: {error}")))?;
        let cannot_start = |error: String| End::Failed(format!("cannot start {shown}: {error}"));
        let launch = piped_launch(command, console_of(watch)).map_err(|error| cannot_start(error.to_string()))?;
        let PipedSuspended { suspended, stdout, stderr } = launch.start_suspended_within(&job).map_err(|error| cannot_start(error.to_string()))?;
        let running = suspended.assign_and_resume(&job).map_err(|error| cannot_start(error.to_string()))?;
        let shared = Arc::new(Shared::new());
        let (done, readers) = mpsc::channel();
        let reading = Reading {
            echo,
            capped: echo != Echo::Console,
        };
        relayed(stdout, Reader::new(false, reading, out_log), &shared, done.clone());
        relayed(stderr, Reader::new(true, reading, err_log), &shared, done);
        let watched = Watched {
            running: &running,
            job: &job,
            progress: &shared.progress,
            readers: &readers,
            shown: &shown,
        };
        let raise = self.call.interrupt.filter(|_| starts.raise);
        let ended = watched.watch(watch, self.call.interrupt, raise)?;
        let heard = std::mem::take(&mut *held(&shared.heard));
        match called_of(ended, shared.progress.too_long(), heard) {
            Ok(called) => Ok(called),
            Err(Unfinished::Failed(why)) => Err(End::Failed(why)),
            Err(Unfinished::Interrupted(note)) => {
                // The interrupt stays the run's end, whose caller stops a command the call may have started.
                if let Some(note) = note {
                    self.say(format!("gate: {note}"));
                }
                Err(End::Interrupted)
            }
        }
    }

    /// Appends the gate's own `line` to the log and says it on stderr; the first failure to is kept.
    pub(super) fn say(&mut self, line: String) {
        if let Err(why) = super::say(self.call.log, &[&line], io::stderr().lock()) {
            self.problem.get_or_insert(why);
        }
        self.lines.push(line);
    }

    /// The outcome of an attempt that ended at `end`; a line that could not be written fails the run.
    fn outcome(mut self, end: End) -> RemoteOutcome {
        let killed = matches!(end, End::Killed);
        let (status, reason) = match end {
            End::Status(status) => (status, None),
            End::Refused(reason) => (REFUSED, Some(reason)),
            End::Interrupted => (i64::from(CONTROL_C_EXIT.cast_signed()), None),
            End::Killed => (i64::from(KILLED), None),
            End::Lost(_) => (i64::from(SSH_FAILED), None),
            End::Failed(why) => {
                self.say(format!("{FAILED_RUN}: {why}"));
                (FAILED, None)
            }
        };
        // A line that could not be written fails the run, unless the watchdog killed it: the kill stays the run's end.
        let (status, reason, killed) = match (&self.problem, killed) {
            (Some(_), false) => (FAILED, None, false),
            _ => (status, reason, killed),
        };
        RemoteOutcome {
            status,
            reason,
            killed,
            lines: self.lines,
            detail: self.problem,
        }
    }
}

#[cfg(test)]
impl<'c, 'a> Attempt<'c, 'a> {
    /// An attempt of `call` through `transport` with no pause before a retry, as the sync-back's tests drive one.
    pub(super) fn for_tests(call: &'c RemoteCall<'a>, transport: &'c dyn Transport) -> Attempt<'c, 'a> {
        Attempt {
            call,
            transport,
            limit: git::GIT_LIMIT,
            pause: Duration::ZERO,
            limits: Limits::for_tests(),
            one_call: None,
            interrupt_at: None,
            lines: Vec::new(),
            problem: None,
        }
    }
}

#[cfg(test)]
impl Limits {
    /// Limits no test reaches, the bounded calls' bound aside: an hour for the stall window and the ceiling.
    fn for_tests() -> Limits {
        Limits {
            started: Instant::now(),
            stall: Duration::from_secs(3600),
            ceiling: Duration::from_secs(3600),
            bound: Duration::from_secs(30),
        }
    }
}

/// How a watched call ended: it exited with its status; the client cut it; or a Ctrl+C ended it. With what went wrong
/// ending it.
#[derive(Debug)]
enum CallEnd {
    Exited(i32),
    Cut(Cut, Option<String>),
    Interrupted(Option<String>),
}

/// Why a watched call gave no result: the client failed it, or a Ctrl+C ended it, with what else went wrong meanwhile.
#[derive(Debug)]
enum Unfinished {
    Failed(String),
    Interrupted(Option<String>),
}

/// The call that ended as `ended`, a capped one having sent a line `too_long` or not, from what its streams said: a
/// reader's problem fails a call that ended on its own, and is kept as a note of one the client cut or a Ctrl+C
/// ended, whose end stands.
fn called_of(ended: CallEnd, too_long: bool, mut heard: Heard) -> Result<Called, Unfinished> {
    let (status, cut, ending) = match ended {
        CallEnd::Interrupted(ending) => return Err(Unfinished::Interrupted(joined(heard.problem.take(), ending))),
        CallEnd::Cut(cut, ending) => (KILLED, Some(cut), ending),
        // A line past the cap cuts the call even when it ended before the watch saw the line.
        CallEnd::Exited(_) if too_long => (KILLED, Some(Cut::TooLong), None),
        CallEnd::Exited(status) => (status, None, None),
    };
    let problem = joined(heard.problem.take(), ending);
    if let (None, Some(problem)) = (cut, &problem) {
        return Err(Unfinished::Failed(problem.clone()));
    }
    Ok(Called {
        status,
        cut,
        problem,
        output_seen: heard.output_seen,
        first_error: heard.first_error,
        last_error: heard.last_error,
        lines: heard.kept.into_lines(),
    })
}

/// A reader's problem and a failure to end the call, joined, or either alone.
fn joined(read: Option<String>, ending: Option<String>) -> Option<String> {
    match (read, ending) {
        (Some(read), Some(ending)) => Some(format!("{read}; {ending}")),
        (read, ending) => read.or(ending),
    }
}

/// A started call the client watches: its process and job, when its streams last sent a byte, the channel each of its
/// two readers says its end on, and its program as shown.
struct Watched<'w> {
    running: &'w Running,
    job: &'w Job,
    progress: &'w Progress,
    readers: &'w Receiver<()>,
    shown: &'w str,
}

impl Watched<'_> {
    /// Watches the call as `watch` says until both its readers ended and it exited: how it ended; the attempt's
    /// interrupt when the console `interrupt` ended it. `raise`, when given, is raised once the call sent its first
    /// byte, as a Ctrl+C would be.
    fn watch(self, watch: Watch, interrupt: Option<Interrupt>, mut raise: Option<Interrupt>) -> Result<CallEnd, End> {
        let begun = Instant::now();
        let tick = watch.tick();
        let mut open = 2;
        loop {
            if open == 0
                && let Some(code) = self.running.wait(Duration::ZERO).map_err(|error| cannot_wait(self.shown, &error))?
            {
                return Ok(CallEnd::Exited(code.cast_signed()));
            }
            if self.progress.heard_any()
                && let Some(interrupt) = raise.take()
            {
                interrupt.raise();
            }
            if watch.interruptible() && interrupt.is_some_and(Interrupt::is_raised) {
                return Ok(CallEnd::Interrupted(self.end(open)));
            }
            if let Some(cut) = watch.cut(Instant::now(), begun, self.progress.idle(), self.progress.too_long()) {
                return Ok(CallEnd::Cut(cut, self.end(open)));
            }
            open -= self.readers_ended(open, tick);
        }
    }

    /// Waits at most `tick` for one of the `open` readers to end: how many did.
    fn readers_ended(&self, open: usize, tick: Duration) -> usize {
        if open == 0 {
            thread::sleep(PAUSE_STEP.min(tick));
            return 0;
        }
        match self.readers.recv_timeout(tick) {
            Ok(()) => 1,
            Err(RecvTimeoutError::Timeout) => 0,
            Err(RecvTimeoutError::Disconnected) => open,
        }
    }

    /// Ends every process of the call's job and waits [`KILL_WAIT`] for its own, then gives its `open` readers [`DRAIN`]
    /// to end, and stops waiting on them: a process outside the job may still hold their pipes. What went wrong, if
    /// anything.
    fn end(&self, open: usize) -> Option<String> {
        let shown = self.shown;
        if let Err(error) = self.job.terminate(KILLED.cast_unsigned()) {
            return Some(format!("cannot end {shown}: {error}"));
        }
        match self.running.wait(KILL_WAIT) {
            Ok(Some(_)) => {}
            Ok(None) => return Some(format!("{shown} did not end within {} s of its kill", KILL_WAIT.as_secs())),
            Err(error) => return Some(format!("cannot wait for {shown}: {error}")),
        }
        let deadline = Instant::now() + DRAIN;
        let mut open = open;
        while open > 0 {
            match self.readers.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => open -= 1,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        None
    }
}

fn cannot_wait(shown: &str, error: &io::Error) -> End {
    End::Failed(format!("cannot wait for {shown}: {error}"))
}

/// How a call starts: whether its job lets a process in it start one outside it, which only the ssh of a transport
/// whose `gate box` runs on this machine needs; and whether the console interrupt is raised after its first output
/// byte.
#[derive(Clone, Copy)]
struct Starts {
    breakaway: bool,
    raise: bool,
}

/// An anonymous job of its own for a call, which ends every process in it when it is terminated or dropped, and which
/// no other process can open by a name. With `breakaway`, a process in it may start one outside it, as a box run on
/// this machine starts its command; without, it refuses that, so no ssh or git started in it can leave it and outlive
/// a kill.
fn call_job(breakaway: bool) -> io::Result<Job> {
    if breakaway {
        Job::anonymous_kill_on_close_breakaway()
    } else {
        Job::anonymous_kill_on_close()
    }
}

/// The console a call watched as `watch` is created on: this process's, whose Ctrl+C reaches it too, for every call a
/// Ctrl+C ends; a hidden one of its own for a bounded call, the `stop` or the `boot`, so that a second Ctrl+C cannot
/// end it. ssh in batch mode reads nothing from a console.
fn console_of(watch: Watch) -> PipedConsole {
    if watch.interruptible() {
        PipedConsole::Attached
    } else {
        PipedConsole::Hidden
    }
}

/// The launch of `command`, created suspended on `console`: its program, words and working folder, and this process's
/// environment with the command's own changes.
fn piped_launch(command: &Command, console: PipedConsole) -> io::Result<PipedLaunch> {
    let words: Vec<Vec<u16>> = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|word| word.encode_wide().collect())
        .collect();
    let directory = match command.get_current_dir() {
        Some(folder) => folder.to_path_buf(),
        None => std::env::current_dir()?,
    };
    Ok(PipedLaunch {
        application: PathBuf::from(command.get_program()),
        command_line: gate_args::command_line(&words),
        directory,
        environment: EnvironmentBlock::new(environment_of(command))?,
        console,
    })
}

/// This process's environment with `command`'s changes: each variable it sets replaces the one of its name, and each
/// it removes is gone, names compared as Windows compares them.
fn environment_of(command: &Command) -> Vec<(OsString, OsString)> {
    let mut variables: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (name, value) in command.get_envs() {
        variables.retain(|(kept, _)| !same_variable_name(kept, name));
        variables.extend(value.map(|value| (name.to_owned(), value.to_owned())));
    }
    variables
}

/// What a bounded call, `what` it does, that ended as `called`, limited by `bound`, says: nothing when it exited 0.
fn bounded_line(what: &str, called: &Called, bound: Duration) -> Option<String> {
    match (called.cut, called.status) {
        (Some(Cut::Late(_)), _) => Some(format!("gate: {what} did not finish in {} s", watchdog::whole_seconds(bound))),
        (Some(cut), _) => Some(format!("gate: {what} failed: {}", cut.reason("it"))),
        (None, 0) => None,
        (None, status) => Some(format!("gate: {what} failed (status {status})")),
    }
}

/// Waits out `pause` in short steps, ending it when the console `interrupt` is raised.
fn paused(pause: Duration, interrupt: Option<Interrupt>) -> Result<(), End> {
    let deadline = Instant::now() + pause;
    loop {
        if interrupt.is_some_and(Interrupt::is_raised) {
            return Err(End::Interrupted);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(PAUSE_STEP));
    }
}

/// A git failure as the attempt's end: an interrupt is the run's interrupt, any other a client failure.
fn git_end(error: GitError) -> End {
    match error {
        GitError::Interrupted => End::Interrupted,
        GitError::Failed(why) => End::Failed(why),
    }
}

/// What one call's two streams said so far, kept for the call's end, each line cut to the cap: whether it wrote on
/// standard output, its first and last standard error lines, its last lines, and the first problem reading them.
#[derive(Default)]
struct Heard {
    kept: Kept,
    output_seen: bool,
    first_error: Option<String>,
    last_error: Option<String>,
    problem: Option<String>,
}

impl Heard {
    /// Keeps one `line` of standard error when `error`, else of standard output, cut to the cap: a heartbeat only
    /// marks the first error line.
    fn record(&mut self, line: &str, error: bool) {
        let line = watchdog::truncated(line);
        if error {
            self.first_error.get_or_insert_with(|| line.to_owned());
        } else {
            self.output_seen = true;
        }
        if error && is_heartbeat(line) {
            return;
        }
        if error {
            self.last_error = Some(line.to_owned());
        }
        self.kept.push(line);
    }

    /// Keeps `problem` unless one came first.
    fn failed(&mut self, problem: String) {
        self.problem.get_or_insert(problem);
    }
}

/// When a call's streams last sent a byte, whether they sent any, and whether a capped one sent a line past the cap:
/// what the watcher reads, with no lock and no wait on a reader's writes.
struct Progress {
    begun: Instant,
    /// Milliseconds from `begun` to the last byte.
    last_byte: AtomicU64,
    any: AtomicBool,
    too_long: AtomicBool,
}

impl Progress {
    fn new() -> Progress {
        Progress {
            begun: Instant::now(),
            last_byte: AtomicU64::new(0),
            any: AtomicBool::new(false),
            too_long: AtomicBool::new(false),
        }
    }

    /// Marks a byte that came now.
    fn mark(&self) {
        let millis = u64::try_from(self.begun.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_byte.store(millis, Ordering::Relaxed);
        self.any.store(true, Ordering::Relaxed);
    }

    /// How long nothing has come, or since the call started when nothing came yet.
    fn idle(&self) -> Duration {
        let last = Duration::from_millis(self.last_byte.load(Ordering::Relaxed));
        self.begun.elapsed().saturating_sub(last)
    }

    fn heard_any(&self) -> bool {
        self.any.load(Ordering::Relaxed)
    }

    fn too_long(&self) -> bool {
        self.too_long.load(Ordering::Relaxed)
    }

    fn cut_too_long(&self) {
        self.too_long.store(true, Ordering::Relaxed);
    }
}

/// What a call's two readers share with each other and its watcher: what they heard, behind a lock no write is made
/// under, and their progress.
struct Shared {
    heard: Mutex<Heard>,
    progress: Progress,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            heard: Mutex::new(Heard::default()),
            progress: Progress::new(),
        }
    }
}

/// Writes `text`, line end and all, to `stream` in one write, and flushes it.
fn say_on(mut stream: impl Write, text: &str) -> io::Result<()> {
    stream.write_all(text.as_bytes())?;
    stream.flush()
}

fn held(heard: &Mutex<Heard>) -> MutexGuard<'_, Heard> {
    heard.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a reader takes a call's lines: where they go, and whether a line past the cap stops it.
#[derive(Clone, Copy)]
struct Reading {
    echo: Echo,
    capped: bool,
}

/// One stream's reader: standard error when `error`; how it takes the lines; its own handle to the log, `None` when
/// the lines stay out of it; the line read so far; and, while a line past the cap is passed on in pieces, that line's
/// first 64 KiB as text.
struct Reader {
    error: bool,
    reading: Reading,
    log: Option<File>,
    /// At most [`LINE_CAP`] bytes and a carriage return; while a long line is passed on, only the start of a
    /// character the last piece cut.
    pending: Vec<u8>,
    head: Option<String>,
}

impl Reader {
    fn new(error: bool, reading: Reading, log: Option<File>) -> Reader {
        Reader {
            error,
            reading,
            log,
            pending: Vec::new(),
            head: None,
        }
    }

    /// Takes `part`, the stream's next bytes up to its next line end and without it, which ends the line when `ended`.
    /// A line past the cap stops a capped reading, `false`; an uncapped one passes it on in pieces as they come.
    fn take(&mut self, part: &[u8], ended: bool, shared: &Shared) -> bool {
        let Some(part) = self.after_updates(part) else {
            shared.progress.cut_too_long();
            return false;
        };
        if self.head.is_none() && self.pending.len() + part.len() <= LINE_CAP + 1 {
            self.pending.extend_from_slice(part);
            if ended {
                let bytes = std::mem::take(&mut self.pending);
                return self.line_ended(&bytes, shared);
            }
            return true;
        }
        if self.head.is_none() && self.reading.capped {
            shared.progress.cut_too_long();
            return false;
        }
        let mut piece = std::mem::take(&mut self.pending);
        piece.extend_from_slice(part);
        if self.head.is_none() {
            let start = String::from_utf8_lossy(piece.get(..LINE_CAP).unwrap_or(&piece));
            self.head = Some(watchdog::truncated(&start).to_owned());
        }
        self.streamed(piece, ended, shared);
        true
    }

    /// `part` less what a git call's reading drops of its progress updates: at a carriage return that more bytes
    /// follow, the line read so far and the bytes up to it go, as [`last_update`] drops them, so only the last
    /// update counts against the cap. A carriage return at the end of `part` may start a line end, and waits. `None`
    /// when the update a carriage return ends is past the cap, however the pipe split it.
    fn after_updates<'p>(&mut self, part: &'p [u8]) -> Option<&'p [u8]> {
        if !matches!(self.reading.echo, Echo::Progress | Echo::Unlogged) || part.is_empty() {
            return Some(part);
        }
        // A carriage return that ended the last part ended its update too, so none of the line read so far is carried.
        let carried = if self.pending.last() == Some(&b'\r') { 0 } else { self.pending.len() };
        let followed = part.get(..part.len() - 1).unwrap_or_default();
        if let Some(at) = followed.iter().rposition(|byte| *byte == b'\r') {
            // The updates the part's carriage returns end, the first of them the one the line read so far started.
            let mut ended = followed.get(..at).unwrap_or_default().split(|byte| *byte == b'\r').map(<[u8]>::len);
            let first = ended.next().unwrap_or_default();
            if carried + first > LINE_CAP || ended.any(|length| length > LINE_CAP) {
                return None;
            }
            self.pending.clear();
            return part.get(at + 1..);
        }
        if carried == 0 {
            self.pending.clear();
        }
        Some(part)
    }

    /// The stream ended: the line read so far is its last.
    fn finish(&mut self, shared: &Shared) {
        let bytes = std::mem::take(&mut self.pending);
        if self.head.is_some() {
            self.streamed(bytes, true, shared);
        } else if !bytes.is_empty() {
            self.line_ended(&bytes, shared);
        }
    }

    /// A whole line, `bytes`, came: it is passed on and kept, unless it is past the cap of a capped reading, which
    /// stops, `false`.
    fn line_ended(&mut self, bytes: &[u8], shared: &Shared) -> bool {
        if self.reading.capped && watchdog::too_long(bytes) {
            shared.progress.cut_too_long();
            return false;
        }
        let line = line_of(bytes, self.reading.echo);
        if !(self.error && is_heartbeat(&line)) {
            self.passed(&line, true, shared);
        }
        held(&shared.heard).record(&line, self.error);
        true
    }

    /// Passes on `piece`, the next of a line past the cap, which ends it when `ended`; else the start of a character
    /// it cuts waits for the next piece. The line's head is kept once it ends.
    fn streamed(&mut self, mut piece: Vec<u8>, ended: bool, shared: &Shared) {
        if ended {
            self.passed(&line_of(&piece, self.reading.echo), true, shared);
            if let Some(head) = self.head.take() {
                held(&shared.heard).record(&head, self.error);
            }
            return;
        }
        let mut whole = watchdog::complete(&piece);
        // A carriage return may start the line end the next piece finishes: it waits for it.
        if whole > 0 && piece.get(whole - 1) == Some(&b'\r') {
            whole -= 1;
        }
        self.pending = piece.split_off(whole);
        self.passed(&String::from_utf8_lossy(&piece), false, shared);
    }

    /// Writes `text`, with a line end when `end`, to the log unless the reading keeps it out, and on its own stream
    /// when it echoes to the console; the first failure to is kept.
    fn passed(&mut self, text: &str, end: bool, shared: &Shared) {
        let mut said = String::with_capacity(text.len() + 1);
        said.push_str(text);
        if end {
            said.push('\n');
        }
        if let Some(log) = &mut self.log
            && let Err(error) = log.write_all(said.as_bytes())
        {
            held(&shared.heard).failed(format!("cannot write to the log: {error}"));
        }
        if self.reading.echo == Echo::Console {
            let written = if self.error {
                say_on(io::stderr().lock(), &said)
            } else {
                say_on(io::stdout().lock(), &said)
            };
            if let Err(error) = written {
                held(&shared.heard).failed(format!("cannot say a line: {error}"));
            }
        }
    }
}

/// Reads `stream` on a thread of its own as [`relay`] does, then says so on `done`.
fn relayed(stream: impl Read + Send + 'static, reader: Reader, shared: &Arc<Shared>, done: Sender<()>) {
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        relay(stream, reader, &shared);
        // A killed call's watcher stops listening once its drain is over: a reader that ends later has no one to tell.
        done.send(()).ok();
    });
}

/// Reads `stream` line by line through `reader` until it ends, or a capped reading meets a line past the cap, each
/// byte that comes marking the call's progress; each line without its line end, read as UTF-8 with U+FFFD for what is
/// not. Neither a write nor a wait for one is made under the lock the watcher could take.
fn relay(stream: impl Read, mut reader: Reader, shared: &Shared) {
    let mut buffered = BufReader::new(stream);
    loop {
        let chunk = match buffered.fill_buf() {
            Ok([]) => {
                reader.finish(shared);
                return;
            }
            Ok(chunk) => chunk,
            Err(failure) => {
                held(&shared.heard).failed(format!("cannot read a call's output: {failure}"));
                return;
            }
        };
        shared.progress.mark();
        let (part, used, ended) = match chunk.iter().position(|byte| *byte == b'\n') {
            Some(end) => (chunk.get(..end).unwrap_or(chunk), end + 1, true),
            None => (chunk, chunk.len(), false),
        };
        let going = reader.take(part, ended, shared);
        buffered.consume(used);
        if !going {
            return;
        }
    }
}

/// The line `bytes` holds, without its line end, as UTF-8 with U+FFFD for what is not; a progress line as its last
/// update shows it.
fn line_of(bytes: &[u8], echo: Echo) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text.strip_suffix('\n').unwrap_or(&text);
    let line = line.strip_suffix('\r').unwrap_or(line);
    let line = if echo == Echo::Progress { last_update(line) } else { line };
    line.to_owned()
}

/// Whether `line` is exactly the box's heartbeat: its prefix and one or more digits.
fn is_heartbeat(line: &str) -> bool {
    line.strip_prefix(HEARTBEAT)
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The builder's version, from the last `gate: remote version <v>` line of its preflight; `None` when it said none.
fn their_version(lines: &[String]) -> Option<&str> {
    lines.iter().rev().find_map(|line| line.strip_prefix(VERSION)).map(str::trim)
}

/// The builder's boot time, from the last `gate: remote boot <time>` line of a preflight or a `boot` call; `None` when
/// it said none, or one with no time. A time that is no timestamp is kept as it is, for [`after_loss`] to refuse.
fn boot_time(lines: &[String]) -> Option<&str> {
    lines
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix(BOOT))
        .map(str::trim)
        .filter(|time| !time.is_empty())
}

/// What a lost connection turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum Loss<'n> {
    /// The builder runs the same boot, or its boot time is unknown: only the connection was lost.
    Lost,
    /// The builder booted again, at this time.
    Restarted(&'n str),
    /// The builder gave no boot time to compare by.
    Unreachable,
}

/// What a lost connection was, from the builder's boot time its preflight said, `before`, and the one it gave after
/// the loss, `now`: a restart when both are timestamps and `now` is more than 30 s later, the slack the box's reading
/// of its boot time needs; no answer when it gave none, or either is no timestamp; else, and with no time `before`, a
/// lost connection. An earlier boot is no restart: a clock that stepped back is not one.
fn after_loss<'n>(before: Option<&str>, now: Option<&'n str>) -> Loss<'n> {
    match (before, now) {
        (None, _) => Loss::Lost,
        (Some(_), None) => Loss::Unreachable,
        (Some(before), Some(now)) => match (before.parse::<Timestamp>(), now.parse::<Timestamp>()) {
            (Ok(was), Ok(is)) if is.duration_since(was) > BOOT_SLACK => Loss::Restarted(now),
            (Ok(_), Ok(_)) => Loss::Lost,
            _ => Loss::Unreachable,
        },
    }
}

/// The line refusing a builder `host` whose version, `theirs`, is not `ours`; `None` when they are the same. A
/// builder that said no version runs one unknown; one that did is shown by its first 4 KiB, escaped.
fn version_refusal(host: &str, theirs: Option<&str>, ours: &str) -> Option<String> {
    (theirs != Some(ours)).then(|| {
        let shown = theirs.map_or_else(|| "unknown".to_owned(), |theirs| escaped(first_bytes(theirs, git::FAILURE_BYTES)));
        format!("gate: not routed: {host} runs gate {shown}, this PC {ours}")
    })
}

/// `text` cut to its first `most` bytes, at a character's boundary.
fn first_bytes(text: &str, most: usize) -> &str {
    let mut end = text.len().min(most);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

/// The reason of the box's own refusal line for the work folder `key` among `lines`; `None` when there is none, as
/// for a command that exits 75 itself.
fn refusal(key: &str, lines: &[String]) -> Option<FallbackReason> {
    let busy = busy_line(key);
    let tools = format!("gate: remote work folder {key} could not restore its dotnet tools");
    let has = |wanted: &str| lines.iter().any(|line| line.trim() == wanted);
    if has(&busy) {
        Some(FallbackReason::Busy)
    } else if has(&tools) {
        Some(FallbackReason::Tools)
    } else {
        None
    }
}

/// The size git's push reports writing, from the last `Writing objects: 100% (…), <size>` it said; `up to date` when
/// it wrote nothing. A line the builder sent, `remote: …`, is passed over, and a value that is not a size, a number and
/// a unit, says nothing of the size.
fn push_size(lines: &[String]) -> String {
    const WRITING: &str = "Writing objects: 100% (";
    lines
        .iter()
        .rev()
        .filter(|line| !line.starts_with("remote:"))
        .find_map(|line| {
            let rest = &line[line.rfind(WRITING)? + WRITING.len()..];
            let size = rest.split_once("), ")?.1.split(['|', ',', '\r']).next()?.trim();
            SIZE.is_match(size).then(|| size.to_owned())
        })
        .unwrap_or_else(|| "up to date".to_owned())
}

/// A size as git says one: a number, maybe with a fraction, then `byte`, `bytes` or a binary unit, maybe per second.
#[expect(clippy::expect_used, reason = "the pattern is a constant, compiled by the unit test of the push size")]
static SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+(\.[0-9]+)? ?([KMGT]?i?B|bytes?)(/s)?$").expect("the size pattern compiles"));

/// `elapsed` in seconds to one decimal, rounded half to even as PowerShell's `[math]::Round` rounds, with no `.0`.
fn seconds(elapsed: Duration) -> String {
    let tenths = (elapsed.as_secs_f64() * 10.0).round_ties_even() / 10.0;
    format!("{tenths}")
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose interrupt or transport cannot be made must fail loudly")]
mod tests {
    use std::path::Path;
    use std::sync::LazyLock;
    use std::time::{Duration, Instant};

    use gate_state::{FallbackReason, StateRoot};
    use gate_win32::{Interrupt, PipedConsole};

    use super::{
        Attempt, CallEnd, Called, Cut, Echo, End, GIT_SSH_FAILED, Heard, LINE_CAP, Limits, Loss, Reader, Reading, SSH_FAILED, Shared, Unfinished,
        Watch, after_loss, boot_time, called_of, console_of, held, is_heartbeat, last_update, paused, push_size, refusal, relay, seconds,
        their_version, version_refusal,
    };
    use crate::remote::git::GIT_LIMIT;
    use crate::remote::payload::Location;
    use crate::remote::snapshot::Snapshot;
    use crate::remote::transport::Local;
    use crate::remote::watchdog::Kill;
    use crate::remote::{RemoteCall, busy_line};
    use crate::run::CONTROL_C_EXIT;

    /// A state root no gate uses; no test here takes its key's lock.
    static STATE: LazyLock<StateRoot> = LazyLock::new(|| StateRoot::new(r"C:\gate-client-tests", None));
    /// A pause no test could wait out: one that ends, ends on the interrupt.
    const LONG: Duration = Duration::from_secs(600);

    /// The console interrupt, raised: it is never lowered, so every test here that reads it sees it raised.
    fn raised() -> Interrupt {
        let interrupt = Interrupt::install().expect("the console handler is installed");
        interrupt.raise();
        interrupt
    }

    /// A call whose interrupt is raised, logging to `log`, which no test here writes.
    fn call(log: &Path) -> RemoteCall<'_> {
        RemoteCall {
            host: "builder",
            key: "app-1a2b3c4d",
            root: r"C:\no-such-worktree",
            repo: "app",
            subfolder: "",
            words: &[],
            outputs: &[],
            log,
            timeout: 60,
            stall: 60,
            tail: 0,
            no_markers: false,
            interrupt: Some(raised()),
            state: &STATE,
        }
    }

    fn attempt<'c, 'a>(call: &'c RemoteCall<'a>, transport: &'c Local) -> Attempt<'c, 'a> {
        Attempt {
            call,
            transport,
            limit: GIT_LIMIT,
            pause: LONG,
            limits: Limits::for_tests(),
            one_call: None,
            interrupt_at: None,
            lines: Vec::new(),
            problem: None,
        }
    }

    /// A transport whose every call is made through a stand-in that does not exist, so a call made by mistake fails to
    /// start rather than running anything.
    fn nowhere() -> Local {
        Local::new(r"local:C:\builder", Some(Path::new(r"C:\gate-client-tests-no-such-folder\fake-ssh.exe"))).expect("a local transport")
    }

    /// The location of a run call no test makes.
    fn location() -> Location {
        Location::under(r"C:\builder", "app", "app-1a2b3c4d", "0123456789ab")
    }

    /// A call that ended with `status` and wrote `first_error`, its first and only line, on standard error.
    fn ended_with(status: i32, first_error: &str) -> Called {
        Called {
            status,
            cut: None,
            problem: None,
            output_seen: false,
            first_error: Some(first_error.to_owned()),
            last_error: Some(first_error.to_owned()),
            lines: vec![first_error.to_owned()],
        }
    }

    /// Whether the attempt said it stopped, or tried to stop, the command.
    fn stopped(attempt: &Attempt<'_, '_>) -> bool {
        attempt.lines.iter().any(|line| line.starts_with("gate: stopping the command on builder"))
    }

    #[test]
    fn a_ctrl_c_after_a_run_call_that_failed_before_the_banner_stops_nothing() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);

        let ended = attempt.run_call_made(&location(), Limits::for_tests(), |_| {
            Ok(ended_with(SSH_FAILED, "Connection timed out during banner exchange"))
        });

        assert!(matches!(ended, Err(End::Interrupted)), "{ended:?}");
        assert!(!stopped(&attempt), "the command never started: {:?}", attempt.lines);
    }

    #[test]
    fn a_ctrl_c_that_ends_a_running_run_call_stops_the_command() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);

        let ended = attempt.run_call_made(&location(), Limits::for_tests(), |_| Err(End::Interrupted));

        assert!(matches!(ended, Err(End::Interrupted)), "{ended:?}");
        assert!(stopped(&attempt), "the stop is made: {:?}", attempt.lines);
    }

    #[test]
    fn a_ctrl_c_raised_by_the_time_the_run_call_returned_its_status_stops_the_command() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);

        let ended = attempt.run_call_made(&location(), Limits::for_tests(), |_| Ok(ended_with(0, "the command's line")));

        assert!(matches!(ended, Err(End::Interrupted)), "an interrupt, not the status: {ended:?}");
        assert!(stopped(&attempt), "the stop is made: {:?}", attempt.lines);
    }

    #[test]
    fn a_watchdog_cut_stays_a_kill_whatever_else_went_wrong() {
        let mut heard = Heard::default();
        heard.failed("cannot write to the log: the disk is full".to_owned());
        let cut = Cut::Watchdog(Kill::Stalled);

        let called =
            called_of(CallEnd::Cut(cut, Some("cannot end ssh.exe: access denied".to_owned())), false, heard).expect("a kill, not the run's failure");

        assert_eq!((called.status, called.cut), (124, Some(cut)));
        assert_eq!(
            called.problem.as_deref(),
            Some("cannot write to the log: the disk is full; cannot end ssh.exe: access denied"),
            "both problems are kept as the kill's note"
        );
    }

    #[test]
    fn a_ctrl_c_whose_kill_failed_stays_an_interrupt_with_a_note() {
        let mut heard = Heard::default();
        heard.failed("cannot say a line: the pipe is closed".to_owned());
        let ending = Some("ssh.exe did not end within 30 s of its kill".to_owned());

        let ended = called_of(CallEnd::Interrupted(ending), false, heard);

        assert!(
            matches!(
                &ended,
                Err(Unfinished::Interrupted(Some(note)))
                    if note == "cannot say a line: the pipe is closed; ssh.exe did not end within 30 s of its kill"
            ),
            "an interrupt, so the caller stops the command: {:?}",
            ended.err()
        );
    }

    #[test]
    fn a_git_call_s_progress_updates_count_against_the_cap_one_at_a_time() {
        let updates = (0..5000).fold(String::new(), |text, step| {
            text + "Receiving objects: " + &(step % 100).to_string() + "%\r"
        });
        assert!(updates.len() > LINE_CAP, "the updates together pass the cap");
        let input = format!("{updates}Receiving objects: 100%, done.\r\n");
        for echo in [Echo::Progress, Echo::Unlogged] {
            let shared = Shared::new();
            let reading = Reading { echo, capped: true };

            relay(input.as_bytes(), Reader::new(true, reading, None), &shared);

            assert!(!shared.progress.too_long(), "no one update passes the cap");
            let lines = std::mem::take(&mut *held(&shared.heard)).kept.into_lines();
            assert_eq!(lines, ["Receiving objects: 100%, done."], "the last update is kept");
        }
    }

    #[test]
    fn a_reader_s_problem_fails_a_call_that_ended_on_its_own() {
        let mut heard = Heard::default();
        heard.failed("cannot say a line: the pipe is closed".to_owned());
        let failed = called_of(CallEnd::Exited(0), false, heard);
        assert!(
            matches!(&failed, Err(Unfinished::Failed(why)) if why == "cannot say a line: the pipe is closed"),
            "{:?}",
            failed.err()
        );
        let cut = called_of(CallEnd::Exited(0), true, Heard::default()).expect("a cut call");
        assert_eq!(
            (cut.status, cut.cut),
            (124, Some(Cut::TooLong)),
            "a line past the cap seen after the exit"
        );
    }

    #[test]
    fn a_fetch_ends_at_the_stall_window_or_the_run_s_ceiling_whichever_comes_first() {
        let second = Duration::from_secs(1);
        let started = Instant::now();
        let limits = Limits {
            started,
            stall: 10 * second,
            ceiling: 60 * second,
            bound: 30 * second,
        };
        let begun = started + 40 * second;
        let fetch = Watch::Fetch(limits);
        assert_eq!(fetch.cut(started + 59 * second, begun, 9 * second, false), None, "neither yet");
        assert_eq!(
            fetch.cut(started + 50 * second, begun, 10 * second, false),
            Some(Cut::Silent(10 * second)),
            "silent for the stall window"
        );
        assert_eq!(
            fetch.cut(started + 60 * second, begun, second, false),
            Some(Cut::Ceiling),
            "a live fetch at the run's ceiling"
        );
        assert_eq!(fetch.cut(started + 41 * second, begun, Duration::ZERO, true), Some(Cut::TooLong));
        assert_eq!(
            Watch::Run(limits).cut(started + 60 * second, begun, second, false),
            Some(Cut::Watchdog(Kill::TimedOut))
        );
        assert_eq!(
            Watch::Deadline(5 * second).cut(begun + 5 * second, begun, Duration::ZERO, false),
            Some(Cut::Late(5 * second)),
            "a deadline counts from the call's own start"
        );
        assert_eq!(Watch::Deadline(5 * second).cut(begun + 4 * second, begun, Duration::ZERO, false), None);
    }

    /// What a stream of `input` gives a reader of standard error that keeps the lines and caps them when `capped`, its
    /// pieces written to the file at `log` when one is given.
    fn read_through(input: &[u8], capped: bool, log: Option<&Path>) -> Shared {
        let shared = Shared::new();
        let file = log.map(|path| std::fs::File::create(path).expect("the log can be made"));
        let reading = Reading { echo: Echo::LogOnly, capped };
        relay(input, Reader::new(true, reading, file), &shared);
        shared
    }

    #[test]
    fn a_capped_reading_stops_at_a_line_past_64_kib_and_keeps_none_of_it() {
        let input = [vec![b'x'; LINE_CAP + 1].as_slice(), b"\nafter\n"].concat();
        let shared = read_through(&input, true, None);
        assert!(shared.progress.too_long(), "the line is past the cap");
        let heard = std::mem::take(&mut *held(&shared.heard));
        assert!(heard.kept.into_lines().is_empty(), "nothing of it, nor after it, is kept");
        assert_eq!(heard.first_error, None);

        let at_cap = [vec![b'x'; LINE_CAP].as_slice(), b"\r\n"].concat();
        let shared = read_through(&at_cap, true, None);
        assert!(!shared.progress.too_long(), "exactly the cap, and a line end");
        let lines = std::mem::take(&mut *held(&shared.heard)).kept.into_lines();
        assert_eq!(lines.iter().map(String::len).collect::<Vec<_>>(), [LINE_CAP]);
    }

    #[test]
    fn an_uncapped_reading_passes_a_line_past_64_kib_on_whole_and_keeps_its_first_64_kib() {
        // 9 reads of 8 KiB end inside the two-byte character, whose second byte comes with the next read.
        let line = format!("{}\u{e9}{}", "x".repeat(9 * 8192 - 1), "y".repeat(10));
        let input = format!("{line}\nnext\n");
        let log = std::env::temp_dir().join(format!("gate-client-long-line-{}.log", std::process::id()));

        let shared = read_through(input.as_bytes(), false, Some(&log));

        let written = std::fs::read(&log).expect("the log was written");
        std::fs::remove_file(&log).expect("the log can be removed");
        assert!(written == input.as_bytes(), "the log holds every byte, the character whole");
        assert!(!shared.progress.too_long(), "an uncapped reading is never cut");
        let heard = std::mem::take(&mut *held(&shared.heard));
        assert_eq!(heard.first_error.as_deref().map(str::len), Some(LINE_CAP), "the first line's head");
        assert_eq!(heard.last_error.as_deref(), Some("next"));
        let lines = heard.kept.into_lines();
        assert_eq!(lines.iter().map(String::len).collect::<Vec<_>>(), [LINE_CAP, 4]);
    }

    fn unwritten_log() -> std::path::PathBuf {
        std::env::temp_dir().join("gate-client-tests-no-such-folder").join("out.log")
    }

    #[test]
    fn a_ctrl_c_ends_the_retry_pause_at_once() {
        assert!(matches!(paused(LONG, Some(raised())), Err(End::Interrupted)));
        assert!(paused(Duration::from_millis(1), None).is_ok(), "no interrupt: the pause runs out");
    }

    #[test]
    fn a_ctrl_c_after_a_banner_drop_makes_no_retry() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let mut attempt = attempt(&call, &transport);
        let mut made = 0;

        let ended = attempt.with_retry(SSH_FAILED, |_| {
            made += 1;
            Ok(ended_with(SSH_FAILED, "Connection timed out during banner exchange"))
        });

        assert!(matches!(ended, Err(End::Interrupted)));
        assert_eq!(made, 1, "the call is not made again");
        assert!(!log.exists(), "no retry line is said");
    }

    #[test]
    fn a_ctrl_c_before_the_preflight_the_push_or_the_run_call_ends_the_attempt_as_an_interrupt() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let mut attempt = attempt(&call, &transport);
        let snapshot = Snapshot {
            commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            bridge: None,
        };
        let location = Location::under(r"C:\builder", "app", "app-1a2b3c4d", "0123456789ab");

        assert!(matches!(attempt.preflight(&location), Err(End::Interrupted)), "the preflight");
        assert!(matches!(attempt.push(&snapshot, false), Err(End::Interrupted)), "the push");
        assert!(matches!(attempt.run_call("line", &location), Err(End::Interrupted)), "the run call");
        let outcome = attempt.outcome(End::Interrupted);
        assert_eq!(outcome.status, i64::from(CONTROL_C_EXIT.cast_signed()), "{outcome:?}");
        assert!(outcome.lines.is_empty(), "an interrupt says no failure line: {outcome:?}");
        assert_eq!(outcome.fallback(), None, "an interrupt is no fallback");
        assert!(!log.exists(), "nothing is said");
    }

    #[test]
    fn a_sync_back_git_call_dropped_before_the_banner_is_retried_once() {
        // The local transport runs git against a bare repository path, never through the ssh stand-in, so the only way
        // to reach the sync-back's retry is the decision itself, as a call that failed before the banner.
        let log = std::env::temp_dir().join(format!("gate-client-sync-back-retry-{}.log", std::process::id()));
        let mut given = call(&log);
        given.interrupt = None;
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let mut attempt = attempt(&given, &transport);
        attempt.pause = Duration::ZERO;
        let mut made = 0;

        let retried = attempt.with_retry(GIT_SSH_FAILED, |_| {
            made += 1;
            Ok(ended_with(
                GIT_SSH_FAILED,
                "kex_exchange_identification: Connection closed by remote host",
            ))
        });

        let (called, again) = retried.expect("a call that ends without an interrupt");
        assert_eq!(made, 2, "the git call is made once more");
        assert!(again, "the retry failed before the banner too");
        assert_eq!(called.status, GIT_SSH_FAILED);
        let said = std::fs::read_to_string(&log).expect("the retry line was written");
        let retry_line = "gate: builder did not answer the ssh banner; trying once more";
        assert_eq!(said.lines().filter(|line| *line == retry_line).count(), 1, "{said}");
        std::fs::remove_file(&log).expect("the log can be removed");
    }

    #[test]
    fn the_box_s_busy_line_is_the_one_its_refusal_is_read_from() {
        assert_eq!(busy_line("app-1a2b3c4d"), "gate: remote work folder app-1a2b3c4d is busy");
        assert_eq!(refusal("app-1a2b3c4d", &lines(&[&busy_line("app-1a2b3c4d")])), Some(FallbackReason::Busy));
    }

    #[test]
    fn a_progress_line_is_what_its_last_update_shows() {
        assert_eq!(
            last_update("Writing objects:  50% (1/2)\rWriting objects: 100% (2/2), 1 KiB | 1 KiB/s, done."),
            "Writing objects: 100% (2/2), 1 KiB | 1 KiB/s, done."
        );
        assert_eq!(last_update("To C:\\builder\\app.git"), "To C:\\builder\\app.git", "a line with no update");
    }

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    #[test]
    fn only_the_box_s_own_line_makes_a_75_a_refusal() {
        let key = "app-1a2b3c4d";
        assert_eq!(
            refusal(key, &lines(&["out", "gate: remote work folder app-1a2b3c4d is busy"])),
            Some(FallbackReason::Busy)
        );
        assert_eq!(
            refusal(
                key,
                &lines(&["gate: remote work folder app-1a2b3c4d could not restore its dotnet tools  "])
            ),
            Some(FallbackReason::Tools)
        );
        assert_eq!(refusal(key, &lines(&["the command exits 75 itself"])), None, "a command's own 75");
        assert_eq!(
            refusal(key, &lines(&["gate: remote work folder other-00000000 is busy"])),
            None,
            "another work folder's line"
        );
        assert_eq!(
            refusal(key, &lines(&["x gate: remote work folder app-1a2b3c4d is busy"])),
            None,
            "the whole line"
        );
    }

    #[test]
    fn a_builder_of_another_version_or_none_is_refused() {
        let preflight = lines(&[
            "gate: remote created D:\\gate-remote\\app.git",
            "gate: remote version 0.1.0",
            "gate: remote boot 2026-10-10T08:00:00.0000000Z",
        ]);
        assert_eq!(their_version(&preflight), Some("0.1.0"));
        assert_eq!(version_refusal("builder", Some("0.1.0"), "0.1.0"), None, "the same version");
        assert_eq!(
            version_refusal("builder", Some("0.2.0"), "0.1.0"),
            Some("gate: not routed: builder runs gate 0.2.0, this PC 0.1.0".to_owned())
        );
        assert_eq!(their_version(&lines(&["gate: remote boot x"])), None);
        assert_eq!(
            version_refusal("builder", None, "0.1.0"),
            Some("gate: not routed: builder runs gate unknown, this PC 0.1.0".to_owned())
        );
    }

    #[test]
    fn the_boot_time_is_the_preflight_s_last_boot_line() {
        let preflight = lines(&["gate: remote version 0.1.0", "gate: remote boot 2026-10-10T08:00:00.0000000Z"]);
        assert_eq!(boot_time(&preflight), Some("2026-10-10T08:00:00.0000000Z"));
        assert_eq!(boot_time(&lines(&["gate: remote version 0.1.0"])), None, "no boot line");
        assert_eq!(
            boot_time(&lines(&["gate: remote could not read the boot time: access denied"])),
            None,
            "the box could not read it"
        );
        assert_eq!(boot_time(&lines(&["gate: remote boot "])), None, "a boot line with no time");
        assert_eq!(boot_time(&lines(&["gate: remote boot  \t"])), None, "a boot line with only spaces");
        assert_eq!(
            boot_time(&lines(&["gate: remote boot last tuesday"])),
            Some("last tuesday"),
            "a time that is no timestamp is kept, to be compared as text"
        );
    }

    const BOOTED: &str = "2026-10-10T08:00:00.0000000Z";

    #[test]
    fn only_a_boot_more_than_30_s_later_is_a_restart() {
        let first = Some(BOOTED);
        let later = "2026-10-10T08:00:31.0000000Z";
        assert_eq!(after_loss(first, Some(later)), Loss::Restarted(later), "31 s later is another boot");
        assert_eq!(after_loss(first, Some(BOOTED)), Loss::Lost, "the same time");
        assert_eq!(
            after_loss(first, Some("2026-10-10T08:00:30.0000000Z")),
            Loss::Lost,
            "exactly 30 s later is the same boot"
        );
        assert_eq!(
            after_loss(first, Some("2026-10-10T07:59:00.0000000Z")),
            Loss::Lost,
            "a minute earlier is a clock stepped back, not a restart"
        );
    }

    #[test]
    fn a_boot_time_that_is_no_timestamp_or_none_is_no_answer() {
        assert_eq!(after_loss(Some(BOOTED), None), Loss::Unreachable, "no boot time now");
        assert_eq!(
            after_loss(Some("last tuesday"), Some("last tuesday")),
            Loss::Unreachable,
            "neither a timestamp"
        );
        assert_eq!(
            after_loss(Some(BOOTED), Some("2026-10-10T09:00:00.0000000")),
            Loss::Unreachable,
            "now is no timestamp"
        );
        assert_eq!(
            after_loss(Some("last tuesday"), Some(BOOTED)),
            Loss::Unreachable,
            "before is no timestamp"
        );
    }

    #[test]
    fn with_no_boot_time_at_the_preflight_only_the_connection_was_lost() {
        assert_eq!(after_loss(None, None), Loss::Lost);
        assert_eq!(after_loss(None, Some(BOOTED)), Loss::Lost);
    }

    #[test]
    fn a_ctrl_c_by_the_end_of_a_lost_connection_s_stop_is_an_interrupt() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);

        let ended = attempt.connection_lost(&location(), None, 0..0);

        assert!(matches!(ended, End::Interrupted), "{ended:?}");
        assert!(stopped(&attempt), "the stop is made first: {:?}", attempt.lines);
        assert!(
            attempt.lines.iter().any(|line| line == "gate: lost the connection to builder"),
            "{:?}",
            attempt.lines
        );
    }

    #[test]
    fn a_ctrl_c_by_the_end_of_a_lost_connection_s_stop_makes_no_boot_call() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);

        let ended = attempt.connection_lost(&location(), Some(BOOTED), 0..0);

        assert!(matches!(ended, End::Interrupted), "an interrupt, not a fallback: {ended:?}");
        assert!(stopped(&attempt), "the stop is made first: {:?}", attempt.lines);
        assert!(
            !attempt.lines.iter().any(|line| line.contains("for its boot time")),
            "no boot call: {:?}",
            attempt.lines
        );
        assert_eq!(attempt.lines.last().map(String::as_str), Some("gate: lost the connection to builder"));
    }

    #[test]
    fn a_fallback_after_a_lost_connection_leaves_the_run_call_s_lines_out() {
        let log = unwritten_log();
        // No Ctrl+C: one by the end of the stop would end the run there, with no fallback.
        let call = RemoteCall {
            interrupt: None,
            ..call(&log)
        };
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        attempt.lines = lines(&[
            "gate: pushed snapshot 0123abcd in 0.1 s (up to date)",
            "error CS1002: ; expected",
            "the command's line",
        ]);

        let ended = attempt.connection_lost(&location(), Some(BOOTED), 1..3);

        assert!(matches!(ended, End::Refused(FallbackReason::Unreachable)), "{ended:?}");
        assert_eq!(
            attempt.lines.first().map(String::as_str),
            Some("gate: pushed snapshot 0123abcd in 0.1 s (up to date)")
        );
        assert!(
            !attempt.lines.iter().any(|line| line.contains("CS1002") || line == "the command's line"),
            "{:?}",
            attempt.lines
        );
        assert_eq!(
            attempt.lines.last().map(String::as_str),
            Some("gate: builder unreachable after the connection was lost; the run did not finish there")
        );
    }

    #[test]
    fn a_heartbeat_is_its_prefix_and_digits_only() {
        assert!(is_heartbeat("gate-remote-heartbeat: 1250"));
        assert!(!is_heartbeat("gate-remote-heartbeat: "), "no digits");
        assert!(!is_heartbeat("gate-remote-heartbeat: 12x"), "not only digits");
        assert!(!is_heartbeat("gate-remote-heartbeat:12"), "no space");
    }

    #[test]
    fn the_push_size_is_the_last_writing_line_s_size() {
        let pushed = lines(&[
            "Enumerating objects: 3, done.",
            "Writing objects:  33% (1/3)\rWriting objects: 100% (3/3), 268 bytes | 268.00 KiB/s, done.",
            "Total 3 (delta 0), reused 0 (delta 0), pack-reused 0",
        ]);
        assert_eq!(push_size(&pushed), "268 bytes");
        assert_eq!(push_size(&lines(&["Writing objects: 100% (9/9), 1.20 MiB, done."])), "1.20 MiB");
        assert_eq!(
            push_size(&lines(&["Writing objects: 100% (3/3), 2.50 KiB | 1.25 KiB/s, done."])),
            "2.50 KiB",
            "a well-formed size is kept"
        );
        assert_eq!(push_size(&lines(&["Everything up-to-date"])), "up to date");
    }

    #[test]
    fn a_push_size_is_never_read_from_the_builder_s_lines_nor_from_text_that_is_not_a_size() {
        let link = "remote: Writing objects: 100% (1/1), \u{1b}]8;;http://evil\u{7}9 GiB\u{1b}]8;;\u{7}, done.";
        assert_eq!(push_size(&lines(&[link])), "up to date", "a line the builder sent, with an OSC 8 link");
        assert_eq!(
            push_size(&lines(&["remote: Writing objects: 100% (1/1), 9 GiB, done."])),
            "up to date",
            "a line the builder sent, with a well-formed size"
        );
        assert_eq!(
            push_size(&lines(&["Writing objects: 100% (1/1), 268 bytes, done.", link])),
            "268 bytes",
            "the builder's later line is passed over"
        );
        for bad in [
            "Writing objects: 100% (3/3), \u{1b}[31m268 KiB, done.",
            "Writing objects: 100% (3/3), lots, done.",
            "Writing objects: 100% (3/3), 1.2.3 MiB, done.",
            "Writing objects: 100% (3/3), 12 PiB, done.",
        ] {
            assert_eq!(push_size(&lines(&[bad])), "up to date", "{bad:?}");
        }
    }

    #[test]
    fn a_builder_s_version_is_shown_escaped_and_cut_to_4_kib() {
        assert_eq!(
            version_refusal("builder", Some("0.2.0\u{1b}[31m\u{202e}"), "0.1.0"),
            Some(r"gate: not routed: builder runs gate 0.2.0\u{1b}[31m\u{202e}, this PC 0.1.0".to_owned())
        );
        let long = "9".repeat(5000);
        assert_eq!(
            version_refusal("builder", Some(&long), "0.1.0"),
            Some(format!("gate: not routed: builder runs gate {}, this PC 0.1.0", "9".repeat(4096))),
            "its first 4 KiB"
        );
        let escapes = "\u{1b}".repeat(5000);
        assert_eq!(
            version_refusal("builder", Some(&escapes), "0.1.0").map(|line| line.matches(r"\u{1b}").count()),
            Some(4096),
            "cut before it is escaped"
        );
    }

    #[test]
    fn a_progress_update_past_the_cap_is_cut_however_the_pipe_splits_it() {
        let update = vec![b'x'; 70_000];
        let tail_and_return = [update.get(60_000..).unwrap_or_default(), b"\rabc".as_slice()].concat();
        let head = update.get(..60_000).unwrap_or_default();
        let together: [(&[u8], bool); 2] = [(head, false), (&tail_and_return, true)];
        let apart: [(&[u8], bool); 3] = [(head, false), (update.get(60_000..).unwrap_or_default(), false), (b"\rabc", true)];
        for echo in [Echo::Progress, Echo::Unlogged] {
            for (chunks, how) in [
                (&together[..], "the update's tail and its return in one chunk"),
                (&apart[..], "the return in the next chunk"),
            ] {
                let shared = Shared::new();
                let mut reader = Reader::new(true, Reading { echo, capped: true }, None);
                for (part, ended) in chunks {
                    if !reader.take(part, *ended, &shared) {
                        break;
                    }
                }
                assert!(shared.progress.too_long(), "{how}");
            }
        }
    }

    #[test]
    fn the_stop_call_is_made_on_a_console_of_its_own_so_a_second_ctrl_c_cannot_end_it() {
        let second = Duration::from_secs(1);
        assert_eq!(console_of(Watch::Bounded(second)), PipedConsole::Hidden, "the stop");
        for watch in [
            Watch::Run(Limits::for_tests()),
            Watch::Fetch(Limits::for_tests()),
            Watch::Deadline(second),
        ] {
            assert_eq!(console_of(watch), PipedConsole::Attached, "{watch:?}");
        }
    }

    #[test]
    fn the_push_s_seconds_have_one_decimal_rounded_half_to_even() {
        assert_eq!(seconds(Duration::from_millis(250)), "0.2");
        assert_eq!(seconds(Duration::from_millis(1340)), "1.3");
        assert_eq!(seconds(Duration::from_secs(2)), "2");
    }
}
