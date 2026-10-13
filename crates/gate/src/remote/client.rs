//! The Rust remote client, in the phases the PowerShell client runs: the snapshot; the run call built and checked
//! against the most cmd takes before anything is sent; the preflight and the builder's version; the push and the saved
//! ref; and the run call, its output passed through to the log and the console. A call that failed before the ssh
//! banner is made once more, unless the box said the run's start marker. A Ctrl+C ends the attempt between its calls,
//! during the retry pause, and during any call; one during the run call stops the command on the builder. Each call
//! goes through the [`Transport`]; every process is created suspended, put in an anonymous job of its own that ends its
//! whole tree when it is killed, and only then let run. The preflight, the push and the run call run under the run's
//! watchdog, whose clock starts with the attempt: a call that sends nothing for the stall window, or outlives the run's
//! ceiling, is killed. A kill before the run call falls back `unreachable`, as the command never started; a kill of the
//! run call stops the command and ends the run 124. A run call that exits 255 other than a failure before the banner
//! lost its connection: nothing is synced back and the command is stopped; then, when the preflight said the builder's
//! boot time, the builder is asked for it again. One that booted more than 30 s later restarted, and the run falls back
//! `rebooted`; one that gives no boot time, or when either value is no timestamp, falls back `unreachable`; else the
//! run ends 255. No call holds more than 64 KiB of a line in memory, nor keeps more than a call's last 1,000 lines,
//! totalling at most 1 MiB: a longer line ends any call but the run call, whose output is passed on whole. The run
//! takes the work-folder key's lock, [`hold_key`], before its remote slot, so a second routed run of one worktree falls
//! back `busy` at once.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::num::NonZeroU64;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::{FallbackReason, KernelObject, key_hash};
use gate_win32::{AcquireError, Interrupt, Job, NamedMutex, OwnedMutexGuard, PipedConsole, Running};
use jiff::{SignedDuration, Timestamp};
use regex::Regex;

use super::git::{self, Git, GitError, last_update};
use super::launch::{DRAIN, KILL_WAIT, KILLED, call_job, launched, piped_launch};
use super::outputs;
use super::parity;
use super::payload::{self, Location, RunCall};
use super::retry::{self, Ended, GIT_SSH_FAILED, SSH_FAILED};
use super::snapshot::{self, Snapshot};
use super::syncback;
use super::transport::{Local, Program, Transport};
use super::watchdog::{self, Cut, Kept, Kill, LINE_CAP};
use super::{FAILED, FAILED_RUN, REFUSED, Refusal, RemoteCall, RemoteClient, RemoteOutcome, refused_line, started_line};
use crate::boxside::outputs::OUTPUTS_BUDGET;
use crate::boxside::payload::escaped;
use crate::environment::Variables;
use crate::outputs::archive::sent_limit;
use crate::run::{CONTROL_C_EXIT, ERROR_FILE_NOT_FOUND, find_program};

/// The statuses of a preflight whose builder has no `gate box` that takes the run: cmd's for a program it cannot find,
/// and a gate's usage error for a payload it does not read.
const NOT_RECOGNIZED: i32 = 9009;
const USAGE: i32 = 2;
/// The most bytes a call's job may have written and still be made once more after a failure before the banner: ssh's
/// own failure writes one line, and a second fetch from a builder would start a fresh count against its cap.
const RETRY_WRITTEN: u64 = 1 << 20;
/// How often the retry pause looks at the console interrupt.
const PAUSE_STEP: Duration = Duration::from_millis(20);
/// The box's heartbeat, `gate-remote-heartbeat: <digits>` on standard error: progress, kept out of the log.
const HEARTBEAT: &str = "gate-remote-heartbeat: ";
/// The preflight's lines the client reads.
const VERSION: &str = "gate: remote version ";
const CREATED: &str = "gate: remote created ";
/// The builder's boot time, which the preflight and the `boot` call say.
const BOOT: &str = "gate: remote boot ";
/// The builder's SDK list, which the preflight says: `gate: remote sdks <v1,v2|none>`.
const SDKS: &str = "gate: remote sdks ";
/// How far apart two boot times of one builder may be and still be one boot.
const BOOT_SLACK: SignedDuration = SignedDuration::from_secs(30);
/// This gate's version, which the builder's must be.
const OURS: &str = env!("CARGO_PKG_VERSION");
/// The line said when `--test-remote-interrupt-at` is given but the console handler that would carry the interrupt
/// could not be installed.
const NO_HANDLER: &str = "gate: --test-remote-interrupt-at needs the console handler; it could not be installed";
/// The most of a call's standard output one read of a raw copy takes.
const RAW_CHUNK: usize = 64 * 1024;

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
    /// `--test-local-sdks`: this PC's SDK list, read in place of `dotnet --list-sdks`.
    local_sdks: Option<String>,
    /// The most bytes the job of the sync-back's fetch may write: `--test-remote-fetch-cap-bytes`, else 4 GiB.
    fetch_cap: u64,
    /// The most bytes the files a back commit writes into the worktree may total: `--test-remote-apply-cap-bytes`,
    /// else 4 GiB.
    apply_cap: u64,
    /// The box's outputs budget: `--test-remote-outputs-budget-bytes`, else 50 GiB. What the outputs fetch is capped
    /// at, and what the install unpacks its archive to.
    outputs_budget: u64,
    /// `--test-own-start-fails`: this gate's start, which names the sync-back's quarantine, cannot be read.
    own_start_fails: bool,
}

impl RustClient {
    /// The client of the local transport `transport` names, with the request's seams; else the line saying why the
    /// transport cannot be used, or why the stand-in `--test-remote-ssh` names cannot be found.
    pub(crate) fn local(request: &RunRequest, transport: &str) -> Result<RustClient, String> {
        let seams = &request.test_seams;
        // The transport's form is refused first, so `--test-remote-transport remote:x` reports the form and not a
        // stand-in it cannot find; the stand-in is then looked for, and the transport built with it.
        Local::new(transport, seams.remote_ssh.as_deref())?;
        let stub = seams.remote_ssh.as_deref().map(|word| stand_in(transport, word)).transpose()?;
        let local = Local::new(transport, stub.as_deref())?;
        Ok(RustClient {
            transport: Box::new(local),
            limit: git::limit(seams),
            pause: retry::pause(seams),
            bound: retry::bound(seams),
            stall_ms: seams.stall_ms,
            timeout_ms: seams.remote_timeout_ms,
            watch_call: seams.remote_watch_call.clone(),
            interrupt_at: seams.remote_interrupt_at.clone(),
            local_sdks: seams.local_sdks.clone(),
            fetch_cap: seams.remote_fetch_cap_bytes.unwrap_or(watchdog::FETCH_CAP),
            apply_cap: seams.remote_apply_cap_bytes.unwrap_or(watchdog::APPLY_CAP),
            outputs_budget: seams.remote_outputs_budget_bytes.unwrap_or(OUTPUTS_BUDGET),
            own_start_fails: seams.own_start_fails,
        })
    }
}

/// The stand-in `--test-remote-ssh` names, resolved as the gate resolves its own programs: a name with a folder part
/// against the gate's working folder, a bare name along its `PATH` alone; else the line saying why it cannot be found.
fn stand_in(transport: &str, word: &Path) -> Result<PathBuf, String> {
    let cannot = |why: String| format!("gate: cannot use the remote transport '{transport}': {why}");
    let folder = std::env::current_dir().map_err(|error| cannot(format!("cannot read the working directory: {error}")))?;
    let word = word.to_string_lossy();
    find_program(&word, &Variables::from_process(), &folder).map_err(|why| cannot(format!("cannot find --test-remote-ssh {word}: {why}")))
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
            local_sdks: self.local_sdks.as_deref(),
            fetch_cap: self.fetch_cap,
            apply_cap: self.apply_cap,
            outputs_budget: self.outputs_budget,
            own_start_fails: self.own_start_fails,
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
    /// The watchdog killed the run call, and the command was stopped, or it killed the outputs call: status 124.
    Killed,
    /// The run call's connection was lost: it exited 255, ssh's own failure status, other than before the banner. The
    /// run call returns it before anything is stopped, with where the run call's own lines are among the attempt's, so
    /// a fallback can leave them out. Nothing is synced back: status 255.
    Lost(Range<usize>),
}

/// What the preflight found: whether it created the bare repository just now, the builder's boot time, `None` when it
/// said none, and the SDKs it lists, `None` when it said no SDK line.
struct Prepared {
    created: bool,
    boot: Option<String>,
    sdks: Option<Vec<String>>,
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
    /// Killed once it sent nothing for the stall window of these limits, or at the run's ceiling, or once its job wrote
    /// more bytes than the cap when it has one: the sync-back's fetch, and the outputs call.
    Fetch(Limits, Option<u64>),
    /// Killed once it ran for this long.
    Deadline(Duration),
    /// As [`Watch::Deadline`], but a Ctrl+C does not end it: the bounded calls, the `stop`, made after one, and the
    /// `boot`.
    Bounded(Duration),
}

impl Watch {
    /// Why a call watched so, started at `begun`, is killed at `now`, nothing having come from it for `idle`, with
    /// what its readers have `raised`, and the bytes its job has `written`, `None` when they were not read; `None`
    /// while it may run on. A raised line past the line cap comes first, then a raw copy past its archive cap, then a
    /// fetch's job bytes past its cap, then its clocks.
    fn cut(self, now: Instant, begun: Instant, idle: Duration, raised: Raised, written: Option<u64>) -> Option<Cut> {
        if let Some(cut) = raised.cut() {
            return Some(cut);
        }
        if let Some(cut) = self.over(written) {
            return Some(cut);
        }
        match self {
            Watch::Run(limits) => {
                let elapsed = now.saturating_duration_since(limits.started);
                watchdog::kill(idle, elapsed, limits.stall, limits.ceiling).map(Cut::Watchdog)
            }
            Watch::Fetch(limits, _) if idle >= limits.stall => Some(Cut::Silent(limits.stall)),
            Watch::Fetch(limits, _) => (now.saturating_duration_since(limits.started) >= limits.ceiling).then_some(Cut::Ceiling),
            Watch::Deadline(limit) | Watch::Bounded(limit) => (now.saturating_duration_since(begun) >= limit).then_some(Cut::Late(limit)),
        }
    }

    /// The cut of a call watched so whose job has `written` bytes: [`Cut::TooBig`] once a fetch with a cap wrote more
    /// than it; `None` for any other call, and when the bytes were not read.
    fn over(self, written: Option<u64>) -> Option<Cut> {
        match (self, written) {
            (Watch::Fetch(_, Some(cap)), Some(bytes)) if bytes > cap => Some(Cut::TooBig(cap)),
            _ => None,
        }
    }

    /// The most bytes the job of a call watched so may write: a fetch's cap; `None` for any other call.
    fn byte_cap(self) -> Option<u64> {
        match self {
            Watch::Fetch(_, cap) => cap,
            Watch::Run(_) | Watch::Deadline(_) | Watch::Bounded(_) => None,
        }
    }

    /// How long the client waits between two looks at a call watched so.
    fn tick(self) -> Duration {
        match self {
            Watch::Run(limits) | Watch::Fetch(limits, _) => watchdog::tick(limits.stall),
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
/// it wrote but heartbeats, each cut to the cap; for a run call, whether the box said the run's start marker and the
/// refusal its tag named; and, for a call the client killed, what else went wrong meanwhile.
pub(super) struct Called {
    pub(super) status: i32,
    pub(super) cut: Option<Cut>,
    /// A reader's problem, or a failure to end the call, which a kill keeps as a note rather than as the run's failure.
    pub(super) problem: Option<String>,
    output_seen: bool,
    first_error: Option<String>,
    pub(super) last_error: Option<String>,
    pub(super) lines: Vec<String>,
    /// Its last standard error line that is the box's outputs line, however many lines came after it.
    outputs_line: Option<String>,
    /// Whether the box said the run's start marker on standard error.
    started: bool,
    /// The reason the box's refusal tag for the run named on standard error, the first when it said more than one.
    refused: Option<FallbackReason>,
    /// The bytes its job had written at the last look, for a call whose watch caps them; `None` for any other call, or
    /// when no look succeeded.
    written: Option<u64>,
}

impl Called {
    /// Whether the call's job wrote more than [`RETRY_WRITTEN`]: more than a failure before the banner writes, so a call
    /// that reached a builder, which is not made again.
    fn wrote_much(&self) -> bool {
        self.written.is_some_and(|bytes| bytes > RETRY_WRITTEN)
    }

    /// Whether the call failed before the banner, for a program that exits `fail_status` when ssh failed.
    fn before_the_banner(&self, fail_status: i32) -> bool {
        retry::before_the_banner(&Ended {
            status: self.status,
            fail_status,
            output_seen: self.output_seen,
            first_error: self.first_error.as_deref(),
            started: self.started,
        })
    }

    /// The box's refusal of the run, from its tag; none once the box said the start marker, which no refusal follows.
    ///
    /// The nonce in the tag and the marker only tells runs apart: it is no secret, as it is on the box's and ssh's
    /// command lines. That a command cannot forge a refusal rests on the box saying the marker before it creates the
    /// command's process, and on this ignoring every tag once the marker was seen, whatever order the two came in.
    fn refusal(&self) -> Option<FallbackReason> {
        self.refused.filter(|_| !self.started)
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
    /// The call, `preflight`, `run` or `outputs`, after whose first output byte the console interrupt is raised, as a
    /// Ctrl+C would; or `install`, once the install has moved its first file and before it moves a second, so its line
    /// reads `1 of {m}`: an install of a single file ends before the seam is reached.
    interrupt_at: Option<&'c str>,
    /// `--test-local-sdks`: this PC's SDK list, read in place of `dotnet --list-sdks`.
    local_sdks: Option<&'c str>,
    /// The most bytes the job of the sync-back's fetch may write.
    pub(super) fetch_cap: u64,
    /// The most bytes the files a back commit writes into the worktree may total.
    pub(super) apply_cap: u64,
    /// The box's outputs budget: the outputs fetch is capped at zstd's worst case for it, and the install unpacks its
    /// archive to it.
    pub(super) outputs_budget: u64,
    /// `--test-own-start-fails`: this gate's start, which names the sync-back's quarantine, cannot be read.
    pub(super) own_start_fails: bool,
    lines: Vec<String>,
    problem: Option<String>,
}

impl Attempt<'_, '_> {
    /// The phases in order; `Err` for an end before the run call.
    fn phases(&mut self) -> Result<End, End> {
        let snapshot = self.snapshot()?;
        let location = Location::under(self.transport.root(), self.call.repo, self.call.key, &payload::job_nonce());
        let nonce = payload::run_nonce();
        let sent = outputs::sent(self)?;
        let run = RunCall {
            words: self.call.words,
            subfolder: self.call.subfolder,
            nonce: &nonce,
            snapshot: &snapshot.commit,
            outputs: sent.globs,
            full: sent.full,
        };
        let run_line = payload::capped("run", payload::line("run", &payload::run_word(&location, &run))).map_err(End::Failed)?;
        let prepared = self.preflight(&location)?;
        self.parity(&snapshot, prepared.sdks.as_deref())?;
        self.push(&snapshot, prepared.created)?;
        // After the push: a run refused before it, at the preflight or by the parity check, leaves no marker.
        outputs::marked(&sent)?;
        let (end, heard) = self.run_call(&run_line, &location, sent.manifest.as_ref(), &nonce)?;
        // The sync-back runs after a run call that ended with the command's own status; a refusal, an interrupt, a
        // kill, a lost connection or a failed call ended the attempt before it, so a box refusal's 75 never syncs back.
        // The pending marker stays on every end but a passing run whose box packed nothing.
        match end {
            End::Status(status) => {
                let synced = syncback::sync_back(self, &snapshot, status)?;
                let brought = outputs::accepted(heard.as_deref(), self.call.key, &nonce);
                let ran = outputs::Ran {
                    location: &location,
                    nonce: &nonce,
                    status,
                };
                Ok(outputs::ended(self, &sent, &ran, synced, brought.as_ref()))
            }
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
        let (mut called, _) = self.with_retry(SSH_FAILED, |attempt| attempt.ssh(&line, Echo::LogOnly, watch, raise, CallOnly::default()))?;
        let theirs = their_version(&called.lines).map(str::to_owned);
        let created = called.lines.iter().any(|line| line.starts_with(CREATED));
        let boot = boot_time(&called.lines).map(str::to_owned);
        let sdks = sdks_of(&called.lines);
        self.lines.append(&mut called.lines);
        self.not_started_if_killed("the preflight", &called)?;
        let host = self.call.host;
        match called.status {
            0 => {}
            SSH_FAILED => {
                let last = git::failure_text(&[called.last_error.as_deref().unwrap_or_default()]);
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
        Ok(Prepared { created, boot, sdks })
    }

    /// The toolchain parity check, made after the preflight and before the push, so a refusal costs no transfer: a
    /// builder whose preflight listed the SDKs `sdks` must have what the snapshot needs, as [`parity::need`] reads it
    /// from the snapshot's root `global.json`, whatever the letter case of its name, and, with no pin, this PC's newest
    /// SDK major. One that lacks it is refused `parity`, with the mismatch line. A builder that listed no SDK line, a
    /// snapshot that is no .NET tree, or one whose listing or `global.json` cannot be read whole is not checked, the
    /// listing with a line.
    fn parity(&mut self, snapshot: &Snapshot, sdks: Option<&[String]>) -> Result<(), End> {
        let Some(sdks) = sdks else {
            return Ok(());
        };
        self.go_on()?;
        let git = Git::find(Path::new(self.call.root)).map_err(End::Failed)?;
        let listing = match self.git_bytes(&git, &["ls-tree", "-r", "-z", "--name-only", &snapshot.commit]) {
            Ok(listing) => listing,
            Err(GitError::OverCap(why)) => {
                self.say(format!("gate: cannot list the snapshot's files: {why}; skipping the toolchain check"));
                return Ok(());
            }
            Err(error) => return Err(git_end(error)),
        };
        let Ok(listing) = String::from_utf8(listing) else {
            self.say("gate: cannot list the snapshot's files: a path is not valid UTF-8; skipping the toolchain check".to_owned());
            return Ok(());
        };
        let names: Vec<&str> = listing.split('\0').filter(|name| !name.is_empty()).collect();
        if !parity::is_dotnet_tree(names.iter().copied()) {
            return Ok(());
        }
        // dotnet reads a root `global.json` whatever the letter case of its name, as Windows opens it.
        let global_json = names.iter().find(|name| name.eq_ignore_ascii_case("global.json")).copied();
        let pin = match global_json {
            Some(name) => match self.git_bytes(&git, &["show", &format!("{}:{name}", snapshot.commit)]) {
                Ok(bytes) => parity::pin_of(Some(&String::from_utf8_lossy(&bytes))),
                // A `global.json` past the cap could not be read whole, so it holds no readable pin.
                Err(GitError::OverCap(_)) => parity::Pin::unreadable(),
                Err(error) => return Err(git_end(error)),
            },
            None => parity::pin_of(None),
        };
        if pin.found && pin.version.is_none() {
            self.say(parity::NO_READABLE_PIN.to_owned());
        }
        let Some(need) = parity::need(&pin, sdks, || self.local_major()) else {
            return Ok(());
        };
        self.say(parity::mismatch_line(self.call.repo, &need, self.call.host, sdks));
        Err(End::Refused(FallbackReason::Parity))
    }

    /// What git writes on standard output for `args` in the worktree, as the bytes it wrote, a call the console
    /// interrupt ends; a failure is the run's to tell apart.
    fn git_bytes(&self, git: &Git, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let call = git::Call {
            interrupt: self.call.interrupt,
            ..git::Call::plain(Path::new(self.call.root), args, self.limit)
        };
        git.output_bytes(&call)
    }

    /// This PC's newest .NET SDK major, from `--test-local-sdks` when given, else from `dotnet --list-sdks`; 0 when no
    /// dotnet is found or it lists no SDK. A list that cannot be read is said, and checks no major.
    fn local_major(&mut self) -> u64 {
        if let Some(listing) = self.local_sdks {
            return parity::newest_major(parity::listed(listing).iter().map(String::as_str));
        }
        match local_sdk_listing(Path::new(self.call.root)) {
            Ok(listing) => parity::newest_major(listing.lines().filter_map(|line| line.split_whitespace().next())),
            Err(why) => {
                self.say(format!("gate: cannot read this PC's SDK list: {why}; checking no SDK major"));
                0
            }
        }
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
        let ssh_command = target.ssh_command.as_deref().map(|ssh| ("GIT_SSH_COMMAND", std::ffi::OsStr::new(ssh)));
        let watch = Watch::Run(self.limits_for("push"));
        let (mut called, _) = self.with_retry(GIT_SSH_FAILED, |attempt| {
            attempt.git(&git, &args, ssh_command.as_slice(), Echo::Progress, watch)
        })?;
        let size = push_size(&called.lines);
        self.lines.append(&mut called.lines);
        self.not_started_if_killed("the push", &called)?;
        if called.status != 0 {
            self.say(format!("gate: pushing the snapshot to {} failed (status {})", target.url, called.status));
            return Err(End::Refused(FallbackReason::Unreachable));
        }
        snapshot::save(root, key, &snapshot.commit, self.limit).map_err(git_end)?;
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
    /// banner twice, as the command never started; a refusal when its 75 comes with the box's refusal tag for the run
    /// whose nonce is `nonce` and no start marker; a lost connection when it exited 255 otherwise. A watchdog kill says
    /// its line, stops the command and ends the run killed; a Ctrl+C stops the command too. Each call made is fed
    /// `input` on its standard input, when given. With the end, the last standard error line of the call that counts
    /// that is the box's outputs line.
    fn run_call(&mut self, line: &str, location: &Location, input: Option<&Arc<str>>, nonce: &str) -> Result<(End, Option<String>), End> {
        self.go_on()?;
        let limits = self.limits_for("run");
        let raise = self.raises_at("run");
        let nonce: Arc<str> = Arc::from(nonce);
        let mut heard = None;
        let end = self.run_call_made(location, limits, |attempt| {
            let run = CallOnly {
                input: input.cloned(),
                nonce: Some(Arc::clone(&nonce)),
                sink: None,
            };
            let called = attempt.ssh(line, Echo::Console, Watch::Run(limits), raise, run)?;
            heard.clone_from(&called.outputs_line);
            Ok(called)
        })?;
        Ok((end, heard))
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
        let reason = called.refusal();
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

    /// Makes a call with `make` and, when it failed before the banner and its job did not write more than 1 MiB, says
    /// so, pauses and makes it once more; a Ctrl+C
    /// before the retry or during its pause ends the attempt instead. The call that counts, its lines not yet kept, and
    /// whether the retry failed before the banner too.
    pub(super) fn with_retry(&mut self, fail_status: i32, mut make: impl FnMut(&mut Self) -> Result<Called, End>) -> Result<(Called, bool), End> {
        let mut first = make(self)?;
        if !first.before_the_banner(fail_status) || first.wrote_much() {
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
        let mut made = self.ssh(&line, Echo::LogOnly, Watch::Bounded(bound), false, CallOnly::default());
        let again = matches!(&made, Ok(first) if first.before_the_banner(SSH_FAILED)) && retry::retry_allowed(bound, begun.elapsed(), self.pause);
        if again {
            if let Ok(first) = &mut made {
                self.lines.append(&mut first.lines);
            }
            self.say(retry::retry_line(self.call.host));
            thread::sleep(self.pause);
            made = self.ssh(
                &line,
                Echo::LogOnly,
                Watch::Bounded(bound.saturating_sub(begun.elapsed())),
                false,
                CallOnly::default(),
            );
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
    pub(super) fn raises_at(&self, verb: &str) -> bool {
        self.interrupt_at == Some(verb)
    }

    /// Says `line` on stderr alone; the first failure to is kept.
    fn told(&mut self, line: &str) {
        if let Err(error) = say_on(io::stderr().lock(), &format!("{line}\n")) {
            self.problem.get_or_insert_with(|| format!("cannot say a line: {error}"));
        }
    }

    /// One call of `gate box` on the builder carrying `line`, watched as `watch` says; the console interrupt raised
    /// after its first output byte when `raise`; what only this call gives it, `only`.
    fn ssh(&mut self, line: &str, echo: Echo, watch: Watch, raise: bool, only: CallOnly) -> Result<Called, End> {
        let Program { program, args } = self.transport.call(self.call.host, line);
        let mut command = Command::new(program);
        command.args(args);
        let starts = Starts {
            breakaway: self.transport.box_runs_here(),
            raise,
            input: only.input,
            nonce: only.nonce,
            sink: only.sink,
        };
        self.started(&command, echo, watch, starts)
    }

    /// The outputs call to the builder at `location` for the archive `name`: `gate box outputs`, made once more when
    /// it failed before the banner, watched as a fetch by `limits`, and the console interrupt raised after its first
    /// output byte with `--test-remote-interrupt-at outputs`. Each attempt writes its standard output raw to the file
    /// `archive` makes new for it; its standard error lines go to the log. The call that counts, its lines kept; else
    /// why the file or the call could not be made, or the interrupt.
    pub(super) fn outputs_call(&mut self, location: &Location, name: &str, archive: &mut outputs::Archive, limits: Limits) -> Result<Called, End> {
        self.go_on()?;
        let line = payload::capped("outputs", payload::line("outputs", &payload::outputs_word(location, name))).map_err(End::Failed)?;
        let raise = self.raises_at("outputs");
        let (mut called, _) = self.with_retry(SSH_FAILED, |attempt| {
            let cap = sent_limit(attempt.outputs_budget);
            let only = CallOnly {
                sink: Some(archive.create(cap).map_err(End::Failed)?),
                ..CallOnly::default()
            };
            attempt.ssh(&line, Echo::LogOnly, Watch::Fetch(limits, None), raise, only)
        })?;
        self.lines.append(&mut called.lines);
        Ok(called)
    }

    /// git with `args` in the worktree, given the variables `set` beside the ones every call is given, among them the ssh
    /// it runs as `GIT_SSH_COMMAND` when there is one, its lines going where `echo` says, watched as `watch` says.
    pub(super) fn git(&mut self, git: &Git, args: &[String], set: &[(&str, &std::ffi::OsStr)], echo: Echo, watch: Watch) -> Result<Called, End> {
        let mut command = git.command(Path::new(self.call.root));
        command.args(args).envs(set.iter().copied());
        let starts = Starts {
            breakaway: false,
            raise: false,
            input: None,
            nonce: None,
            sink: None,
        };
        self.started(&command, echo, watch, starts)
    }

    /// Starts `command` with no standard input, or one fed what `starts` gives on a thread of its own and then closed,
    /// created suspended and put in an anonymous job of its own before it runs, and reads its two streams line by line
    /// into the log, and onto the console as `echo` says, its standard output instead copied raw into the sink
    /// `starts` gives when it gives one, until both end and it has exited, watched as `watch` says meanwhile. Every
    /// call's line is capped at 64 KiB but the run call's, whose longer lines are passed on in pieces. A call the watch
    /// cuts has its whole job ended, and its readers are given [`DRAIN`] to end; a Ctrl+C ends a call a bound does not
    /// limit the same way, as the attempt's interrupt.
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
        let feed_log = match (&starts.input, &err_log) {
            (Some(_), Some(log)) => Some(log.try_clone().map_err(cannot_log)?),
            _ => None,
        };
        let (suspended, stdout, stderr, stdin) = launched(&launch, &job, starts.input.is_some()).map_err(|error| cannot_start(error.to_string()))?;
        let running = suspended.assign_and_resume(&job).map_err(|error| cannot_start(error.to_string()))?;
        if let (Some(stdin), Some(input)) = (stdin, starts.input) {
            fed(stdin, input, feed_log);
        }
        let shared = Arc::new(Shared::new());
        let (done, readers) = mpsc::channel();
        let reading = Reading {
            echo,
            capped: echo != Echo::Console,
            lines: starts.nonce.as_deref().map(|nonce| Arc::new(ProtocolLines::new(nonce))),
        };
        let raw = if let Some(sink) = starts.sink {
            Some(relayed_raw(
                stdout,
                Capped {
                    inner: sink.file,
                    cap: sink.cap,
                    shared: Arc::clone(&shared),
                    written: 0,
                },
                sink.shown,
                &shared,
                done.clone(),
            ))
        } else {
            relayed(stdout, Reader::new(false, reading.clone(), out_log), &shared, done.clone());
            None
        };
        relayed(stderr, Reader::new(true, reading, err_log), &shared, done);
        let watched = Watched {
            running: &running,
            job: &job,
            progress: &shared.progress,
            readers: &readers,
            shown: &shown,
        };
        let raise = self.call.interrupt.filter(|_| starts.raise);
        let watching = watched.watch(watch, self.call.interrupt, raise);
        if let Some(raw) = raw {
            // The call's end has come: the copy writes nothing more into an archive the fetch is about to remove.
            shared.stop();
            let drained = watching.as_ref().is_ok_and(|watching| watching.ended.drained());
            self.ended_raw(raw, drained);
        }
        let Watching { ended, looks } = watching?;
        if let Some(line) = &looks.unread {
            self.logged(line);
        }
        if watch.byte_cap().is_some() && !matches!(ended, CallEnd::Exited(0)) {
            self.emptied(&job, &shown);
        }
        let heard = std::mem::take(&mut *held(&shared.heard));
        match called_of(ended, shared.progress.raised(), heard) {
            Ok(called) => Ok(Called {
                written: looks.written,
                ..called
            }),
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

    /// Gives the raw copy's reader `raw` the rest of its [`DRAIN`] to end, when the call's own end did not already
    /// drain the readers (`drained`), joins it, and says a reader that panicked or one that has not ended by then. The
    /// sink has already been made to stop writing, so nothing more goes into the archive the fetch is about to remove.
    fn ended_raw(&mut self, raw: thread::JoinHandle<()>, drained: bool) {
        let deadline = if drained { Instant::now() } else { Instant::now() + DRAIN };
        while !raw.is_finished() && Instant::now() < deadline {
            thread::sleep(PAUSE_STEP);
        }
        if raw.is_finished() {
            if raw.join().is_err() {
                self.logged("gate: the outputs call's raw copy panicked");
            }
        } else {
            self.logged(&format!("gate: the outputs call's raw copy did not end within {} s", DRAIN.as_secs()));
        }
    }

    /// Appends the gate's own `line` to the log and says it on stderr; the first failure to is kept.
    pub(super) fn say(&mut self, line: String) {
        if let Err(why) = super::say(self.call.log, &[&line], io::stderr().lock()) {
            self.problem.get_or_insert(why);
        }
        self.lines.push(line);
    }

    /// Ends every process of `job`, the job of the capped call `shown` that did not exit cleanly, and waits at most
    /// [`KILL_WAIT`] for them to be gone, so the files they hold open are let go before the sync-back removes what they
    /// left: a process that outlived the call's own exit is ended at once rather than waited for. Logs why it could
    /// not end them, or stopped waiting before they were gone.
    fn emptied(&mut self, job: &Job, shown: &str) {
        if let Err(error) = job.terminate(KILLED.cast_unsigned()) {
            self.logged(&format!("gate: cannot end {shown}: {error}"));
        }
        let deadline = Instant::now() + KILL_WAIT;
        loop {
            match job.accounting() {
                Ok(held) if held.active_processes == 0 => return,
                Ok(_) if Instant::now() >= deadline => {
                    self.logged(&format!("gate: the processes of {shown} did not end within {} s", KILL_WAIT.as_secs()));
                    return;
                }
                Ok(_) => thread::sleep(PAUSE_STEP),
                Err(error) => {
                    self.logged(&format!("gate: cannot read the processes of {shown}: {error}"));
                    return;
                }
            }
        }
    }

    /// Appends the gate's own `line` to the log alone; the first failure to is kept.
    fn logged(&mut self, line: &str) {
        if let Err(why) = super::say(self.call.log, &[line], io::sink()) {
            self.problem.get_or_insert(why);
        }
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
            brought_back: None,
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
            local_sdks: None,
            fetch_cap: watchdog::FETCH_CAP,
            apply_cap: watchdog::APPLY_CAP,
            outputs_budget: OUTPUTS_BUDGET,
            own_start_fails: false,
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

/// How a watched call ended: it exited with its status; it exited, but the look after its exit found it past a cap; the
/// client cut it; or a Ctrl+C ended it. With what went wrong ending it.
#[derive(Debug)]
enum CallEnd {
    Exited(i32),
    ExitedOver(Cut),
    Cut(Cut, Option<String>),
    Interrupted(Option<String>),
}

impl CallEnd {
    /// Whether the call's own end gave its readers their whole [`DRAIN`]: the watch heard both of them end, or the
    /// cut's [`Watched::end`] waited the drain out. It gives up on them early, and says so in its problem, when it
    /// cannot end the call's job, when the call's own process outlives [`KILL_WAIT`], and when it cannot wait.
    fn drained(&self) -> bool {
        match self {
            CallEnd::Exited(_) | CallEnd::ExitedOver(_) | CallEnd::Cut(_, None) | CallEnd::Interrupted(None) => true,
            CallEnd::Cut(_, Some(_)) | CallEnd::Interrupted(Some(_)) => false,
        }
    }
}

/// Why a watched call gave no result: the client failed it, or a Ctrl+C ended it, with what else went wrong meanwhile.
#[derive(Debug)]
enum Unfinished {
    Failed(String),
    Interrupted(Option<String>),
}

/// The call that ended as `ended`, with what its readers have `raised` by its end, from what its streams said: a
/// reader's problem fails a call that ended on its own, and is kept as a note of one the client cut or a Ctrl+C
/// ended, whose end stands.
fn called_of(ended: CallEnd, raised: Raised, mut heard: Heard) -> Result<Called, Unfinished> {
    let (status, cut, ending) = match ended {
        CallEnd::Interrupted(ending) => return Err(Unfinished::Interrupted(joined(heard.problem.take(), ending))),
        CallEnd::Cut(cut, ending) => (KILLED, Some(cut), ending),
        // A line past the line cap, or a raw copy past its archive cap, cuts the call even when it ended before the
        // watch saw either, and comes before bytes past a job's cap that the look after the exit found.
        CallEnd::Exited(_) | CallEnd::ExitedOver(_) if raised.cut().is_some() => (KILLED, raised.cut(), None),
        CallEnd::ExitedOver(cut) => (KILLED, Some(cut), None),
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
        outputs_line: heard.outputs_line,
        started: heard.started,
        refused: heard.refused,
        written: None,
    })
}

/// A reader's problem and a failure to end the call, joined, or either alone.
fn joined(read: Option<String>, ending: Option<String>) -> Option<String> {
    match (read, ending) {
        (Some(read), Some(ending)) => Some(format!("{read}; {ending}")),
        (read, ending) => read.or(ending),
    }
}

/// What watching a call came to: how it ended, and what the looks at the bytes its job wrote found.
struct Watching {
    ended: CallEnd,
    looks: Looks,
}

/// What the looks at the bytes a capped call's job wrote found: the count at the last look that succeeded, and the
/// line saying the first look that failed.
#[derive(Default)]
struct Looks {
    written: Option<u64>,
    unread: Option<String>,
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
    /// Watches the call as `watch` says until both its readers ended and it exited: how it ended, and the line saying
    /// the first look at the bytes its job wrote that failed, which cuts nothing; the attempt's interrupt when the
    /// console `interrupt` ended it. A fetch with a cap has those bytes looked at on each tick and once more after its
    /// exit, so a fetch that ends between two ticks is found past its cap too. `raise`, when given, is raised once the
    /// call sent its first byte, as a Ctrl+C would be.
    fn watch(self, watch: Watch, interrupt: Option<Interrupt>, mut raise: Option<Interrupt>) -> Result<Watching, End> {
        let begun = Instant::now();
        let tick = watch.tick();
        let mut open = 2;
        let mut looks = Looks::default();
        loop {
            if open == 0
                && let Some(code) = self.running.wait(Duration::ZERO).map_err(|error| cannot_wait(self.shown, &error))?
            {
                let over = watch.over(self.written(watch, &mut looks));
                let ended = over.map_or(CallEnd::Exited(code.cast_signed()), CallEnd::ExitedOver);
                return Ok(Watching { ended, looks });
            }
            if self.progress.heard_any()
                && let Some(interrupt) = raise.take()
            {
                interrupt.raise();
            }
            if watch.interruptible() && interrupt.is_some_and(Interrupt::is_raised) {
                let ended = CallEnd::Interrupted(self.end(open));
                return Ok(Watching { ended, looks });
            }
            let written = self.written(watch, &mut looks);
            if let Some(cut) = watch.cut(Instant::now(), begun, self.progress.idle(), self.progress.raised(), written) {
                let ended = CallEnd::Cut(cut, self.end(open));
                return Ok(Watching { ended, looks });
            }
            open -= self.readers_ended(open, tick);
        }
    }

    /// The bytes the call's job has written, looked at only when `watch` caps them, and kept in `looks`; `None` when
    /// it does not, or when the look failed, whose first failure's line `looks` keeps.
    fn written(&self, watch: Watch, looks: &mut Looks) -> Option<u64> {
        watch.byte_cap()?;
        match self.job.written_bytes() {
            Ok(bytes) => {
                looks.written = Some(bytes);
                Some(bytes)
            }
            Err(error) => {
                looks
                    .unread
                    .get_or_insert_with(|| format!("gate: cannot read the bytes {} wrote: {error}", self.shown));
                None
            }
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
/// byte; and what is fed on its standard input, `None` for none; and the run's nonce its readers match the box's start
/// marker and refusal tags by, `None` for every call but the run call; and the sink its standard output is copied to
/// raw, `None` for every call but the outputs call.
struct Starts {
    breakaway: bool,
    raise: bool,
    input: Option<Arc<str>>,
    nonce: Option<Arc<str>>,
    sink: Option<RawSink>,
}

/// What one call of `gate box` alone gives it: the run call what is fed on its standard input and the run's nonce, the
/// outputs call the sink its standard output is copied to raw; none of them for any other call.
#[derive(Default)]
struct CallOnly {
    input: Option<Arc<str>>,
    nonce: Option<Arc<str>>,
    sink: Option<RawSink>,
}

/// Where a call's standard output is copied raw, byte for byte, rather than read as lines: the outputs call's archive
/// file, its path as the line saying it cannot be written shows it, and the most bytes the copy may write into it.
pub(super) struct RawSink {
    pub(super) file: File,
    pub(super) shown: String,
    pub(super) cap: NonZeroU64,
}

/// The sink a raw copy writes through: the archive's file and the cap it may write, with the call's progress to raise
/// the passed flag on. Past the cap no further byte reaches the file, and the copy reports no failure, so it ends with
/// its stream rather than on a write error; the watch cuts the call for the flag. Once the call's end has come, nothing
/// more is written either: the archive is about to be removed.
struct Capped<W: Write> {
    inner: W,
    cap: NonZeroU64,
    shared: Arc<Shared>,
    written: u64,
}

impl<W: Write> Write for Capped<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.shared.stopped() {
            return Ok(buffer.len());
        }
        let room = self.cap.get().saturating_sub(self.written);
        if room == 0 {
            self.shared.progress.cap_passed(self.cap.get());
            return Ok(buffer.len());
        }
        let taken = usize::try_from(room).unwrap_or(usize::MAX).min(buffer.len());
        let written = self.inner.write(buffer.get(..taken).unwrap_or(buffer))?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Writes `input` to a call's `stdin` on a thread of its own, then closes it. A write that fails, as one to a call that
/// ended without reading its input does, is noted in the call's `log` and never fails the call, whose own end stands.
fn fed(mut stdin: File, input: Arc<str>, log: Option<File>) {
    thread::spawn(move || {
        let Err(error) = stdin.write_all(input.as_bytes()) else {
            return;
        };
        drop(stdin);
        if let Some(mut log) = log {
            // A note that cannot be written has nowhere else to go: the call's readers own its console streams.
            let _ = writeln!(log, "gate: the run call's input was not sent: {error}");
        }
    });
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
        GitError::Failed(why) | GitError::OverCap(why) => End::Failed(why),
    }
}

/// What one call's two streams said so far, kept for the call's end, each line cut to the cap: whether it wrote on
/// standard output, its first and last standard error lines, its last standard error line that is the box's outputs
/// line, whether the box said the run's start marker and the refusal its first tag named, its last lines, and the
/// first problem reading them.
#[derive(Default)]
struct Heard {
    kept: Kept,
    output_seen: bool,
    first_error: Option<String>,
    last_error: Option<String>,
    outputs_line: Option<String>,
    started: bool,
    refused: Option<FallbackReason>,
    problem: Option<String>,
}

impl Heard {
    /// Keeps one `line` of standard error when `error`, else of standard output, cut to the cap: a heartbeat, and the
    /// box's start marker or refusal tag its reader read the line as, `protocol`, only mark the first error line, the
    /// latter two also marking the run started or refused.
    fn record(&mut self, line: &str, error: bool, protocol: Option<Protocol>) {
        let line = watchdog::truncated(line);
        if error {
            self.first_error.get_or_insert_with(|| line.to_owned());
        } else {
            self.output_seen = true;
        }
        match protocol {
            Some(Protocol::Started) => {
                self.started = true;
                return;
            }
            Some(Protocol::Refused(reason)) => {
                self.refused.get_or_insert(reason);
                return;
            }
            None => {}
        }
        if error && is_heartbeat(line) {
            return;
        }
        if error {
            self.last_error = Some(line.to_owned());
        }
        outputs::keep_outputs_line(&mut self.outputs_line, line, error);
        self.kept.push(line);
    }

    /// Keeps `problem` unless one came first.
    fn failed(&mut self, problem: String) {
        self.problem.get_or_insert(problem);
    }
}

/// When a call's streams last sent a byte, whether they sent any, whether a capped one sent a line past the cap, and
/// the cap a raw copy of its standard output passed: what the watcher reads, with no lock and no wait on a reader's
/// writes.
struct Progress {
    begun: Instant,
    /// Milliseconds from `begun` to the last byte.
    last_byte: AtomicU64,
    any: AtomicBool,
    too_long: AtomicBool,
    /// The cap a raw copy of the call's standard output passed, 0 until one did: every cap is above 0.
    passed: AtomicU64,
}

/// What a call's readers have raised that cuts it: a line past the line cap, and the cap a raw copy of its standard
/// output passed, `None` until one did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Raised {
    too_long: bool,
    passed: Option<u64>,
}

impl Raised {
    /// The cut a raised flag calls for, a line past the line cap first: `None` while neither was raised.
    fn cut(self) -> Option<Cut> {
        if self.too_long {
            return Some(Cut::TooLong);
        }
        self.passed.map(Cut::ArchiveTooBig)
    }
}

impl Progress {
    fn new() -> Progress {
        Progress {
            begun: Instant::now(),
            last_byte: AtomicU64::new(0),
            any: AtomicBool::new(false),
            too_long: AtomicBool::new(false),
            passed: AtomicU64::new(0),
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

    /// The cap a raw copy of the call's standard output passed, `None` until one did.
    fn passed(&self) -> Option<u64> {
        match self.passed.load(Ordering::Relaxed) {
            0 => None,
            cap => Some(cap),
        }
    }

    /// Raises the flag for a raw copy that passed `cap` bytes.
    fn cap_passed(&self, cap: u64) {
        self.passed.store(cap, Ordering::Relaxed);
    }

    /// What the call's readers have raised by now.
    fn raised(&self) -> Raised {
        Raised {
            too_long: self.too_long(),
            passed: self.passed(),
        }
    }
}

/// What a call's two readers share with each other and its watcher: what they heard, behind a lock no write is made
/// under, their progress, and whether the call's end has come.
struct Shared {
    heard: Mutex<Heard>,
    progress: Progress,
    /// Set once the watch has returned: the call ended, so a reader still going writes nothing more into what the
    /// fetch is about to remove.
    stopped: AtomicBool,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            heard: Mutex::new(Heard::default()),
            progress: Progress::new(),
            stopped: AtomicBool::new(false),
        }
    }

    /// Marks that the call has ended.
    fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Whether the call has ended.
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
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

/// How a reader takes a call's lines: where they go, whether a line past the cap stops it, and the run's start marker
/// and refusal tags its lines are matched against, `None` for every call but the run call.
#[derive(Clone)]
struct Reading {
    echo: Echo,
    capped: bool,
    lines: Option<Arc<ProtocolLines>>,
}

impl Reading {
    /// The box's start marker or refusal tag for the run that `line` is, of standard error when `error`; `None` for a
    /// line of standard output, any line of a call that is no run call, and any other line.
    fn protocol(&self, line: &str, error: bool) -> Option<Protocol> {
        self.lines.as_deref().filter(|_| error).and_then(|lines| lines.of(line))
    }
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
    /// stops, `false`. The box's heartbeat, start marker and refusal tag are kept but never passed on.
    fn line_ended(&mut self, bytes: &[u8], shared: &Shared) -> bool {
        if self.reading.capped && watchdog::too_long(bytes) {
            shared.progress.cut_too_long();
            return false;
        }
        let line = line_of(bytes, self.reading.echo);
        let protocol = self.reading.protocol(&line, self.error);
        if protocol.is_none() && !(self.error && is_heartbeat(&line)) {
            self.passed(&line, true, shared);
        }
        held(&shared.heard).record(&line, self.error, protocol);
        true
    }

    /// Passes on `piece`, the next of a line past the cap, which ends it when `ended`; else the start of a character
    /// it cuts waits for the next piece. The line's head is kept once it ends.
    fn streamed(&mut self, mut piece: Vec<u8>, ended: bool, shared: &Shared) {
        if ended {
            self.passed(&line_of(&piece, self.reading.echo), true, shared);
            if let Some(head) = self.head.take() {
                held(&shared.heard).record(&head, self.error, None);
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

/// Copies `stream` into `sink`, shown as `shown`, on a thread of its own as [`relay_raw`] does, then says so on `done`,
/// and hands the thread back, so the fetch can join it before it removes the archive the copy wrote to.
fn relayed_raw(
    stream: impl Read + Send + 'static,
    mut sink: impl Write + Send + 'static,
    shown: String,
    shared: &Arc<Shared>,
    done: Sender<()>,
) -> thread::JoinHandle<()> {
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        relay_raw(stream, &mut sink, &shown, &shared);
        // Closed before the end is said: a retry removes the file and makes it anew, which an open handle would refuse.
        drop(sink);
        // A killed call's watcher stops listening once its drain is over: a reader that ends later has no one to tell.
        done.send(()).ok();
    })
}

/// Copies `stream` into `sink`, shown as `shown`, byte for byte until it ends, in chunks of at most [`RAW_CHUNK`]: no/// Copies `stream` into `sink`, shown as `shown`, byte for byte until it ends, in chunks of at most [`RAW_CHUNK`]: no
/// line is read, so no length cuts it. Each chunk marks the call's progress, and marks that the call wrote on standard
/// output, so a call that sent a byte is past the banner. A read or a write that fails is the call's problem, and ends
/// the copy. No write is made under the lock the watcher could take.
fn relay_raw(mut stream: impl Read, sink: &mut impl Write, shown: &str, shared: &Shared) {
    let mut buffer = vec![0_u8; RAW_CHUNK];
    loop {
        let read = match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            Err(failure) if failure.kind() == io::ErrorKind::Interrupted => continue,
            Err(failure) => {
                held(&shared.heard).failed(format!("cannot read a call's output: {failure}"));
                return;
            }
        };
        shared.progress.mark();
        held(&shared.heard).output_seen = true;
        if let Err(error) = sink.write_all(buffer.get(..read).unwrap_or_default()) {
            held(&shared.heard).failed(format!("cannot write {shown}: {error}"));
            return;
        }
    }
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

/// The SDKs the builder lists, from the last `gate: remote sdks <v1,v2|none>` line of its preflight, without `none`;
/// `None` when it said no such line, or one with no list, which skips the parity check.
fn sdks_of(lines: &[String]) -> Option<Vec<String>> {
    lines
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix(SDKS))
        .map(str::trim)
        .filter(|listing| !listing.is_empty())
        .map(parity::listed)
}

/// What `dotnet --list-sdks` lists on this PC, empty when no dotnet is found along the gate's own `PATH`, read against
/// `folder`; else why it could not be read.
fn local_sdk_listing(folder: &Path) -> Result<String, String> {
    let dotnet = match find_program("dotnet", &Variables::from_process(), folder) {
        Ok(dotnet) => dotnet,
        // `find_program` says a word it does not find with this message, and every other failure with another.
        Err(message) if message == gate_win32::win32_message(ERROR_FILE_NOT_FOUND) => return Ok(String::new()),
        Err(message) => return Err(message),
    };
    // Started under the spawn lock, so it inherits no launch's handles.
    let output = gate_win32::spawn(
        Command::new(&dotnet)
            .arg("--list-sdks")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )
    .and_then(Child::wait_with_output)
    .map_err(|error| format!("cannot start {}: {error}", dotnet.display()))?;
    if !output.status.success() {
        let status = output.status.code().map_or_else(|| "unknown".to_owned(), |code| code.to_string());
        return Err(format!("dotnet --list-sdks exited with status {status}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

/// The watchdog's line for a call watched as a fetch by `limits` and cut for `cut`: STALLED for one that sent nothing
/// for the stall window, TIMED OUT for one still running at the ceiling; `None` for any other cut.
pub(super) fn watchdog_line(cut: Cut, limits: Limits) -> Option<String> {
    let kill = match cut {
        Cut::Silent(_) => Kill::Stalled,
        Cut::Ceiling => Kill::TimedOut,
        Cut::Watchdog(_) | Cut::Late(_) | Cut::TooLong | Cut::TooBig(_) | Cut::ArchiveTooBig(_) => return None,
    };
    Some(watchdog::kill_line(kill, limits.stall, limits.ceiling))
}

/// `text` cut to its first `most` bytes, at a character's boundary.
pub(super) fn first_bytes(text: &str, most: usize) -> &str {
    let mut end = text.len().min(most);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

/// A line the box says on a run call's standard error for the client alone, besides its heartbeat: the start marker
/// said just before the command starts, or a refusal tag and the fallback reason it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Started,
    Refused(FallbackReason),
}

/// The start marker and the two refusal tags of one run, made once for its run call's readers.
struct ProtocolLines {
    started: String,
    busy: String,
    tools: String,
}

impl ProtocolLines {
    /// The lines of the run whose nonce is `nonce`.
    fn new(nonce: &str) -> ProtocolLines {
        ProtocolLines {
            started: started_line(nonce),
            busy: refused_line(nonce, Refusal::Busy),
            tools: refused_line(nonce, Refusal::Tools),
        }
    }

    /// What `line`, compared as a whole line, is of the run: its start marker or one of its refusal tags; `None` for
    /// any other line, another run's marker or tag included, as for a command's own line.
    fn of(&self, line: &str) -> Option<Protocol> {
        if line == self.started {
            Some(Protocol::Started)
        } else if line == self.busy {
            Some(Protocol::Refused(FallbackReason::Busy))
        } else if line == self.tools {
            Some(Protocol::Refused(FallbackReason::Tools))
        } else {
            None
        }
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
    use std::collections::VecDeque;
    use std::io::{self, Cursor, Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, LazyLock, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use gate_state::{FallbackReason, StateRoot};
    use gate_win32::{Interrupt, PipedConsole};

    use super::{
        Attempt, CallEnd, Called, Capped, Cut, Echo, End, GIT_SSH_FAILED, Heard, LINE_CAP, Limits, Loss, NonZeroU64, OUTPUTS_BUDGET, ProtocolLines,
        Raised, Reader, Reading, SSH_FAILED, Shared, Unfinished, Watch, after_loss, boot_time, called_of, console_of, held, is_heartbeat,
        last_update, paused, push_size, relay, relay_raw, relayed_raw, sdks_of, seconds, their_version, version_refusal,
    };
    use crate::remote::git::GIT_LIMIT;
    use crate::remote::outputs;
    use crate::remote::payload::Location;
    use crate::remote::snapshot::Snapshot;
    use crate::remote::transport::Local;
    use crate::remote::watchdog::{self, Kill};
    use crate::remote::{Refusal, RemoteCall, busy_line, refused_line, started_line};
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
            state_dir: Path::new(r"C:\gate-client-tests-state"),
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
            local_sdks: None,
            fetch_cap: watchdog::FETCH_CAP,
            apply_cap: watchdog::APPLY_CAP,
            outputs_budget: OUTPUTS_BUDGET,
            own_start_fails: false,
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
            outputs_line: None,
            started: false,
            refused: None,
            written: None,
        }
    }

    /// What a call's progress holds as its last byte's time once a test unmarked it: no mark leaves it so.
    const UNMARKED: u64 = u64::MAX;

    /// A stream that gives the chunks `left`, one a read, then its end; before each read it notes whether the call's progress was
    /// marked since the read before and whether the call counts as having written on standard output, then unmarks it.
    struct Chunks<'s> {
        left: VecDeque<Vec<u8>>,
        shared: &'s Shared,
        seen: Vec<(bool, bool)>,
    }

    impl Read for Chunks<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let marked = self.shared.progress.last_byte.load(Ordering::Relaxed) != UNMARKED;
            self.seen.push((marked, held(&self.shared.heard).output_seen));
            self.shared.progress.last_byte.store(UNMARKED, Ordering::Relaxed);
            let Some(chunk) = self.left.pop_front() else {
                return Ok(0);
            };
            buffer.get_mut(..chunk.len()).expect("one chunk fits one read").copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn a_raw_copy_passes_binary_whole_and_marks_progress_and_output_per_chunk() {
        // No line end, and bytes no UTF-8 reading would keep: the zstd magic, NUL, a carriage return, 0x80 and 0xff.
        let pattern = [0x28_u8, 0xb5, 0x2f, 0xfd, 0, b'\r', 0x80, 0xff];
        let binary = |length: usize| -> Vec<u8> { pattern.iter().copied().cycle().take(length).collect() };
        let chunks = [binary(64 * 1024), binary(40_000), binary(1_234)];
        let whole = chunks.concat();
        assert!(whole.len() > 64 * 1024 && !whole.contains(&b'\n'), "past a line's cap, with no line end");
        let shared = Shared::new();
        shared.progress.last_byte.store(UNMARKED, Ordering::Relaxed);
        let mut stream = Chunks {
            left: chunks.into_iter().collect(),
            shared: &shared,
            seen: Vec::new(),
        };
        let mut sink = Vec::new();

        relay_raw(&mut stream, &mut sink, "the archive", &shared);

        assert_eq!(sink, whole, "every byte, in order, none cut");
        let before_any_byte = (false, false);
        let after_a_chunk = (true, true);
        assert_eq!(stream.seen, [before_any_byte, after_a_chunk, after_a_chunk, after_a_chunk]);
        assert!(!shared.progress.too_long(), "no line is read, so no cap cuts it");
        assert_eq!(held(&shared.heard).problem, None);
    }

    /// Whether the attempt said it stopped, or tried to stop, the command.
    fn stopped(attempt: &Attempt<'_, '_>) -> bool {
        attempt.lines.iter().any(|line| line.starts_with("gate: stopping the command on builder"))
    }

    /// A sink that, once dropped, waits a while before it marks itself closed: an end said before the drop is seen
    /// before the mark.
    struct SlowToClose(Arc<AtomicBool>);

    impl Write for SlowToClose {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for SlowToClose {
        fn drop(&mut self) {
            thread::sleep(Duration::from_millis(200));
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A sink that takes `room` bytes, then fails every write.
    struct Full {
        room: usize,
    }

    impl Write for Full {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.room == 0 {
                return Err(io::Error::other("the disk is full"));
            }
            let taken = bytes.len().min(self.room);
            self.room -= taken;
            Ok(taken)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A retry's make removes the file the first attempt wrote: a handle still open would leave its name pending
    /// deletion, and the new file could not be made.
    #[test]
    fn a_raw_copy_closes_its_sink_before_it_says_it_ended() {
        let closed = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared::new());
        let (done, ended) = mpsc::channel();

        let raw = relayed_raw(
            &b"archive bytes"[..],
            SlowToClose(Arc::clone(&closed)),
            "the archive".to_owned(),
            &shared,
            done,
        );

        ended.recv().expect("the copy says it ended");
        assert!(closed.load(Ordering::SeqCst), "the sink was closed first");
        raw.join().expect("the copy's thread ended");
    }

    /// A sink that records what it is written, so a copy's bytes can be checked once its thread has ended.
    struct Recording(Arc<Mutex<Vec<u8>>>);

    impl Write for Recording {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("the sink is not poisoned").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The raw copy hands back the thread that writes the archive, so the fetch can join it within the drain before the
    /// archive it wrote to is removed.
    #[test]
    fn a_raw_copy_hands_back_its_reader_thread_to_be_joined() {
        let shared = Arc::new(Shared::new());
        let (done, ended) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));

        let raw = relayed_raw(
            &b"archive bytes"[..],
            Recording(Arc::clone(&written)),
            "the archive".to_owned(),
            &shared,
            done,
        );

        ended.recv().expect("the copy says it ended");
        raw.join().expect("the copy's thread ended without panicking");
        assert_eq!(&*written.lock().expect("the sink is not poisoned"), b"archive bytes");
    }

    #[test]
    fn a_raw_copy_whose_sink_fails_says_why_and_ends() {
        let shared = Arc::new(Shared::new());
        let (done, ended) = mpsc::channel();

        let raw = relayed_raw(
            Cursor::new(vec![0x28_u8; 200_000]),
            Full { room: 70_000 },
            "the archive".to_owned(),
            &shared,
            done,
        );

        ended.recv().expect("the copy says it ended");
        raw.join().expect("the copy's thread ended");
        let problem = held(&shared.heard).problem.clone();
        assert_eq!(problem.as_deref(), Some("cannot write the archive: the disk is full"));
    }

    /// The raw copy writes no byte past its cap, raises the call's passed flag rather than failing the write, and ends.
    #[test]
    fn a_raw_copy_past_its_cap_writes_no_further_byte_and_raises_the_call_s_flag() {
        let cap = NonZeroU64::new(1_000).expect("the cap is above 0");
        let room = usize::try_from(cap.get()).expect("the cap fits a usize");
        let shared = Arc::new(Shared::new());
        let mut stream = Cursor::new(vec![b'x'; 3 * room]);
        let mut sink = Capped {
            inner: Vec::new(),
            cap,
            shared: Arc::clone(&shared),
            written: 0,
        };

        relay_raw(&mut stream, &mut sink, "the archive", &shared);

        assert_eq!(sink.inner.len(), room, "no byte past the cap is written");
        assert_eq!(shared.progress.passed(), Some(cap.get()), "the cap is raised on the call's progress");
        assert_eq!(shared.progress.raised().cut(), Some(Cut::ArchiveTooBig(cap.get())));
        assert_eq!(held(&shared.heard).problem, None, "the cap is no write failure");
    }

    /// A stream of exactly the cap is not past it: every byte is written and no flag is raised.
    #[test]
    fn a_raw_copy_of_exactly_its_cap_writes_all_of_it_and_raises_nothing() {
        let cap = NonZeroU64::new(1_000).expect("the cap is above 0");
        let room = usize::try_from(cap.get()).expect("the cap fits a usize");
        let shared = Arc::new(Shared::new());
        let mut stream = Cursor::new(vec![b'x'; room]);
        let mut sink = Capped {
            inner: Vec::new(),
            cap,
            shared: Arc::clone(&shared),
            written: 0,
        };

        relay_raw(&mut stream, &mut sink, "the archive", &shared);

        assert_eq!(sink.inner.len(), room, "the cap's own bytes all reach the archive");
        assert_eq!(shared.progress.passed(), None, "at the cap is not past it");
        assert_eq!(shared.progress.raised().cut(), None);
    }

    /// One byte past the cap is past it: the cap's bytes reach the archive, no more do, and the flag is raised.
    #[test]
    fn a_raw_copy_one_byte_past_its_cap_writes_the_cap_and_raises_the_flag() {
        let cap = NonZeroU64::new(1_000).expect("the cap is above 0");
        let room = usize::try_from(cap.get()).expect("the cap fits a usize");
        let shared = Arc::new(Shared::new());
        let mut stream = Cursor::new(vec![b'x'; room + 1]);
        let mut sink = Capped {
            inner: Vec::new(),
            cap,
            shared: Arc::clone(&shared),
            written: 0,
        };

        relay_raw(&mut stream, &mut sink, "the archive", &shared);

        assert_eq!(sink.inner.len(), room, "the byte past the cap does not reach the archive");
        assert_eq!(shared.progress.passed(), Some(cap.get()), "the cap is raised on the call's progress");
        assert_eq!(shared.progress.raised().cut(), Some(Cut::ArchiveTooBig(cap.get())));
    }

    /// Once the call has ended, the copy writes nothing more into the archive the fetch is about to remove.
    #[test]
    fn a_raw_copy_of_a_call_that_has_stopped_writes_nothing() {
        let cap = NonZeroU64::new(1_000).expect("the cap is above 0");
        let shared = Arc::new(Shared::new());
        let mut stream = Cursor::new(vec![b'x'; 500]);
        let mut sink = Capped {
            inner: Vec::new(),
            cap,
            shared: Arc::clone(&shared),
            written: 0,
        };
        shared.stop();

        relay_raw(&mut stream, &mut sink, "the archive", &shared);

        assert_eq!(sink.inner.len(), 0, "nothing is written into an archive the fetch is about to remove");
        assert_eq!(shared.progress.passed(), None, "the cap is not raised by a copy that writes nothing");
        assert_eq!(held(&shared.heard).problem, None, "and it is no write failure");
    }

    #[test]
    fn a_call_that_passed_a_raw_copy_s_cap_is_cut_for_it_even_after_its_exit() {
        let second = Duration::from_secs(1);
        let started = Instant::now();
        let limits = Limits {
            started,
            stall: 10 * second,
            ceiling: 60 * second,
            bound: 30 * second,
        };
        let cap = 262_144;
        let raised = Raised {
            too_long: false,
            passed: Some(cap),
        };
        let fetched = Watch::Fetch(limits, None);

        assert_eq!(
            fetched.cut(started + second, started, Duration::ZERO, raised, None),
            Some(Cut::ArchiveTooBig(cap)),
            "the watch cuts it for the archive it passed"
        );
        assert_eq!(
            fetched.cut(started + second, started, Duration::ZERO, Raised { too_long: true, ..raised }, None),
            Some(Cut::TooLong),
            "a line past the line cap comes first"
        );
        let over = called_of(CallEnd::Exited(0), raised, Heard::default()).expect("a cut call");
        assert_eq!((over.status, over.cut), (124, Some(Cut::ArchiveTooBig(cap))), "the look after the exit");
    }

    #[test]
    fn an_outputs_call_cut_for_the_archive_it_passed_keeps_the_run_s_status() {
        let log = unwritten_log();
        let mut call = call(&log);
        // No interrupt is raised: this cut keeps the run's status, unlike the failed fetch a Ctrl+C or a bad call end.
        call.interrupt = None;
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let archive = outputs::Archive::new(
            Path::new(r"C:\gate-client-tests-no-such-folder"),
            PathBuf::from(r"C:\gate-client-tests-no-such-folder\app.tar.zst"),
        );
        let mut cut = ended_with(0, "");
        cut.cut = Some(Cut::ArchiveTooBig(262_144));

        let end = outputs::judged(&mut attempt, &archive, &cut, Limits::for_tests(), End::Status(0));

        assert!(matches!(end, End::Status(0)), "the run keeps its status: {end:?}");
        assert_eq!(attempt.lines, ["gate: remote outputs not brought back: the archive passed 256.0 KiB"]);
    }

    #[test]
    fn an_outputs_archive_past_the_fetch_cap_is_removed_when_its_cut_is_judged() {
        let log = unwritten_log();
        let mut call = call(&log);
        call.interrupt = None;
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let folder = std::env::temp_dir().join(format!("gate-client-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("the folder can be made");
        let path = folder.join(".tmp").join("app.tar.zst");
        let mut archive = outputs::Archive::new(&folder, path.clone());
        let mut sink = archive.create(NonZeroU64::MAX).expect("the archive is made");
        sink.file.write_all(b"partial").expect("the file can be written");
        drop(sink);
        let mut cut = ended_with(0, "");
        cut.cut = Some(Cut::ArchiveTooBig(262_144));

        let end = outputs::judged(&mut attempt, &archive, &cut, Limits::for_tests(), End::Status(0));

        assert!(matches!(end, End::Status(0)), "the run keeps its status: {end:?}");
        assert!(!path.exists(), "the partial archive a cut fetch wrote is removed");
        assert_eq!(attempt.lines, ["gate: remote outputs not brought back: the archive passed 256.0 KiB"]);
    }

    /// The outputs call failed, but the console interrupt was raised by its end: the interrupt is the run's end, and
    /// nothing is said, as after a run call.
    #[test]
    fn a_ctrl_c_by_the_end_of_a_failed_outputs_call_is_an_interrupt() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let archive = outputs::Archive::new(
            Path::new(r"C:\gate-client-tests-no-such-folder"),
            PathBuf::from(r"C:\gate-client-tests-no-such-folder\app.tar.zst"),
        );
        let failed = ended_with(SSH_FAILED, "Connection reset by peer");

        let end = outputs::judged(&mut attempt, &archive, &failed, Limits::for_tests(), End::Status(0));

        assert!(matches!(end, End::Interrupted), "{end:?}");
        assert_eq!(attempt.lines, Vec::<String>::new());
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
    fn the_outputs_line_survives_a_stdout_flood_after_it() {
        let mut heard = Heard::default();
        let line = "gate-remote-outputs: 2 app-1a2b3c4d-0123456789abcdef0123456789abcdef.tar.zst";
        heard.record("gate-remote-outputs: 0", false, None);
        heard.record(line, true, None);
        for number in 0..1500 {
            heard.record(&format!("build output {number}"), false, None);
        }
        heard.record("gate-remote-outputs: 0", false, None);
        heard.record("gate: remote said something else", true, None);

        let called = called_of(CallEnd::Exited(0), Raised::default(), heard).expect("a call that ended");

        assert!(!called.lines.iter().any(|kept| kept == line), "the window no longer holds the line");
        assert_eq!(called.outputs_line.as_deref(), Some(line), "the last standard error outputs line is kept");
    }

    #[test]
    fn a_watchdog_cut_stays_a_kill_whatever_else_went_wrong() {
        let mut heard = Heard::default();
        heard.failed("cannot write to the log: the disk is full".to_owned());
        let cut = Cut::Watchdog(Kill::Stalled);

        let called = called_of(
            CallEnd::Cut(cut, Some("cannot end ssh.exe: access denied".to_owned())),
            Raised::default(),
            heard,
        )
        .expect("a kill, not the run's failure");

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

        let ended = called_of(CallEnd::Interrupted(ending), Raised::default(), heard);

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
            let reading = Reading {
                echo,
                capped: true,
                lines: None,
            };

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
        let failed = called_of(CallEnd::Exited(0), Raised::default(), heard);
        assert!(
            matches!(&failed, Err(Unfinished::Failed(why)) if why == "cannot say a line: the pipe is closed"),
            "{:?}",
            failed.err()
        );
        let cut = called_of(
            CallEnd::Exited(0),
            Raised {
                too_long: true,
                ..Raised::default()
            },
            Heard::default(),
        )
        .expect("a cut call");
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
        let fetch = Watch::Fetch(limits, None);
        assert_eq!(
            fetch.cut(started + 59 * second, begun, 9 * second, Raised::default(), None),
            None,
            "neither yet"
        );
        assert_eq!(
            fetch.cut(started + 50 * second, begun, 10 * second, Raised::default(), None),
            Some(Cut::Silent(10 * second)),
            "silent for the stall window"
        );
        assert_eq!(
            fetch.cut(started + 60 * second, begun, second, Raised::default(), None),
            Some(Cut::Ceiling),
            "a live fetch at the run's ceiling"
        );
        assert_eq!(
            fetch.cut(
                started + 41 * second,
                begun,
                Duration::ZERO,
                Raised {
                    too_long: true,
                    ..Raised::default()
                },
                None
            ),
            Some(Cut::TooLong)
        );
        assert_eq!(
            Watch::Run(limits).cut(started + 60 * second, begun, second, Raised::default(), None),
            Some(Cut::Watchdog(Kill::TimedOut))
        );
        assert_eq!(
            Watch::Deadline(5 * second).cut(begun + 5 * second, begun, Duration::ZERO, Raised::default(), None),
            Some(Cut::Late(5 * second)),
            "a deadline counts from the call's own start"
        );
        assert_eq!(
            Watch::Deadline(5 * second).cut(begun + 4 * second, begun, Duration::ZERO, Raised::default(), None),
            None
        );
    }

    #[test]
    fn a_capped_fetch_is_cut_once_it_wrote_more_than_its_cap_and_an_uncapped_one_never_for_its_bytes() {
        let second = Duration::from_secs(1);
        let started = Instant::now();
        let limits = Limits {
            started,
            stall: 10 * second,
            ceiling: 60 * second,
            bound: 30 * second,
        };
        let (now, cap) = (started + second, 262_144);
        let capped = Watch::Fetch(limits, Some(cap));
        assert_eq!(
            capped.cut(now, started, Duration::ZERO, Raised::default(), Some(cap - 1)),
            None,
            "under the cap"
        );
        assert_eq!(
            capped.cut(now, started, Duration::ZERO, Raised::default(), Some(cap)),
            None,
            "at the cap is not over it"
        );
        assert_eq!(
            capped.cut(now, started, Duration::ZERO, Raised::default(), Some(cap + 1)),
            Some(Cut::TooBig(cap)),
            "over the cap"
        );
        assert_eq!(
            capped.cut(now, started, Duration::ZERO, Raised::default(), None),
            None,
            "a count that could not be read cuts nothing"
        );
        assert_eq!(
            capped.cut(now, started, 10 * second, Raised::default(), Some(cap + 1)),
            Some(Cut::TooBig(cap)),
            "its size before its silence"
        );
        assert_eq!(
            capped.cut(
                now,
                started,
                Duration::ZERO,
                Raised {
                    too_long: true,
                    ..Raised::default()
                },
                Some(cap + 1)
            ),
            Some(Cut::TooLong),
            "a line past the cap first"
        );
        assert_eq!(
            Watch::Fetch(limits, None).cut(now, started, Duration::ZERO, Raised::default(), Some(u64::MAX)),
            None,
            "a fetch with no cap"
        );
        assert_eq!(
            Watch::Run(limits).cut(now, started, Duration::ZERO, Raised::default(), Some(u64::MAX)),
            None,
            "a call that is no fetch"
        );
    }

    #[test]
    fn a_fetch_found_over_its_cap_after_its_exit_is_cut_for_it_unless_a_line_was_too_long() {
        let cap = 262_144;
        let over = called_of(CallEnd::ExitedOver(Cut::TooBig(cap)), Raised::default(), Heard::default()).expect("a cut call");
        assert_eq!((over.status, over.cut), (124, Some(Cut::TooBig(cap))), "the look after the exit");
        let both = called_of(
            CallEnd::ExitedOver(Cut::TooBig(cap)),
            Raised {
                too_long: true,
                ..Raised::default()
            },
            Heard::default(),
        )
        .expect("a cut call");
        assert_eq!((both.status, both.cut), (124, Some(Cut::TooLong)), "a line past the cap wins");
    }

    /// What a stream of `input` gives a reader of standard error that keeps the lines and caps them when `capped`, its
    /// pieces written to the file at `log` when one is given.
    fn read_through(input: &[u8], capped: bool, log: Option<&Path>) -> Shared {
        let shared = Shared::new();
        let file = log.map(|path| std::fs::File::create(path).expect("the log can be made"));
        let reading = Reading {
            echo: Echo::LogOnly,
            capped,
            lines: None,
        };
        relay(input, Reader::new(true, reading, file), &shared);
        shared
    }

    /// The nonce of the run the protocol cases read.
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    /// The run call of the run whose nonce is [`NONCE`] that said `stdout` and `stderr` and exited 75, each stream read
    /// as the run call reads it; with what its readers wrote to the log, which a line it passes on reaches as it reaches
    /// the console.
    fn run_call_said(stdout: &str, stderr: &str, test: &str) -> (Called, String) {
        let log = std::env::temp_dir().join(format!("gate-client-protocol-{}-{test}.log", std::process::id()));
        let shared = Shared::new();
        let reading = Reading {
            echo: Echo::LogOnly,
            capped: false,
            lines: Some(std::sync::Arc::new(ProtocolLines::new(NONCE))),
        };
        let file = std::fs::File::create(&log).expect("the log can be made");
        let other = file.try_clone().expect("the log can be shared");
        relay(stdout.as_bytes(), Reader::new(false, reading.clone(), Some(other)), &shared);
        relay(stderr.as_bytes(), Reader::new(true, reading, Some(file)), &shared);
        let heard = std::mem::take(&mut *held(&shared.heard));
        let called = called_of(CallEnd::Exited(75), Raised::default(), heard).expect("a call that ended");
        let said = std::fs::read_to_string(&log).expect("the log can be read");
        std::fs::remove_file(&log).expect("the log can be removed");
        (called, said)
    }

    #[test]
    fn a_refusal_is_read_from_this_run_s_tag_on_standard_error_alone() {
        let busy = busy_line("app-1a2b3c4d");
        let tools = "gate: remote work folder app-1a2b3c4d could not restore its dotnet tools";
        let busy_tag = refused_line(NONCE, Refusal::Busy);
        let tools_tag = refused_line(NONCE, Refusal::Tools);
        let marker = started_line(NONCE);
        let other = refused_line("fedcba9876543210fedcba9876543210", Refusal::Busy);
        let cases: [(&str, String, String, Option<FallbackReason>); 10] = [
            ("busy", String::new(), format!("{busy_tag}\n{busy}\n"), Some(FallbackReason::Busy)),
            (
                "busy-crlf",
                String::new(),
                format!("{busy_tag}\r\n{busy}\r\n"),
                Some(FallbackReason::Busy),
            ),
            ("after-marker-crlf", String::new(), format!("{marker}\r\n{busy_tag}\r\n{busy}\r\n"), None),
            ("tools", String::new(), format!("{tools_tag}\n{tools}\n"), Some(FallbackReason::Tools)),
            ("untagged-busy", String::new(), format!("{busy}\n"), None),
            ("untagged-tools", String::new(), format!("{tools}\n"), None),
            ("stdout", format!("{busy_tag}\n"), format!("{busy}\n"), None),
            ("other-nonce", String::new(), format!("{other}\n{busy}\n"), None),
            ("after-marker", String::new(), format!("{marker}\n{busy_tag}\n{busy}\n"), None),
            ("padded", String::new(), format!("{busy_tag} \n{busy}\n"), None),
        ];
        for (name, stdout, stderr, refused) in cases {
            let (called, _) = run_call_said(&stdout, &stderr, name);
            assert_eq!(called.refusal(), refused, "{name}");
        }
    }

    #[test]
    fn the_start_marker_and_the_refusal_tags_are_kept_out_of_the_log_and_the_lines() {
        let busy = busy_line("app-1a2b3c4d");
        let busy_tag = refused_line(NONCE, Refusal::Busy);
        let marker = started_line(NONCE);

        let (refused, said) = run_call_said("", &format!("{busy_tag}\n{busy}\n"), "hidden-tag");
        assert_eq!(said, format!("{busy}\n"), "the tag is not passed on");
        assert_eq!(refused.lines, std::slice::from_ref(&busy), "nor kept");

        let (started, said) = run_call_said("out\n", &format!("{marker}\nerr\n"), "hidden-marker");
        assert!(started.started, "the marker is heard");
        assert_eq!(said.lines().collect::<Vec<_>>().len(), 2, "{said:?}");
        assert!(!said.contains("gate-remote-started"), "the marker is not passed on: {said:?}");
        assert!(!started.lines.contains(&marker), "nor kept: {:?}", started.lines);

        let (crlf, said) = run_call_said("", &format!("{marker}\r\n"), "hidden-marker-crlf");
        assert!(crlf.started, "a marker ended CR LF is heard");
        assert_eq!(said, "", "and not passed on");

        let other = started_line("fedcba9876543210fedcba9876543210");
        let (another, said) = run_call_said("", &format!("{other}\n"), "another-marker");
        assert!(!another.started, "another run's marker is a line like any other");
        assert_eq!(said, format!("{other}\n"), "and is passed on");
    }

    #[test]
    fn a_run_call_whose_box_said_the_start_marker_never_failed_before_the_banner() {
        let marker = started_line(NONCE);
        let (mut called, _) = run_call_said("", &format!("Connection timed out during banner exchange\n{marker}\n"), "banner-words");
        called.status = SSH_FAILED;
        assert!(!called.before_the_banner(SSH_FAILED), "the command's own banner words, after the marker");

        let (mut unmarked, _) = run_call_said("", "Connection timed out during banner exchange\n", "banner-unmarked");
        unmarked.status = SSH_FAILED;
        assert!(unmarked.before_the_banner(SSH_FAILED), "ssh's own words, with no marker");
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
        assert!(
            matches!(attempt.run_call("line", &location, None, NONCE), Err(End::Interrupted)),
            "the run call"
        );
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

    /// A fetch that failed as if before the banner but whose job wrote more than 1 MiB is not made again, so a builder
    /// cannot buy a second fetch of a fresh count: ssh's own failure writes one line.
    #[test]
    fn a_call_whose_job_wrote_more_than_1_mib_is_not_made_again_after_a_failure_before_the_banner() {
        const MIB: u64 = 1 << 20;
        let log = std::env::temp_dir().join(format!("gate-client-retry-written-{}.log", std::process::id()));
        let mut given = call(&log);
        given.interrupt = None;
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let mut attempt = attempt(&given, &transport);
        attempt.pause = Duration::ZERO;
        for (written, times) in [(Some(MIB + 1), 1), (Some(MIB), 2), (None, 2)] {
            let mut made = 0;
            let retried = attempt.with_retry(GIT_SSH_FAILED, |_| {
                made += 1;
                Ok(Called {
                    written,
                    ..ended_with(GIT_SSH_FAILED, "kex_exchange_identification: Connection closed by remote host")
                })
            });
            let (called, _) = retried.expect("a call that ends without an interrupt");
            assert_eq!(made, times, "{written:?}");
            assert_eq!(called.status, GIT_SSH_FAILED);
        }
        std::fs::remove_file(&log).expect("the log can be removed");
    }

    #[test]
    fn the_box_s_protocol_lines_name_the_run_s_nonce() {
        assert_eq!(busy_line("app-1a2b3c4d"), "gate: remote work folder app-1a2b3c4d is busy");
        assert_eq!(started_line(NONCE), format!("gate-remote-started: {NONCE}"));
        assert_eq!(refused_line(NONCE, Refusal::Busy), format!("gate-remote-refused: {NONCE} busy"));
        assert_eq!(refused_line(NONCE, Refusal::Tools), format!("gate-remote-refused: {NONCE} tools"));
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
    fn the_builder_s_sdks_are_its_preflight_s_last_sdks_line() {
        let preflight = lines(&[
            "gate: remote version 0.1.0",
            "gate: remote sdks 8.0.100",
            "gate: remote sdks 8.0.425, 10.0.100",
            "gate: remote boot 2026-10-10T08:00:00.0000000Z",
        ]);
        assert_eq!(sdks_of(&preflight), Some(vec!["8.0.425".to_owned(), "10.0.100".to_owned()]));
        assert_eq!(
            sdks_of(&lines(&["gate: remote sdks 9.0.300,10.0.100"])),
            Some(vec!["9.0.300".to_owned(), "10.0.100".to_owned()])
        );
        assert_eq!(
            sdks_of(&lines(&["gate: remote sdks none"])),
            Some(Vec::new()),
            "a builder with no dotnet lists none"
        );
        assert_eq!(sdks_of(&lines(&["gate: remote version 0.1.0"])), None, "no SDK line skips the check");
        assert_eq!(
            sdks_of(&lines(&["gate: remote could not read the SDK list: access denied"])),
            None,
            "the box could not read it"
        );
        assert_eq!(sdks_of(&lines(&["gate: remote sdks  "])), None, "an SDK line with no list");
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
                let reading = Reading {
                    echo,
                    capped: true,
                    lines: None,
                };
                let mut reader = Reader::new(true, reading, None);
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
            Watch::Fetch(Limits::for_tests(), None),
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
