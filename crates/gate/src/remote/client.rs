//! The Rust remote client, in the phases the PowerShell client runs: the snapshot; the run call built and checked
//! against the most cmd takes before anything is sent; the preflight and the builder's version; the push and the saved
//! ref; and the run call, its output passed through to the log and the console. A call that failed before the ssh
//! banner is made again up to three times, for as long as every call fails the same way, unless the box said the run's
//! start marker. A Ctrl+C ends the attempt between its calls, during the retry pause, and during any call; one during
//! the run call stops the command on the builder. Each call goes through the [`Transport`]; every process is created
//! suspended, put in an anonymous job of its own that ends its whole tree when it is killed, and only then let run.
//! The preflight, the push and the run call run under the run's watchdog, whose clock starts with the attempt: a call
//! that sends nothing for the stall window, or outlives the run's ceiling, is killed. A kill before the run call falls
//! back `unreachable`, as the command never started; a kill of the run call stops the command and ends the run 124. A
//! run call that exits 255 other than a failure before the banner lost its connection: nothing is synced back and the
//! command is stopped; then, when the preflight said the builder's boot time, the builder is asked for it again. One
//! that booted more than 30 s later restarted, and the run falls back `rebooted`; one that gives no boot time, or when
//! either value is no timestamp, falls back `unreachable`; else the run ends 255. No call holds more than 64 KiB of a
//! line in memory, nor keeps more than a call's last 1,000 lines, totalling at most 1 MiB: a longer line ends any call
//! but the run call, whose output is passed on whole. The run takes the work-folder key's lock ([`hold_key`], then
//! [`hold_key_waiting`] in slices from `run.rs` up to its bound) before its remote slot.

use std::borrow::Cow;
use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::Ipv4Addr;
use std::num::NonZeroU64;
use std::ops::Range;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::BoxVerb;
use gate_args::TestSeams;
use gate_args::is_godot_console;
use gate_state::v1::{self, WarmFailure};
use gate_state::{FallbackReason, KernelObject, key_hash};
use gate_win32::{AcquireError, Interrupt, Job, NamedMutex, OwnedMutexGuard, PipedConsole, Running, Share};
use gate_wire::build_version;
use gate_wire::stamp::{self, Tag};
use jiff::{SignedDuration, Timestamp};
use regex::Regex;

use gate_router::{Config, DROPPED_NO_BUILD_LINE, SendPlan, build_settings, escaped, repo_name};
use gate_wire::{PairRefused, Payload, SHA256_DIGITS, channel_prefix, is_cross_not_brought_back, is_lower_hex};

use super::channel;
use super::drain::{self, Drain, DrainLine};
use super::git::{self, Git, GitBounds, GitError, NO_CURRENT_FOLDER, last_update};
use super::held::{Held, sibling_who};
use super::inputs;
use super::launch::{DRAIN, KILL_WAIT, KILLED, call_job, launched, piped_launch};
use super::lfs;
use super::outputs::{self, Unbuilt};
use super::parity;
use super::payload::{self, Location, RunCall, WarmCall};
use super::retry::{self, Ended, GIT_SSH_FAILED, SSH_FAILED};
use super::snapshot::{self, Bounded, Place, Snapshot};
use super::syncback;
#[cfg(feature = "test-seams")]
use super::tamper;
use super::toolchain::{self, GodotChoice, Manifest, NoChosenGodot, Pin, Reported};
#[cfg(feature = "test-seams")]
use super::transport::Local;
use super::transport::ssh::Ssh;
use super::transport::{Profile, Program, Transport, Wire};
use super::watchdog::{self, Cut, Kept, Kill, LINE_CAP};
use super::worktree_lock::{self, Folder, Locked, Wait};
use super::{
    BoxKill, FAILED, FAILED_RUN, INSTALL_UNTRUSTED, KeyWait, REFUSED, Refusal, RemoteCall, RemoteClient, RemoteOutcome, Split, WarmEnd, Warmth,
    adjusted_prefix, install_why, killed_line, refused_line, root_busy_why, root_why, started_line,
};
use crate::boxside::outputs::{NAMES_SKIPPED, OUTPUTS_BUDGET};
use crate::durations::{Usual, hang_hint, routed_usual};
use crate::environment::Variables;
use crate::no_build;
use crate::outputs::archive::sent_limit;
use crate::route::{self, open_followed};
#[cfg(feature = "test-seams")]
use crate::run::find_program;
use crate::run::{CONTROL_C_EXIT, is_within, resolved};

/// The statuses of a preflight whose builder has no `gate box` that takes the run: cmd's for a program it cannot find,
/// a gate's usage error for a payload it does not read, and a Unix shell's for a program it found but could not run and
/// for one it did not find, the last two only when the shell said no line of the box's own ([`no_box`]).
pub(crate) const NOT_RECOGNIZED: i32 = 9009;
pub(crate) const USAGE: i32 = 2;
pub(crate) const SHELL_CANNOT_EXECUTE: i32 = 126;
pub(crate) const SHELL_NOT_FOUND: i32 = 127;

/// Whether the preflight call that ended with `status`, its standard output `lines` and standard error `errors`, came
/// from a builder with no gate box that takes the run: cmd's status for a program it cannot find, a gate's usage
/// status, or a Unix shell's 126 or 127 with no line of the box's own on either stream, since a box that ran says
/// `gate: …` before it ends with a status.
pub(crate) fn no_box<'a>(status: i32, lines: &'a [String], errors: impl IntoIterator<Item = &'a str>) -> bool {
    if matches!(status, NOT_RECOGNIZED | USAGE) {
        return true;
    }
    matches!(status, SHELL_CANNOT_EXECUTE | SHELL_NOT_FOUND) && !box_spoke(lines, errors)
}

/// Whether a preflight call said one of the box's own lines, `gate: …` once its stamp is taken off, on standard output
/// `lines` or standard error `errors`: both boxes stamp the lines they say themselves.
fn box_spoke<'a>(lines: &'a [String], errors: impl IntoIterator<Item = &'a str>) -> bool {
    lines
        .iter()
        .map(String::as_str)
        .chain(errors)
        .any(|line| stamp::unstamped(line).text.starts_with("gate:"))
}

/// How a no-box line ends for a call that ended with `status` and gave `last` on standard error: `: {last}`, escaped,
/// after a Unix shell's 126 or 127 only, whose statuses can be a program's own failure; nothing otherwise, and nothing
/// for a shell that said no line.
pub(crate) fn shell_said(status: i32, last: Option<&str>) -> String {
    match last.filter(|line| !line.trim().is_empty()) {
        Some(last) if matches!(status, SHELL_CANNOT_EXECUTE | SHELL_NOT_FOUND) => format!(": {}", git::failure_text(&[last])),
        _ => String::new(),
    }
}

/// The most bytes a call's job may have written and still be made again after a failure before the banner: ssh's own
/// failure writes one line, and another fetch from a builder would start a fresh count against its cap.
const RETRY_WRITTEN: u64 = 1 << 20;
/// How often the retry pause looks at the console interrupt.
const PAUSE_STEP: Duration = Duration::from_millis(20);
/// The box's heartbeat, `gate-remote-heartbeat: <digits>` on standard error: progress, kept out of the log.
const HEARTBEAT: &str = "gate-remote-heartbeat: ";
/// The preflight's lines the client reads.
const VERSION: &str = "gate: remote version ";
/// The work folder's warm-up, `gate: remote warm <hash|none>`, said right after the version line; the space keeps the
/// box's `gate: remote warm-up …` lines from matching.
const WARM: &str = "gate: remote warm ";
/// The preflight's line, right after the warm line, when it found the work folder's tree new: nothing had the name
/// `.git` there.
pub(crate) const NEW_WORK_TREE: &str = "gate: remote new work tree";
pub(crate) const CREATED: &str = "gate: remote created ";
/// The builder's boot time, which the preflight and the `boot` call say.
const BOOT: &str = "gate: remote boot ";
/// The builder's SDK list, which the preflight says: `gate: remote sdks <v1,v2|none>`.
const SDKS: &str = "gate: remote sdks ";
/// How far apart two boot times of one builder may be and still be one boot.
const BOOT_SLACK: SignedDuration = SignedDuration::from_secs(30);
/// The line said when `--test-remote-interrupt-at` is given but the console handler that would carry the interrupt
/// could not be installed.
const NO_HANDLER: &str = "gate: --test-remote-interrupt-at needs the console handler; it could not be installed";
/// The most of a call's standard output one read of a raw copy takes.
const RAW_CHUNK: usize = 64 * 1024;
/// How often an outputs pull says its progress while it runs, counted from the pull's start, before its first call,
/// across all its calls.
const OUTPUTS_PROGRESS: Duration = Duration::from_secs(30);

/// The Rust client: over ssh, or over the local transport of `--test-remote-transport`.
pub(crate) struct RustClient {
    /// The way to the builder: `None` for the ssh transport until a routed attempt first needs it ([`found`]), so a
    /// run that never routes never looks for ssh.
    transport: Option<Box<dyn Transport>>,
    /// The bounds of each git call of the attempt, its limit and its readers' drain; the drain also bounds the readers
    /// of this PC's `dotnet --list-sdks` read.
    git: GitBounds,
    /// The pause before a retry.
    pause: Duration,
    /// The pauses before the first and the second retry of a transfer after the command.
    transfer_pauses: [Duration; retry::TRANSFER_RETRIES],
    /// How long a bounded call, the `stop` or the `boot`, may take.
    bound: Duration,
    /// `--test-stall-ms`: the stall window, given as it is, with no floor.
    stall_ms: Option<u64>,
    /// `--test-remote-timeout-ms`: with `--test-remote-watch-call` naming a call other than `run`, that call's transfer
    /// ceiling, or the `ls-remote` deadline, in milliseconds.
    timeout_ms: Option<u64>,
    /// `--test-remote-backstop-ms`: the run call's backstop, in milliseconds, in place of five ceilings and 60 s.
    backstop_ms: Option<u64>,
    /// How far before the moment it is taken each attempt's snapshot time is put: `--test-remote-snapshot-age-ms`, else
    /// none.
    snapshot_age: Duration,
    /// `--test-remote-watch-call`: the one call `--test-stall-ms` applies to, when it is not the whole run's, and
    /// `--test-remote-timeout-ms` too, which names a call other than `run`; the backstop seam is the run call's alone.
    watch_call: Option<String>,
    /// `--test-remote-interrupt-at`: the call after whose first output byte the client raises the console interrupt.
    interrupt_at: Option<String>,
    /// `--test-remote-channel-fault`: the fault the PC's side of an outputs pull's data channel takes.
    channel_fault: Option<String>,
    /// `--test-local-sdks`: this PC's SDK list, read in place of `dotnet --list-sdks`.
    local_sdks: Option<String>,
    /// How long this PC's `dotnet --list-sdks` may take: `--test-dotnet-limit-ms`, else 30 s.
    dotnet_limit: Duration,
    /// The most bytes the job of the sync-back's fetch may write: `--test-remote-fetch-cap-bytes`, else 4 GiB.
    fetch_cap: u64,
    /// The most bytes the files a back commit writes into the worktree may total: `--test-remote-apply-cap-bytes`,
    /// else 4 GiB.
    apply_cap: u64,
    /// The box's outputs budget: `--test-remote-outputs-budget-bytes`, else 50 GiB. What the outputs fetch is capped
    /// at, and what the install unpacks its archive to.
    outputs_budget: u64,
    /// The most bytes one put of the inputs may total, decompressed: `--test-remote-inputs-budget-bytes`, else 4 GiB.
    inputs_budget: u64,
    /// The most bytes one put of LFS objects may total: `--test-remote-lfs-budget-bytes`, else 16 GiB.
    lfs_budget: u64,
    /// The most bytes of one call's output written to the gate's log: `--test-remote-log-cap-bytes`, else 256 MiB.
    log_cap: u64,
    /// `--test-own-start-fails`: this gate's start, which names the sync-back's quarantine, cannot be read.
    own_start_fails: bool,
    /// `--test-remote-hint-from-pull`: the hang hint is due only during the outputs pull, counted from its start.
    hint_from_pull: bool,
    /// How often an outputs pull says its progress: `--test-outputs-progress-ms`, else 30 s.
    outputs_progress: Duration,
    /// `--test-remote-tamper`: the change the client makes to its own folders at one point of the run.
    #[cfg(feature = "test-seams")]
    tamper: Option<String>,
    /// What the drain polls and the drained attempts since the last attempt that was not drained learned of the work
    /// folder, which the next attempt's preflight then cannot ([`Carried`]).
    carried: Carried,
}

/// What a drain poll, or an attempt the builder's drain refused, learned of the work folder of one builder that the
/// next attempt's own preflight cannot, the box having made it by then: whether the box laid out the work tree new,
/// which counts the next attempt's tree as new ([`Attempt::cold_ceiling`]), and the bare repositories it created, which
/// the next attempt pushes as created ([`Attempt::pushes`]). Carried to each attempt until one is not drained.
#[derive(Default)]
struct Carried {
    /// The builder whose work folder this is.
    host: String,
    new_tree: bool,
    created: Vec<String>,
}

impl Carried {
    /// Takes in what the preflight of an attempt or poll to `host` found: a `new_tree`, and the `created` bares.
    fn add(&mut self, host: &str, new_tree: bool, created: &[String]) {
        if self.host != host {
            *self = Carried {
                host: host.to_owned(),
                ..Carried::default()
            };
        }
        self.new_tree |= new_tree;
        for bare in created {
            if !self.created.contains(bare) {
                self.created.push(bare.clone());
            }
        }
    }

    /// What this carries to an attempt to `host`: all of it for the builder it was learned of, nothing for another.
    fn taken_for(self, host: &str) -> Carried {
        if self.host == host { self } else { Carried::default() }
    }
}

impl RustClient {
    /// The client over `ssh.exe`, whose builder root is the builder layout's, with the run's `seams`. ssh is looked for
    /// along the gate's own `PATH` only once a routed attempt needs it ([`found`]).
    pub(crate) fn ssh(seams: &TestSeams) -> RustClient {
        RustClient::over(None, seams)
    }

    /// The client of the local transport `transport` names, with the request's seams; else the line saying why the
    /// transport cannot be used, or why the stand-in `--test-remote-ssh` names cannot be found.
    #[cfg(feature = "test-seams")]
    pub(crate) fn local(seams: &TestSeams, transport: &str) -> Result<RustClient, String> {
        let local = local_transport(transport, seams.remote_ssh.as_deref())?;
        Ok(RustClient::over(Some(Box::new(local)), seams))
    }

    /// The client over `transport`, the ssh one looked for later when `None`, each limit its constant unless `seams`
    /// gives one in its place, as only a gate built with the test seams can.
    fn over(transport: Option<Box<dyn Transport>>, seams: &TestSeams) -> RustClient {
        RustClient {
            transport,
            git: GitBounds::of(seams),
            pause: retry::pause(seams),
            transfer_pauses: retry::transfer_pauses(seams),
            bound: retry::bound(seams),
            stall_ms: seams.stall_ms,
            timeout_ms: seams.remote_timeout_ms,
            backstop_ms: seams.remote_backstop_ms,
            snapshot_age: seams.remote_snapshot_age_ms.map_or(Duration::ZERO, Duration::from_millis),
            watch_call: seams.remote_watch_call.clone(),
            interrupt_at: seams.remote_interrupt_at.clone(),
            channel_fault: seams.remote_channel_fault.clone(),
            local_sdks: seams.local_sdks.clone(),
            dotnet_limit: dotnet_limit(seams),
            fetch_cap: seams.remote_fetch_cap_bytes.unwrap_or(watchdog::FETCH_CAP),
            apply_cap: seams.remote_apply_cap_bytes.unwrap_or(watchdog::APPLY_CAP),
            outputs_budget: seams.remote_outputs_budget_bytes.unwrap_or(OUTPUTS_BUDGET),
            inputs_budget: seams.remote_inputs_budget_bytes.unwrap_or(inputs::INPUTS_BUDGET),
            lfs_budget: seams.remote_lfs_budget_bytes.unwrap_or(lfs::LFS_BUDGET),
            log_cap: seams.remote_log_cap_bytes.unwrap_or(watchdog::LOG_CAP),
            own_start_fails: seams.own_start_fails,
            hint_from_pull: seams.remote_hint_from_pull,
            outputs_progress: seams.outputs_progress_ms.map_or(OUTPUTS_PROGRESS, Duration::from_millis),
            #[cfg(feature = "test-seams")]
            tamper: seams.remote_tamper.clone(),
            carried: Carried::default(),
        }
    }
}

/// The local transport `transport` names, its calls made through the stand-in `ssh` names when one is given; else the
/// line saying why the transport cannot be used, or why the stand-in cannot be found.
#[cfg(feature = "test-seams")]
pub(crate) fn local_transport(transport: &str, ssh: Option<&Path>) -> Result<Local, String> {
    // The transport's form is refused first, so `--test-remote-transport remote:x` reports the form and not a stand-in
    // it cannot find; the stand-in is then looked for, and the transport built with it.
    Local::new(transport, ssh)?;
    let stub = ssh.map(|word| stand_in(transport, word)).transpose()?;
    Local::new(transport, stub.as_deref())
}

/// The stand-in `--test-remote-ssh` names, resolved as the gate resolves its own programs: a name with a folder part
/// against the gate's working folder, a bare name along its `PATH` alone; else the line saying why it cannot be found.
#[cfg(feature = "test-seams")]
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
    match hold_key_waiting(call, Duration::ZERO).map_err(Some)? {
        KeyHold::Held(held) => Ok(held),
        KeyHold::Busy | KeyHold::Interrupted => Err(None),
    }
}

/// How long one slice of [`hold_key_waiting`]'s wait lasts, between two looks at the console interrupt. A run that
/// waits in slices of its own ([`crate::run`]) takes each one as a call's whole wait.
pub(crate) const KEY_WAIT_SLICE: Duration = Duration::from_millis(100);

/// How a wait for the work-folder key's lock ended.
pub(crate) enum KeyHold {
    /// The lock is held.
    Held(OwnedMutexGuard),
    /// Another holder still had it at the end of the wait.
    Busy,
    /// The console interrupt was raised during the wait.
    Interrupted,
}

/// The lock of `call`'s work-folder key, the one [`hold_key`] takes, waited for up to `wait` in slices of
/// [`KEY_WAIT_SLICE`], the call's interrupt looked at between them; `Err` with the line saying why it could not be
/// opened or taken.
pub(crate) fn hold_key_waiting(call: &RemoteCall<'_>, wait: Duration) -> Result<KeyHold, String> {
    let name = call.state.object_name(KernelObject::Routed { hash: key_hash(call.key) });
    let begun = Instant::now();
    loop {
        // A wait that times out gives its handle up, so each slice waits on one of its own.
        let mutex = NamedMutex::open(&name).map_err(|error| format!("gate: cannot open the mutex {name}: {error}"))?;
        let left = wait.saturating_sub(begun.elapsed());
        match mutex.acquire_owned(left.min(KEY_WAIT_SLICE)) {
            Ok(held) => return Ok(KeyHold::Held(held)),
            Err(AcquireError::Timeout) if left <= KEY_WAIT_SLICE => return Ok(KeyHold::Busy),
            Err(AcquireError::Timeout) => {}
            Err(AcquireError::Failed(error)) => return Err(format!("gate: cannot take the mutex {name}: {error}")),
        }
        if call.interrupt.is_some_and(Interrupt::is_raised) {
            return Ok(KeyHold::Interrupted);
        }
    }
}

/// The command of one call of `program`: its words, the system folder `system` as its working folder and
/// [`NO_CURRENT_FOLDER`] set, so neither ssh nor a program or library it looks for by name, a `ProxyCommand`'s say, is
/// found in the gate's own working folder, which may be the worktree.
fn call_command(program: Program, system: &Path) -> Command {
    let mut command = Command::new(program.program);
    command.args(program.args).current_dir(system).env(NO_CURRENT_FOLDER, "1");
    command
}

/// The transport in `slot`, the ssh one found along the gate's own `PATH` and kept there when the slot is empty; else
/// why ssh cannot be found.
fn found(slot: &mut Option<Box<dyn Transport>>) -> Result<&mut dyn Transport, String> {
    let transport = match slot {
        Some(transport) => transport,
        None => slot.insert(Box::new(Ssh::find()?)),
    };
    Ok(transport.as_mut())
}

impl RemoteClient for RustClient {
    fn hold_key(&self, call: &RemoteCall<'_>) -> Result<Option<OwnedMutexGuard>, Option<String>> {
        hold_key(call).map(Some)
    }

    fn hold_key_waiting(&self, call: &RemoteCall<'_>, wait: Duration) -> Result<KeyWait, String> {
        Ok(match hold_key_waiting(call, wait)? {
            KeyHold::Held(held) => KeyWait::Held(Some(held)),
            KeyHold::Busy => KeyWait::Busy,
            KeyHold::Interrupted => KeyWait::Interrupted,
        })
    }

    /// Looks for ssh on the routed attempt's first need of it, so a run that cannot find it falls back `client` before
    /// it takes a key lock, a ticket or a remote slot.
    fn ready(&mut self) -> Result<(), String> {
        found(&mut self.transport).map(drop)
    }

    fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
        self.attempted(call, Phases::All)
    }

    fn poll_drain(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
        self.attempted(call, Phases::DrainPoll)
    }
}

/// What one attempt of the client makes: every phase of the run, or the preflight alone of a drain poll.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phases {
    All,
    DrainPoll,
}

impl RustClient {
    /// One attempt of `call` with fresh limits, making `phases`, and its outcome. A drain poll says neither the raised
    /// stall line nor the missing handler's, which the run's own attempts say.
    fn attempted(&mut self, call: &RemoteCall<'_>, phases: Phases) -> RemoteOutcome {
        let transport = match found(&mut self.transport) {
            Ok(transport) => transport,
            Err(why) => return RemoteOutcome::not_started(why),
        };
        // The worktrees are known only now: a transport whose program lies inside any of them starts nothing.
        let roots = std::iter::once(call.root).chain(call.siblings.iter().map(super::Sibling::root));
        for root in roots {
            if let Err(why) = transport.judge(Path::new(root)) {
                return RemoteOutcome::not_started(why);
            }
        }
        let started = Instant::now();
        let one = self.watch_call.as_deref();
        // With `--test-remote-watch-call`, the stall seam is that one call's, and the run keeps its own.
        let stall_ms = if one.is_some() { None } else { self.stall_ms };
        let (stall, raised) = watchdog::stall_window(call.stall, stall_ms);
        let mut attempt = Attempt {
            call,
            transport: &*transport,
            git: self.git,
            lock_wait: self.git.limit,
            pause: self.pause,
            transfer_pauses: self.transfer_pauses,
            transfer_failed: false,
            limits: Limits {
                started,
                stall,
                command: Duration::from_secs(call.timeout),
                backstop: self.backstop_ms.map_or_else(|| watchdog::backstop(call.timeout), Duration::from_millis),
                transfer: TRANSFER_CEILING,
                bound: self.bound,
                transfer_from: None,
            },
            one_call: one.map(|name| OneCall {
                name,
                stall: self.stall_ms.map(Duration::from_millis),
                ceiling: self.timeout_ms.map(Duration::from_millis),
            }),
            interrupt_at: self.interrupt_at.as_deref(),
            channel_fault: self.channel_fault.as_deref(),
            local_sdks: self.local_sdks.as_deref(),
            dotnet_limit: self.dotnet_limit,
            fetch_cap: self.fetch_cap,
            apply_cap: self.apply_cap,
            outputs_budget: self.outputs_budget,
            inputs_budget: self.inputs_budget,
            lfs_budget: self.lfs_budget,
            log_cap: self.log_cap,
            own_start_fails: self.own_start_fails,
            hint_from_pull: self.hint_from_pull,
            #[cfg(feature = "test-seams")]
            tamper: self.tamper.as_deref(),
            brought_back: None,
            dropped_no_build: call.dropped_no_build,
            split: Split::default(),
            lines: Vec::new(),
            problem: None,
            hang_said: false,
            command_started: None,
            snapshot_taken: None,
            snapshot_age: self.snapshot_age,
            outputs_progress: self.outputs_progress,
            pulling: None,
            pull_periods_said: Cell::new(0),
            pulled_bytes: 0,
            pulled_last: None,
            adjusted: None,
            box_killed: None,
            held_run: None,
            ceiling: None,
            places: Vec::new(),
            local_major: None,
            warm: None,
            marker_seen: false,
            raised: None,
            backstop_seam: self.backstop_ms.map(Duration::from_millis),
            carried: match phases {
                Phases::All => std::mem::take(&mut self.carried).taken_for(call.host()),
                Phases::DrainPoll => Carried::default(),
            },
            log_only: phases == Phases::DrainPoll,
        };
        if phases == Phases::DrainPoll {
            let (end, polled) = attempt.drain_polled();
            if let Some(prepared) = polled {
                self.carried.add(call.host(), prepared.new_tree, &prepared.created);
            }
            return attempt.outcome(end);
        }
        if let Some(line) = raised {
            attempt.say(line);
        }
        if attempt.interrupt_at.is_some() && call.interrupt.is_none() {
            attempt.told(NO_HANDLER);
        }
        let end = match attempt.phases() {
            Ok(end) | Err(end) => end,
        };
        // An attempt the drain refused, before its run call or at it, never used the work folder its preflight found:
        // the next attempt takes it on.
        if matches!(end, End::Draining(_)) {
            self.carried = std::mem::take(&mut attempt.carried);
        }
        attempt.outcome(end)
    }
}

/// What the line of a run that stays here for one of its pair's repositories starts with.
const NOT_ROUTED: &str = "gate: not routed: ";
/// Why a worktree whose lock another run holds past the lock wait keeps the run here: the holder may be another routed
/// run's snapshot, push or sync-back.
const BUSY: &str = "is busy with another routed run";
/// Why a pair's repository keeps the run here when git finds it elsewhere after its save than at its check.
const MOVED_GIT: &str = "changed its git location while it was held";

/// The status of an attempt that ended `status`, the warm-up having ended as `warm`: a warm-up that failed never hands
/// back a passing status, as one the box did not confirm ends 0.
fn warm_status(status: i64, warm: Option<&WarmEnd>) -> i64 {
    if status == 0 && matches!(warm, Some(WarmEnd::Failed { .. })) {
        FAILED
    } else {
        status
    }
}

/// What `attempt`'s run call carries of its outputs ([`outputs::sent`]): nothing for a warm-up, which reads, saves and
/// marks no outputs state whatever output globs its call holds.
fn outputs_sent<'a>(attempt: &mut Attempt<'_, 'a>) -> Result<outputs::Sent<'a>, End> {
    match attempt.call.warmth {
        Warmth::WarmUp { .. } => Ok(outputs::Sent::nothing()),
        Warmth::Unasked | Warmth::Consented(_) => outputs::sent(attempt),
    }
}

/// The end of a run that stays here for its repository said as `who`, the main one's folder name or `sibling {name}`,
/// for `why`: `gate: not routed: {who} {why}`.
fn not_routed(who: &str, why: impl std::fmt::Display) -> End {
    End::NotRouted(format!("{NOT_ROUTED}{who} {why}"))
}

/// How the run's repository `name` at `at` in its order is said in a refusal: the main one, first, by its folder name;
/// a sibling as `sibling {name}`.
fn who(at: usize, name: &str) -> String {
    if at == 0 { name.to_owned() } else { sibling_who(name) }
}

/// The end of a run whose worktree `name`'s lock another routed run holds past the wait, the main one's, named
/// `leaf`, or a sibling's.
fn busy(leaf: &str, name: &str) -> End {
    let who = if name == leaf { name.to_owned() } else { sibling_who(name) };
    not_routed(&who, BUSY)
}

/// `end`, a failure of the pair's sibling `name`, as the run staying here naming it; any other end as it is.
fn sibling_end(name: &str, end: End) -> End {
    match end {
        End::Failed(why) => not_routed(&sibling_who(name), format_args!("cannot be sent: {why}")),
        end => end,
    }
}

/// `end` of the pair's repository at `at` in the run's order, named `name`: the main one's, first, as it is; a
/// sibling's as [`sibling_end`] has it.
fn repo_end(at: usize, name: &str, end: End) -> End {
    if at == 0 { end } else { sibling_end(name, end) }
}

/// Whether the paths `a` and `b` name one folder as git spells them: equal but for `\` against `/` and ASCII case.
fn same_path(a: &str, b: &str) -> bool {
    a.replace('\\', "/").eq_ignore_ascii_case(&b.replace('\\', "/"))
}

/// The single repository's root `call` names, opened following links without delete sharing, so it cannot be moved
/// or replaced while the handle is open; one whose id cannot be read, or is not the one the route was decided for,
/// keeps the run here naming it, as a pair's root does ([`Held::hold`]).
fn root_checked(call: &RemoteCall<'_>) -> Result<File, End> {
    let cannot = |error: io::Error| not_routed(call.leaf, format_args!("cannot be checked: {error}"));
    let folder = open_followed(Path::new(call.root), Share::NoDelete).map_err(cannot)?;
    let (volume, file) = gate_win32::file_id(&folder).map_err(cannot)?;
    if (gate_router::FolderId { volume, file }) != call.root_id {
        return Err(not_routed(call.leaf, "is not the folder the route was decided for"));
    }
    Ok(folder)
}

/// The pair's repository `repo` as its git calls ask about it, bounded at the pair's parent folder `ceiling`.
fn bounded<'b>(repo: &'b Repo, ceiling: &'b Path) -> Bounded<'b> {
    Bounded {
        root: Path::new(&repo.root),
        ceiling,
        place: None,
    }
}

/// The line of a snapshot listing that cannot be read, for `why`: the main repository's, `of` `None`, skips the
/// toolchain check; the sibling `of`'s skips its SDK check.
fn skipped_listing(of: Option<&str>, why: &str) -> String {
    match of {
        None => format!("gate: cannot list the snapshot's files: {why}; skipping the toolchain check"),
        Some(name) => format!("gate: cannot list the snapshot's files of {name}: {why}; skipping its SDK check"),
    }
}

/// A repository the run sends: its name in the run's lines, the main one's folder name or a sibling's; its worktree's
/// root; the name its bare repository on the builder is asked for by; and that bare repository as the box names it.
struct Repo {
    name: String,
    root: String,
    on_builder: String,
    bare: String,
}

/// What a run holds from before its snapshots until its last save: a pair's folders ([`Held`]), `None` for a run of one
/// repository, and the worktree locks of its root and its siblings, so no other run's sync-back writes one of them
/// while it is snapshotted and pushed.
struct RunHold {
    held: Option<Held>,
    _locks: Vec<Locked>,
}

/// One repository's push: the repository, at `at` in the run's order, its snapshot, and whether the preflight created
/// its bare repository just now.
struct Push<'r> {
    at: usize,
    repo: &'r Repo,
    snapshot: &'r Snapshot,
    created: bool,
}

/// How the attempt ended.
#[derive(Debug)]
pub(super) enum End {
    /// The run call ended with this status, the command's own.
    Status(i64),
    /// The builder cannot take the run, for this reason: status 75, and the run falls back here.
    Refused(FallbackReason),
    /// The builder's work folder does not hold the PC's build the run's `SentDigest` names: the box's `sent` tag, which
    /// no fallback answers, as the client runs the call once more with `--no-build` dropped ([`Attempt::run_called`]).
    SentRefused,
    /// The client failed, for this reason: status 1, with no fallback.
    Failed(String),
    /// The console interrupt ended it.
    Interrupted,
    /// The watchdog killed the run call, and the command was stopped, or it killed a fetch after it, the sync-back's or
    /// the outputs call, at its transfer ceiling or, the outputs call, for its silence: status 124.
    Killed,
    /// The run call's connection was lost: it exited 255, ssh's own failure status, other than before the banner. The
    /// run call returns it before anything is stopped, with where the run call's own lines are among the attempt's, so
    /// a fallback can leave them out. Nothing is synced back: status 255.
    Lost(Range<usize>),
    /// One of a sibling pair's repositories cannot be sent as the route decided, for the reason this line, `gate: not
    /// routed: sibling {name} {why}`, says: the run falls back `client` before the run call.
    NotRouted(String),
    /// A transfer after the command, the sync-back's or the outputs call, failed as a connection fails on every attempt,
    /// for this reason: [`Attempt::transferred`] turns it into the TRANSFER FAILED line and the run's status.
    TransferFailed(String),
    /// The builder's drain refused a preflight, inputs, LFS or run call with its `drain` tag, which no fallback
    /// answers: status 75 with no reason, and the run waits behind the drain.
    Draining(Drain),
}

/// What the preflight found: the bare repositories it created just now, as its `gate: remote created {bare}` lines name
/// them, the builder's boot time, `None` when it said none, the SDKs it lists, `None` when it said no SDK line, the
/// tools its `Needs` named, and what the builder said of each of them, in that order, a tool it said nothing of left
/// out, and whether it said the work folder's tree is new ([`new_work_tree`]).
struct Prepared {
    created: Vec<String>,
    new_tree: bool,
    boot: Option<String>,
    sdks: Option<Vec<String>>,
    sent: Vec<String>,
    tools: Vec<(String, Reported)>,
}

/// The versions, `major.minor.patch`, of the builder's installs a routed run uses, as the parity step chose them after
/// the preflight: its Godot install and its Unreal Engine install, each `None` when none is chosen; and the names the
/// main repo's manifest `[env]` sets to the Unreal Engine install's root, sent whether or not one is chosen.
struct Chosen {
    godot: Option<String>,
    unreal: Option<String>,
    engine_env: Vec<String>,
}

/// What the snapshot's tree gives the parity check, read before the preflight: the git that listed it, the worktree it
/// was asked in, the names of its files and submodules, and its `.gate/toolchain.toml`, `None` when it has none or is
/// a sibling's, which is not read, `Err` with why when it cannot be read or is no regular file, and the Godot versions
/// its project files pin, with a line per file left unread. A sibling's tree gives no manifest and no pins: only the
/// main repository's say what the run needs.
struct Tree {
    git: Git,
    folder: PathBuf,
    names: Vec<String>,
    /// The manifest; for a main repository whose tree has none, its copy in the declarations folder this PC's routing
    /// file names, if any ([`Attempt::declared_manifest`]).
    manifest: Option<Result<Manifest, String>>,
    /// The full path of the manifest when it is that copy; `None` for the tree's own.
    manifest_file: Option<String>,
    /// The `Godot.NET.Sdk` versions its `*.csproj` files name, each version once, in the order the tree lists them.
    godot_pins: Vec<Pin>,
    /// One line per project file whose pin could not be read, in the same order.
    godot_skips: Vec<String>,
    /// The main repository's blobs the LFS listing reads, from the same listing; none for a sibling's.
    small: lfs::Small,
    /// The `/`-separated paths of the main repository's `.gitattributes` files, at any depth, from the same listing;
    /// none for a sibling's.
    attributes: Vec<String>,
}

/// The repo's toolchain manifest, as its path at the repo root is spelled.
const MANIFEST_PATH: &str = ".gate/toolchain.toml";
/// The manifest's name in a declarations folder.
const MANIFEST_NAME: &str = "toolchain.toml";

/// One entry of the snapshot's tree as `git ls-tree -l -z` lists one: the mode, the object type (`blob`, `tree` or
/// `commit`), the object id, the size of a blob (`None` for any other entry, which git gives `-`) and the path.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    mode: String,
    kind: String,
    oid: String,
    size: Option<u64>,
    path: String,
}

/// The entries of a `git ls-tree -l -z` listing, each `<mode> <type> <oid> <size>\t<path>` ended by a NUL, the size
/// padded with spaces and `-` for an entry that is no blob; `None` when an entry is not of that form.
fn entries_of(listing: &str) -> Option<Vec<Entry>> {
    listing
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let (meta, path) = record.split_once('\t')?;
            let mut parts = meta.split_ascii_whitespace();
            let (mode, kind, oid, size) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            let size = match size {
                "-" => None,
                size => Some(size.parse().ok()?),
            };
            parts.next().is_none().then(|| Entry {
                mode: mode.to_owned(),
                kind: kind.to_owned(),
                oid: oid.to_owned(),
                size,
                path: path.to_owned(),
            })
        })
        .collect()
}

/// The blobs of the snapshot's tree `entries` the LFS listing reads ([`lfs::is_small_blob`]), and whether the tree
/// holds a `.gitattributes` file at any depth.
fn small_blobs_of(entries: &[Entry]) -> lfs::Small {
    let blobs = entries
        .iter()
        .filter(|entry| entry.size.is_some_and(|size| lfs::is_small_blob(&entry.mode, &entry.kind, size)))
        .map(|entry| lfs::Blob {
            path: entry.path.clone(),
            id: entry.oid.clone(),
        })
        .collect();
    let attributes = entries.iter().any(|entry| entry.kind == "blob" && lfs::is_attributes(&entry.path));
    lfs::Small { blobs, attributes }
}

/// The entry among `entries` that is the repo's manifest, its path matched ignoring ASCII case: the one spelled exactly
/// when it is a regular file, else the only match when it is one. `Err` with why for a match that is a submodule, a
/// symbolic link, a folder or another kind of entry, and for two matches that differ only in letter case. `None`
/// when nothing matches.
fn manifest_entry(entries: &[Entry]) -> Option<Result<&Entry, String>> {
    let matches: Vec<&Entry> = entries.iter().filter(|entry| entry.path.eq_ignore_ascii_case(MANIFEST_PATH)).collect();
    if let Some(exact) = matches
        .iter()
        .copied()
        .find(|entry| entry.path == MANIFEST_PATH && regular(entry).is_ok())
    {
        return Some(Ok(exact));
    }
    match matches.as_slice() {
        [] => None,
        [only] => Some(regular(only)),
        [first, second, ..] => Some(Err(format!("{} and {} differ only in letter case", first.path, second.path))),
    }
}

/// This PC's routing file, through which the declarations folder is found: `None` when there is none, and when it
/// cannot be used, the line saying why pushed onto `lines`.
fn declarations_config(lines: &mut Vec<String>) -> Option<Config> {
    match route::remote_json_path(std::env::var_os("LOCALAPPDATA").as_deref()).map(|path| route::read_remote_json(&path)) {
        None => None,
        Some(Ok(config)) => config,
        Some(Err(problem)) => {
            lines.push(format!(
                "gate: ignored {}: {}; reading no declarations folder",
                problem.path.display(),
                escaped(&problem.reason)
            ));
            None
        }
    }
}

/// Whether the snapshot's `entries` hold a `.gate` (in any letter case) that is not a folder: the repo's own answer
/// that it has no manifest, so no declarations folder is read for one.
fn gate_is_no_folder(entries: &[Entry]) -> bool {
    entries
        .iter()
        .any(|entry| entry.path.eq_ignore_ascii_case(".gate") && entry.kind != "tree")
}

/// `entry` when it is a regular file, a blob of mode 100644 or 100755; else `Err` with what it is.
fn regular(entry: &Entry) -> Result<&Entry, String> {
    let path = &entry.path;
    match (entry.mode.as_str(), entry.kind.as_str()) {
        ("100644" | "100755", "blob") => Ok(entry),
        ("160000", _) => Err(format!("{path} is a submodule, not a file")),
        ("120000", _) => Err(format!("{path} is a symbolic link, not a file")),
        (_, "tree") => Err(format!("{path} is a folder, not a file")),
        (mode, _) => Err(format!("{path} is not a regular file (mode {mode})")),
    }
}

/// The most project files a tree's Godot pins are read from: files past it are not read, and one line names how many
/// were left.
const CSPROJ_FILES: usize = 64;

/// The most bytes of one project file read for its Godot pin.
const CSPROJ_BYTES: usize = 256 * 1024;

/// The most bytes one project file's frame takes in a `git cat-file --batch` answer beside its own text: the object id,
/// the type, the size and the line ends, with room to spare.
const CSPROJ_FRAME: usize = 128;

/// The most bytes one `git cat-file --batch` read of the project files can write: [`CSPROJ_FILES`] files of at most
/// [`CSPROJ_BYTES`] each, every frame within [`CSPROJ_FRAME`].
const CSPROJ_READ: usize = CSPROJ_FILES * (CSPROJ_BYTES + CSPROJ_FRAME);

/// The most bytes one `git cat-file --batch-check` answer for the project files can write: [`CSPROJ_FILES`] lines of an
/// object size and a line end, with room to spare.
const CSPROJ_SIZES: usize = CSPROJ_FILES * 64;

/// What the name of a project file ends in.
const CSPROJ_SUFFIX: &str = ".csproj";

/// Whether `path` names a project file: its last characters [`CSPROJ_SUFFIX`], in any ASCII letter case, at whatever
/// depth of the tree it lies.
fn is_csproj(path: &str) -> bool {
    path.get(path.len().saturating_sub(CSPROJ_SUFFIX.len())..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(CSPROJ_SUFFIX))
}

/// The project files among a tree's `entries` whose Godot pins are read, with the line naming the files left past
/// [`CSPROJ_FILES`], `None` when none were left: the first [`CSPROJ_FILES`] entries whose path ends in `.csproj`, in
/// the listing's order, a folder named like a project file left out as no file at all.
fn csproj_files(entries: &[Entry]) -> (Vec<&Entry>, Option<String>) {
    let mut files: Vec<&Entry> = entries.iter().filter(|entry| entry.kind != "tree" && is_csproj(&entry.path)).collect();
    let left = files.len().saturating_sub(CSPROJ_FILES);
    files.truncate(CSPROJ_FILES);
    let line = (left > 0).then(|| format!("gate: {left} more project files not read for a Godot pin; not checking them"));
    (files, line)
}

/// Why the project file `entry` is no file the run can read, worded as [`regular`] words the same kinds without the
/// path the skip line names already; `None` when it is a regular file.
fn unreadable_csproj(entry: &Entry) -> Option<String> {
    match (entry.mode.as_str(), entry.kind.as_str()) {
        ("100644" | "100755", "blob") => None,
        ("160000", _) => Some("it is a submodule, not a file".to_owned()),
        ("120000", _) => Some("it is a symbolic link, not a file".to_owned()),
        (_, "tree") => Some("it is a folder, not a file".to_owned()),
        (mode, _) => Some(format!("it is not a regular file (mode {mode})")),
    }
}

/// The line said for a project file whose Godot pin cannot be read, the file and why both escaped as other lines escape
/// theirs.
fn godot_skip_line(file: &str, why: &str) -> String {
    format!("gate: cannot read the Godot pin of {}: {}; not checking it", escaped(file), escaped(why))
}

/// Why a project file is left unread: its text is past [`CSPROJ_BYTES`].
fn oversized_reason() -> String {
    format!("its text is more than {} KiB", CSPROJ_BYTES / 1024)
}

/// The run's failure for a git answer no project file's pin can be read from, `why` naming what it answered.
fn csproj_end(why: &str) -> End {
    End::Failed(format!("gate: cannot read the Godot pin of the project files: {}", escaped(why)))
}

/// The size of each blob of `files`, from the `git cat-file --batch-check=%(objectsize)` answer `answer`, one line for
/// each in their order; `Err` with what it answered when it names no size for one of them.
fn sizes_of(answer: &[u8], files: &[&Entry]) -> Result<Vec<u64>, String> {
    let answer = String::from_utf8_lossy(answer);
    let lines: Vec<&str> = answer.lines().collect();
    if lines.len() != files.len() {
        return Err(format!("it answered {} line(s) for {} project file(s)", lines.len(), files.len()));
    }
    lines
        .iter()
        .zip(files)
        .map(|(line, file)| {
            line.trim()
                .parse::<u64>()
                .map_err(|_| format!("it answered '{}' for {}", line.trim(), file.path))
        })
        .collect()
}

/// The text of each blob of `files`, from the `git cat-file --batch` answer `answer`, one for each in their order: each
/// object's header line, then its bytes, then the line end after them; `Err` with what it answered when the answer is
/// not shaped so.
fn texts_of(answer: &[u8], files: &[&Entry]) -> Result<Vec<Vec<u8>>, String> {
    let mut texts = Vec::with_capacity(files.len());
    let mut rest = answer;
    for file in files {
        let Some(end) = rest.iter().position(|byte| *byte == b'\n') else {
            return Err(format!("it gave no header for {}", file.path));
        };
        let header = String::from_utf8_lossy(&rest[..end]);
        let Some(size) = header.rsplit(' ').next().and_then(|size| size.trim().parse::<usize>().ok()) else {
            return Err(format!("it named no size for {}: '{}'", file.path, header));
        };
        let body = &rest[end + 1..];
        if body.len() < size + 1 {
            return Err(format!("it gave {} byte(s) for {}", body.len(), file.path));
        }
        texts.push(body[..size].to_vec());
        rest = &body[size + 1..];
    }
    Ok(texts)
}

/// The project files of `files` whose blobs are read, and the line for each file no file at all, in the order of
/// `files`.
fn readable_files<'a>(files: &[&'a Entry]) -> (Vec<&'a Entry>, Vec<String>) {
    let mut readable = Vec::new();
    let mut skips = Vec::new();
    for file in files {
        match unreadable_csproj(file) {
            Some(why) => skips.push(godot_skip_line(&file.path, &why)),
            None => readable.push(*file),
        }
    }
    (readable, skips)
}

/// The project files of `files` within [`CSPROJ_BYTES`], as the sizes `sizes` gives them one for one, and the line for
/// each file left past it; `Err` when the sizes do not answer for `files` one for one.
fn within_cap<'a>(files: &[&'a Entry], sizes: Vec<u64>) -> Result<(Vec<&'a Entry>, Vec<String>), End> {
    if sizes.len() != files.len() {
        return Err(csproj_end(&format!(
            "it named the size of {} of {} project file(s)",
            sizes.len(),
            files.len()
        )));
    }
    let mut kept = Vec::new();
    let mut skips = Vec::new();
    for (file, size) in files.iter().zip(sizes) {
        if size > CSPROJ_BYTES as u64 {
            skips.push(godot_skip_line(&file.path, &oversized_reason()));
        } else {
            kept.push(*file);
        }
    }
    Ok((kept, skips))
}

/// The pins the project file texts `texts` give, one for each of `files`, and the line for each whose bytes are not
/// UTF-8; `Err` when the texts do not answer for `files` one for one.
fn pins_of(files: &[&Entry], texts: Vec<Vec<u8>>) -> Result<(Vec<Pin>, Vec<String>), End> {
    if texts.len() != files.len() {
        return Err(csproj_end(&format!("it read {} of {} project file(s)", texts.len(), files.len())));
    }
    let mut pins: Vec<Pin> = Vec::new();
    let mut skips = Vec::new();
    for (file, bytes) in files.iter().zip(texts) {
        let Ok(text) = std::str::from_utf8(&bytes) else {
            skips.push(godot_skip_line(&file.path, "not valid UTF-8"));
            continue;
        };
        for pin in toolchain::godot_sdk_pins(text) {
            if !pins.contains(&pin) {
                pins.push(pin);
            }
        }
    }
    Ok((pins, skips))
}

/// The `Godot.NET.Sdk` versions the project files among a tree's `entries` pin ([`toolchain::godot_sdk_pins`]), and the
/// line for each file left out: the files [`csproj_files`] names, `size` asked for their sizes as one
/// `cat-file --batch-check` answers them and `read` handed the ones within [`CSPROJ_BYTES`] as one `cat-file --batch`
/// reads them, so a file past that cap is left unread. The skips come in this order: a file that is no file at all,
/// then a file past [`CSPROJ_BYTES`], then a file whose text is not UTF-8, each in the tree's order, then the count of
/// the files left past [`CSPROJ_FILES`]. The same version pinned twice is one pin.
fn csproj_pins(
    entries: &[Entry],
    size: impl FnOnce(&[&Entry]) -> Result<Vec<u64>, End>,
    read: impl FnOnce(&[&Entry]) -> Result<Vec<Vec<u8>>, End>,
) -> Result<(Vec<Pin>, Vec<String>), End> {
    let (files, capped) = csproj_files(entries);
    let (readable, mut skips) = readable_files(&files);
    let sizes = if readable.is_empty() { Vec::new() } else { size(&readable)? };
    let (kept, over) = within_cap(&readable, sizes)?;
    skips.extend(over);
    let texts = if kept.is_empty() { Vec::new() } else { read(&kept)? };
    let (pins, unread) = pins_of(&kept, texts)?;
    skips.extend(unread);
    skips.extend(capped);
    Ok((pins, skips))
}

impl Tree {
    /// The `Needs` the preflight sends for the tree ([`toolchain::routed_needs`]): the tools its manifest declares, and
    /// the exact versions its project files pin of godot when the manifest does not name godot itself; a Godot command
    /// (`godot_command`) has the builder list its installs even when the repo requires no version.
    fn needs(&self, godot_command: bool) -> Vec<(String, String)> {
        toolchain::routed_needs(self.manifest.as_ref(), &self.godot_pins, godot_command)
    }

    /// Whether the repo's Godot pins were read ([`toolchain::godot_pins_read`]): its manifest reads or is absent, and
    /// every project file's pin was read.
    fn godot_pins_read(&self) -> bool {
        toolchain::godot_pins_read(self.manifest.as_ref(), &self.godot_skips)
    }
}

/// The ceiling of each transfer, whatever `--timeout` says: the setup up to the end of the push, counted from the
/// attempt's start; the inputs' diff and put calls, and each fetch after the run call, the sync-back's and the
/// outputs', each counted from its own start.
pub(super) const TRANSFER_CEILING: Duration = Duration::from_mins(30);

/// The attempt's clock, which the setup's calls count on, and the limits the client's calls are watched by: the stall window; the
/// command's ceiling, `--timeout`, which the box counts in load-adjusted time and the backstop line names; the run
/// call's backstop, which only the run call is cut at; the transfer ceiling, which the setup's calls and each fetch
/// after the run call are cut at; and the bound of the bounded calls, the `stop` and the `boot`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    started: Instant,
    stall: Duration,
    command: Duration,
    backstop: Duration,
    transfer: Duration,
    bound: Duration,
    /// Where a fetch's or a put's transfer ceiling counts from when not from its own start: the first attempt's start
    /// of a transfer after the command that is made again ([`Attempt::transfer_call`]), whose attempts share one ceiling.
    transfer_from: Option<Instant>,
}

impl Limits {
    /// Why a setup call watched by these limits is killed at `now`, nothing having come from it for `idle`: stalled
    /// once silent for the stall window, else cut at the transfer ceiling counted from [`Limits::started`]; `None`
    /// while it may run on.
    fn setup_cut(self, now: Instant, idle: Duration) -> Option<Cut> {
        let elapsed = now.saturating_duration_since(self.started);
        match watchdog::kill(idle, elapsed, self.stall, self.transfer)? {
            Kill::Stalled => Some(Cut::Watchdog(Kill::Stalled)),
            Kill::Backstop => Some(Cut::Ceiling(self.transfer)),
        }
    }
}

/// `--test-remote-watch-call`: the one call, `preflight`, `push`, `inputs-diff` (the inputs' diff call), `inputs` (the
/// inputs' put call), `run`, `ls-remote`, `fetch` or `outputs`, that the
/// stall window of `--test-stall-ms` applies to, and the ceiling of `--test-remote-timeout-ms` for any call but `run`,
/// counted from its own start: its transfer ceiling. For `ls-remote`, which has a deadline rather than a stall window,
/// the ceiling is that deadline.
#[derive(Clone, Copy)]
struct OneCall<'c> {
    name: &'c str,
    stall: Option<Duration>,
    ceiling: Option<Duration>,
}

/// How a call is watched while it runs.
#[derive(Clone, Copy, Debug)]
pub(super) enum Watch {
    /// By the run's watchdog under these limits: killed once it sent nothing for the stall window, or it outlived the
    /// run call's backstop counted from its own start, so a call made again after one that failed before the banner,
    /// and the pause before it, count nothing against it: the run call.
    Run(Limits),
    /// Killed once it sent nothing for the stall window of these limits, or at the transfer ceiling counted from the
    /// attempt's start: the setup's calls, the preflight and the push.
    Setup(Limits),
    /// Killed once it sent nothing for the stall window of these limits, or at the transfer ceiling counted from its own
    /// start, or from its transfer's first attempt when it is made again ([`Watch::counted_from`]), or once its job
    /// wrote more bytes than the cap when it has one: the sync-back's fetch, the outputs call, and the inputs' diff
    /// call, which has no cap.
    Fetch(Limits, Option<u64>),
    /// Killed once, while its feeder still writes the archive, neither a byte was fed nor one of its output came for
    /// the stall window of these limits, or at the transfer ceiling counted from its own start, which alone watches it
    /// once the feeder has finished: the inputs' put call, whose archive goes the other way and whose box installs it
    /// without a word.
    Put(Limits),
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
                let elapsed = now.saturating_duration_since(begun);
                watchdog::kill(idle, elapsed, limits.stall, limits.backstop).map(Cut::Watchdog)
            }
            Watch::Setup(limits) => limits.setup_cut(now, idle),
            Watch::Fetch(limits, _) | Watch::Put(limits) if idle >= limits.stall => Some(Cut::Silent(limits.stall)),
            Watch::Fetch(limits, _) | Watch::Put(limits) => {
                let from = limits.transfer_from.unwrap_or(begun);
                (now.saturating_duration_since(from) >= limits.transfer).then_some(Cut::Ceiling(limits.transfer))
            }
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
            Watch::Run(_) | Watch::Setup(_) | Watch::Put(_) | Watch::Deadline(_) | Watch::Bounded(_) => None,
        }
    }

    /// The watch of one attempt of a transfer after the command whose first attempt began at `first`: a fetch's
    /// transfer ceiling counts from then, so every attempt shares it; any other watch as it is.
    fn counted_from(self, first: Instant) -> Watch {
        match self {
            Watch::Fetch(mut limits, cap) => {
                limits.transfer_from = Some(first);
                Watch::Fetch(limits, cap)
            }
            other => other,
        }
    }

    /// The transfer ceiling a call watched so is cut at: a fetch's; `None` for a call [`Attempt::transfer_call`] makes
    /// with any other watch, a deadline's.
    fn transfer_ceiling(self) -> Option<Duration> {
        match self {
            Watch::Fetch(limits, _) => Some(limits.transfer),
            Watch::Run(_) | Watch::Setup(_) | Watch::Put(_) | Watch::Deadline(_) | Watch::Bounded(_) => None,
        }
    }

    /// How long the client waits between two looks at a call watched so.
    fn tick(self) -> Duration {
        match self {
            Watch::Run(limits) | Watch::Setup(limits) | Watch::Fetch(limits, _) | Watch::Put(limits) => watchdog::tick(limits.stall),
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact the call's streams said, read by name; no two form a state machine"
)]
pub(super) struct Called {
    pub(super) status: i32,
    pub(super) cut: Option<Cut>,
    /// A reader's problem, or a failure to end the call, which a kill keeps as a note rather than as the run's failure.
    pub(super) problem: Option<String>,
    output_seen: bool,
    first_error: Option<String>,
    pub(super) last_error: Option<String>,
    pub(super) lines: Vec<String>,
    /// Its last standard error line that is the box's outputs line, however many lines came after it, and the size
    /// line heard just before it.
    outputs: Option<outputs::OutputsLines>,
    /// Its last standard error line that is the box's back commits line, however many lines came after it.
    back_commits: Option<String>,
    /// Whether it said on standard error that the box skipped outputs for names this PC cannot take.
    names_skipped: bool,
    /// Whether the box said on standard error, stamped as its own line, that the run's cross build brought no Windows
    /// outputs back ([`is_cross_not_brought_back`]).
    cross_not_brought_back: bool,
    /// Whether the box said the run's start marker on standard error.
    started: bool,
    /// What the box's refusal tag for the run named on standard error, the first when it said more than one.
    refused: Option<Tagged>,
    /// What the box's `drain` refusal and the place and barrier lines after it said, once its tag was heard.
    drain: Option<Drain>,
    /// The kill the box's kill tag for the run named on standard error, the last when it said more than one.
    box_killed: Option<BoxKill>,
    /// The load-adjusted seconds the box's last adjusted line on standard error said.
    adjusted: Option<f64>,
    /// The bytes its job had written at the last look, for a call whose watch caps them; `None` for any other call, or
    /// when no look succeeded.
    written: Option<u64>,
    /// What its raw copy had written into its sink when the watch cut it, for a call with a raw copy the watch cut;
    /// `None` for any other.
    pub(super) received: Option<Received>,
    /// How the data channel an outputs call asked for ended on the PC's side; `None` for a call that asked for none.
    pub(super) channel: Option<channel::Dialled>,
    /// Its last standard error line saying what the box sent over a data channel ([`gate_wire::channel_sent_line`]).
    pub(super) channel_sent: Option<String>,
}

/// What a transfer after the command came to ([`Attempt::transfer_call`]): the call that counts, whether it failed as
/// a connection fails with no retry left, and the reason of the first call that failed so, when one was made again.
pub(super) struct Transferred {
    pub(super) called: Called,
    pub(super) exhausted: bool,
    pub(super) first: Option<String>,
}

/// What a call's raw copy had written into its sink when the watch cut the call: the bytes, and how long the call had
/// run by then, counted from its own start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Received {
    pub(super) bytes: u64,
    pub(super) after: Duration,
}

impl Called {
    /// Whether the call's job wrote more than [`RETRY_WRITTEN`]: more than a failure before the banner writes, so a call
    /// that reached a builder, which is not made again.
    fn wrote_much(&self) -> bool {
        self.written.is_some_and(|bytes| bytes > RETRY_WRITTEN)
    }

    /// Whether the call, a transfer after the command by a program that exits `fail_status` when ssh failed, failed as a
    /// connection fails, so it is made again ([`retry::transfer_failed`]).
    pub(super) fn transfer_failed(&self, fail_status: i32) -> bool {
        retry::transfer_failed(&self.ended(fail_status))
    }

    /// Whether the call failed before the banner, for a program that exits `fail_status` when ssh failed.
    fn before_the_banner(&self, fail_status: i32) -> bool {
        retry::before_the_banner(&self.ended(fail_status))
    }

    /// How the call, by a program that exits `fail_status` when ssh failed, ended, as both retry rules read it.
    fn ended(&self, fail_status: i32) -> Ended<'_> {
        Ended {
            status: self.status,
            fail_status,
            output_seen: self.output_seen,
            first_error: self.first_error.as_deref(),
            started: self.started,
            cut: self.cut,
            said: self.said().collect(),
        }
    }

    /// Every line of the call it kept: its first and its last standard error lines, and its last lines but heartbeats.
    pub(super) fn said(&self) -> impl Iterator<Item = &str> {
        self.first_error.iter().chain(&self.last_error).chain(&self.lines).map(String::as_str)
    }

    /// The box's refusal of the run, from its tag; none once the box said the start marker, which no refusal follows.
    ///
    /// The nonce in the tag and the marker only tells runs apart: it is no secret, as it is on the box's and ssh's
    /// command lines. That a command cannot forge a refusal rests on the box saying the marker before it creates the
    /// command's process, and on this ignoring every tag once the marker was seen, whatever order the two came in.
    fn refusal(&self) -> Option<FallbackReason> {
        match self.refused.filter(|_| !self.started) {
            Some(Tagged::Fallback(reason)) => Some(reason),
            Some(Tagged::Sent | Tagged::Drain | Tagged::Channel) | None => None,
        }
    }

    /// Whether the box refused the outputs call's data channel by its `channel` tag, read as [`Called::refusal`] reads a
    /// tag: the channel did not complete, nothing was installed and the archive is kept, so the pull goes over ssh.
    pub(super) fn channel_refused(&self) -> bool {
        self.refused == Some(Tagged::Channel) && !self.started
    }

    /// What the box's drain said when it refused the call by its `drain` tag, read as [`Called::refusal`] reads a tag;
    /// `None` for any other call.
    fn draining(&self) -> Option<Drain> {
        self.drain.filter(|_| self.refused == Some(Tagged::Drain) && !self.started)
    }

    /// Whether the box refused the run for the PC's build its `SentDigest` names, by its `sent` tag, read as
    /// [`Called::refusal`] reads a tag.
    fn sent_refusal(&self) -> bool {
        self.refused == Some(Tagged::Sent) && !self.started
    }

    /// A call that exited with `status`, said nothing, and asked for no data channel, for a test to fill in.
    #[cfg(test)]
    pub(super) fn exited_for_tests(status: i32) -> Called {
        Called {
            status,
            cut: None,
            problem: None,
            output_seen: false,
            first_error: None,
            last_error: None,
            lines: Vec::new(),
            outputs: None,
            back_commits: None,
            names_skipped: false,
            cross_not_brought_back: false,
            started: false,
            refused: None,
            drain: None,
            box_killed: None,
            adjusted: None,
            written: None,
            received: None,
            channel: None,
            channel_sent: None,
        }
    }
}

/// What a box's refusal tag names: the fallback reason of the run's refusal, or `sent`, the PC's build the run's
/// `SentDigest` names, which no fallback answers ([`End::SentRefused`]), or `drain`, the builder's drain, which no
/// fallback answers either ([`End::Draining`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tagged {
    Fallback(FallbackReason),
    Sent,
    Drain,
    /// The box's `channel` refusal of an outputs call's data channel, which the pull over ssh answers.
    Channel,
}

/// What the box's line says after its `channel` refusal tag, before why ([`gate_wire::channel_not_completed_line`]).
const NOT_COMPLETED: &str = "gate: data channel not completed: ";

/// Why the outputs call that ended as `called`, refused by the box's `channel` tag, did not complete its data channel:
/// the PC's own reason when it had one, else that the box said no size, else the box's from its line, else that the
/// PC's channel did not end within the call's drain, else that the box did not complete it. A PC side that gave up
/// because the call ended has no reason of its own.
fn channel_why(called: &Called) -> String {
    match &called.channel {
        Some(channel::Dialled::Failed(why)) => return why.clone(),
        Some(channel::Dialled::NoSize) => return "the box said no archive size".to_owned(),
        _ => {}
    }
    let box_why = called
        .said()
        .find_map(|line| line.find(NOT_COMPLETED).map(|at| line[at + NOT_COMPLETED.len()..].to_owned()));
    match (box_why, &called.channel) {
        (Some(why), _) => why,
        (None, Some(channel::Dialled::NotEnded)) => "the channel did not end".to_owned(),
        (None, _) => "the box did not complete it".to_owned(),
    }
}

/// One run of the client: the call, the way to the builder, the limits, every line the attempt wrote to the log, and
/// the first line that could not be written.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact of the attempt, read by name; no two form a state machine"
)]
pub(super) struct Attempt<'c, 'a> {
    pub(super) call: &'c RemoteCall<'a>,
    pub(super) transport: &'c dyn Transport,
    /// The bounds of each git call of the attempt, its limit and its readers' drain; the drain also bounds the readers
    /// of this PC's `dotnet --list-sdks` read.
    pub(super) git: GitBounds,
    /// How long the sync-back waits for a worktree's lock another run holds: the git limit, which the sync-back's tests
    /// shorten without shortening its git calls.
    pub(super) lock_wait: Duration,
    pause: Duration,
    /// The pauses before the first and the second retry of a transfer after the command ([`Attempt::transfer_call`]).
    transfer_pauses: [Duration; retry::TRANSFER_RETRIES],
    /// Whether the command exited 0 but a transfer after it failed every attempt ([`Attempt::transferred`]).
    transfer_failed: bool,
    pub(super) limits: Limits,
    one_call: Option<OneCall<'c>>,
    /// The call, `preflight`, `run` or `outputs`, after whose first output byte the console interrupt is raised, as a
    /// Ctrl+C would; or `install`, once the install has moved its first file and before it moves a second, so its line
    /// reads `1 of {m}`: an install of a single file ends before the seam is reached; or `stop`, as `run` and then once
    /// more while the stop call that Ctrl+C leads to runs; or `apply`, once a pair's sync-back has checked every patch
    /// and before it applies the first; or `lfs`, at the start of the LFS phase, before its first call.
    pub(super) interrupt_at: Option<&'c str>,
    /// `--test-remote-channel-fault`: the fault the PC's side of the outputs pull's data channel takes once it dialled
    /// the box ([`channel::Fault`]).
    channel_fault: Option<&'c str>,
    /// `--test-local-sdks`: this PC's SDK list, read in place of `dotnet --list-sdks`.
    local_sdks: Option<&'c str>,
    /// How long this PC's `dotnet --list-sdks` may take before it is killed and its list counts as unreadable.
    dotnet_limit: Duration,
    /// The most bytes the job of the sync-back's fetch may write.
    pub(super) fetch_cap: u64,
    /// The most bytes the files a back commit writes into the worktree may total.
    pub(super) apply_cap: u64,
    /// The box's outputs budget: the outputs fetch is capped at zstd's worst case for it, and the install unpacks its
    /// archive to it.
    pub(super) outputs_budget: u64,
    /// The most bytes one put of the inputs may total, decompressed: a set past it is not sent.
    pub(super) inputs_budget: u64,
    /// The most bytes one put of LFS objects may total: a set past it is not sent.
    pub(super) lfs_budget: u64,
    /// The most bytes of one call's output, its two streams counted together, written to the gate's log.
    log_cap: u64,
    /// `--test-own-start-fails`: this gate's start, which names the sync-back's quarantine, cannot be read.
    pub(super) own_start_fails: bool,
    /// `--test-remote-hint-from-pull`: the hang hint is not due before the outputs pull starts, and counts from the
    /// pull's start instead of the box's start marker.
    hint_from_pull: bool,
    /// `--test-remote-tamper`: the change made to the run's own folders at one point of it ([`tamper::at`]).
    #[cfg(feature = "test-seams")]
    tamper: Option<&'c str>,
    /// Whether the run's outputs were brought back, as its outputs step ended ([`outputs::ended`]); `None` for a run
    /// that never reached it.
    pub(super) brought_back: Option<bool>,
    /// Whether the builder was sent the words with `--no-build` dropped ([`RemoteOutcome::dropped_no_build`]).
    dropped_no_build: bool,
    /// How long the run call and each transfer took, as far as the attempt got.
    pub(super) split: Split,
    lines: Vec<String>,
    problem: Option<String>,
    /// Whether the run call or the outputs pull said the hang hint, which an attempt says once.
    hang_said: bool,
    /// When the box's start marker of the run came, which the hang hint counts the run from through the outputs pull
    /// too; `None` until a run call heard it.
    command_started: Option<Instant>,
    /// The moment the attempt began making its snapshot, by this PC's clock ([`Attempt::snapshots`]), less the
    /// `--test-remote-snapshot-age-ms` age when a test gives one: the last write each file its outputs install moves into
    /// place takes; `None` until it began.
    pub(super) snapshot_taken: Option<std::time::SystemTime>,
    /// How far before the moment the snapshot begins its time is put: none but under `--test-remote-snapshot-age-ms`.
    snapshot_age: Duration,
    /// How often an outputs pull says its progress while it runs.
    outputs_progress: Duration,
    /// The outputs pull the call being made is, while the outputs call is made; `None` for every other call.
    pulling: Option<Pull>,
    /// The progress periods the outputs pull being made has said, one count across all its calls, so a call made again
    /// says no period an earlier call of the pull said; 0 as each pull starts.
    pull_periods_said: Cell<u128>,
    /// The bytes the last call with a raw copy had written into its sink by its end: what the outputs pull received,
    /// which for a pull whose call was made again is the last call's alone, each call writing its archive anew.
    pub(super) pulled_bytes: u64,
    /// When that call's raw copy wrote its last byte, `None` when it wrote none: the outputs archive's last byte, which
    /// for a pull whose call was made again is the last call's too.
    pub(super) pulled_last: Option<Instant>,
    /// The load-adjusted seconds the box said it counted for the command, `None` until the run call that counts said
    /// them.
    adjusted: Option<f64>,
    /// The kill the box's kill tag named, set only when the run call ended as the box's own kill.
    box_killed: Option<BoxKill>,
    /// The run's folders and locks, held from before its snapshots until its last save; `None` before and once let go.
    held_run: Option<RunHold>,
    /// The folder every git call about a pair's repository stops its search at, the pair's parent folder; `None` for a
    /// run of one repository.
    ceiling: Option<PathBuf>,
    /// Where git found each of a pair's repositories at its check, by its root, which every later git call about it is
    /// given ([`Bounded::variables`]) while the pair is held; empty for a run of one repository and once let go.
    places: Vec<(String, Place)>,
    /// This PC's newest .NET SDK major once [`Attempt::local_major`] has read it, so one run lists the SDKs once
    /// however many of its repositories need it; `None` before.
    local_major: Option<u64>,
    /// How a warm-up's run call ended, once it has ([`Attempt::warm_ran`]); `None` before, and for a call that is no
    /// warm-up.
    warm: Option<WarmEnd>,
    /// Whether a call of the attempt said the box's start marker, however the call ended, a failure included.
    marker_seen: bool,
    /// The run call's ceiling, in whole seconds, once it was raised for a work tree the preflight found new
    /// ([`Attempt::raise_ceiling`]); `None` while it is the call's own.
    raised: Option<u64>,
    /// `--test-remote-backstop-ms`: the run call's backstop, which a raised ceiling leaves as it is; `None` without the
    /// seam.
    backstop_seam: Option<Duration>,
    /// What the drain polls and drained attempts before this attempt learned of the work folder, with what this
    /// attempt's own preflight found taken in ([`Carried`]).
    carried: Carried,
    /// Whether the attempt is a drain poll, whose own lines go to the log alone ([`Attempt::say`]): the console of a
    /// run waiting behind the drain gets only the drain's lines.
    log_only: bool,
}

/// A send of the PC's build that landed ([`Attempt::sent_build`]): the digest of its manifest line, the run call's
/// `SentDigest`, and the withheld-outputs marker it took out, which a run the box then refuses for that build writes.
struct Landed {
    digest: String,
    withheld: Option<PathBuf>,
}

/// How a run call ended ([`Attempt::run_call`]): its end, the last standard error lines of the call that counts that
/// are the box's outputs line, its outputs size line and its back commits line, whether that call said on standard
/// error that the box skipped outputs for names this PC cannot take, whether the box said the run's start marker, and
/// the call's last standard error line that is neither a heartbeat nor one of the box's protocol lines.
struct RunEnd {
    end: End,
    outputs: Option<outputs::OutputsLines>,
    back_commits: Option<String>,
    names_skipped: bool,
    cross_not_brought_back: bool,
    started: bool,
    last_error: Option<String>,
}

impl Attempt<'_, '_> {
    /// The profile of the call's builder over the transport ([`super::transport::Transport::profile`]).
    pub(super) fn profile(&self) -> Profile {
        self.transport.profile(self.call.builder)
    }

    /// The drain poll's one phase, the preflight with no needs at the run's place, which makes the same word the run's
    /// own preflight does: its end, [`End::Status`] 0 once the builder admits the run, and what the preflight found,
    /// the work tree new and the bares created, which the box makes only for a poll it admits.
    fn drain_polled(&mut self) -> (End, Option<Prepared>) {
        let mut repos = self.repos();
        let polled = self.location(&mut repos).and_then(|location| self.preflight(&location, &[]));
        match polled {
            Ok(prepared) => (End::Status(0), Some(prepared)),
            Err(end) => (end, None),
        }
    }

    /// The phases in order; `Err` for an end before the run call.
    fn phases(&mut self) -> Result<End, End> {
        #[cfg(feature = "test-seams")]
        tamper::at(self.tamper, tamper::BEFORE_HOLD, self.call)?;
        self.hold_run()?;
        let mut repos = self.repos();
        let snapshots = self.snapshots(&repos)?;
        let location = self.location(&mut repos)?;
        let nonce = payload::run_nonce();
        let mut sent = outputs_sent(self)?;
        let inputs = inputs::sent(self)?;
        let Some((snapshot, siblings)) = snapshots.split_first() else {
            return Err(End::Failed("the snapshot made no commit".to_owned()));
        };
        let sibling_snapshots: Vec<(String, String)> = repos
            .iter()
            .skip(1)
            .zip(siblings)
            .map(|(repo, sibling)| (repo.name.clone(), sibling.commit.clone()))
            .collect();
        let trees = self.trees(&repos, &snapshots)?;
        let lfs = self.lfs_sending(&repos, &snapshots, &trees)?;
        let godot_command = self.godot_command();
        let mut needs = trees
            .first()
            .and_then(Option::as_ref)
            .map(|tree| tree.needs(godot_command))
            .unwrap_or_default();
        let git_lfs = self.git_lfs_needed(lfs.is_some(), &repos, &trees, &mut needs)?;
        let prepared = self.preflight(&location, &needs)?;
        self.carried.add(self.call.host(), prepared.new_tree, &prepared.created);
        let chosen = self.parity(&repos, &snapshots, &trees, &prepared, git_lfs)?;
        let raised = self.cold_ceiling(self.carried.new_tree);
        let created = self.carried.created.clone();
        self.pushes(&repos, &snapshots, &created)?;
        // The pair's folders are let go before the run call, which never runs while they are held.
        self.release_run()?;
        // After the push, so a route refused before it sends no input, and before the marker, so one the inputs refuse
        // leaves none.
        let inputs_landed = self.sent_contents(&location, &nonce, inputs.as_ref(), lfs.as_ref())?;
        let (sent_digest, withheld) = self
            .sent_build(&location, &mut sent)?
            .map_or((None, None), |landed| (Some(landed.digest), landed.withheld));
        // After the push: a run refused before it, at the preflight or by the parity check, leaves no marker.
        outputs::marked(&sent)?;
        if let Some(raised) = raised {
            self.raise_ceiling(raised);
        }
        // After the preflight, whose installs the Godot and Unreal Engine versions are chosen from, and after the send,
        // whose landing keeps `--no-build`.
        let run = RunCall {
            words: match self.call.send {
                Some(plan) if sent_digest.is_some() => &plan.words,
                _ => self.call.words,
            },
            subfolder: self.call.subfolder,
            nonce: &nonce,
            snapshot: &snapshot.commit,
            outputs: sent.globs,
            failure_outputs: sent.failure_globs,
            full: sent.full,
            timeout: raised.unwrap_or(self.call.timeout),
            host: self.call.host(),
            sibling_snapshots: &sibling_snapshots,
            inputs_digest: inputs.as_ref().filter(|_| inputs_landed).map(|built| built.digest.as_str()),
            lfs_digest: lfs.as_ref().map(|sending| sending.digest.as_str()),
            sent_digest: sent_digest.as_deref(),
            env: self.call.env,
            godot_version: chosen.godot.as_deref(),
            unreal_version: chosen.unreal.as_deref(),
            engine_env: &chosen.engine_env,
            desktop: self.call.desktop,
            cross_build: self.call.cross_build,
        };
        let (called, nonce) = self.run_called(&location, &run, &mut sent, withheld)?;
        // A warm-up brings nothing back: its end is classed, and neither the sync-back nor the outputs step follows.
        if self.warm_ran(&called) {
            return Ok(self.warm_ended(called, &location, prepared.boot.as_deref()));
        }
        // The sync-back runs after a run call that ended with the command's own status; a refusal, an interrupt, a
        // kill, a lost connection or a failed call ended the attempt before it, so a box refusal's 75 never syncs back.
        // The pending marker stays on every end but a passing run whose box packed nothing.
        match called.end {
            End::Status(status) => {
                let back_commits = called.back_commits.as_deref().and_then(syncback::back_commits);
                let synced = match syncback::targets(self.call, snapshot, siblings) {
                    Some(targets) => syncback::sync_back(self, &targets, back_commits.as_deref(), status),
                    None => Ok(syncback::unmatched(self, status)),
                };
                let end = synced.map(|synced| {
                    let brought = outputs::accepted(called.outputs.as_ref(), self.call.key, &nonce);
                    let ran = outputs::Ran {
                        location: &location,
                        nonce: &nonce,
                        status,
                        names_skipped: called.names_skipped,
                        cross_not_brought_back: called.cross_not_brought_back,
                    };
                    outputs::ended(self, &sent, &ran, synced, brought.as_ref())
                });
                self.transferred(status, end)
            }
            End::Lost(output) => Ok(self.connection_lost(&location, prepared.boot.as_deref(), output)),
            end => Ok(self.refused_end(end, called.last_error.as_deref())),
        }
    }

    /// The end of a run whose command exited `status` once its transfers after it ended at `end`. A transfer that failed
    /// every attempt says the TRANSFER FAILED line with the command's exit and its reason, and ends the run as
    /// [`super::TRANSFER_FAILED`] after a command that exited 0, else as the command's own status; any other end stands.
    fn transferred(&mut self, status: i64, end: Result<End, End>) -> Result<End, End> {
        let why = match end {
            Ok(End::TransferFailed(why)) | Err(End::TransferFailed(why)) => why,
            end => return end,
        };
        let host = self.call.host();
        self.say(format!("gate: TRANSFER FAILED: the command exited {status} on {host}; {why}"));
        self.transfer_failed = status == 0;
        Ok(End::Status(if status == 0 { super::TRANSFER_FAILED } else { status }))
    }

    /// Whether the run needs git-lfs of the builder, its need then added to `needs` ([`toolchain::with_git_lfs`]): when
    /// it sends LFS content (`sending`), or when any of `repos`, whose trees `trees` holds, uses the LFS filter
    /// ([`Attempt::lfs_filtered`]), as the box's checkout then runs git-lfs. A listing git fails refuses the route `lfs`;
    /// a Ctrl+C ends the run.
    fn git_lfs_needed(&mut self, sending: bool, repos: &[Repo], trees: &[Option<Tree>], needs: &mut Vec<(String, String)>) -> Result<bool, End> {
        let git_lfs = sending || self.lfs_filtered(repos, trees)?;
        if git_lfs {
            toolchain::with_git_lfs(needs);
        }
        Ok(git_lfs)
    }

    /// The run's inputs `inputs` and its LFS content `lfs` sent to `location` for the run of `nonce`, each when the run
    /// has some ([`inputs::send`], [`lfs::send`]), the inputs first: whether the inputs landed, so the run call carries
    /// their digest, which a run with none never does; else the end a send ended the attempt on.
    fn sent_contents(&mut self, location: &Location, nonce: &str, inputs: Option<&inputs::Built>, lfs: Option<&lfs::Sending>) -> Result<bool, End> {
        let landed = match inputs {
            Some(built) => inputs::send(self, location, nonce, built)?,
            None => false,
        };
        if let Some(sending) = lfs {
            lfs::send(self, location, nonce, sending)?;
        }
        Ok(landed)
    }

    /// The send of the PC's build the route plans ([`inputs::sent_build`]), after the inputs and the LFS content. Once it
    /// landed, the builder keeps `--no-build` and builds nothing: the withheld-outputs marker is taken out of `sent`, so
    /// none is written, and the run's outcome no longer counts `--no-build` dropped. `None` with no send planned, and for
    /// a send that did not land or was not made while the last build's outputs are not back ([`Attempt::outputs_not_back`]),
    /// the dropped line then said; else the end a Ctrl+C or an `install` refusal ended it on.
    fn sent_build(&mut self, location: &Location, sent: &mut outputs::Sent<'_>) -> Result<Option<Landed>, End> {
        let Some(plan) = self.call.send else {
            return Ok(None);
        };
        if let Some(why) = self.outputs_not_back(plan) {
            self.logged_kept(inputs::build_not_sent(&why));
            self.takes_the_drop(plan.dropped_line())?;
            return Ok(None);
        }
        let Some(digest) = inputs::sent_build(self, location, &plan.bin)? else {
            self.takes_the_drop(plan.dropped_line())?;
            return Ok(None);
        };
        self.dropped_no_build = false;
        Ok(Some(Landed {
            digest,
            withheld: sent.take_withheld(),
        }))
    }

    /// Why `plan`'s send is not made while the last build's outputs are not back here: the worktree's nothing-back record
    /// stands, read as a run here reads it ([`no_build::read_record`]), and names no configuration or the run's own,
    /// the last folder of [`SendPlan::bin`], in any letter case; a record that cannot be read counts as standing. The
    /// PC's `bin` would then be older than the builder's build, and the run takes the drop. `None` with no record, one
    /// of another configuration, and for a send planned because the run's last build ran here
    /// ([`SendPlan::built_here`]) whose entry still stands ([`Attempt::built_here_stands`]): every routed build removes
    /// the built-here record before it runs, so its standing entry says a build here came after the last routed one.
    fn outputs_not_back(&self, plan: &SendPlan) -> Option<String> {
        if plan.built_here && self.built_here_stands(plan) {
            return None;
        }
        let path = v1::nothing_back_path(self.call.state_dir, self.call.key);
        let configuration = plan.bin.rsplit('/').next().unwrap_or_default().to_lowercase();
        let standing = || format!("the last build's outputs are not back: {} stands", path.display());
        match no_build::read_record(&path) {
            Ok(None) => None,
            Ok(Some(record)) => record
                .settings
                .configuration
                .is_none_or(|named| named.to_lowercase() == configuration)
                .then(standing),
            Err(error) => Some(format!("cannot read {}: {error}", path.display())),
        }
    }

    /// Whether the built-here entry `plan` was planned from still stands, read again under the key lock the run holds:
    /// the record's entry the route matched ([`SendPlan::project_key`]) under the settings [`SendPlan::words`] set
    /// ([`no_build::entry_stands`]). A routed build removes the record under that lock between the route and here.
    fn built_here_stands(&self, plan: &SendPlan) -> bool {
        let record = no_build::read_built_here(&v1::built_here_path(self.call.state_dir, self.call.key));
        no_build::entry_stands(&record, &plan.project_key, &build_settings(&plan.words))
    }

    /// Says `line`, the dropped line of a send that is not made or did not land, as the builder is about to run the
    /// words whose `--no-build` the route dropped, which build there: the worktree's built-here record is removed first,
    /// under the key lock the run holds ([`no_build::clear_built_here`]).
    ///
    /// # Errors
    ///
    /// A record that cannot be removed refuses the route `inputs`, as its entries would outlast the build there: the
    /// run goes on here, the line saying why kept for its log.
    fn takes_the_drop(&mut self, line: &'static str) -> Result<(), End> {
        if let Err(cannot) = no_build::clear_built_here(&v1::built_here_path(self.call.state_dir, self.call.key)) {
            self.logged_kept(cannot);
            return Err(End::Refused(FallbackReason::Inputs));
        }
        self.say(line.to_owned());
        Ok(())
    }

    /// The run call of `run` to `location`, fed `sent`'s manifest line, and the nonce of the call that counts. A call
    /// the box refused for the PC's build its `SentDigest` names ([`End::SentRefused`]) is made once more, under a nonce
    /// new to it, with the words whose `--no-build` the route dropped and no `SentDigest`, as for a send that did not
    /// land: the dropped line said first, and the withheld-outputs marker the landed send took, `withheld`, written.
    fn run_called(
        &mut self,
        location: &Location,
        run: &RunCall<'_>,
        sent: &mut outputs::Sent<'_>,
        withheld: Option<PathBuf>,
    ) -> Result<(RunEnd, String), End> {
        let input = payload::input(&self.run_word(location, run), sent.manifest.as_deref());
        let called = self.run_call(location, &input, run.nonce)?;
        if !matches!(called.end, End::SentRefused) {
            return Ok((called, run.nonce.to_owned()));
        }
        self.dropped_no_build = true;
        let dropped = self.call.send.map_or(DROPPED_NO_BUILD_LINE, SendPlan::dropped_line);
        self.takes_the_drop(dropped)?;
        sent.withhold(withheld);
        outputs::marked(sent)?;
        let nonce = payload::run_nonce();
        let again = RunCall {
            words: self.call.words,
            nonce: &nonce,
            sent_digest: None,
            ..*run
        };
        let input = payload::input(&self.run_word(location, &again), sent.manifest.as_deref());
        let called = self.run_call(location, &input, &nonce)?;
        Ok((called, nonce))
    }

    /// The end of a run call that ended at `end`, its last error line `last_error`: a refusal of its inputs or of its
    /// LFS content said in the log ([`inputs::run_refused`], [`lfs::run_refused`]); any other end as it is.
    fn refused_end(&mut self, end: End, last_error: Option<&str>) -> End {
        match end {
            End::Refused(FallbackReason::Inputs) => inputs::run_refused(self),
            End::Refused(FallbackReason::Lfs) => lfs::run_refused(self, last_error),
            end => end,
        }
    }

    /// Whether the call runs a Godot console, which the preflight's Godot need and [`Attempt::parity`]'s Godot rule
    /// both read: a run's first word, or any of a warm-up's commands' first words, since a warm-up's call carries none
    /// of its own.
    fn godot_command(&self) -> bool {
        match self.call.warmth {
            Warmth::WarmUp { commands, .. } => commands.iter().any(|command| command.first().is_some_and(|word| is_godot_console(word))),
            Warmth::Unasked | Warmth::Consented(_) => self.call.words.first().is_some_and(|word| is_godot_console(word)),
        }
    }

    /// The payload word of the run call `run` to `location`: a warm-up's for a warm-up, else a run's.
    fn run_word(&self, location: &Location, run: &RunCall<'_>) -> String {
        match self.call.warmth {
            Warmth::WarmUp { commands, hash } => payload::warm_word(location, run, &WarmCall { commands, hash }),
            Warmth::Unasked | Warmth::Consented(_) => payload::run_word(location, run),
        }
    }

    /// The ceiling the run call sends a run whose work tree the preflight found new (`new_tree`), raised from the
    /// call's ([`watchdog::cold_ceiling`]); `None` for a tree that is not new, and for a warm-up, whose tree is new by
    /// design.
    fn cold_ceiling(&self, new_tree: bool) -> Option<u64> {
        let warm_up = matches!(self.call.warmth, Warmth::WarmUp { .. });
        (new_tree && !warm_up).then(|| watchdog::cold_ceiling(self.call.timeout))
    }

    /// Gives the run call the `raised` ceiling, and the backstop it sets unless the backstop seam gives one, and says
    /// the cold note once ([`cold_line`]): `gate warm` for a run whose repo has warm-up consent and a warm-up file
    /// that parsed.
    fn raise_ceiling(&mut self, raised: u64) {
        self.raised = Some(raised);
        self.limits.command = Duration::from_secs(raised);
        self.limits.backstop = self.backstop_seam.unwrap_or_else(|| watchdog::backstop(raised));
        let warm_up = matches!(self.call.warmth, Warmth::Consented(_));
        self.say(cold_line(self.call.timeout, raised, warm_up));
    }

    /// The hang hint a call watches for until the attempt has said it: the run call's (`run_call`), or the outputs
    /// pull's once the run call heard the box's start marker, from the call's usual p90, which a run whose ceiling was
    /// raised for a new work tree has none of ([`routed_usual`]); `None` for any other call. With
    /// `--test-remote-hint-from-pull` the run call watches for none, and the pull's counts from the pull's start.
    fn run_hang(&self, run_call: bool) -> Option<RunHang<'_>> {
        let phase = if run_call {
            if self.hint_from_pull {
                return None;
            }
            HangPhase::Running
        } else {
            let pull = self.pulling?;
            let command_started = self.command_started?;
            HangPhase::Pulling {
                counted_from: if self.hint_from_pull { pull.started } else { command_started },
                pull_started: pull.started,
                bringing: pull.bringing,
            }
        };
        routed_usual(self.call.usual, self.raised.is_some())
            .filter(|_| !self.hang_said)
            .map(|usual| RunHang {
                usual,
                log: self.call.log,
                phase,
                builder: self.call.host(),
            })
    }

    /// The pull's progress the call being made says: the outputs pull's, against the attempt's one count of the periods
    /// the pull said; `None` for every call but an outputs call.
    fn pull_watch(&self) -> Option<PullWatch<'_>> {
        self.pulling.map(|pull| PullWatch {
            total: pull.total,
            started: pull.started,
            bringing: pull.bringing,
            every: self.outputs_progress,
            log: self.call.log,
            periods_said: &self.pull_periods_said,
        })
    }

    /// Whether the call is a warm-up, whose run call ended as `called`: its end is then classed and kept for the outcome.
    fn warm_ran(&mut self, called: &RunEnd) -> bool {
        let Warmth::WarmUp { hash, .. } = self.call.warmth else {
            return false;
        };
        self.warm = Some(warm_end(hash, called.started, &called.end, called.last_error.as_deref()));
        true
    }

    /// The end of a warm-up whose run call ended as `called`, which never reaches the sync-back or the outputs step: a
    /// lost connection is handled as a run's is, and an interrupt it ends on leaves no record; a refusal of its inputs
    /// or its LFS content is handled as a run's is; any other end is the attempt's as it is.
    fn warm_ended(&mut self, called: RunEnd, location: &Location, boot: Option<&str>) -> End {
        match called.end {
            End::Lost(output) => {
                let end = self.connection_lost(location, boot, output);
                if matches!(end, End::Interrupted) {
                    self.warm = Some(WarmEnd::Deleted);
                }
                end
            }
            end => self.refused_end(end, called.last_error.as_deref()),
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
        snapshot::take(&roots, self.call.key, self.call.builder.id.as_deref(), self.git, self.call.interrupt)
            .map_err(git_end)?
            .pop()
            .ok_or_else(|| End::Failed("the snapshot made no commit".to_owned()))
    }

    /// For a call that carries siblings, holds the pair's folders ([`Held::hold`]) and bounds every git call about the
    /// pair at the root's parent; for a run of one repository, checks its root ([`root_checked`]). Then, for every run,
    /// takes the worktree locks of the root and of each sibling, in the order of their ids, waiting for one another
    /// routed run holds until the lock wait has passed. A folder that cannot be held or checked, and a lock still held
    /// past the wait, keep the run here naming the repository.
    fn hold_run(&mut self) -> Result<(), End> {
        self.go_on()?;
        let call = self.call;
        // A single repository's root stays open, so it cannot be moved or replaced, until its lock holds it.
        let (held, _root) = if call.siblings.is_empty() {
            (None, Some(root_checked(call)?))
        } else {
            let held = Held::hold(call.root, call.root_id, call.leaf, &call.siblings);
            (Some(held.map_err(|refused| End::NotRouted(refused.line()))?), None)
        };
        let main = Folder {
            name: call.leaf,
            root: call.root,
            id: call.root_id,
        };
        let siblings = call.siblings.iter().map(|sibling| Folder {
            name: sibling.name(),
            root: sibling.root(),
            id: sibling.id(),
        });
        let folders: Vec<Folder<'_>> = std::iter::once(main).chain(siblings).collect();
        let leaf = call.leaf.to_owned();
        let wait = Wait::until(self.lock_wait, call.interrupt, move |name, _| busy(&leaf, name));
        let locks = worktree_lock::held_or(call.state, &folders, &wait, |line| self.say(line))?;
        if held.is_some() {
            let Some(parent) = Path::new(call.root).parent() else {
                return Err(not_routed(call.leaf, "has no parent folder"));
            };
            self.ceiling = Some(parent.to_path_buf());
        }
        self.held_run = Some(RunHold { held, _locks: locks });
        Ok(())
    }

    /// Reads every held folder of a pair again ([`Held::recheck`]): one that moved keeps the run here naming it.
    fn recheck_pair(&self) -> Result<(), End> {
        match self.held_pair() {
            Some(held) => held.recheck().map_err(|refused| End::NotRouted(refused.line())),
            None => Ok(()),
        }
    }

    /// The pair's folders while they are held; `None` for a run of one repository and once let go.
    fn held_pair(&self) -> Option<&Held> {
        self.held_run.as_ref().and_then(|run| run.held.as_ref())
    }

    /// Lets the run's folders and locks go once every repository is pushed and saved, after reading every held folder's
    /// path again ([`Attempt::recheck_pair`]).
    fn release_run(&mut self) -> Result<(), End> {
        let rechecked = self.recheck_pair();
        self.places.clear();
        self.held_run = None;
        rechecked
    }

    /// Holds the git folder and the common folder git named at `place` for the repository said as `who`
    /// ([`Held::hold_git`]); the common folder's id.
    fn hold_git(&mut self, who: &str, place: &Place) -> Result<gate_router::FolderId, End> {
        let Some(held) = self.held_run.as_mut().and_then(|run| run.held.as_mut()) else {
            return Err(End::Failed(format!("the folders of {who} are not held")));
        };
        held.hold_git(who, &place.git_dir, &place.common)
            .map_err(|refused| End::NotRouted(refused.line()))
    }

    /// Where git found the pair's repository whose root is `folder` at its check; else, for a root spelled other than
    /// the one recorded then, why it has none.
    fn place_at(&self, folder: &Path) -> Result<&Place, String> {
        self.places
            .iter()
            .find(|(root, _)| Path::new(root) == folder)
            .map(|(_, place)| place)
            .ok_or_else(|| format!("the git location of {} was not recorded at its check", folder.display()))
    }

    /// The pair's repository whose root is `folder` as its git calls ask about it: while the pair is held, at the place
    /// git found it at its check ([`Attempt::place_at`]), one with none recorded failing; once let go, bounded at the
    /// pair's parent alone; `None` for a run of one repository.
    fn bound_at<'b>(&'b self, folder: &'b Path) -> Result<Option<Bounded<'b>>, String> {
        let Some(ceiling) = self.ceiling.as_deref() else {
            return Ok(None);
        };
        let place = if self.held_pair().is_some() {
            Some(self.place_at(folder)?)
        } else {
            None
        };
        Ok(Some(Bounded {
            root: folder,
            ceiling,
            place,
        }))
    }

    /// The repositories the run sends: the main one, named by its folder name, then each sibling in declared order.
    fn repos(&self) -> Vec<Repo> {
        let call = self.call;
        let main = Repo {
            name: call.leaf.to_owned(),
            root: call.root.to_owned(),
            on_builder: call.repo.to_owned(),
            bare: String::new(),
        };
        let siblings = call.siblings.iter().map(|sibling| Repo {
            name: sibling.name().to_owned(),
            root: sibling.root().to_owned(),
            on_builder: repo_name(sibling.name()),
            bare: String::new(),
        });
        std::iter::once(main).chain(siblings).collect()
    }

    /// The snapshot of each of `repos`, in order. A pair's repositories are each asked first, git bounded at the
    /// pair's parent, where git finds them: one whose top folder is not its root, one whose git folders cannot be held
    /// ([`Held::hold_git`]), and two that share a common folder, keep the run here, as a sibling's git failure does;
    /// the main one's git failure ends the run. Each is then snapshotted at the place git found it, and the pair's
    /// folders read again after each snapshot.
    fn snapshots(&mut self, repos: &[Repo]) -> Result<Vec<Snapshot>, End> {
        let now = std::time::SystemTime::now();
        // A seam's age past the clock's start takes the start, so no age can panic.
        self.snapshot_taken = Some(now.checked_sub(self.snapshot_age).unwrap_or(std::time::SystemTime::UNIX_EPOCH));
        let Some(ceiling) = self.ceiling.clone() else {
            return self.snapshot().map(|snapshot| vec![snapshot]);
        };
        let mut commons: Vec<(&str, gate_router::FolderId)> = Vec::with_capacity(repos.len());
        for (at, repo) in repos.iter().enumerate() {
            self.go_on()?;
            let place = snapshot::place_of(bounded(repo, &ceiling), self.git, self.call.interrupt)
                .map_err(|error| repo_end(at, &repo.name, git_end(error)))?;
            let who = who(at, &repo.name);
            if !same_path(&place.top, &repo.root) {
                return Err(not_routed(
                    &who,
                    format_args!("is not the top folder of its git repository: git names {}", place.top),
                ));
            }
            let common = self.hold_git(&who, &place)?;
            if let Some((other, _)) = commons.iter().find(|(_, seen)| *seen == common) {
                return Err(not_routed(&who, format_args!("shares its git repository with {other}")));
            }
            commons.push((&repo.name, common));
            self.places.push((repo.root.clone(), place));
        }
        let mut snapshots = Vec::with_capacity(repos.len());
        for (at, repo) in repos.iter().enumerate() {
            let root = Path::new(&repo.root);
            let bound = Bounded {
                place: Some(self.place_at(root).map_err(End::Failed)?),
                ..bounded(repo, &ceiling)
            };
            let builder = self.call.builder.id.as_deref();
            let taken = snapshot::take_bounded(bound, self.call.key, builder, self.git, self.call.interrupt);
            snapshots.push(taken.map_err(|error| repo_end(at, &repo.name, git_end(error)))?);
            #[cfg(feature = "test-seams")]
            if at == 0 {
                tamper::at(self.tamper, tamper::AFTER_FIRST_SNAPSHOT, self.call)?;
            }
            self.recheck_pair()?;
        }
        Ok(snapshots)
    }

    /// The run's place on the builder, with the pair of `repos` when it has siblings, read back with the box's own
    /// reader before anything is sent, which names each sibling's bare repository; each of `repos` is given its bare. A
    /// pair the box would refuse keeps the run here naming the sibling.
    fn location(&mut self, repos: &mut [Repo]) -> Result<Location, End> {
        let warm_up = matches!(self.call.warmth, Warmth::WarmUp { .. });
        let location =
            Location::under(&self.profile(), self.call.repo, self.call.key, &payload::job_nonce()).with_session(self.call.session, warm_up);
        let Some((main, siblings)) = repos.split_first_mut() else {
            return Ok(location);
        };
        main.bare.clone_from(&location.bare);
        if siblings.is_empty() {
            return Ok(location);
        }
        let names: Vec<String> = siblings.iter().map(|repo| repo.name.clone()).collect();
        let location = location.with_pair(self.call.leaf, &names);
        let decoded = Payload::decode(&payload::location_word(&location), location.form).map_err(PairRefused::from);
        let pair = match decoded.and_then(|payload| payload.laid_out()) {
            Ok(Some(pair)) => pair,
            Ok(None) => return Err(End::Failed("the run's payload carries no Pair".to_owned())),
            Err(refused) => {
                let why = refused.line.strip_prefix("gate: ").unwrap_or(&refused.line);
                // A refusal of no one sibling is of the pair as a whole, which the main repository's place carries.
                let who = refused.sibling.as_deref().map_or_else(|| self.call.leaf.to_owned(), sibling_who);
                return Err(not_routed(&who, format_args!("cannot be laid out on the builder: {why}")));
            }
        };
        for (repo, sibling) in siblings.iter_mut().zip(pair.siblings()) {
            sibling.bare().clone_into(&mut repo.bare);
        }
        Ok(location)
    }

    /// The tree the parity check reads of each of `repos`' `snapshots`: the main one's as [`Attempt::tree`] reads it, a
    /// sibling's names alone; a sibling's git failure keeps the run here naming it.
    fn trees(&mut self, repos: &[Repo], snapshots: &[Snapshot]) -> Result<Vec<Option<Tree>>, End> {
        let mut trees = Vec::with_capacity(snapshots.len());
        for (at, (repo, snapshot)) in repos.iter().zip(snapshots).enumerate() {
            let tree = if at == 0 {
                self.tree(snapshot)?
            } else {
                self.sibling_tree(repo, snapshot).map_err(|end| sibling_end(&repo.name, end))?
            };
            trees.push(tree);
        }
        Ok(trees)
    }

    /// The LFS content the run sends ([`lfs::sending`]): the objects the pointers of the main repository's snapshot
    /// name, `None` when it holds none. The main repository's blobs come from its tree's listing (`trees`), and a
    /// sibling's from a listing of its own ([`Attempt::lfs_listed`]). A pair any of whose snapshots holds a pointer is
    /// refused `lfs`, naming the repository; a Ctrl+C ends the run.
    fn lfs_sending(&mut self, repos: &[Repo], snapshots: &[Snapshot], trees: &[Option<Tree>]) -> Result<Option<lfs::Sending>, End> {
        let mut sending = None;
        let main_small = trees.first().and_then(Option::as_ref).map(|tree| &tree.small);
        for (at, (repo, snapshot)) in repos.iter().zip(snapshots).enumerate() {
            self.go_on()?;
            let Some(listed) = self.lfs_listed(repo, snapshot, (at == 0).then_some(main_small))? else {
                continue;
            };
            if listed.pointers.is_empty() {
                continue;
            }
            if repos.len() > 1 {
                return Err(lfs::refused(self, &lfs::pair_why(&repo.name)));
            }
            sending = Some(lfs::sending(listed).map_err(|why| lfs::refused(self, &why))?);
        }
        Ok(sending)
    }

    /// The pointers of `repo`'s `snapshot`: for the main repository (`main` given), those among the blobs of its tree's
    /// listing ([`lfs::listed_among`]), `None` when its tree could not be listed (past the git output cap, not UTF-8,
    /// not of `ls-tree`'s form), which has no LFS phase, as before LFS content was sent; for a sibling (`main` not
    /// given), those of a listing of its own ([`lfs::listed`]). A main tree holding a `.gitattributes` in a repository
    /// whose `info/attributes` is not empty is refused `lfs` first ([`lfs::unseen_attributes`]) when the repository
    /// uses LFS at all ([`lfs::tracks_lfs_filter`]), and so is a listing git fails; a Ctrl+C ends the run.
    #[expect(clippy::option_option, reason = "the main repository's listing, given, may be one that could not be made")]
    fn lfs_listed(&mut self, repo: &Repo, snapshot: &Snapshot, main: Option<Option<&lfs::Small>>) -> Result<Option<lfs::Listed>, End> {
        let root = Path::new(&repo.root);
        let (bounds, interrupt) = (self.git, self.call.interrupt);
        let small = match main {
            Some(None) => return Ok(None),
            Some(Some(small)) => Some(small),
            None => None,
        };
        if small.is_some_and(|small| small.attributes) {
            let unseen = self.lfs_bound(root).map_err(Unbuilt::Failed);
            let unseen = unseen.and_then(|bound| lfs::unseen_attributes(bound, bounds, interrupt));
            if let Some(why) = self.lfs_unbuilt(repo, unseen)? {
                let uses = lfs::tracks_lfs_filter(root, bounds, interrupt).map_err(lfs::unbuilt);
                if self.lfs_unbuilt(repo, uses)? {
                    return Err(lfs::refused(self, &why));
                }
            }
        }
        let listed = match (self.lfs_bound(root), small) {
            (Ok(bound), Some(small)) => lfs::listed_among(bound, &snapshot.commit, small, bounds, interrupt),
            (Ok(bound), None) => lfs::listed(bound, &snapshot.commit, bounds, interrupt),
            (Err(why), _) => Err(Unbuilt::Failed(why)),
        };
        self.lfs_unbuilt(repo, listed).map(Some)
    }

    /// Whether any of `repos` uses LFS at all, so the builder's checkout runs git-lfs for its files whether or not its
    /// snapshot holds a pointer: for the main repository whose tree `trees` holds, a `.gitattributes` file of that
    /// listing names the LFS filter as it is on disk, which is what the snapshot sent ([`lfs::names_lfs_filter_among`]);
    /// for a sibling, or a main tree that could not be listed, as [`lfs::tracks_lfs_filter`] reads it. A listing git
    /// fails refuses the route `lfs`, naming the repository; a Ctrl+C ends the run.
    fn lfs_filtered(&mut self, repos: &[Repo], trees: &[Option<Tree>]) -> Result<bool, End> {
        let (bounds, interrupt) = (self.git, self.call.interrupt);
        for (at, repo) in repos.iter().enumerate() {
            let root = Path::new(&repo.root);
            let uses = match trees.first().and_then(Option::as_ref) {
                Some(tree) if at == 0 => Ok(lfs::names_lfs_filter_among(root, &tree.attributes)),
                _ => lfs::tracks_lfs_filter(root, bounds, interrupt).map_err(lfs::unbuilt),
            };
            if self.lfs_unbuilt(repo, uses)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// What an LFS listing step of `repo` gave: the interrupt ends the run, and a failure refuses the route `lfs`,
    /// naming the repository ([`lfs::unlisted`]).
    fn lfs_unbuilt<T>(&mut self, repo: &Repo, result: Result<T, Unbuilt>) -> Result<T, End> {
        match result {
            Ok(value) => Ok(value),
            Err(Unbuilt::Interrupted) => Err(End::Interrupted),
            Err(Unbuilt::Failed(why)) => Err(lfs::refused(self, &lfs::unlisted(&repo.name, &why))),
        }
    }

    /// The repository whose root is `root` as the LFS listing asks git about it: a pair's as [`Attempt::bound_at`]
    /// gives it; the run's one repository bounded at its root's parent folder.
    fn lfs_bound<'b>(&'b self, root: &'b Path) -> Result<Bounded<'b>, String> {
        let alone = Bounded {
            root,
            ceiling: root.parent().unwrap_or(root),
            place: None,
        };
        Ok(self.bound_at(root)?.unwrap_or(alone))
    }

    /// The names of the files of the sibling `repo`'s `snapshot`, which its SDK check reads; its toolchain manifest is
    /// not read, as only the main repository's says what the run needs.
    fn sibling_tree(&mut self, repo: &Repo, snapshot: &Snapshot) -> Result<Option<Tree>, End> {
        self.go_on()?;
        let folder = PathBuf::from(&repo.root);
        let git = Git::find(&folder).map_err(End::Failed)?;
        let names = self.file_names(&folder, &git, &snapshot.commit, &repo.name)?;
        Ok(names.map(|names| Tree {
            git,
            folder,
            names,
            manifest: None,
            manifest_file: None,
            godot_pins: Vec::new(),
            godot_skips: Vec::new(),
            small: lfs::Small::default(),
            attributes: Vec::new(),
        }))
    }

    /// The preflight: readies the work folder, asks the builder to probe the tools of `needs`, and checks the builder
    /// runs this gate's version. Whether it created the bare repository just now, the builder's boot time, its SDKs and
    /// what it said of each tool sent. A builder with no `gate box` that takes the run is not routed, as one of another
    /// version is not.
    fn preflight(&mut self, location: &Location, needs: &[(String, String)]) -> Result<Prepared, End> {
        self.go_on()?;
        let nonce: Arc<str> = Arc::from(payload::run_nonce());
        let input = payload::input(&payload::preflight_word(location, needs, &nonce), None);
        let sent: Vec<String> = needs.iter().map(|(name, _)| name.clone()).collect();
        let watch = Watch::Setup(self.limits_for("preflight"));
        let raise = self.raises_at("preflight");
        let (mut called, _) = self.with_retry(SSH_FAILED, |attempt| {
            let only = CallOnly {
                tag_nonce: Some(Arc::clone(&nonce)),
                ..CallOnly::plain(Arc::clone(&input))
            };
            attempt.ssh(BoxVerb::Preflight, Echo::LogOnly, watch, raise, only)
        })?;
        let new_tree = new_work_tree(&called.lines);
        let created = called
            .lines
            .iter()
            .filter_map(|line| line.strip_prefix(CREATED))
            .map(str::to_owned)
            .collect();
        let boot = boot_time(&called.lines).map(str::to_owned);
        let sdks = sdks_of(&called.lines);
        let tools = toolchain::reported(&called.lines, &sent);
        let ended = preflight_ended(self.call.host(), self.profile().root(), &called);
        self.lines.append(&mut called.lines);
        self.not_started_if_killed("the preflight", &called)?;
        if let Err((line, end)) = ended {
            if let Some(line) = line {
                self.say(line);
            }
            return Err(end);
        }
        Ok(Prepared {
            created,
            new_tree,
            boot,
            sdks,
            sent,
            tools,
        })
    }

    /// The snapshot's tree as the parity check reads it, listed before the preflight by one recursive listing that
    /// names its folders too ([`Attempt::tree_entries`]): the names of its files and submodules, and its
    /// `.gate/toolchain.toml` ([`Attempt::manifest`]), found among all its entries ([`manifest_entry`]), which name a
    /// folder at that path too. A listing past the git output cap, or one that is not UTF-8 or not of `ls-tree`'s form,
    /// is said and gives `None`, which skips the whole check.
    fn tree(&mut self, snapshot: &Snapshot) -> Result<Option<Tree>, End> {
        self.go_on()?;
        let folder = PathBuf::from(self.call.root);
        let git = Git::find(&folder).map_err(End::Failed)?;
        let Some(entries) = self.tree_entries(&git, &snapshot.commit)? else {
            return Ok(None);
        };
        let names = entries
            .iter()
            .filter(|entry| entry.kind != "tree")
            .map(|entry| entry.path.clone())
            .collect();
        let (manifest, manifest_file) = match self.manifest(&git, &entries)? {
            Some(read) => {
                self.note_shadowed_manifest();
                (Some(read), None)
            }
            None if gate_is_no_folder(&entries) => (None, None),
            None => self.declared_manifest(),
        };
        let (godot_pins, godot_skips) = self.godot_pins(&git, &entries)?;
        Ok(Some(Tree {
            git,
            folder,
            names,
            manifest,
            manifest_file,
            godot_pins,
            godot_skips,
            small: small_blobs_of(&entries),
            attributes: entries
                .iter()
                .filter(|entry| entry.kind == "blob" && lfs::is_attributes(&entry.path))
                .map(|entry| entry.path.clone())
                .collect(),
        }))
    }

    /// The names of the files and submodules of the snapshot's tree `treeish`, asked in the worktree `folder`, as its
    /// listing spells them, one per NUL; `None`, with a line naming the sibling `of`, when that listing is past the git
    /// output cap or is not UTF-8.
    fn file_names(&mut self, folder: &Path, git: &Git, treeish: &str, of: &str) -> Result<Option<Vec<String>>, End> {
        let Some(listing) = self.listed(folder, git, &["ls-tree", "-r", "-z", "--name-only", treeish], Some(of))? else {
            return Ok(None);
        };
        Ok(Some(listing.split('\0').filter(|name| !name.is_empty()).map(str::to_owned).collect()))
    }

    /// The entries of `git ls-tree -r -t -l -z` over the tree `treeish` ([`entries_of`]), every file, submodule and
    /// folder at any depth, each with its mode, type, object id, blob size and path from the root; `None`, with a line,
    /// when that listing is past the git output cap, is not UTF-8 or is not of that form.
    fn tree_entries(&mut self, git: &Git, treeish: &str) -> Result<Option<Vec<Entry>>, End> {
        let Some(listing) = self.listed(Path::new(self.call.root), git, &["ls-tree", "-r", "-t", "-l", "-z", treeish], None)? else {
            return Ok(None);
        };
        let Some(entries) = entries_of(&listing) else {
            self.say("gate: cannot list the snapshot's files: an entry is not of git ls-tree's form; skipping the toolchain check".to_owned());
            return Ok(None);
        };
        Ok(Some(entries))
    }

    /// What one listing of `args` in the worktree `folder` answered as text; `None`, with a line, when it is past the
    /// git output cap or is not UTF-8, either of which skips the whole toolchain check, or for the sibling `of` its SDK
    /// check alone. Any other git failure ends the run.
    fn listed(&mut self, folder: &Path, git: &Git, args: &[&str], of: Option<&str>) -> Result<Option<String>, End> {
        self.go_on()?;
        let listing = match self.git_bytes_in(folder, git, args) {
            Ok(listing) => listing,
            Err(GitError::OverCap(why)) => {
                self.say(skipped_listing(of, &why));
                return Ok(None);
            }
            Err(error) => return Err(git_end(error)),
        };
        let Ok(listing) = String::from_utf8(listing) else {
            self.say(skipped_listing(of, "a path is not valid UTF-8"));
            return Ok(None);
        };
        Ok(Some(listing))
    }

    /// The snapshot's `.gate/toolchain.toml` among the candidate `entries` ([`manifest_entry`]), read by its object id;
    /// `None` when it has none, `Err` with why when it is no regular file, is past the git output cap or does not read.
    /// Any other git failure ends the run. `--test-remote-interrupt-at manifest` raises the console interrupt just
    /// before the read.
    fn manifest(&mut self, git: &Git, entries: &[Entry]) -> Result<Option<Result<Manifest, String>>, End> {
        let entry = match manifest_entry(entries) {
            None => return Ok(None),
            Some(Err(why)) => return Ok(Some(Err(why))),
            Some(Ok(entry)) => entry,
        };
        if let Some(interrupt) = self.call.interrupt.filter(|_| self.raises_at("manifest")) {
            interrupt.raise();
        }
        self.go_on()?;
        let read = match self.git_bytes(git, &["cat-file", "blob", &entry.oid]) {
            Ok(bytes) => toolchain::read_bytes(&bytes),
            Err(GitError::OverCap(why)) => Err(why),
            Err(error) => return Err(git_end(error)),
        };
        self.go_on()?;
        Ok(Some(read))
    }

    /// The repo's copy of its manifest in the declarations folder this PC's routing file names, read for a snapshot
    /// whose tree has none ([`route::read_declarations_copy`]): as it reads, `Err` with why when it cannot be read, with
    /// its full path; `None` when there is none. Why the folder cannot be used, and where a copy was read from, go to
    /// the log alone.
    fn declared_manifest(&mut self) -> (Option<Result<Manifest, String>>, Option<String>) {
        let mut lines = Vec::new();
        let config = declarations_config(&mut lines);
        let (read, source) = route::read_declarations_copy(self.call.repo, config.as_ref(), MANIFEST_NAME);
        lines.extend(source.log_lines(self.call.repo, MANIFEST_NAME));
        for line in route::unsaid(lines) {
            self.logged_kept(line);
        }
        let read = match read {
            Ok(None) => return (None, None),
            Ok(Some(bytes)) => toolchain::read_bytes(&bytes),
            Err(why) => Err(why),
        };
        (Some(read), Some(source.said(MANIFEST_NAME)))
    }

    /// Logs that the snapshot's own manifest shadows its copy in the declarations folder this PC's routing file names,
    /// when anything is there by that name ([`route::declared_copy`]): the log alone, once a run, as routing notes the
    /// files it reads.
    fn note_shadowed_manifest(&mut self) {
        let mut lines = Vec::new();
        let config = declarations_config(&mut lines);
        if let Some(path) = route::declared_copy(self.call.repo, config.as_ref(), MANIFEST_NAME) {
            lines.push(route::shadows_line(MANIFEST_NAME, &path));
        }
        for line in route::unsaid(lines) {
            self.logged_kept(line);
        }
    }

    /// The `Godot.NET.Sdk` versions the snapshot's project files pin ([`csproj_pins`]), read in two git calls whatever
    /// their count — one `cat-file --batch-check` for their sizes, so a file past [`CSPROJ_BYTES`] is left unread, then
    /// one `cat-file --batch`, bounded by [`CSPROJ_READ`], for the rest — and the line for each file left out. A tree
    /// holding no project file asks git nothing. Any git failure ends the run.
    fn godot_pins(&mut self, git: &Git, entries: &[Entry]) -> Result<(Vec<Pin>, Vec<String>), End> {
        self.go_on()?;
        let attempt = &*self;
        let pins = csproj_pins(
            entries,
            |files| {
                let answer = attempt.blobs(git, &["cat-file", "--batch-check=%(objectsize)"], files, CSPROJ_SIZES)?;
                sizes_of(&answer, files).map_err(|why| csproj_end(&why))
            },
            |files| {
                let answer = attempt.blobs(git, &["cat-file", "--batch"], files, CSPROJ_READ)?;
                texts_of(&answer, files).map_err(|why| csproj_end(&why))
            },
        )?;
        self.go_on()?;
        Ok(pins)
    }

    /// What git writes for `args` in the worktree, its standard input fed the object ids of `files`, one to a line, and
    /// its standard output read up to `cap` bytes, as one `cat-file` call; a Ctrl+C, a failure and an answer past the
    /// cap each end the run.
    fn blobs(&self, git: &Git, args: &[&str], files: &[&Entry], cap: usize) -> Result<Vec<u8>, End> {
        let ids: String = files.iter().flat_map(|file| [file.oid.as_str(), "\n"]).collect();
        self.git_bytes_within(Path::new(self.call.root), git, args, Some(ids.as_bytes()), cap)
            .map_err(git_end)
    }

    /// The toolchain parity check, made after the preflight and before the push, so a refusal costs no transfer, over
    /// the snapshot's `tree`, which `None` skips: the builder must have the SDK [`Attempt::sdk_needs`] reads and the
    /// tools the manifest declares, as [`toolchain::judge`] weighs what the `prepared` preflight said of them. The SDK
    /// step's own lines come first, its "cannot read this PC's SDK list" among them; then each manifest problem and
    /// tool not checked, in order; then a builder that lacks any of it says one mismatch line per need, the SDK's
    /// before the tools', and is refused `parity` once. A pair's repositories are each checked so, in order, a sibling
    /// for its own SDK alone, as its own `global.json` pins it, its lines naming it; the tools are the main one's, and
    /// git-lfs when the run sends LFS content (`lfs`).
    ///
    /// `Ok` with the version of the builder's Godot install the run uses ([`toolchain::godot_choice`]), `None` when none
    /// is chosen; a main repo pinning several Godot versions chooses none, and a call that runs a Godot console
    /// ([`Attempt::godot_command`]) then says why after the mismatch lines and is refused `parity`. A repo pinning no Godot
    /// version, or taking any, chooses none too, and such a run uses the newest install the builder listed
    /// ([`toolchain::no_chosen_godot`]), warning when the repo pins none, said before the run starts and only when the
    /// run is not refused; it stays here, refused `parity`, when the builder lists no install, did not list its
    /// installs, or the repo's pins could not be read. A pin no listed install meets is refused as the parity mismatch it
    /// is.
    ///
    /// The Unreal Engine version is chosen beside it ([`toolchain::unreal_choice`]): the highest the builder listed that
    /// meets the main repo's manifest `unreal` pin, none for no pin or any version; the manifest's `[env]` names are
    /// carried beside it ([`toolchain::engine_env`]).
    fn parity(&mut self, repos: &[Repo], snapshots: &[Snapshot], trees: &[Option<Tree>], prepared: &Prepared, lfs: bool) -> Result<Chosen, End> {
        let mut refused = false;
        let main = trees.first().and_then(Option::as_ref);
        let pins_read = main.is_some_and(Tree::godot_pins_read);
        let required = main.and_then(|tree| toolchain::godot_required(tree.manifest.as_ref(), &tree.godot_pins));
        for (at, ((repo, snapshot), tree)) in repos.iter().zip(snapshots).zip(trees).enumerate() {
            let Some(tree) = tree else {
                continue;
            };
            let mut needs = self.repo_needs(at, repo, snapshot, tree, prepared)?;
            if at == 0 {
                needs.extend(self.tool_needs(tree, prepared, required.as_deref(), lfs));
            }
            if needs.is_empty() {
                continue;
            }
            let sdks = prepared.sdks.as_deref().unwrap_or_default();
            for line in parity::lines(&repo.on_builder, &needs, self.call.host(), sdks) {
                self.say(line);
            }
            refused = true;
        }
        let mut warning = None;
        let godot_command = self.godot_command();
        let chosen = match toolchain::godot_choice(required.as_deref(), &prepared.tools) {
            GodotChoice::None => match toolchain::no_chosen_godot(required.as_deref(), pins_read, godot_command, &prepared.tools, self.call.host()) {
                NoChosenGodot::None => None,
                NoChosenGodot::Newest { version, warning: why } => {
                    warning = why;
                    Some(version)
                }
                NoChosenGodot::Refuse(line) => {
                    self.say(line);
                    refused = true;
                    None
                }
            },
            GodotChoice::Version(version) => Some(version),
            GodotChoice::Several(list) => {
                if godot_command {
                    self.say(toolchain::several_godot_line(&list));
                    refused = true;
                }
                None
            }
        };
        if refused {
            return Err(End::Refused(FallbackReason::Parity));
        }
        // Said here, on the run's `Ok` path alone, so a refused run is not told which version it would have used; still
        // before the push and the run start.
        if let Some(warning) = warning {
            self.say(warning);
        }
        let unreal = main.and_then(|tree| toolchain::unreal_required(tree.manifest.as_ref()));
        Ok(Chosen {
            godot: chosen,
            unreal: toolchain::unreal_choice(unreal.as_deref(), &prepared.tools),
            engine_env: main.map(|tree| toolchain::engine_env(tree.manifest.as_ref())).unwrap_or_default(),
        })
    }

    /// What the builder lacks for the repository `repo`, at `at` in the run's order, whose `snapshot` has the tree
    /// `tree`: its SDK, the SDK step's own lines said; the main one's tools are [`Attempt::tool_needs`]'. A sibling's git
    /// failure keeps the run here naming it.
    fn repo_needs(&mut self, at: usize, repo: &Repo, snapshot: &Snapshot, tree: &Tree, prepared: &Prepared) -> Result<Vec<parity::Need>, End> {
        self.go_on()?;
        let needs = self.sdk_needs(snapshot, tree, prepared.sdks.as_deref());
        needs.map_err(|end| repo_end(at, &repo.name, end))
    }

    /// What the builder lacks of the tools the main repository's tree `tree` declares and of the Godot versions `godot`
    /// it requires ([`toolchain::godot_required`]), as the `prepared` preflight said of them, each manifest problem, tool
    /// not checked and project file left unread said; with `lfs`, a snapshot holding LFS pointers or a repository whose
    /// tracked attributes name the LFS filter ([`Attempt::lfs_filtered`]), git-lfs at 3.1.3 or later, or the manifest's
    /// own minimum when higher, too ([`toolchain::judge_with_git_lfs`]).
    fn tool_needs(&mut self, tree: &Tree, prepared: &Prepared, godot: Option<&[Pin]>, lfs: bool) -> Vec<parity::Need> {
        let host = self.call.host();
        let file = tree.manifest_file.as_deref().unwrap_or(toolchain::MANIFEST_SAID);
        let manifest = tree.manifest.as_ref().map(|read| (read, file));
        let judge = if lfs { toolchain::judge_with_git_lfs } else { toolchain::judge };
        let judged = judge(manifest, godot, &prepared.sent, &prepared.tools, host);
        for line in judged.skips {
            self.say(line);
        }
        for line in &tree.godot_skips {
            self.say(line.clone());
        }
        judged.needs
    }

    /// The SDK needs of the snapshot whose `tree` the builder whose preflight listed the SDKs `sdks` lacks, as
    /// [`parity::need`] reads them from the snapshot's root `global.json`, whatever the letter case of its name, and,
    /// with no pin, this PC's newest SDK major. None when the builder listed no SDK line or the tree is no .NET tree.
    fn sdk_needs(&mut self, snapshot: &Snapshot, tree: &Tree, sdks: Option<&[String]>) -> Result<Vec<parity::Need>, End> {
        let Some(sdks) = sdks else {
            return Ok(Vec::new());
        };
        if !parity::is_dotnet_tree(tree.names.iter().map(String::as_str)) {
            return Ok(Vec::new());
        }
        // dotnet reads a root `global.json` whatever the letter case of its name, as Windows opens it.
        let global_json = tree.names.iter().find(|name| name.eq_ignore_ascii_case("global.json"));
        let pin = match global_json {
            Some(name) => match self.git_bytes_in(&tree.folder, &tree.git, &["show", &format!("{}:{name}", snapshot.commit)]) {
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
        let needs = parity::need(&pin, sdks, || self.local_major());
        // A Ctrl+C while this PC's SDK list was read ends the run here, as an interrupt, before the push.
        self.go_on()?;
        Ok(needs)
    }

    /// What git writes on standard output for `args` in the worktree, as the bytes it wrote, a call the console
    /// interrupt ends; a failure is the run's to tell apart.
    pub(super) fn git_bytes(&self, git: &Git, args: &[&str]) -> Result<Vec<u8>, GitError> {
        self.git_bytes_in(Path::new(self.call.root), git, args)
    }

    /// What git writes on standard output for `args` in the worktree `folder`, as [`Attempt::git_bytes`] asks it, a pair's
    /// repository asked at the place git found it at its check ([`Attempt::bound_at`]).
    fn git_bytes_in(&self, folder: &Path, git: &Git, args: &[&str]) -> Result<Vec<u8>, GitError> {
        self.git_bytes_within(folder, git, args, None, git::OUTPUT_CAP)
    }

    /// [`Attempt::git_bytes_in`] with the bytes `stdin` fed on git's standard input and its standard output read up to
    /// `cap` bytes, for a call git answers from what it is fed and a caller whose own cap bounds the answer.
    fn git_bytes_within(&self, folder: &Path, git: &Git, args: &[&str], stdin: Option<&[u8]>, cap: usize) -> Result<Vec<u8>, GitError> {
        // A run of one repository gives git no variable of its own.
        let set = self
            .bound_at(folder)
            .map_err(GitError::Failed)?
            .map(|bound| bound.variables())
            .unwrap_or_default();
        git.output_bytes_within(&self.git_call(folder, args, &set, stdin), cap)
    }

    /// The attempt's git call in `folder` with `args`, given the variables `set` and fed `stdin`: under the attempt's
    /// git bounds, and ended by the call's console interrupt.
    fn git_call<'b>(
        &self,
        folder: &'b Path,
        args: &'b [&'b str],
        set: &'b [(&'b str, &'b std::ffi::OsStr)],
        stdin: Option<&'b [u8]>,
    ) -> git::Call<'b> {
        git::Call {
            set,
            interrupt: self.call.interrupt,
            stdin,
            ..git::Call::bounded(folder, args, self.git)
        }
    }

    /// This PC's newest .NET SDK major, from `--test-local-sdks` when given, else from `dotnet --list-sdks`, read once
    /// a run with the folders of every worktree it sends, a pair's siblings' among them, kept from dotnet's search
    /// ([`local_sdk_listing`]); 0 when no dotnet is found or it lists no SDK. A list that cannot be read is said, and
    /// checks no major; one whose read failed while the console interrupt is raised says nothing and checks no major,
    /// and [`Attempt::parity`] then ends the run as an interrupt. `--test-remote-interrupt-at sdks` raises the interrupt
    /// just before the read.
    fn local_major(&mut self) -> u64 {
        if let Some(major) = self.local_major {
            return major;
        }
        let major = self.read_local_major();
        self.local_major = Some(major);
        major
    }

    /// This PC's newest .NET SDK major as [`Attempt::local_major`] reads it, read now.
    fn read_local_major(&mut self) -> u64 {
        if let Some(listing) = self.local_sdks {
            return parity::newest_major(parity::listed(listing).iter().map(String::as_str));
        }
        if let Some(interrupt) = self.call.interrupt.filter(|_| self.raises_at("sdks")) {
            interrupt.raise();
        }
        let call = self.call;
        let worktrees: Vec<&Path> = std::iter::once(Path::new(call.root))
            .chain(call.siblings.iter().map(|sibling| Path::new(sibling.root())))
            .collect();
        match local_sdk_listing(Variables::from_process(), &worktrees, self.dotnet_limit, self.git.drain, call.interrupt) {
            Ok(listing) => parity::newest_major(gate_wire::sdk_versions(&listing)),
            Err(_) if self.call.interrupt.is_some_and(Interrupt::is_raised) => 0,
            Err(error) => {
                let why = String::from(error);
                self.say(format!("gate: cannot read this PC's SDK list: {why}; checking no SDK major"));
                0
            }
        }
    }

    /// Pushes each of `repos`' `snapshots` in order, the main one first, each then saved, under one transfer clock;
    /// the preflight `created` the bare repositories it names just now. The run's push time is the pushes' sum.
    fn pushes(&mut self, repos: &[Repo], snapshots: &[Snapshot], created: &[String]) -> Result<(), End> {
        self.each_push(repos, snapshots, created, Self::push)
    }

    /// Makes each push of `repos`' `snapshots` in order through `push`, every one watched by the one transfer clock
    /// started before the first.
    fn each_push(
        &mut self,
        repos: &[Repo],
        snapshots: &[Snapshot],
        created: &[String],
        mut push_one: impl FnMut(&mut Self, &Push<'_>, Watch) -> Result<(), End>,
    ) -> Result<(), End> {
        let watch = Watch::Setup(self.limits_for("push"));
        for (at, (repo, snapshot)) in repos.iter().zip(snapshots).enumerate() {
            let push = Push {
                at,
                repo,
                snapshot,
                created: created.contains(&repo.bare),
            };
            push_one(self, &push, watch)?;
        }
        Ok(())
    }

    /// Asks git again, given no place, where the pair's repository `repo`, at `at` in the run's order, is, once it is
    /// saved: a place other than `bound`'s, the one checked, keeps the run here naming it, as a `core.worktree`, a
    /// `commondir` or a `gitdir:` written while it was held would.
    fn kept_its_place(&self, at: usize, repo: &Repo, bound: Bounded<'_>) -> Result<(), End> {
        let unplaced = Bounded { place: None, ..bound };
        let again = snapshot::place_of(unplaced, self.git, self.call.interrupt);
        let again = again.map_err(|error| repo_end(at, &repo.name, git_end(error)))?;
        if bound.place == Some(&again) {
            Ok(())
        } else {
            Err(not_routed(&who(at, &repo.name), MOVED_GIT))
        }
    }

    /// Pushes one repository's snapshot, and its bridge unless its bare repository was created just now, with no hooks
    /// and no submodules, then saves the local ref and says the push: `gate: pushed snapshot …` for a run of one
    /// repository, `gate: remote pushed snapshot {name} …` for each of a pair's. A push that fails is `unreachable`; a
    /// sibling's save that fails keeps the run here naming it, the main one's ends the run.
    fn push(&mut self, push: &Push<'_>, watch: Watch) -> Result<(), End> {
        self.go_on()?;
        let Push { at, repo, snapshot, created } = *push;
        let (root, key, pair) = (Path::new(&repo.root), self.call.key, !self.call.siblings.is_empty());
        let target = self.transport.push_target(&self.profile(), &repo.on_builder);
        let target = target.map_err(|why| repo_end(at, &repo.name, End::Failed(why)))?;
        // Only the transport's own protocol may carry the push, so no `pushInsteadOf` a repository's config names can
        // turn it into a protocol helper: ssh to a builder, a local path for the test transport's bare repositories.
        // git reads `GIT_ALLOW_PROTOCOL` in place of every `protocol.*.allow`, a repository's own config's among them.
        let protocol = if target.ssh_command.is_some() { "ssh" } else { "file" };
        let mut args: Vec<String> = ["push", "--no-verify", "--no-recurse-submodules", "--progress"].map(str::to_owned).into();
        args.extend(target.receive_pack.iter().map(|pack| format!("--receive-pack={pack}")));
        args.extend(["--".to_owned(), target.url.clone(), format!("+{}:refs/gate/{key}", snapshot.commit)]);
        let bridge = snapshot.bridge.as_deref().filter(|_| !created);
        args.extend(bridge.map(|bridge| format!("+{bridge}:refs/gate-bridge/{key}")));
        let git = Git::find(root).map_err(|why| repo_end(at, &repo.name, End::Failed(why)))?;
        let started = Instant::now();
        let ceiling = self.ceiling.clone();
        let place = self.bound_at(root).map_err(End::Failed)?.and_then(|bound| bound.place).cloned();
        let bound = ceiling.as_deref().map(|ceiling| Bounded {
            root,
            ceiling,
            place: place.as_ref(),
        });
        // A run of one repository gives git no variable of its own.
        let variables = bound.map(|bound| bound.variables()).unwrap_or_default();
        let ssh_command = target.ssh_command.as_deref().map(|ssh| ("GIT_SSH_COMMAND", std::ffi::OsStr::new(ssh)));
        let allowed = ("GIT_ALLOW_PROTOCOL", std::ffi::OsStr::new(protocol));
        let set: Vec<(&str, &std::ffi::OsStr)> = ssh_command.into_iter().chain([allowed]).chain(variables.iter().copied()).collect();
        let (mut called, _) = self.with_retry(GIT_SSH_FAILED, |attempt| {
            attempt.git_started(git.command(root), &args, &set, Echo::Progress, watch)
        })?;
        let size = push_size(&called.lines);
        self.lines.append(&mut called.lines);
        self.not_started_if_killed("the push", &called)?;
        if called.status != 0 {
            let of = if pair { format!(" of {}", repo.name) } else { String::new() };
            self.say(format!(
                "gate: pushing the snapshot{of} to {} failed (status {})",
                target.url, called.status
            ));
            return Err(End::Refused(FallbackReason::Unreachable));
        }
        let reference = snapshot::snapshot_ref(key, self.call.builder.id.as_deref());
        let saved = snapshot::save(root, &variables, &reference, &snapshot.commit, self.git);
        saved.map_err(|error| repo_end(at, &repo.name, git_end(error)))?;
        // The push is timed to its save; the check of where git finds the repository is not part of it.
        let took = started.elapsed();
        if let Some(bound) = bound {
            self.kept_its_place(at, repo, bound)?;
        }
        self.pushed(repo, &snapshot.commit, &size, took);
        Ok(())
    }

    /// Adds `took`, the time of the push of `repo`'s `commit` of `size`, to the run's push time, the pushes' sum, and
    /// says the push.
    fn pushed(&mut self, repo: &Repo, commit: &str, size: &str, took: Duration) {
        self.split.push += took;
        let short = commit.get(..8).unwrap_or(commit);
        let line = if self.call.siblings.is_empty() {
            format!("gate: pushed snapshot {short} in {} s ({size})", seconds(took))
        } else {
            format!("gate: remote pushed snapshot {} {short} in {} s ({size})", repo.name, seconds(took))
        };
        self.say(line);
    }

    /// Ends the call `what`, a preflight or a push, that the client killed as the fallback `unreachable`: the command
    /// never started, so nothing is stopped. Its line names the builder unreachable, not the kill: the run goes on here
    /// with the same log, whose verdict would read a kill line as its own.
    fn not_started_if_killed(&mut self, what: &str, called: &Called) -> Result<(), End> {
        let Some(cut) = called.cut else {
            return Ok(());
        };
        self.say(format!("gate: remote {} unreachable: {}", self.call.host(), cut.reason(what)));
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

    /// The limits the call `name` is watched by: the attempt's, whose clock the setup's calls count on; the run call and
    /// the fetches count from their own start ([`Watch`]). For the one call `--test-remote-watch-call` names, the seams'
    /// stall window and, for any call but `run`, whose backstop has a seam of its own, its transfer ceiling, counted
    /// from now.
    pub(super) fn limits_for(&self, name: &str) -> Limits {
        let mut limits = self.limits;
        let Some(one) = self.one_call.filter(|one| one.name == name) else {
            return limits;
        };
        limits.stall = one.stall.unwrap_or(limits.stall);
        if let Some(ceiling) = one.ceiling {
            limits.started = Instant::now();
            limits.transfer = ceiling;
        }
        limits
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

    /// The run call to the builder at `location`: the command's own status; `unreachable` when the call and each of its
    /// retries failed before the banner, as the command never started; a refusal when its 75 comes with the box's refusal
    /// tag for the run whose nonce is `nonce` and no start marker; a lost connection when it exited 255 otherwise. A
    /// watchdog kill says
    /// its line, stops the command and ends the run killed; a Ctrl+C stops the command too. Each call made is fed
    /// `input` on its standard input ([`payload::input`]). With the end, what the call that counts said of the outputs
    /// ([`RunEnd`]).
    fn run_call(&mut self, location: &Location, input: &Arc<str>, nonce: &str) -> Result<RunEnd, End> {
        self.go_on()?;
        let limits = self.limits_for("run");
        // `stop` raises the interrupt during the run call too, so the stop it leads to can be interrupted again.
        let raise = self.raises_at("run") || self.raises_at("stop");
        let nonce: Arc<str> = Arc::from(nonce);
        let mut heard = None;
        let mut back_commits = None;
        let mut names_skipped = false;
        let mut cross_not_brought_back = false;
        let mut adjusted = None;
        let mut started = false;
        let mut last_error = None;
        // The command's time counts from the start of the call that counts, as its backstop does.
        let mut begun = None;
        let made = self.run_call_made(location, limits, |attempt| {
            begun = Some(Instant::now());
            let run = CallOnly {
                input: Input::Text(Arc::clone(input)),
                nonce: Some(Arc::clone(&nonce)),
                tag_nonce: None,
                sink: None,
                wire: Wire::Compressed,
                channel: None,
            };
            let called = attempt.ssh(BoxVerb::Run, Echo::Console, Watch::Run(limits), raise, run)?;
            heard.clone_from(&called.outputs);
            back_commits.clone_from(&called.back_commits);
            names_skipped = called.names_skipped;
            cross_not_brought_back = called.cross_not_brought_back;
            adjusted = called.adjusted;
            started = called.started;
            last_error.clone_from(&called.last_error);
            Ok(called)
        });
        self.split.command = begun.map_or(Duration::ZERO, |begun| begun.elapsed());
        self.adjusted = adjusted;
        let end = made?;
        Ok(RunEnd {
            end,
            outputs: heard,
            back_commits,
            names_skipped,
            cross_not_brought_back,
            started,
            last_error,
        })
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
                self.stop_after_ctrl_c(location);
                return Err(End::Interrupted);
            }
            Err(end) => return Err(end),
        };
        let not_installed = run_install_refused(self.call.host(), &called);
        // The run call's refusal tag for its install is followed by the box's install line, which the client turns into
        // the same `gate check --builder` line the preflight and inputs paths say. Read before the call's lines are kept.
        let tagged = install_tag_refusal(self.call.host(), called.lines.iter().chain(&called.last_error).map(String::as_str));
        let kept = self.lines.len();
        self.lines.append(&mut called.lines);
        let output = kept..self.lines.len();
        if let Some(Cut::Watchdog(kill)) = called.cut {
            self.say(watchdog::kill_line(
                kill,
                limits.stall,
                limits.backstop,
                limits.command.as_secs(),
                self.call.host(),
            ));
            self.noted(&called);
            self.stop(location);
            return Ok(End::Killed);
        }
        // The box killed the command itself and said so: its job is already ended, so nothing is stopped.
        if let Some(kill) = called.box_killed.filter(|_| called.status == KILLED) {
            self.noted(&called);
            self.box_killed = Some(kill);
            return Ok(End::Killed);
        }
        if again {
            return Ok(End::Refused(FallbackReason::Unreachable));
        }
        if self.go_on().is_err() {
            // A refused install started nothing, and the stop call would be refused by the same tripwire.
            if not_installed.is_none() {
                self.stop_after_ctrl_c(location);
            }
            return Err(End::Interrupted);
        }
        // The box refused before it read the run's request, so there is nothing to stop.
        if let Some(line) = not_installed {
            self.say(line);
            return Ok(End::Refused(FallbackReason::Install));
        }
        let end = status_end(&called, output);
        // The box refused the run for its install with its `install` tag instead: say the same line the call's plain
        // refusal says, so `gate check --builder` is pointed at on both paths.
        if matches!(end, End::Refused(FallbackReason::Install))
            && let Some(line) = tagged
        {
            self.say(line);
        }
        Ok(end)
    }

    /// Makes a call with `make` and, when it failed before the banner and its job did not write more than 1 MiB, says
    /// so, pauses and makes it again, up to [`retry::PRE_BANNER_RETRIES`] more times for as long as every call fails
    /// the same way; a Ctrl+C before a retry or during its pause ends the attempt instead. The call that counts, its
    /// lines not yet kept, and whether that call failed before the banner too.
    pub(super) fn with_retry(&mut self, fail_status: i32, mut make: impl FnMut(&mut Self) -> Result<Called, End>) -> Result<(Called, bool), End> {
        let mut called = make(self)?;
        for retry in 1..=retry::PRE_BANNER_RETRIES {
            if !called.before_the_banner(fail_status) || called.wrote_much() {
                return Ok((called, false));
            }
            self.lines.append(&mut called.lines);
            self.go_on()?;
            self.say(retry::retry_line(self.call.host(), retry, retry::PRE_BANNER_RETRIES));
            paused(self.pause, self.call.interrupt)?;
            self.go_on()?;
            called = make(self)?;
        }
        let again = called.before_the_banner(fail_status);
        Ok((called, again))
    }

    /// A transfer after the command, each attempt made with `make` under `watch` by a program that exits `fail_status`
    /// when ssh failed. A call that failed as a connection fails ([`Called::transfer_failed`]) has its problem noted
    /// and is said with the reason `reason` gives it, then made again after its pause, up to
    /// [`retry::TRANSFER_RETRIES`] times; a Ctrl+C before a retry or during its pause ends the attempt instead. Every
    /// attempt of a fetch shares one transfer ceiling, counted from the first one's start: with less than the pause
    /// left of it, the call is not made again but ends as one cut at the ceiling. What the transfer came to, or why a
    /// call could not be made.
    pub(super) fn transfer_call(
        &mut self,
        fail_status: i32,
        watch: Watch,
        mut make: impl FnMut(&mut Self, Watch) -> Result<Called, End>,
        reason: impl Fn(&Called) -> String,
    ) -> Result<Transferred, End> {
        let begun = Instant::now();
        let watch = watch.counted_from(begun);
        let mut first = None;
        let mut retries = (1..).zip(self.transfer_pauses);
        loop {
            let mut called = make(self, watch)?;
            if !called.transfer_failed(fail_status) {
                return Ok(Transferred {
                    called,
                    exhausted: false,
                    first,
                });
            }
            let Some((retry, pause)) = retries.next() else {
                return Ok(Transferred {
                    called,
                    exhausted: true,
                    first,
                });
            };
            if let Some(ceiling) = watch.transfer_ceiling()
                && ceiling.saturating_sub(begun.elapsed()) < pause
            {
                called.cut = Some(Cut::Ceiling(ceiling));
                return Ok(Transferred {
                    called,
                    exhausted: false,
                    first,
                });
            }
            self.noted(&called);
            let why = reason(&called);
            self.go_on()?;
            self.say(retry::transfer_retry_line(self.call.host(), &why, retry));
            paused(pause, self.call.interrupt)?;
            self.go_on()?;
            first.get_or_insert(why);
        }
    }

    /// After the run call's connection to the builder at `location` was lost: stops the command, then, when the
    /// preflight said the builder's boot time, `before`, asks for it again, as [`after_loss`] reads the two. A restart
    /// falls back `rebooted`, and no boot time to compare by `unreachable`; either leaves the run call's own lines,
    /// `output` among the attempt's, out of the run that falls back. Else the connection was lost, and a Ctrl+C that
    /// came meanwhile ends the run as an interrupt; one that came by the end of the stop ends it there, with no boot
    /// call.
    fn connection_lost(&mut self, location: &Location, before: Option<&str>, output: Range<usize>) -> End {
        let host = self.call.host();
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
                let shown = escaped(git::first_bytes(booted, git::FAILURE_BYTES));
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
        let what = format!("asking {} for its boot time", self.call.host());
        let called = self.bounded(BoxVerb::Boot, location, &what)?;
        (called.status == 0 && called.cut.is_none())
            .then(|| boot_time(&called.lines).map(str::to_owned))
            .flatten()
    }

    /// Stops the command the run call started on the builder at `location`, with the `stop` call, a bounded call; the
    /// run's end is the caller's however it went.
    fn stop(&mut self, location: &Location) {
        let what = format!("stopping the command on {}", self.call.host());
        self.bounded(BoxVerb::Stop, location, &what);
    }

    /// [`Attempt::stop`] after a Ctrl+C, saying first how long the stop may take, its bound in whole seconds, so the
    /// wait that follows the Ctrl+C is explained. A Ctrl+C during the stop does not end it and says nothing more.
    fn stop_after_ctrl_c(&mut self, location: &Location) {
        let bound = watchdog::whole_seconds(self.limits.bound);
        self.say(format!("gate: stopping the command on {} (up to {bound} s)", self.call.host()));
        self.stop(location);
    }

    /// The bounded call `verb` to the builder at `location`, which the bound limits and a Ctrl+C does not end; it is
    /// made once more after a failure before the banner while 5 s of the bound would be left. Keeps the lines of the
    /// call that counts, says when it did not finish or failed, `what` it does named, and gives it back; `None` when it
    /// could not be made. With `--test-remote-interrupt-at` naming `verb`, its first call raises the console interrupt
    /// as soon as it runs.
    fn bounded(&mut self, verb: BoxVerb, location: &Location, what: &str) -> Option<Called> {
        let input = payload::input(&payload::location_word(location), None);
        let bound = self.limits.bound;
        let begun = Instant::now();
        let raise = self.raises_at(verb.as_str());
        let mut made = self.ssh(verb, Echo::LogOnly, Watch::Bounded(bound), raise, CallOnly::plain(Arc::clone(&input)));
        let again = matches!(&made, Ok(first) if first.before_the_banner(SSH_FAILED)) && retry::retry_allowed(bound, begun.elapsed(), self.pause);
        if again {
            if let Ok(first) = &mut made {
                self.lines.append(&mut first.lines);
            }
            self.say(retry::retry_line(self.call.host(), 1, 1));
            thread::sleep(self.pause);
            made = self.ssh(
                verb,
                Echo::LogOnly,
                Watch::Bounded(bound.saturating_sub(begun.elapsed())),
                false,
                CallOnly::plain(input),
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

    /// One call of `gate box` on the builder for `verb`, its line [`Profile::line`], fed its request on its standard
    /// input; watched as `watch` says; the console interrupt raised after its first output byte when `raise`; what only
    /// this call gives it, `only`.
    fn ssh(&mut self, verb: BoxVerb, echo: Echo, watch: Watch, raise: bool, only: CallOnly) -> Result<Called, End> {
        let system = gate_win32::system_directory().map_err(|error| End::Failed(format!("cannot read the system folder: {error}")))?;
        let command = call_command(self.transport.call(self.call.host(), &self.profile().line(verb), only.wire), &system);
        let starts = Starts {
            breakaway: self.transport.box_runs_here(),
            raise,
            input: Some(Fed { verb, input: only.input }),
            nonce: only.nonce,
            tag_nonce: only.tag_nonce,
            sink: only.sink,
            channel: only.channel,
        };
        self.started(&command, echo, watch, starts)
    }

    /// The outputs call to the builder at `location` for the archive `name`: `gate box outputs`, made again when it
    /// failed before the banner, watched as `watch` says, a fetch's, and the console interrupt raised after its first
    /// output byte with `--test-remote-interrupt-at outputs`. Each attempt writes its standard output raw to the file
    /// `archive` makes new for it; its standard error lines go to the log. Each attempt is watched as the outputs pull
    /// `pull`, whose progress it says against the archive's size when the box said it. What the pull received is
    /// counted from nothing before the call, so a Ctrl+C that ends it before it starts leaves no earlier call's figures.
    /// The call that counts, its lines kept and still on it; else why the file or the call could not be made, or the
    /// interrupt.
    pub(super) fn outputs_call(
        &mut self,
        location: &Location,
        name: &str,
        archive: &mut outputs::Archive,
        watch: Watch,
        pull: Pull,
    ) -> Result<Called, End> {
        self.pulled_bytes = 0;
        self.pulled_last = None;
        self.go_on()?;
        let input = payload::input(&payload::outputs_word(location, name, None), None);
        let raise = self.raises_at("outputs");
        self.pull_periods_said.set(0);
        self.pulling = Some(pull);
        let towards = self.channel_towards();
        // Why the proofs could not be drawn, said once for the call when its last attempt could not draw them either.
        let mut undrawn = None;
        let retried = self.with_retry(SSH_FAILED, |attempt| {
            let cap = sent_limit(attempt.outputs_budget);
            let sink = Some(archive.create(cap).map_err(End::Failed)?);
            let only = match towards.map(|(data, local)| channel::Asked::drawn(data, local)) {
                Some(Ok(asked)) => {
                    undrawn = None;
                    // A nonce of the attempt's own, so only this attempt's reader takes its box's port line.
                    let nonce = payload::run_nonce();
                    let word = payload::outputs_word(location, name, Some((&asked, &nonce)));
                    CallOnly {
                        input: Input::Text(payload::input(&word, None)),
                        nonce: None,
                        tag_nonce: Some(Arc::from(nonce)),
                        sink,
                        wire: Wire::Plain,
                        channel: Some(channel::Dial {
                            asked,
                            size: pull.total,
                            fault: attempt.channel_fault.and_then(channel::Fault::named),
                        }),
                    }
                }
                drawn => {
                    if let Some(Err(why)) = drawn {
                        undrawn = Some(why);
                    }
                    CallOnly {
                        sink,
                        ..CallOnly::plain(Arc::clone(&input))
                    }
                }
            };
            attempt.ssh(BoxVerb::Outputs, Echo::LogOnly, watch, raise, only)
        });
        if let Some(why) = undrawn {
            self.logged(&channel::not_used_line(&why));
        }
        let retried = match retried {
            Ok((called, _)) if called.channel_refused() => {
                self.logged(&channel::not_used_line(&channel_why(&called)));
                self.lines.extend_from_slice(&called.lines);
                self.counted_with_channel(|attempt| {
                    attempt.with_retry(SSH_FAILED, |attempt| {
                        let only = CallOnly {
                            sink: Some(archive.create(sent_limit(attempt.outputs_budget)).map_err(End::Failed)?),
                            ..CallOnly::plain(Arc::clone(&input))
                        };
                        attempt.ssh(BoxVerb::Outputs, Echo::LogOnly, watch, raise, only)
                    })
                })
            }
            other => other,
        };
        self.pulling = None;
        let (called, _) = retried?;
        self.lines.extend_from_slice(&called.lines);
        Ok(called)
    }

    /// Makes `fallback`, the pull over ssh after the box refused the data channel, counting what came over the channel
    /// before it, the pull's figure as the channel's call left it, into the pull's total once, however the fallback
    /// ends: before its watch ran, or after it, which sets the figure to the ssh call's own.
    fn counted_with_channel<T>(&mut self, fallback: impl FnOnce(&mut Self) -> T) -> T {
        let over_channel = self.pulled_bytes;
        self.pulled_bytes = 0;
        let ended = fallback(self);
        self.pulled_bytes += over_channel;
        ended
    }

    /// The builder's `data` address and this PC's own address toward it, when the outputs pull goes over a data channel
    /// ([`channel::planned`]); `None` when it goes over ssh, the line saying why logged when the builder has `data`.
    fn channel_towards(&mut self) -> Option<(Ipv4Addr, Ipv4Addr)> {
        let towards = channel::planned(self.call.builder).and_then(|data| match data {
            Some(data) => channel::local_toward(data).map(|local| Some((data, local))),
            None => Ok(None),
        });
        match towards {
            Ok(towards) => towards,
            Err(why) => {
                self.logged(&channel::not_used_line(&why));
                None
            }
        }
    }

    /// One inputs call, `gate box inputs`, made again when it failed before the banner, watched as `watch` says,
    /// and the console interrupt raised after its first output byte with `--test-remote-interrupt-at inputs`. Each
    /// attempt is fed `head`, then, when an archive is given, the open file's first bytes up to its packed length, read
    /// from its start at a position of its own; its standard output goes raw into the sink `sink` makes anew when
    /// one is given, else to the log with its standard error. The call that counts, its lines kept; else why the
    /// archive, the sink or the call could not be made, or the interrupt.
    pub(super) fn inputs_call(
        &mut self,
        (head, nonce): (&Arc<str>, &str),
        archive: Option<(&File, u64)>,
        sink: Option<&dyn Fn() -> Result<RawSink, String>>,
        watch: Watch,
    ) -> Result<Called, End> {
        let raise = self.raises_at("inputs");
        self.archive_call(BoxVerb::Inputs, raise, (head, nonce, archive), sink, watch)
    }

    /// One LFS call, `gate box lfs`, made as [`Attempt::inputs_call`] makes an inputs call, but for the console
    /// interrupt, which `--test-remote-interrupt-at lfs` raises at the LFS phase's start instead.
    pub(super) fn lfs_call(
        &mut self,
        (head, nonce): (&Arc<str>, &str),
        archive: Option<(&File, u64)>,
        sink: Option<&dyn Fn() -> Result<RawSink, String>>,
        watch: Watch,
    ) -> Result<Called, End> {
        self.archive_call(BoxVerb::Lfs, false, (head, nonce, archive), sink, watch)
    }

    /// One call of `gate box <verb>` fed `head`, whose word carries the call's `nonce`, and, when given, an archive
    /// ([`Attempt::inputs_call`]), the console interrupt raised after its first output byte when `raise`. A call the
    /// builder's drain refused by its tag for `nonce` ends the attempt [`End::Draining`].
    fn archive_call(
        &mut self,
        verb: BoxVerb,
        raise: bool,
        (head, nonce, archive): (&Arc<str>, &str, Option<(&File, u64)>),
        sink: Option<&dyn Fn() -> Result<RawSink, String>>,
        watch: Watch,
    ) -> Result<Called, End> {
        self.go_on()?;
        let nonce: Arc<str> = Arc::from(nonce);
        let (called, _) = self.with_retry(SSH_FAILED, |attempt| {
            let input = match archive {
                Some((file, length)) => {
                    let file = file
                        .try_clone()
                        .map_err(|error| End::Failed(format!("cannot read the archive sent: {error}")))?;
                    Input::Streamed(Arc::clone(head), Archived::new(file, length))
                }
                None => Input::Text(Arc::clone(head)),
            };
            let sink = sink.map(|make| make()).transpose().map_err(End::Failed)?;
            let only = CallOnly {
                input,
                nonce: None,
                tag_nonce: Some(Arc::clone(&nonce)),
                sink,
                wire: Wire::Plain,
                channel: None,
            };
            attempt.ssh(verb, Echo::LogOnly, watch, raise, only)
        })?;
        self.lines.extend(called.lines.iter().cloned());
        if let Some(line) = install_refused(self.call.host(), &called) {
            self.say(line);
            return Err(End::Refused(FallbackReason::Install));
        }
        if let Some(drain) = called.draining() {
            return Err(End::Draining(drain));
        }
        Ok(called)
    }

    /// git with `args` in the worktree at `folder`, a pair's sibling's among them, made as [`Attempt::git_started`]
    /// makes its call, its lines kept out of the log.
    pub(super) fn git_in(
        &mut self,
        folder: &Path,
        git: &Git,
        args: &[String],
        set: &[(&str, &std::ffi::OsStr)],
        watch: Watch,
    ) -> Result<Called, End> {
        self.git_started(git.command(folder), args, set, Echo::Unlogged, watch)
    }

    /// The git `command` with `args`, given the variables `set` beside the ones every call is given, among them the ssh
    /// it runs as `GIT_SSH_COMMAND` when there is one, its lines going where `echo` says, watched as `watch` says.
    fn git_started(
        &mut self,
        mut command: Command,
        args: &[String],
        set: &[(&str, &std::ffi::OsStr)],
        echo: Echo,
        watch: Watch,
    ) -> Result<Called, End> {
        command.args(args).envs(set.iter().copied());
        let starts = Starts {
            breakaway: false,
            raise: false,
            input: None,
            nonce: None,
            tag_nonce: None,
            sink: None,
            channel: None,
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
        let (suspended, stdout, stderr, stdin) = launched(&launch, &job, starts.input.is_some()).map_err(|error| cannot_start(error.to_string()))?;
        let running = suspended.assign_and_resume(&job).map_err(|error| cannot_start(error.to_string()))?;
        let feeding = Arc::new(Feeding::default());
        let feeder = stdin.zip(starts.input).map(|(stdin, input)| fed(stdin, input, Arc::clone(&feeding)));
        let shared = Arc::new(Shared::new());
        // Only the run call carries the run's nonce.
        let run_call = starts.nonce.is_some();
        let (done, readers) = mpsc::channel();
        let reading = Reading {
            echo,
            capped: echo != Echo::Console,
            lines: match (starts.nonce.as_deref(), starts.tag_nonce.as_deref()) {
                (Some(nonce), _) => Some(Arc::new(ProtocolLines::new(nonce))),
                (None, Some(nonce)) => Some(Arc::new(ProtocolLines::tags(nonce))),
                (None, None) => None,
            },
            log_cap: self.log_cap,
        };
        let copied = starts.sink.is_some();
        let asked_channel = starts.channel.is_some();
        let raw = if let Some(sink) = starts.sink {
            let capped = Capped {
                inner: sink.file,
                cap: sink.cap,
                shared: Arc::clone(&shared),
                written: 0,
            };
            Some(match starts.channel {
                // The archive comes over the channel: the call's standard output is read as lines, as any call's.
                Some(dial) => {
                    relayed(stdout, Reader::new(false, reading.clone(), out_log), &shared, done.clone());
                    dialled(dial, capped, sink.shown, &shared)
                }
                None => relayed_raw(stdout, capped, sink.shown, &shared, done.clone()),
            })
        } else {
            relayed(stdout, Reader::new(false, reading.clone(), out_log), &shared, done.clone());
            None
        };
        relayed(stderr, Reader::new(true, reading, err_log), &shared, done);
        let watched = Watched {
            running: &running,
            job: &job,
            progress: &shared.progress,
            feeding: &feeding,
            readers: &readers,
            shown: &shown,
            hang: self.run_hang(run_call),
            pull: self.pull_watch(),
        };
        let raise = self.call.interrupt.filter(|_| starts.raise);
        let watching = watched.watch(watch, self.call.interrupt, raise);
        let mut dialled = None;
        if let Some(raw) = raw {
            // The call's end has come: the copy writes nothing more into an archive the fetch is about to remove, and a
            // data channel's thread, which no drain waits on, gets the whole drain to close its sink and socket, which
            // it does within a slice of the stop, before it is joined.
            shared.stop();
            let drained = watching.ended.as_ref().is_ok_and(|(ended, _)| ended.drained());
            dialled = self.ended_raw(raw, drained && !asked_channel);
        }
        // A channel asked for whose thread gave no end, as one that did not end within the drain, counts as failed.
        let dialled = asked_channel.then(|| dialled.unwrap_or(channel::Dialled::NotEnded));
        let cut = watching.ended.as_ref().ok().and_then(|(_, looks)| looks.received);
        self.kept_from_watch(&shared.progress, copied, run_call, cut);
        let watched = self.watched_out(watching);
        // Kept whatever the call's end, so a warm-up that fails after the marker counts its commands as run.
        self.marker_seen |= held(&shared.heard).started;
        let (ended, looks) = watched?;
        if let Some(line) = &looks.unread {
            self.logged(line);
        }
        if let Some(why) = &looks.unsaid {
            self.problem.get_or_insert_with(|| why.clone());
        }
        if watch.byte_cap().is_some() && !matches!(ended, CallEnd::Exited(0)) {
            self.emptied(&job, &shown);
        }
        let heard = std::mem::take(&mut *held(&shared.heard));
        let called = self.call_result(called_of(ended, shared.progress.raised(), heard), feeder, &looks, copied);
        called.map(|called| Called { channel: dialled, ..called })
    }

    /// The watched call's result from what it came to, `ended`: a call that finished, its feeder's end checked when
    /// `feeder` fed it, with the bytes its job wrote and, for a call with a raw copy (`copied`), what that copy had
    /// received at a cut, as `looks` found; else why it failed, or the interrupt, its note said first.
    fn call_result(&mut self, ended: Result<Called, Unfinished>, feeder: Option<Feeder>, looks: &Looks, copied: bool) -> Result<Called, End> {
        match ended {
            Ok(called) => {
                if let Some(feeder) = feeder {
                    self.fed_out(feeder, &called);
                }
                Ok(Called {
                    written: looks.written,
                    received: looks.received.filter(|_| copied),
                    ..called
                })
            }
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

    /// Keeps what a watched call's `progress` says the attempt reads after it, once its raw copy has stopped: for a call
    /// with a raw copy (`copied`), the bytes it received and when the last of them came, or, for a call the watch cut,
    /// the bytes at the cut, `cut`, and the cut's moment, the figure the watchdog's line says; and for the run call
    /// (`run_call`), when the box's start marker came, once it did.
    fn kept_from_watch(&mut self, progress: &Progress, copied: bool, run_call: bool, cut: Option<Received>) {
        if copied {
            (self.pulled_bytes, self.pulled_last) = match cut {
                Some(Received { bytes, after }) => (bytes, Some(progress.begun + after)),
                None => (progress.received(), progress.last_received()),
            };
        }
        if run_call && let Some(started) = progress.started_at() {
            self.command_started = Some(started);
        }
    }

    /// How the watched call ended, as `watching` says, once the attempt has kept the run call's hang hint: the line said
    /// among its lines, or why it could not be said as its problem, and either way the hint as said, so the attempt never
    /// says it again; also when the watch itself failed after the hint was said.
    fn watched_out(&mut self, watching: Watching) -> Result<(CallEnd, Looks), End> {
        let Watching { ended, hinted } = watching;
        match hinted {
            Some(Ok(line)) => {
                self.hang_said = true;
                self.lines.push(line);
            }
            Some(Err(why)) => {
                self.hang_said = true;
                self.problem.get_or_insert(why);
            }
            None => {}
        }
        ended
    }

    /// For a call that ended as `called` having reached the box's banner, not refused and not with ssh's 255, joins its
    /// `feeder`, given at most [`DRAIN`] to end, and notes in the log a write of its input that failed, naming the call's
    /// verb. A call that ended any of the other ways says why on its own lines, and its unread input is no news, so its
    /// feeder, whose write ends with the call's input pipe, is let go unread. A failed write never fails the call, whose
    /// own end stands; the note is a logged line, so a log it cannot be appended to fails the call as any logged line does.
    fn fed_out(&mut self, feeder: Feeder, called: &Called) {
        // A call that ended before the banner ended with ssh's 255, so the status alone rules it out.
        let reached = called.status != SSH_FAILED && called.refusal().is_none();
        if !reached {
            return;
        }
        let deadline = Instant::now() + DRAIN;
        while !feeder.thread.is_finished() && Instant::now() < deadline {
            thread::sleep(PAUSE_STEP);
        }
        let verb = feeder.verb.as_str();
        if !feeder.thread.is_finished() {
            self.logged(&format!(
                "gate: the {verb} call's input was still being sent {} s after the call ended",
                DRAIN.as_secs()
            ));
            return;
        }
        match feeder.thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.logged(&format!("gate: the {verb} call's input was not sent: {error}")),
            Err(_) => self.logged(&format!("gate: the {verb} call's input feeder panicked")),
        }
    }

    /// Gives the raw copy's reader `raw` the rest of its [`DRAIN`] to end, when the call's own end did not already
    /// drain it (`drained`, never so for a data channel's thread, which says no drain), joins it, and says a reader
    /// that panicked or one that has not ended by then. The sink has already been made to stop writing, so nothing more
    /// goes into the archive the fetch is about to remove.
    /// How the data channel the thread fed the sink from ended, for a channel's thread; `None` for a raw copy, and for a
    /// thread that panicked or did not end.
    fn ended_raw(&mut self, raw: thread::JoinHandle<Option<channel::Dialled>>, drained: bool) -> Option<channel::Dialled> {
        let deadline = if drained { Instant::now() } else { Instant::now() + DRAIN };
        while !raw.is_finished() && Instant::now() < deadline {
            thread::sleep(PAUSE_STEP);
        }
        if raw.is_finished() {
            if let Ok(dialled) = raw.join() {
                dialled
            } else {
                self.logged("gate: the outputs call's raw copy panicked");
                None
            }
        } else {
            self.logged(&format!("gate: the outputs call's raw copy did not end within {} s", DRAIN.as_secs()));
            None
        }
    }

    /// Appends the gate's own `line` to the log alone and keeps it among the attempt's lines, so the log of a run here
    /// after a fallback, which starts without what the attempt wrote, says it again; the first failure to is kept.
    pub(super) fn logged_kept(&mut self, line: String) {
        self.logged(&line);
        self.lines.push(line);
    }

    /// Appends the gate's own `line` to the log and says it on stderr, or to the log alone for a drain poll; the first
    /// failure to is kept.
    pub(super) fn say(&mut self, line: String) {
        if self.log_only {
            self.logged(&line);
        } else if let Err(why) = super::say(self.call.log, &[&line], io::stderr().lock()) {
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
    pub(super) fn logged(&mut self, line: &str) {
        if let Err(why) = super::say(self.call.log, &[line], io::sink()) {
            self.problem.get_or_insert(why);
        }
    }

    /// The outcome of an attempt that ended at `end`; a line that could not be written fails the run.
    fn outcome(mut self, end: End) -> RemoteOutcome {
        let killed = matches!(end, End::Killed);
        let warm = self.warm_outcome(&end);
        let mut detail = None;
        let mut draining = None;
        let (status, reason) = match end {
            End::Draining(drain) => {
                draining = Some(drain);
                (REFUSED, None)
            }
            End::NotRouted(line) => {
                detail = Some(line.strip_prefix(NOT_ROUTED).unwrap_or(&line).to_owned());
                self.say(line);
                (REFUSED, Some(FallbackReason::Client))
            }
            End::Status(status) => (status, None),
            End::Refused(reason) => (REFUSED, Some(reason)),
            // Only a run call carrying a `SentDigest` is refused so, and the one run again carries none.
            End::SentRefused => {
                self.say(format!("{FAILED_RUN}: the builder refused the PC's build of a run that sent none"));
                (FAILED, None)
            }
            End::Interrupted => (i64::from(CONTROL_C_EXIT.cast_signed()), None),
            End::Killed => (i64::from(KILLED), None),
            End::Lost(_) => (i64::from(SSH_FAILED), None),
            // A transfer's failure is turned into the run's end once the command's exit is known; one that is not
            // fails the run as any client failure does.
            End::Failed(why) | End::TransferFailed(why) => {
                self.say(format!("{FAILED_RUN}: {why}"));
                (FAILED, None)
            }
        };
        // A line that could not be written fails the run, unless the watchdog killed it: the kill stays the run's end.
        let (status, reason, killed, transfer_failed) = match (&self.problem, killed) {
            (Some(_), false) => (FAILED, None, false, false),
            _ => (status, reason, killed, self.transfer_failed),
        };
        let status = warm_status(status, warm.as_ref());
        RemoteOutcome {
            status,
            reason,
            killed,
            lines: self.lines,
            detail: self.problem.or(detail),
            brought_back: self.brought_back,
            dropped_no_build: self.dropped_no_build,
            split: self.split,
            adjusted: self.adjusted,
            box_killed: self.box_killed,
            transfer_failed,
            warm,
            raised: self.raised,
            draining,
        }
    }

    /// How a warm-up that ended at `end` ended: as its run call was classed, or, its run call having ended it or never
    /// been made, as `end` is, after the start marker when a call said it; `None` for a call that is no warm-up.
    fn warm_outcome(&mut self, end: &End) -> Option<WarmEnd> {
        let Warmth::WarmUp { hash, .. } = self.call.warmth else {
            return None;
        };
        let started = self.marker_seen;
        Some(self.warm.take().unwrap_or_else(|| warm_end(hash, started, end, None)))
    }
}

#[cfg(all(test, feature = "test-seams"))]
impl<'c, 'a> Attempt<'c, 'a> {
    /// An attempt of `call` through `transport` with no pause before a retry, as the sync-back's tests drive one.
    pub(super) fn for_tests(call: &'c RemoteCall<'a>, transport: &'c dyn Transport) -> Attempt<'c, 'a> {
        Attempt {
            call,
            transport,
            git: git::GIT_BOUNDS,
            lock_wait: git::TEST_HANG_GUARD,
            pause: Duration::ZERO,
            transfer_pauses: [Duration::ZERO; crate::remote::retry::TRANSFER_RETRIES],
            transfer_failed: false,
            limits: Limits::for_tests(),
            one_call: None,
            interrupt_at: None,
            channel_fault: None,
            local_sdks: None,
            dotnet_limit: git::DOTNET_LIMIT,
            fetch_cap: watchdog::FETCH_CAP,
            apply_cap: watchdog::APPLY_CAP,
            outputs_budget: OUTPUTS_BUDGET,
            inputs_budget: inputs::INPUTS_BUDGET,
            lfs_budget: lfs::LFS_BUDGET,
            log_cap: watchdog::LOG_CAP,
            own_start_fails: false,
            hint_from_pull: false,
            tamper: None,
            brought_back: None,
            dropped_no_build: call.dropped_no_build,
            split: Split::default(),
            lines: Vec::new(),
            problem: None,
            hang_said: false,
            command_started: None,
            snapshot_taken: None,
            snapshot_age: Duration::ZERO,
            outputs_progress: OUTPUTS_PROGRESS,
            pulling: None,
            pull_periods_said: Cell::new(0),
            pulled_bytes: 0,
            pulled_last: None,
            adjusted: None,
            box_killed: None,
            held_run: None,
            ceiling: None,
            places: Vec::new(),
            local_major: None,
            warm: None,
            marker_seen: false,
            raised: None,
            backstop_seam: None,
            carried: Carried::default(),
            log_only: false,
        }
    }
}

#[cfg(all(test, feature = "test-seams"))]
impl Limits {
    /// Limits no test reaches, the bounded calls' bound aside: an hour for the stall window and the ceiling.
    fn for_tests() -> Limits {
        Limits {
            started: Instant::now(),
            stall: Duration::from_secs(3600),
            command: Duration::from_secs(3600),
            backstop: Duration::from_secs(3600),
            transfer: Duration::from_secs(3600),
            bound: Duration::from_secs(30),
            transfer_from: None,
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
        outputs: heard.outputs,
        back_commits: heard.back_commits,
        names_skipped: heard.names_skipped,
        cross_not_brought_back: heard.cross_not_brought_back,
        started: heard.started,
        refused: heard.refused,
        drain: heard.drain,
        box_killed: heard.box_killed,
        adjusted: heard.adjusted,
        written: None,
        received: None,
        channel: None,
        channel_sent: heard.channel_sent,
    })
}

/// A reader's problem and a failure to end the call, joined, or either alone.
fn joined(read: Option<String>, ending: Option<String>) -> Option<String> {
    match (read, ending) {
        (Some(read), Some(ending)) => Some(format!("{read}; {ending}")),
        (read, ending) => read.or(ending),
    }
}

/// What watching a call came to: how it ended, with what the looks at the bytes its job wrote found, or why it could not
/// be watched; and the run call's hang hint once said, or why it could not be said, which stands either way.
struct Watching {
    ended: Result<(CallEnd, Looks), End>,
    hinted: Option<Result<String, String>>,
}

/// The hang hint a routed run says once its command has run 3x its usual p90: that p90, the log the hint is written to
/// beside stderr, the phase the call watched is, and the builder's alias the phase names.
#[derive(Clone, Copy)]
struct RunHang<'w> {
    usual: Usual,
    log: &'w Path,
    phase: HangPhase,
    builder: &'w str,
}

/// The phase of a routed run a call's hang hint names: the run call, whose command counts from the box's start marker
/// it hears, or the outputs pull, whose run counts from `counted_from`, the start marker the run call heard then (the
/// pull's start under `--test-remote-hint-from-pull`), and whose own time counts from the pull's start,
/// `pull_started`, however many times its call was made, and whose words name what it brings (`bringing`).
#[derive(Clone, Copy)]
enum HangPhase {
    Running,
    Pulling {
        counted_from: Instant,
        pull_started: Instant,
        bringing: outputs::Bringing,
    },
}

impl HangPhase {
    /// How long, now, the run has run and the phase has, with the words naming the phase, for the call whose `progress`
    /// the watch reads: the run call's command and phase both since the box's start marker, `None` before it came; the
    /// outputs pull's run since `counted_from` and its phase since `pull_started`, both from one look at the clock, never
    /// since the call's own start.
    fn times(self, progress: &Progress) -> Option<(Duration, &'static str, Duration)> {
        match self {
            HangPhase::Running => {
                let running = progress.since_started()?;
                Some((running, "running on", running))
            }
            HangPhase::Pulling {
                counted_from,
                pull_started,
                bringing,
            } => {
                let now = Instant::now();
                Some((
                    now.saturating_duration_since(counted_from),
                    bringing.pulling_from(),
                    now.saturating_duration_since(pull_started),
                ))
            }
        }
    }
}

/// The outputs pull an outputs call is: the archive's size in bytes, when the box said it, and when the pull started,
/// which its end line and the hang hint's phase both count from; and what it brings, which its progress line names.
#[derive(Clone, Copy)]
pub(super) struct Pull {
    pub(super) total: Option<u64>,
    pub(super) started: Instant,
    pub(super) bringing: outputs::Bringing,
}

/// The progress an outputs call's watch says of its pull, whose size, start and what it brings it carries ([`Pull`]):
/// how often it says it, the log it is written to beside stderr, and the pull's one count of the periods said, which
/// every call of the pull reads and advances.
#[derive(Clone, Copy)]
struct PullWatch<'w> {
    total: Option<u64>,
    started: Instant,
    bringing: outputs::Bringing,
    every: Duration,
    log: &'w Path,
    periods_said: &'w Cell<u128>,
}

impl PullWatch<'_> {
    /// Whether a progress line is due `elapsed` after the pull's start: a whole period has passed that the pull's count
    /// does not hold yet, which it then holds, so a tick that comes late past more than one finds one line due, and a call
    /// made again finds none due of the periods an earlier call of the pull said.
    fn due(&self, elapsed: Duration) -> bool {
        let periods = elapsed.as_nanos() / self.every.as_nanos().max(1);
        if periods <= self.periods_said.get() {
            return false;
        }
        self.periods_said.set(periods);
        true
    }
}

/// What the looks at the bytes a capped call's job wrote found: the count at the last look that succeeded, and the
/// line saying the first look that failed; for a call the watch cut, what a raw copy of its output had written into its
/// sink by then; and why the first progress line of an outputs pull could not be said.
#[derive(Default)]
struct Looks {
    written: Option<u64>,
    unread: Option<String>,
    received: Option<Received>,
    unsaid: Option<String>,
}

/// What the looks at the bytes a call's feeder wrote to its standard input found: the count at the last look, and
/// when a look last found it grown, the call's start before any did. The put's stall counts its silence: its archive
/// goes out on that input, and nothing says whether ssh's socket sends count among its job's writes.
struct Sending {
    count: u64,
    moved: Instant,
}

impl Sending {
    /// No byte seen yet, the call having `begun` then.
    fn new(begun: Instant) -> Sending {
        Sending { count: 0, moved: begun }
    }

    /// How long, at `now`, the feeder has written nothing, `count` bytes being written so far.
    fn quiet(&mut self, count: u64, now: Instant) -> Duration {
        if count > self.count {
            self.count = count;
            self.moved = now;
        }
        now.saturating_duration_since(self.moved)
    }

    /// How long, at `now`, the put has been silent: while its feeder still writes, `count` bytes written so far, the
    /// shorter of the feeder's silence and its output's, `heard` ago; once the feeder has `finished`, never, since the
    /// box installs the archive without a word and the transfer ceiling alone watches it then.
    fn idle(&mut self, count: u64, finished: bool, heard: Duration, now: Instant) -> Duration {
        let quiet = self.quiet(count, now);
        if finished { Duration::ZERO } else { quiet.min(heard) }
    }
}

/// What a call's feeder has done, shared with its watch: the bytes it has written to the call's standard input, and
/// whether it has finished writing and closed it.
#[derive(Default)]
struct Feeding {
    sent: AtomicU64,
    finished: AtomicBool,
}

/// A started call the client watches: its process and job, when its streams last sent a byte, how many bytes its feeder
/// wrote to its standard input, the channel each of its two readers says its end on, its program as shown, the routed
/// run's hang hint while it is still to be said, and, for an outputs call, the pull's progress it says.
struct Watched<'w> {
    running: &'w Running,
    job: &'w Job,
    progress: &'w Progress,
    feeding: &'w Feeding,
    readers: &'w Receiver<()>,
    shown: &'w str,
    hang: Option<RunHang<'w>>,
    pull: Option<PullWatch<'w>>,
}

impl Watched<'_> {
    /// Watches the call as `watch` says until both its readers ended and it exited: how it ended, and the line saying
    /// the first look at the bytes its job wrote that failed, which cuts nothing; the attempt's interrupt when the
    /// console `interrupt` ended it. A fetch with a cap has those bytes looked at on each tick and once more after its
    /// exit, so a fetch that ends between two ticks is found past its cap too. `raise`, when given, is raised once the
    /// call sent its first byte, as a Ctrl+C would be; at the first look for a call a Ctrl+C does not end, which has no
    /// output to wait for. The run call's hang hint, once said, is kept however the watch ends.
    fn watch(self, watch: Watch, interrupt: Option<Interrupt>, raise: Option<Interrupt>) -> Watching {
        let mut hinted = None;
        let ended = self.until_ended(watch, interrupt, raise, &mut hinted);
        Watching { ended, hinted }
    }

    /// The watch's loop ([`Watched::watch`]), which keeps the run call's hang hint in `hinted` once it is said.
    fn until_ended(
        &self,
        watch: Watch,
        interrupt: Option<Interrupt>,
        mut raise: Option<Interrupt>,
        hinted: &mut Option<Result<String, String>>,
    ) -> Result<(CallEnd, Looks), End> {
        let begun = Instant::now();
        let tick = watch.tick();
        let mut open = 2;
        let mut looks = Looks::default();
        let mut sending = Sending::new(begun);
        loop {
            if hinted.is_none() {
                *hinted = self.hint_if_hung();
            }
            self.say_progress(&mut looks);
            if open == 0
                && let Some(code) = self.running.wait(Duration::ZERO).map_err(|error| cannot_wait(self.shown, &error))?
            {
                let over = watch.over(self.written(watch, &mut looks));
                let ended = over.map_or(CallEnd::Exited(code.cast_signed()), CallEnd::ExitedOver);
                return Ok((ended, looks));
            }
            if self.interrupted(watch, interrupt, &mut raise) {
                let ended = CallEnd::Interrupted(self.end(open));
                return Ok((ended, looks));
            }
            let written = self.written(watch, &mut looks);
            let now = Instant::now();
            let idle = self.idle(watch, &mut sending, now);
            if let Some(cut) = watch.cut(now, begun, idle, self.progress.raised(), written) {
                looks.received = Some(Received {
                    bytes: self.progress.received(),
                    after: now.saturating_duration_since(self.progress.begun),
                });
                let ended = CallEnd::Cut(cut, self.end(open));
                return Ok((ended, looks));
            }
            open -= self.readers_ended(open, tick);
        }
    }

    /// Raises `raise` once, taking it, when the call has sent its first byte, or at once for a call a Ctrl+C does not
    /// end, which has no output to wait for; then whether the console `interrupt` has ended the call watched as `watch`.
    fn interrupted(&self, watch: Watch, interrupt: Option<Interrupt>, raise: &mut Option<Interrupt>) -> bool {
        if (self.progress.heard_any() || !watch.interruptible())
            && let Some(interrupt) = raise.take()
        {
            interrupt.raise();
        }
        watch.interruptible() && interrupt.is_some_and(Interrupt::is_raised)
    }

    /// How long, at `now`, the call watched as `watch` has been silent: a put as its feeder's [`Sending::idle`] says,
    /// since it sends; any other call since its output's last byte.
    fn idle(&self, watch: Watch, sending: &mut Sending, now: Instant) -> Duration {
        match watch {
            Watch::Put(_) => sending.idle(
                self.feeding.sent.load(Ordering::Relaxed),
                self.feeding.finished.load(Ordering::Acquire),
                self.progress.idle(),
                now,
            ),
            _ => self.progress.idle(),
        }
    }

    /// Says the routed run's hang hint, on stderr and in the log, once its command has been running 3x its usual p90,
    /// counted from the box's start marker, naming the phase the call is and how long it has run: the run call's
    /// `running on {builder}` since that marker, the outputs pull's `pulling outputs from {builder}` since the pull's
    /// start, before its first call. The line said, or why it could not be said; `None` before then, and for every other call.
    fn hint_if_hung(&self) -> Option<Result<String, String>> {
        let hang = self.hang?;
        let (running, phase, during) = hang.phase.times(self.progress)?;
        let phase = format!("{phase} {}", hang.builder);
        let line = hang_hint(running.as_secs_f64(), hang.usual, &phase, during.as_secs_f64())?;
        Some(super::say(hang.log, &[&line], io::stderr().lock()).map(|()| line))
    }

    /// Says an outputs pull's progress line ([`outputs::progress_line`]), on stderr and in the log, when one is due
    /// ([`PullWatch::due`]), its elapsed time and rate counted from the pull's start, before its first call, and every
    /// figure on it from one look at that time and one at the bytes received. Why the first line could not be said is kept
    /// in `looks`. Nothing for every other call.
    fn say_progress(&self, looks: &mut Looks) {
        let Some(pull) = self.pull else {
            return;
        };
        let elapsed = pull.started.elapsed();
        if !pull.due(elapsed) {
            return;
        }
        let line = outputs::progress_line(pull.bringing, self.progress.received(), pull.total, elapsed);
        if let Err(why) = super::say(pull.log, &[&line], io::stderr().lock()) {
            looks.unsaid.get_or_insert(why);
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
/// byte; and what is fed on its standard input, `None` for none, as git's calls are; and the run's nonce its readers
/// match the box's start marker and refusal tags by, `None` for every call but the run call; the nonce of a preflight,
/// inputs or LFS call its readers match the box's refusal tags by, which makes it no run call; and the sink its
/// standard output is copied to raw, `None` for every call but the outputs call.
struct Starts {
    breakaway: bool,
    raise: bool,
    input: Option<Fed>,
    nonce: Option<Arc<str>>,
    tag_nonce: Option<Arc<str>>,
    sink: Option<RawSink>,
    /// The data channel the sink is filled from instead of the call's standard output, for an outputs call asking one.
    channel: Option<channel::Dial>,
}

/// What a call of `gate box` is fed on its standard input, `input`, and the `verb` of the call, which the line saying
/// it could not be sent names.
struct Fed {
    verb: BoxVerb,
    input: Input,
}

/// What a call of `gate box` is fed: a text alone, or a text and then a file to its end, the inputs' put's request,
/// manifest and archive.
pub(super) enum Input {
    Text(Arc<str>),
    Streamed(Arc<str>, Archived),
}

/// The inputs archive as one put attempt reads it: a handle on the held file, read by positional reads at an offset of
/// this reader's own, so the position the handle shares with its clones, which a feeder of an earlier attempt may still
/// move, never moves this one; up to the packed `length`.
pub(super) struct Archived {
    file: File,
    offset: u64,
    length: u64,
}

impl Archived {
    /// The first `length` bytes of `file`, read from its start.
    pub(super) fn new(file: File, length: u64) -> Archived {
        Archived { file, offset: 0, length }
    }
}

impl Read for Archived {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let left = usize::try_from(self.length.saturating_sub(self.offset)).unwrap_or(usize::MAX);
        let most = left.min(buffer.len());
        let Some(into) = buffer.get_mut(..most).filter(|into| !into.is_empty()) else {
            return Ok(0);
        };
        let read = self.file.seek_read(into, self.offset)?;
        self.offset += read as u64;
        Ok(read)
    }
}

/// The bytes a feeder writes to a call's standard input at a time.
const FEED_BUFFER: usize = 64 * 1024;

/// The thread feeding a call of `gate box` the `verb` its input, which gives back how the write went.
struct Feeder {
    verb: BoxVerb,
    thread: thread::JoinHandle<io::Result<()>>,
}

/// What one call of `gate box` alone gives it: what is fed on its standard input, the request ([`payload::input`])
/// every call carries; the run call the run's nonce and a compressed connection, a preflight, inputs or LFS call the
/// nonce its word carries, which its refusal tags name, the outputs call the sink its standard output is copied to
/// raw; none of them, and a plain connection, for any other call.
struct CallOnly {
    input: Input,
    nonce: Option<Arc<str>>,
    tag_nonce: Option<Arc<str>>,
    sink: Option<RawSink>,
    wire: Wire,
    /// The data channel the outputs call's archive comes over instead of its standard output, when it asks for one.
    channel: Option<channel::Dial>,
}

impl CallOnly {
    /// A call fed `input` alone, over a plain connection.
    fn plain(input: Arc<str>) -> CallOnly {
        CallOnly {
            input: Input::Text(input),
            nonce: None,
            tag_nonce: None,
            sink: None,
            wire: Wire::Plain,
            channel: None,
        }
    }
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
        self.shared.progress.took(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A writer that adds each byte it writes through to `sent`, which the watch reads as the put's progress.
struct Counted<'s, W: Write> {
    inner: W,
    sent: &'s AtomicU64,
}

impl<W: Write> Write for Counted<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.sent.fetch_add(written as u64, Ordering::Relaxed);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Writes what `fed` gives to a call's `stdin` on a thread of its own, through a [`FEED_BUFFER`] buffer, each byte that
/// reaches it counted in `feeding`, then closes it and marks `feeding` finished; the thread gives back how the write
/// went, which [`Attempt::fed_out`] reads once the call has ended.
fn fed(stdin: File, fed: Fed, feeding: Arc<Feeding>) -> Feeder {
    let Fed { verb, input } = fed;
    let thread = thread::spawn(move || {
        let mut stdin = feeder_writer(stdin, &feeding.sent);
        let written = fed_with(&mut stdin, input);
        drop(stdin);
        feeding.finished.store(true, Ordering::Release);
        written
    });
    Feeder { verb, thread }
}

/// The writer a feeder writes a call's input through to `inner`: a [`FEED_BUFFER`] buffer over a count, in `sent`, of
/// each byte that reaches `inner`.
fn feeder_writer<W: Write>(inner: W, sent: &AtomicU64) -> BufWriter<Counted<'_, W>> {
    BufWriter::with_capacity(FEED_BUFFER, Counted { inner, sent })
}

/// Writes `input` to `stdin` and flushes it: its text, and then the file it streams, to its packed length or until the box closes it.
fn fed_with(stdin: &mut impl Write, input: Input) -> io::Result<()> {
    match input {
        Input::Text(text) => {
            stdin.write_all(text.as_bytes())?;
            stdin.flush()
        }
        Input::Streamed(head, mut file) => {
            // The head reaches the pipe before the archive does, so a pipe closed during it is the failure it is.
            stdin.write_all(head.as_bytes())?;
            stdin.flush()?;
            // A box that refuses before it reads the file closes its input early: the write ends there, and the
            // call's own status and output say how it went.
            match io::copy(&mut file, stdin).and_then(|_| stdin.flush()) {
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                copied => copied,
            }
        }
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
/// standard output, its first and last standard error lines, its last standard error lines that are the box's outputs
/// line and its back commits line, whether a standard error line said the box skipped outputs for names
/// ([`NAMES_SKIPPED`]), whether the box said the run's start marker and the refusal its first tag named, its last
/// lines, and the first problem reading them.
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact the call's streams said, read by name; no two form a state machine"
)]
struct Heard {
    kept: Kept,
    output_seen: bool,
    first_error: Option<String>,
    last_error: Option<String>,
    outputs: Option<outputs::OutputsLines>,
    back_commits: Option<String>,
    names_skipped: bool,
    cross_not_brought_back: bool,
    started: bool,
    refused: Option<Tagged>,
    drain: Option<Drain>,
    box_killed: Option<BoxKill>,
    adjusted: Option<f64>,
    /// The port the box's data channel line for the call named, the first when it said more than one.
    channel_port: Option<u16>,
    /// Its last standard error line saying what the box sent over a data channel.
    channel_sent: Option<String>,
    problem: Option<String>,
}

impl Heard {
    /// Keeps one `line` of standard error when `error`, else of standard output, cut to the cap: a heartbeat, and the
    /// box's start marker, refusal tag, kill tag or adjusted line its reader read the line as, `protocol`, only mark
    /// the first error line, the latter four also marking the run started, refused or killed by the box, or the
    /// load-adjusted seconds it counted ([`Heard::marked`]).
    fn record(&mut self, line: &str, error: bool, protocol: Option<Protocol>) {
        let line = watchdog::truncated(line);
        if error {
            self.first_error.get_or_insert_with(|| line.to_owned());
        } else {
            self.output_seen = true;
        }
        if let Some(protocol) = protocol {
            self.marked(protocol);
            return;
        }
        if error && is_heartbeat(line) {
            return;
        }
        if error && line.contains(CHANNEL_SENT) {
            self.channel_sent = Some(line.to_owned());
        }
        self.kept_line(line, error);
    }

    /// Marks what the box's line read as `protocol` says: the run started, its first refusal, its last kill tag, its last
    /// load-adjusted seconds.
    fn marked(&mut self, protocol: Protocol) {
        match protocol {
            Protocol::Started => self.started = true,
            Protocol::Refused(reason) => {
                self.refused.get_or_insert(Tagged::Fallback(reason));
            }
            Protocol::SentRefused => {
                self.refused.get_or_insert(Tagged::Sent);
            }
            Protocol::Drain => {
                self.refused.get_or_insert(Tagged::Drain);
                self.drain.get_or_insert_with(Drain::default);
            }
            Protocol::DrainLine(line) => {
                if let Some(drain) = &mut self.drain {
                    *drain = drain.with(line);
                }
            }
            Protocol::Killed(kill) => self.box_killed = Some(kill),
            Protocol::Adjusted(seconds) => self.adjusted = Some(seconds),
            Protocol::Channel(port) => {
                self.channel_port.get_or_insert(port);
            }
            Protocol::ChannelRefused => {
                self.refused.get_or_insert(Tagged::Channel);
            }
        }
    }

    /// Keeps `line`, already cut, of standard error when `error`: the last error line, the outputs line and the size line
    /// heard just before it, the back commits line and the skipped names it may be, and the line among those kept for the log.
    fn kept_line(&mut self, line: &str, error: bool) {
        let previous = if error { self.last_error.replace(line.to_owned()) } else { None };
        outputs::keep_outputs_lines(&mut self.outputs, previous.as_deref(), line, error);
        syncback::keep_back_commits_line(&mut self.back_commits, line, error);
        self.names_skipped |= error && line.starts_with(NAMES_SKIPPED);
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
    /// The bytes a raw copy of the call's standard output has written into its sink.
    received: AtomicU64,
    /// Milliseconds from `begun` to the last byte a raw copy wrote into its sink, [`NOT_STARTED`] until one did.
    last_received: AtomicU64,
    /// Milliseconds from `begun` to the box's start marker of the run, [`NOT_STARTED`] until it came.
    started: AtomicU64,
}

/// What [`Progress`] holds for a start marker that has not come.
const NOT_STARTED: u64 = u64::MAX;

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
            received: AtomicU64::new(0),
            last_received: AtomicU64::new(NOT_STARTED),
            started: AtomicU64::new(NOT_STARTED),
        }
    }

    /// Marks a byte that came now. The mark is stored with `Release`, so whoever sees it through [`Progress::heard_any`]'s
    /// `Acquire` sees every count [`Progress::took`] made before it.
    fn mark(&self) {
        let millis = u64::try_from(self.begun.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_byte.store(millis, Ordering::Relaxed);
        self.any.store(true, Ordering::Release);
    }

    /// Marks the box's start marker of the run, which came now.
    fn mark_started(&self) {
        let millis = u64::try_from(self.begun.elapsed().as_millis()).unwrap_or(NOT_STARTED - 1);
        self.started.store(millis, Ordering::Relaxed);
    }

    /// When the box's start marker of the run came, `None` before it did.
    fn started_at(&self) -> Option<Instant> {
        match self.started.load(Ordering::Relaxed) {
            NOT_STARTED => None,
            millis => Some(self.begun + Duration::from_millis(millis)),
        }
    }

    /// How long since the box's start marker of the run came, `None` before it did.
    fn since_started(&self) -> Option<Duration> {
        match self.started.load(Ordering::Relaxed) {
            NOT_STARTED => None,
            millis => Some(self.begun.elapsed().saturating_sub(Duration::from_millis(millis))),
        }
    }

    /// How long nothing has come, or since the call started when nothing came yet.
    fn idle(&self) -> Duration {
        let last = Duration::from_millis(self.last_byte.load(Ordering::Relaxed));
        self.begun.elapsed().saturating_sub(last)
    }

    fn heard_any(&self) -> bool {
        self.any.load(Ordering::Acquire)
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

    /// Counts `bytes` more that a raw copy of the call's standard output wrote into its sink, now. The count is added with
    /// `Release`, so a mark made after it and seen through `Acquire` publishes it too ([`Progress::mark`]).
    fn took(&self, bytes: u64) {
        self.received.fetch_add(bytes, Ordering::Release);
        let millis = u64::try_from(self.begun.elapsed().as_millis()).unwrap_or(NOT_STARTED - 1);
        self.last_received.store(millis, Ordering::Relaxed);
    }

    /// When a raw copy of the call's standard output last wrote into its sink, `None` before it first did.
    fn last_received(&self) -> Option<Instant> {
        match self.last_received.load(Ordering::Relaxed) {
            NOT_STARTED => None,
            millis => Some(self.begun + Duration::from_millis(millis)),
        }
    }

    /// The bytes a raw copy of the call's standard output has written into its sink so far.
    fn received(&self) -> u64 {
        self.received.load(Ordering::Acquire)
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
/// under, their progress, how much of the call's output they wrote to the log, and whether the call's end has come.
struct Shared {
    heard: Mutex<Heard>,
    progress: Progress,
    /// The bytes of the call's output the readers have charged to the log, both streams together, and whether one of
    /// them has said that they passed the log cap: the line is said once, by whichever reader passed it first. A charge
    /// and its write are not one step, so a piece the other reader was cleared to write just before can land after
    /// the cut line: at most one piece per stream, and the log still holds no more than the cap of the call's output.
    logged: AtomicU64,
    log_cut: AtomicBool,
    /// Set once the watch has returned: the call ended, so a reader still going writes nothing more into what the
    /// fetch is about to remove.
    stopped: AtomicBool,
}

/// What a reader does with a piece of the call's output it charged to the log: write it, say once that the call's
/// output passed the log cap, or drop it, as every piece after that one.
#[derive(Debug, PartialEq, Eq)]
enum Logged {
    Write,
    Cut,
    Drop,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            heard: Mutex::new(Heard::default()),
            progress: Progress::new(),
            logged: AtomicU64::new(0),
            log_cut: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }
    }

    /// Charges `bytes` more of the call's output to the log, whose cap is `cap`: [`Logged::Write`] while all the call's
    /// output charged so far fits, else [`Logged::Cut`] for the one charge, of either reader, that first passes it, and
    /// [`Logged::Drop`] for every other.
    fn charged(&self, bytes: usize, cap: u64) -> Logged {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        // The update always gives a value, so it never fails; either way it hands back the sum before it.
        let before = self
            .logged
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |sum| Some(sum.saturating_add(bytes)))
            .unwrap_or_else(|sum| sum);
        if before.saturating_add(bytes) <= cap {
            Logged::Write
        } else if self.log_cut.swap(true, Ordering::AcqRel) {
            Logged::Drop
        } else {
            Logged::Cut
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

/// How a reader takes a call's lines: where they go, whether a line past the cap stops it, the run's start marker and
/// refusal tags its lines are matched against, `None` for every call but the run call, and the most bytes of the
/// call's output its log gets.
#[derive(Clone)]
struct Reading {
    echo: Echo,
    capped: bool,
    lines: Option<Arc<ProtocolLines>>,
    log_cap: u64,
}

impl Reading {
    /// The box's start marker, refusal tag, kill tag or adjusted line for the run that `line` is, of standard error when
    /// `error`; `None` for a line of standard output, any line of a call that is no run call, and any other line.
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
    /// Whether the last piece this reader wrote to the log left its line open, so a cut line it writes next starts on a
    /// line of its own.
    log_open: bool,
}

impl Reader {
    fn new(error: bool, reading: Reading, log: Option<File>) -> Reader {
        Reader {
            error,
            reading,
            log,
            pending: Vec::new(),
            head: None,
            log_open: false,
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
        let first = self.head.is_none();
        if first {
            let start = String::from_utf8_lossy(piece.get(..LINE_CAP).unwrap_or(&piece));
            self.head = Some(watchdog::truncated(stamp::unstamped(&start).text).to_owned());
        }
        self.mark_line(shared);
        self.streamed(piece, ended, first, shared);
        true
    }

    /// Marks the call's progress for a line, or a piece of one, of a run call, whose bytes are not marked as they come
    /// ([`relay`]): the box's lines then mark it, its process lines excepted, so a command that only starts processes
    /// still stalls.
    fn mark_line(&self, shared: &Shared) {
        if self.marks_lines() {
            shared.progress.mark();
        }
    }

    /// Whether this reader marks the call's progress per line rather than per chunk read: a run call's does. A setup
    /// call's git progress ends its updates with a carriage return and no line feed, so its chunks mark it.
    fn marks_lines(&self) -> bool {
        self.reading.lines.as_deref().is_some_and(|lines| lines.run_call)
    }

    /// The box's place or barrier line `line` is, of standard error, once the call's drain tag was heard and before
    /// any start marker: the two lines the box says after the tag ([`drain::line_of`]); `None` for any other line.
    fn drain_line(&self, line: &str, shared: &Shared) -> Option<Protocol> {
        if !self.error || self.reading.lines.is_none() {
            return None;
        }
        let heard = held(&shared.heard);
        if heard.drain.is_none() || heard.started {
            return None;
        }
        drain::line_of(line).map(Protocol::DrainLine)
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
            self.streamed(bytes, true, false, shared);
        } else if !bytes.is_empty() {
            self.line_ended(&bytes, shared);
        }
    }

    /// A whole line, `bytes`, came: it is passed on and kept, unless it is past the cap of a capped reading, which
    /// stops, `false`. The box's heartbeat, start marker and refusal tag, matched on the line as it came, are kept but
    /// never passed on. Every other line is read once without its stamp: the log gets it as it came, stamped `gat` now
    /// when it came with none, and the console and what is kept get its text. A process line is logged alone: it is
    /// neither echoed nor kept, and marks no progress.
    fn line_ended(&mut self, bytes: &[u8], shared: &Shared) -> bool {
        if self.reading.capped && watchdog::too_long(bytes) {
            shared.progress.cut_too_long();
            return false;
        }
        let line = line_of(bytes, self.reading.echo);
        // Matched before any unstamping, so a command's own line, stamped by the box, never reads as one.
        let protocol = self.reading.protocol(&line, self.error).or_else(|| self.drain_line(&line, shared));
        if protocol == Some(Protocol::Started) {
            shared.progress.mark_started();
        }
        if protocol.is_some() || (self.error && is_heartbeat(&line)) {
            self.mark_line(shared);
            held(&shared.heard).record(&line, self.error, protocol);
            return true;
        }
        let read = stamp::unstamped(&line);
        if read.tag == Some(Tag::Gat) && stamp::is_process_line(read.text) {
            self.passed(&line, None, true, shared);
            return true;
        }
        self.mark_line(shared);
        self.passed(&logged_line(&line, read.tag), Some(read.text), true, shared);
        let mut heard = held(&shared.heard);
        heard.record(read.text, self.error, None);
        // Only the box's own stamp tells its line from a copy of it the command printed on the same stream.
        heard.cross_not_brought_back |= self.error && read.tag == Some(Tag::Gat) && is_cross_not_brought_back(read.text);
        true
    }

    /// Passes on `piece`, the next of a line past the cap, which ends it when `ended`; else the start of a character
    /// it cuts waits for the next piece. The `first` piece alone may carry the line's stamp: the log gets it as it
    /// came, stamped `gat` now when it came with none, and the console its text. The line's head is kept once it ends.
    fn streamed(&mut self, mut piece: Vec<u8>, ended: bool, first: bool, shared: &Shared) {
        let text = if ended {
            line_of(&piece, self.reading.echo)
        } else {
            let mut whole = watchdog::complete(&piece);
            // A carriage return may start the line end the next piece finishes: it waits for it.
            if whole > 0 && piece.get(whole - 1) == Some(&b'\r') {
                whole -= 1;
            }
            self.pending = piece.split_off(whole);
            String::from_utf8_lossy(&piece).into_owned()
        };
        if first {
            let read = stamp::unstamped(&text);
            self.passed(&logged_line(&text, read.tag), Some(read.text), ended, shared);
        } else {
            self.passed(&text, Some(&text), ended, shared);
        }
        if ended && let Some(head) = self.head.take() {
            held(&shared.heard).record(&head, self.error, None);
        }
    }

    /// Writes `logged`, with a line end when `end`, to the log unless the reading keeps it out or the call's output
    /// has passed the log cap, which the log is told once, its bytes as written counting toward the cap; and `shown`,
    /// when there is one, on its own stream when it echoes to the console, past the cap too; the first failure to is
    /// kept.
    fn passed(&mut self, logged: &str, shown: Option<&str>, end: bool, shared: &Shared) {
        let line_end = if end { "\n" } else { "" };
        if let Some(log) = &mut self.log {
            let said = format!("{logged}{line_end}");
            let written = match shared.charged(said.len(), self.reading.log_cap) {
                Logged::Write => {
                    self.log_open = !end;
                    log.write_all(said.as_bytes())
                }
                Logged::Cut => {
                    let line = log_cut_line(self.reading.log_cap);
                    let line = if self.log_open { format!("\n{line}") } else { line };
                    log.write_all(line.as_bytes())
                }
                Logged::Drop => Ok(()),
            };
            if let Err(error) = written {
                held(&shared.heard).failed(format!("cannot write to the log: {error}"));
            }
        }
        if let Some(shown) = shown
            && self.reading.echo == Echo::Console
        {
            let said = format!("{shown}{line_end}");
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

/// `line` as the log gets it: as it came when it came stamped, its read `tag` given, else stamped now and tagged
/// `gat`, as a line of an older box or of a setup call comes.
fn logged_line(line: &str, tag: Option<Tag>) -> Cow<'_, str> {
    match tag {
        Some(_) => Cow::Borrowed(line),
        None => Cow::Owned(stamp::stamped(&stamp::now(), Tag::Gat, line)),
    }
}

/// The line the log of a call whose output passed the log cap `cap` gets, once, stamped and tagged `gat`, with its
/// line end.
fn log_cut_line(cap: u64) -> String {
    let text = format!("gate: the builder's output passed {}; the rest is not logged", watchdog::whole_size(cap));
    format!("{}\n", stamp::stamped(&stamp::now(), Tag::Gat, &text))
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
) -> thread::JoinHandle<Option<channel::Dialled>> {
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        relay_raw(stream, &mut sink, &shown, &shared);
        // Closed before the end is said: a retry removes the file and makes it anew, which an open handle would refuse.
        drop(sink);
        // A killed call's watcher stops listening once its drain is over: a reader that ends later has no one to tell.
        done.send(()).ok();
        None
    })
}

/// Fills `sink`, shown as `shown`, from the data channel `dial` on a thread of its own, once the call's standard error
/// said the box's port line ([`Heard::channel_port`]), and hands the thread back; it gives how the channel ended. Each
/// piece read, once written into the sink, marks the call's progress as a raw copy's chunk does ([`relay_raw`]), and so
/// do the connect and the proof exchange, so the stall watch covers a silent channel. The thread ends within a
/// [`channel::pulled`] slice of the call's end ([`Shared::stopped`]), the sink dropped and then the socket shut down and
/// closed before then; it is no reader of the call's streams, so it says nothing on their channel.
fn dialled(
    dial: channel::Dial,
    sink: impl Write + Send + 'static,
    shown: String,
    shared: &Arc<Shared>,
) -> thread::JoinHandle<Option<channel::Dialled>> {
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let port = loop {
            if let Some(port) = held(&shared.heard).channel_port {
                break port;
            }
            if shared.stopped() {
                return Some(channel::Dialled::NoPort);
            }
            thread::sleep(PAUSE_STEP);
        };
        // With no size from the box's run call, the PC cannot tell where the archive ends: it does not dial, and the
        // box's own `channel` refusal at its accept window sends the pull over ssh.
        let Some(size) = dial.size else {
            return Some(channel::Dialled::NoSize);
        };
        let mut fill = Filled {
            sink: Some(sink),
            shown,
            shared: &shared,
        };
        let ended = || shared.stopped();
        Some(channel::pulled(&dial.asked, (port, size, dial.fault), &mut fill, &ended))
    })
}

/// The sink a data channel's thread fills, shown as `shown`, with the call's `shared` state whose progress each step
/// marks; the sink is dropped, closing the archive's file, once the pull ends, before the connection is shut down.
struct Filled<'s, W> {
    sink: Option<W>,
    shown: String,
    shared: &'s Shared,
}

impl<W: Write> channel::Fill for Filled<'_, W> {
    fn chunk(&mut self, bytes: &[u8]) -> Result<(), String> {
        let Some(sink) = self.sink.as_mut() else {
            return Err(format!("{} was closed before the pull ended", self.shown));
        };
        sink.write_all(bytes).map_err(|error| format!("cannot write {}: {error}", self.shown))?;
        self.shared.progress.mark();
        held(&self.shared.heard).output_seen = true;
        Ok(())
    }

    fn mark(&mut self) {
        self.shared.progress.mark();
    }

    fn close(&mut self) {
        drop(self.sink.take());
    }
}

/// Copies `stream` into `sink`, shown as `shown`, byte for byte until it ends, in chunks of at most [`RAW_CHUNK`]: no
/// line is read, so no length cuts it. Each chunk, once written into the sink, marks the call's progress, and marks that
/// the call wrote on standard output, so a call that sent a byte is past the banner, and a chunk whose write failed marks
/// nothing. A capped sink has counted the chunk by then: the count is added with `Release` and the mark stored with
/// `Release` after it, so the interrupt raised once [`Progress::heard_any`] reads the mark with `Acquire` finds the chunk
/// counted by [`Progress::received`], which reads with `Acquire` too. A read or a write that fails is the call's problem,
/// and ends the copy. No write is made under the lock the watcher could take.
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
        if let Err(error) = sink.write_all(buffer.get(..read).unwrap_or_default()) {
            held(&shared.heard).failed(format!("cannot write {shown}: {error}"));
            return;
        }
        shared.progress.mark();
        held(&shared.heard).output_seen = true;
    }
}

/// Reads `stream` line by line through `reader` until it ends, or a capped reading meets a line past the cap, each
/// chunk that comes marking the call's progress, or for a run call each line but a process line ([`Reader::take`]);
/// each line without its line end, read as UTF-8 with U+FFFD for what is not. Neither a write nor a wait for one is
/// made under the lock the watcher could take.
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
        if !reader.marks_lines() {
            shared.progress.mark();
        }
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

/// The seconds `text` spells when it is exactly what the box's adjusted line carries: one or more digits, a point and
/// three digits, making a finite number of seconds; `None` for anything else, a whole part too long for a double
/// included.
fn adjusted_seconds(text: &str) -> Option<f64> {
    let (whole, decimals) = text.split_once('.')?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !(digits(whole) && decimals.len() == 3 && digits(decimals)) {
        return None;
    }
    text.parse::<f64>().ok().filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
}

/// Whether `line` is exactly the box's heartbeat: its prefix and one or more digits.
fn is_heartbeat(line: &str) -> bool {
    line.strip_prefix(HEARTBEAT)
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// How a run call that ended as `called`, its own lines at `output` among the attempt's, ends the run when neither a
/// kill nor an interrupt ended it: a lost connection when it exited 255; a refusal when its 75 comes with the box's
/// refusal tag and no start marker, its `sent` tag among them ([`End::SentRefused`]); else the command's own status.
fn status_end(called: &Called, output: Range<usize>) -> End {
    if called.status == SSH_FAILED {
        return End::Lost(output);
    }
    let status = i64::from(called.status);
    match (called.refusal(), called.draining()) {
        (Some(reason), _) if status == REFUSED => End::Refused(reason),
        (None, Some(drain)) if status == REFUSED => End::Draining(drain),
        (None, None) if status == REFUSED && called.sent_refusal() => End::SentRefused,
        _ => End::Status(status),
    }
}

/// What the builder's preflight said of the work folder's warm-up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Warm {
    /// Warm with the warm-up file of this hash.
    Hash(String),
    /// Not warm: `none`.
    NoHash,
    /// No warm line where one is read, or one that is neither `none` nor a hash: counted as not warm.
    Unseen,
}

/// The work folder's warm-up, from the line right after the preflight's last `gate: remote version` line alone, when it
/// is exactly `gate: remote warm none` or `gate: remote warm ` and 64 lower-case hex digits.
pub(crate) fn warm_of(lines: &[String]) -> Warm {
    let Some(version) = lines.iter().rposition(|line| line.starts_with(VERSION)) else {
        return Warm::Unseen;
    };
    match lines.get(version + 1).and_then(|line| line.strip_prefix(WARM)) {
        Some("none") => Warm::NoHash,
        Some(hash) if is_lower_hex(hash, SHA256_DIGITS) => Warm::Hash(hash.to_owned()),
        _ => Warm::Unseen,
    }
}

/// Whether the preflight said the work folder's tree is new: [`NEW_WORK_TREE`] exactly, right after the warm line that
/// follows its last `gate: remote version` line. A box that never says it reads as not new.
fn new_work_tree(lines: &[String]) -> bool {
    let Some(version) = lines.iter().rposition(|line| line.starts_with(VERSION)) else {
        return false;
    };
    let warm_said = lines.get(version + 1).is_some_and(|line| line.starts_with(WARM));
    warm_said && lines.get(version + 2).is_some_and(|line| line == NEW_WORK_TREE)
}

/// The note a run whose work folder on the builder is new says before its run call, its ceiling raised from `from` to
/// `to` whole seconds: naming `gate warm` when the repo's warm-up is set up (`warm_up`), else the warm-up file it needs.
fn cold_line(from: u64, to: u64, warm_up: bool) -> String {
    let prewarm = if warm_up {
        "prewarm the next new worktree with gate warm"
    } else {
        "a .gate/warmup.toml lets gate warm prewarm the next new worktree"
    };
    format!(
        "gate: remote cold: this worktree's work folder on the builder is new or was cleared; \
         the ceiling is raised from {from} s to {to} s; {prewarm}"
    )
}

/// Why a warm-up the box ran to status 0 is still not warm.
const UNCONFIRMED: &str = "the box did not confirm the warm hash";

/// How the warm-up sent `hash` ended at `end`, its run call having said the start marker when `started`, its last
/// standard error line that is neither a heartbeat nor a protocol line being `last_error`: warm only on status 0 after
/// the marker with that line exactly `gate: remote warm {hash}`; deleted on an interrupt; else failed, refused for a
/// refusal or any end before the marker, as no command ran, and the commands' failure for any end after it.
fn warm_end(hash: &str, started: bool, end: &End, last_error: Option<&str>) -> WarmEnd {
    let failure = match end {
        End::Interrupted => return WarmEnd::Deleted,
        End::Status(0) if started && last_error == Some(format!("{WARM}{hash}").as_str()) => return WarmEnd::Warm,
        End::Refused(_) | End::NotRouted(_) => WarmFailure::Refused,
        _ if started => WarmFailure::Commands,
        _ => WarmFailure::Refused,
    };
    WarmEnd::Failed {
        failure,
        reason: warm_reason(end, started),
    }
}

/// Why a warm-up that ended at `end`, after the start marker when `started`, failed: built from the end alone, never
/// from a line the builder or the commands said.
fn warm_reason(end: &End, started: bool) -> String {
    match end {
        End::Status(0) if started => UNCONFIRMED.to_owned(),
        End::Status(status) if started => format!("command stopped with status {status}"),
        End::Status(status) => format!("the builder ended the warm-up before its commands with status {status}"),
        End::Refused(reason) => format!("the builder refused: {}", reason.as_str()),
        End::SentRefused => "the builder refused: sent".to_owned(),
        End::Draining(_) => "the builder refused: drain".to_owned(),
        End::Killed => "the warm-up was killed: it ran out of its budget or went silent".to_owned(),
        End::Lost(_) => "the connection to the builder was lost".to_owned(),
        End::NotRouted(_) => "the client could not send the warm-up".to_owned(),
        End::Failed(_) | End::TransferFailed(_) => "the client failed".to_owned(),
        End::Interrupted => "the warm-up was interrupted".to_owned(),
    }
}

/// The builder's version, from the last `gate: remote version <v>` line of its preflight; `None` when it said none.
pub(crate) fn their_version(lines: &[String]) -> Option<&str> {
    lines.iter().rev().find_map(|line| line.strip_prefix(VERSION)).map(str::trim)
}

/// The SDKs the builder lists, from the last `gate: remote sdks <v1,v2|none>` line of its preflight, without `none`;
/// `None` when it said no such line, or one with no list, which skips the parity check.
pub(crate) fn sdks_of(lines: &[String]) -> Option<Vec<String>> {
    lines
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix(SDKS))
        .map(str::trim)
        .filter(|listing| !listing.is_empty())
        .map(parity::listed)
}

/// How long this PC's `dotnet --list-sdks` may take: `--test-dotnet-limit-ms` when given, else [`git::DOTNET_LIMIT`].
fn dotnet_limit(seams: &TestSeams) -> Duration {
    seams.dotnet_limit_ms.map_or(git::DOTNET_LIMIT, Duration::from_millis)
}

/// What `dotnet --list-sdks` lists on this PC, as [`git::sdk_listing`] reads it within `limit` and `drain`, with the folders inside each of
/// `worktrees` dropped from the `PATH` of `variables`, each compared by its canonical path so a junction does not get
/// around it, and a dotnet found that still lies inside any of them refused. A `PATH` that cannot be read that way
/// cannot be read at all.
pub(crate) fn local_sdk_listing(
    variables: Variables,
    worktrees: &[&Path],
    limit: Duration,
    drain: Duration,
    interrupt: Option<Interrupt>,
) -> Result<String, GitError> {
    let variables = worktrees
        .iter()
        .try_fold(variables, |variables, worktree| outside_worktree(variables, worktree))
        .map_err(GitError::Failed)?;
    git::sdk_listing(variables, limit, drain, interrupt, worktrees)
}

/// `variables` with the folders inside `worktree` dropped from their `PATH`, as [`path_outside`] rebuilds it, so a
/// program of the repo's own is never found along it; else why the `PATH` cannot be rebuilt. This PC's SDK list and
/// `gate check`'s tool probes look for their programs along it.
pub(crate) fn outside_worktree(variables: Variables, worktree: &Path) -> Result<Variables, String> {
    let path = path_outside(&variables, worktree)?;
    Ok(variables.with("PATH", &path))
}

/// The `PATH` of `variables` without the folders inside `worktree`, each compared by its canonical path: an `npm run`
/// or an activated venv puts folders of the repo first on `PATH`, where a `dotnet.cmd` of the repo would be found.
/// Else why it cannot be rebuilt: an unmatched quote reads every folder after it as one, which no search finds a
/// dotnet in, so a list read along it would look empty rather than unreadable.
fn path_outside(variables: &Variables, worktree: &Path) -> Result<String, String> {
    let worktree = resolved(worktree);
    let path = variables.get("PATH").unwrap_or_default();
    if path.to_string_lossy().matches('"').count() % 2 == 1 {
        return Err("PATH holds an unmatched quote, so the folders after it read as one".to_owned());
    }
    let kept: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|folder| !is_within(&resolved(folder), &worktree))
        .collect();
    std::env::join_paths(kept)
        .map(|joined| joined.to_string_lossy().into_owned())
        .map_err(|error| format!("PATH cannot be rebuilt without the worktree's folders: {error}"))
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
    other_build(theirs, ours).map(|shown| format!("gate: not routed: {host} runs gate {shown}, this PC {ours}; run gate push"))
}

/// The line refusing a builder `host` whose box refused its install in the call `called`: one that exited 1 having
/// said [`INSTALL_UNTRUSTED`] before any start marker ([`install_refusal`]); `None` for any other call, and for one
/// past the start marker, which a command's own output may have said.
fn install_refused(host: &str, called: &Called) -> Option<String> {
    if called.started {
        return None;
    }
    install_refusal(host, called.status, called.lines.iter().chain(&called.last_error).map(String::as_str))
}

/// How the preflight call `called` to the builder `host` under `root` ends the attempt, a kill ruled out first by its
/// caller: the drain's refusal, read before anything else the box said, its version line included; then a refusal
/// tag with the refusal's status, `full` a builder whose own slots are all held; then a refusal of its install or work
/// root, a failed connection, a builder with no `gate box`, any other status, and a builder of
/// another version, each with the line to say. `Ok` for a preflight that passed.
fn preflight_ended(host: &str, root: &str, called: &Called) -> Result<(), (Option<String>, End)> {
    if let Some(drain) = called.draining() {
        return Err((None, End::Draining(drain)));
    }
    // A refusal tag, read as the run call reads one: the box said its own line after it.
    if let Some(reason) = called.refusal()
        && i64::from(called.status) == REFUSED
    {
        return Err((None, End::Refused(reason)));
    }
    if let Some((line, reason)) = preflight_refusal(host, called) {
        return Err((Some(line), End::Refused(reason)));
    }
    match called.status {
        0 => {}
        SSH_FAILED => {
            let last = git::failure_text(&[called.last_error.as_deref().unwrap_or_default()]);
            let line = format!("gate: remote {host} unreachable: {last}");
            return Err((Some(line), End::Refused(FallbackReason::Unreachable)));
        }
        status if no_box(status, &called.lines, called.last_error.as_deref()) => {
            let said = shell_said(status, called.last_error.as_deref());
            let line = format!("gate: not routed: {host} has no gate box that takes this run (preflight status {status}){said}");
            return Err((Some(line), End::Refused(FallbackReason::Version)));
        }
        status => {
            let line = format!("gate: preparing {root} on {host} failed (status {status})");
            return Err((Some(line), End::Status(i64::from(status))));
        }
    }
    // The builder's whole text must be this build's, so another build of the same package version is refused too.
    match version_refusal(host, their_version(&called.lines), &build_version()) {
        Some(line) => Err((Some(line), End::Refused(FallbackReason::Version))),
        None => Ok(()),
    }
}

/// The line refusing a builder `host` whose box refused the preflight call `called`, and the reason the run falls back
/// for: its install, read first ([`install_refused`]), then its work root ([`root_refused`]); `None` for any other call.
fn preflight_refusal(host: &str, called: &Called) -> Option<(String, FallbackReason)> {
    install_refused(host, called)
        .map(|line| (line, FallbackReason::Install))
        .or_else(|| root_refused(host, called).map(|line| (line, FallbackReason::Root)))
}

/// The line refusing a builder `host` whose box refused its work root in the preflight call `called`: one that exited
/// [`super::ROOT_STATUS`] having said, before any start marker, that the root is not locked down ([`root_why`]),
/// which names the installer step that locks it, or that another process holds it open ([`root_busy_why`]), which
/// names none. `None` for any other call, and for an older box, which says neither.
fn root_refused(host: &str, called: &Called) -> Option<String> {
    if called.started {
        return None;
    }
    let said = || called.lines.iter().chain(&called.last_error).map(String::as_str);
    if let Some(why) = root_why(called.status, said()) {
        return Some(format!("gate: not routed: {host}'s work root is not locked down: {}", escaped(why)));
    }
    root_busy_why(called.status, said()).map(|why| format!("gate: not routed: {host}'s work root is busy: {}", escaped(why)))
}

/// [`install_refused`] for the run call, whose lines before the start marker relay the work folder's checkout, restore
/// and hooks: the plain line counts only as the call's one line, as the box says it when it refuses before reading the
/// run's request. Once it has read it, the box refuses for its install with the run's `install` refusal tag instead.
fn run_install_refused(host: &str, called: &Called) -> Option<String> {
    if called.lines.len() != 1 {
        return None;
    }
    install_refused(host, called)
}

/// The line refusing a builder `host` whose box refused its install: its call exited [`INSTALL_STATUS`] with one of
/// `said`, the lines it wrote, starting [`INSTALL_UNTRUSTED`] ([`install_why`]); `None` for any other call. The box's
/// why is shown escaped.
fn install_refusal<'a>(host: &str, status: i32, said: impl IntoIterator<Item = &'a str>) -> Option<String> {
    Some(install_refusal_line(host, install_why(status, said)?))
}

/// The line refusing a builder `host` whose gate install is not trusted, for `why`; the line the preflight's plain
/// refusal and the run call's `install` tag both end in ([`install_refusal`], [`install_tag_refusal`]).
fn install_refusal_line(host: &str, why: &str) -> String {
    format!(
        "gate: not routed: {host}'s gate is not installed where only administrators can change it ({}); run gate check --builder",
        escaped(why)
    )
}

/// The line refusing a builder `host` whose run call was refused with the run's `install` tag: the box's
/// [`INSTALL_UNTRUSTED`] line said after the tag, shown escaped; `None` when the box said none. The call ends at 75, not
/// [`INSTALL_STATUS`], so [`install_refusal`] does not read it.
fn install_tag_refusal<'a>(host: &str, said: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let why = said.into_iter().find_map(|line| line.strip_prefix(INSTALL_UNTRUSTED))?;
    Some(install_refusal_line(host, git::first_bytes(why, git::FAILURE_BYTES)))
}

/// The builder's version, `theirs`, as a line shows it when it is not `ours`; `None` when they are the same. A builder
/// that said no version runs one unknown; one that did is shown by its first 4 KiB, escaped.
pub(crate) fn other_build(theirs: Option<&str>, ours: &str) -> Option<String> {
    (theirs != Some(ours)).then(|| theirs.map_or_else(|| "unknown".to_owned(), |theirs| escaped(git::first_bytes(theirs, git::FAILURE_BYTES))))
}

/// The watchdog's line for a call watched as a fetch, `doing` what it was cut for `cut` doing: STALLED for one that sent
/// nothing for the stall window, TRANSFER TIMED OUT for one still running at its transfer ceiling; `None` for any other
/// cut.
pub(super) fn watchdog_line(cut: Cut, doing: &str) -> Option<String> {
    match cut {
        Cut::Silent(window) => Some(watchdog::stalled_line(window)),
        Cut::Ceiling(ceiling) => Some(watchdog::transfer_line(ceiling, doing)),
        Cut::Watchdog(_) | Cut::Late(_) | Cut::TooLong | Cut::TooBig(_) | Cut::ArchiveTooBig(_) => None,
    }
}

/// A line the box says on a run call's standard error for the client alone, besides its heartbeat: the start marker
/// said just before the command starts, a refusal tag and the fallback reason it names, the `sent` refusal tag, a kill
/// tag and the kill it names, or the adjusted line and the load-adjusted seconds it carries.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Protocol {
    Started,
    Refused(FallbackReason),
    /// The box's refusal tag for a work folder that does not hold the PC's build the run's `SentDigest` names, which
    /// no fallback answers: the client runs the call again with `--no-build` dropped ([`End::SentRefused`]).
    SentRefused,
    /// The box's `drain` refusal tag, which no fallback answers ([`End::Draining`]).
    Drain,
    /// The place or barrier line the box says after its `drain` tag ([`drain::line_of`]).
    DrainLine(DrainLine),
    /// The box's kill tag, said just after its kill line.
    Killed(BoxKill),
    /// The box's adjusted line, said once the command's readers drained.
    Adjusted(f64),
    /// The box's data channel line for the call, and the port it names ([`gate_wire::channel_line`]).
    Channel(u16),
    /// The box's `channel` refusal tag: its data channel did not complete.
    ChannelRefused,
}

/// What starts the box's line saying what it sent over a data channel ([`gate_wire::channel_sent_line`]).
const CHANNEL_SENT: &str = "gate: data channel sent ";

/// The start marker, the eleven refusal tags, the two kill tags and the adjusted line's prefix of one call, made once for
/// its readers, and whether the call is the run call, whose readers mark its progress per line.
struct ProtocolLines {
    run_call: bool,
    drain: String,
    started: String,
    busy: String,
    tools: String,
    layout: String,
    inputs: String,
    sent: String,
    desktop: String,
    install: String,
    lfs: String,
    full: String,
    timed_out: String,
    backstop: String,
    adjusted: String,
    channel: String,
    channel_refused: String,
}

impl ProtocolLines {
    /// The lines of the run call whose nonce is `nonce`.
    fn new(nonce: &str) -> ProtocolLines {
        ProtocolLines {
            run_call: true,
            ..ProtocolLines::tags(nonce)
        }
    }

    /// The lines of the preflight, inputs or LFS call whose nonce is `nonce`, which is no run call.
    fn tags(nonce: &str) -> ProtocolLines {
        ProtocolLines {
            run_call: false,
            drain: refused_line(nonce, Refusal::Drain),
            started: started_line(nonce),
            busy: refused_line(nonce, Refusal::Busy),
            tools: refused_line(nonce, Refusal::Tools),
            layout: refused_line(nonce, Refusal::Layout),
            inputs: refused_line(nonce, Refusal::Inputs),
            sent: refused_line(nonce, Refusal::Sent),
            desktop: refused_line(nonce, Refusal::Desktop),
            install: refused_line(nonce, Refusal::Install),
            lfs: refused_line(nonce, Refusal::Lfs),
            full: refused_line(nonce, Refusal::Full),
            timed_out: killed_line(nonce, BoxKill::TimedOut),
            backstop: killed_line(nonce, BoxKill::Backstop),
            adjusted: adjusted_prefix(nonce),
            channel: channel_prefix(nonce),
            channel_refused: refused_line(nonce, Refusal::Channel),
        }
    }

    /// The port of the box's data channel line for this call that `line` is, compared as a whole line; `None` for any
    /// other line, another call's included.
    fn channel_port(&self, line: &str) -> Option<u16> {
        let port = line.strip_prefix(self.channel.as_str())?;
        // The box writes the port as a number does, so no digit but a lone 0 starts with a 0.
        let digits = !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) && (port == "0" || !port.starts_with('0'));
        digits.then(|| port.parse().ok()).flatten()
    }

    /// What `line`, compared as a whole line, is of the run: its start marker, one of its refusal tags, one of its kill
    /// tags or its adjusted line with seconds the line may carry ([`adjusted_seconds`]); `None` for any other line,
    /// another run's marker, tag or adjusted line included, as for a command's own line.
    fn of(&self, line: &str) -> Option<Protocol> {
        [
            (&self.started, Protocol::Started),
            (&self.busy, Protocol::Refused(FallbackReason::Busy)),
            (&self.tools, Protocol::Refused(FallbackReason::Tools)),
            (&self.layout, Protocol::Refused(FallbackReason::Layout)),
            (&self.inputs, Protocol::Refused(FallbackReason::Inputs)),
            (&self.sent, Protocol::SentRefused),
            (&self.desktop, Protocol::Refused(FallbackReason::Desktop)),
            (&self.install, Protocol::Refused(FallbackReason::Install)),
            (&self.lfs, Protocol::Refused(FallbackReason::Lfs)),
            (&self.full, Protocol::Refused(FallbackReason::Full)),
            (&self.drain, Protocol::Drain),
            (&self.timed_out, Protocol::Killed(BoxKill::TimedOut)),
            (&self.backstop, Protocol::Killed(BoxKill::Backstop)),
            (&self.channel_refused, Protocol::ChannelRefused),
        ]
        .into_iter()
        .find_map(|(tag, protocol)| (line == tag.as_str()).then_some(protocol))
        .or_else(|| self.channel_port(line).map(Protocol::Channel))
        .or_else(|| {
            line.strip_prefix(self.adjusted.as_str())
                .and_then(adjusted_seconds)
                .map(Protocol::Adjusted)
        })
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

#[cfg(all(test, feature = "test-seams"))]
#[expect(clippy::expect_used, reason = "a test whose interrupt or transport cannot be made must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::fs;
    use std::io::{self, Cursor, Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, LazyLock, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use gate_args::TestSeams;
    use gate_state::{FallbackReason, StateRoot};
    use gate_win32::{Interrupt, PipedConsole};
    use gate_wire::{ROOT_UNSAFE, root_busy_line, root_unsafe_line};

    use super::retry::PRE_BANNER_RETRIES;
    use super::{
        Attempt, CallEnd, Called, Capped, Cut, Echo, End, GIT_SSH_FAILED, Heard, LINE_CAP, Limits, Loss, NonZeroU64, OUTPUTS_BUDGET, OneCall,
        ProtocolLines, Push, Raised, Reader, Reading, RustClient, SSH_FAILED, Shared, Split, Ssh, Unfinished, Watch, Watching, after_loss, boot_time,
        called_of, console_of, held, install_refusal, install_refused, install_tag_refusal, install_why, is_heartbeat, last_update,
        local_sdk_listing, path_outside, paused, preflight_refusal, push_size, relay, relay_raw, relayed_raw, root_refused, run_install_refused,
        sdks_of, seconds, their_version, version_refusal,
    };
    use super::{CSPROJ_BYTES, CSPROJ_FILES, Entry, csproj_pins, entries_of, manifest_entry, small_blobs_of};
    use super::{Carried, VERSION, preflight_ended};
    use super::{DRAIN, KILLED, Received};
    use super::{KeyHold, RunEnd, Warm, cold_line, hold_key, hold_key_waiting, new_work_tree, status_end, warm_end, warm_of};
    use crate::boxside::WORK_ROOT_STEP;
    use crate::environment::Variables;
    use crate::remote::drain::Drain;
    use crate::remote::git::{self, DOTNET_LIMIT, GIT_BOUNDS, Git, GitBounds, GitError};
    use crate::remote::held::tests::{Pair, junction};
    use crate::remote::outputs;
    use crate::remote::payload;
    use crate::remote::payload::Location;
    use crate::remote::snapshot::Snapshot;
    use crate::remote::toolchain::Pin;
    use crate::remote::transport::{Local, Profile, Transport};
    use crate::remote::watchdog::{self, Kill, LOG_CAP};
    use crate::remote::worktree_lock::tests::{held_elsewhere, lock_name};
    use crate::remote::{INSTALL_UNTRUSTED, install_untrusted_line};
    use crate::remote::{Refusal, RemoteCall, RemoteClient, Sibling, WarmEnd, Warmth, adjusted_line, busy_line, carried, refused_line, started_line};
    use crate::run::CONTROL_C_EXIT;
    use gate_router::BuilderOs;
    use gate_state::v1::WarmFailure;

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

    /// A Ctrl+C before this PC's SDK list is read starts no dotnet and ends the read as interrupted: the `dotnet.cmd`
    /// its `PATH` holds writes no mark.
    #[test]
    fn an_interrupted_sdk_list_read_starts_no_dotnet() {
        let folder = crate::test_support::temp_dir().join(format!("gate-client-dotnet-{}", std::process::id()));
        fs::create_dir_all(&folder).expect("the stub's folder can be made");
        let marker = folder.join("ran");
        let stub = format!("@echo off\r\n>\"{}\" echo ran\r\necho 9.0.100 [C:\\stub]\r\n", marker.display());
        fs::write(folder.join("dotnet.cmd"), stub).expect("the stub dotnet can be written");
        let variables = Variables::from_process().with("PATH", &folder.to_string_lossy());
        let worktree = Path::new(env!("CARGO_MANIFEST_DIR"));

        let read = local_sdk_listing(variables, &[worktree], DOTNET_LIMIT, DRAIN, Some(raised()));

        let ran = marker.exists();
        fs::remove_dir_all(&folder).expect("the stub's folder can be removed");
        assert_eq!(read, Err(GitError::Interrupted));
        assert!(!ran, "an interrupted read started dotnet");
    }

    #[test]
    fn a_path_with_an_unmatched_quote_cannot_be_rebuilt() {
        let worktree = Path::new(env!("CARGO_MANIFEST_DIR"));
        let unmatched = Variables::from_process().with("PATH", r#"C:\gate-a;"C:\gate-b;C:\gate-c"#);
        assert_eq!(
            path_outside(&unmatched, worktree),
            Err("PATH holds an unmatched quote, so the folders after it read as one".to_owned())
        );
        let quoted = Variables::from_process().with("PATH", r#"C:\gate-a;"C:\gate-b;c";C:\gate-d"#);
        assert_eq!(path_outside(&quoted, worktree), Ok(r#"C:\gate-a;"C:\gate-b;c";C:\gate-d"#.to_owned()));
        let inside = format!(r"C:\gate-a;{}\src;C:\gate-d", worktree.display());
        let with_inside = Variables::from_process().with("PATH", &inside);
        assert_eq!(path_outside(&with_inside, worktree), Ok(r"C:\gate-a;C:\gate-d".to_owned()));
    }

    /// Two readers of one call whose output together passes the log cap, each on a thread of its own and let go at
    /// once, say so in exactly one line between them, and log every whole line that fits under the cap and no other.
    #[test]
    fn two_readers_past_the_log_cap_say_it_once() {
        const CAP: u64 = 4096;
        let log = crate::test_support::temp_dir().join(format!("gate-client-log-cap-{}.log", std::process::id()));
        let file = fs::File::create(&log).expect("the log can be made");
        let shared = Arc::new(Shared::new());
        let reading = Reading {
            echo: Echo::LogOnly,
            capped: true,
            lines: None,
            log_cap: CAP,
        };
        // 100 lines of 101 bytes, line end counted, on each stream: either stream alone passes the cap.
        let input: Vec<u8> = (0..100)
            .flat_map(|number| format!("{number:03}{}\n", "x".repeat(97)).into_bytes())
            .collect();
        let start = Arc::new(std::sync::Barrier::new(2));
        let readers: Vec<thread::JoinHandle<()>> = [false, true]
            .into_iter()
            .map(|error| {
                let (shared, start, input) = (Arc::clone(&shared), Arc::clone(&start), input.clone());
                let reader = Reader::new(error, reading.clone(), Some(file.try_clone().expect("the log can be shared")));
                thread::spawn(move || {
                    start.wait();
                    relay(input.as_slice(), reader, &shared);
                })
            })
            .collect();
        for reader in readers {
            reader.join().expect("a reader ends");
        }
        drop(file);
        let said = crate::remote::log_texts(&log);
        fs::remove_file(&log).expect("the log can be removed");

        let cut = "gate: the builder's output passed 4 KiB; the rest is not logged";
        assert_eq!(said.lines().filter(|line| *line == cut).count(), 1, "{said}");
        let logged: Vec<&str> = said.lines().filter(|line| *line != cut).collect();
        // Each line is logged stamped `gat` on arrival, and the cap counts the bytes written, its stamp among them.
        let written = 101 + "2026-10-22T09:31:38.120001-07:00 gat ".len();
        let fit = usize::try_from(CAP).expect("the cap fits a usize") / written;
        assert_eq!(logged.len(), fit, "the whole lines that fit under the cap: {said}");
        assert!(logged.iter().all(|line| line.len() == 100), "{said}");
    }

    /// A call whose interrupt is raised, logging to `log`, which no test here writes.
    fn call(log: &Path) -> RemoteCall<'_> {
        RemoteCall {
            cross_build: None,
            builder: &crate::test_support::HOST,
            key: "app-1a2b3c4d",
            session: "0123456789abcdef0123456789abcdef",
            root: r"C:\no-such-worktree",
            root_id: gate_router::FolderId { volume: 0, file: [0; 16] },
            leaf: "app",
            siblings: Vec::new(),
            repo: "app",
            subfolder: "",
            words: &[],
            outputs: &[],
            outputs_withheld: false,
            bins_withheld: false,
            failure_outputs: &[],
            dropped_no_build: false,
            send: None,
            desktop: false,
            inputs: &[],
            log,
            timeout: 60,
            usual: None,
            stall: 60,
            tail: 0,
            no_markers: false,
            bring_tracked: &[],
            env: &[],
            interrupt: Some(raised()),
            state: &STATE,
            state_dir: Path::new(r"C:\gate-client-tests-state"),
            warmth: Warmth::Unasked,
        }
    }

    fn attempt<'c, 'a>(call: &'c RemoteCall<'a>, transport: &'c Local) -> Attempt<'c, 'a> {
        Attempt {
            call,
            transport,
            git: GIT_BOUNDS,
            lock_wait: git::TEST_HANG_GUARD,
            pause: LONG,
            transfer_pauses: [LONG; crate::remote::retry::TRANSFER_RETRIES],
            transfer_failed: false,
            limits: Limits::for_tests(),
            one_call: None,
            interrupt_at: None,
            channel_fault: None,
            local_sdks: None,
            dotnet_limit: DOTNET_LIMIT,
            fetch_cap: watchdog::FETCH_CAP,
            apply_cap: watchdog::APPLY_CAP,
            outputs_budget: OUTPUTS_BUDGET,
            log_cap: LOG_CAP,
            own_start_fails: false,
            hint_from_pull: false,
            tamper: None,
            inputs_budget: super::inputs::INPUTS_BUDGET,
            lfs_budget: super::lfs::LFS_BUDGET,
            brought_back: None,
            dropped_no_build: call.dropped_no_build,
            split: Split::default(),
            lines: Vec::new(),
            problem: None,
            hang_said: false,
            command_started: None,
            snapshot_taken: None,
            snapshot_age: Duration::ZERO,
            outputs_progress: super::OUTPUTS_PROGRESS,
            pulling: None,
            pull_periods_said: Cell::new(0),
            pulled_bytes: 0,
            pulled_last: None,
            adjusted: None,
            box_killed: None,
            held_run: None,
            ceiling: None,
            places: Vec::new(),
            local_major: None,
            warm: None,
            marker_seen: false,
            raised: None,
            backstop_seam: None,
            carried: Carried::default(),
            log_only: false,
        }
    }

    /// The call of the routed pair run `run`, carrying `siblings` and logging to `log`, its interrupt not raised, so a
    /// lock it waits for is waited out.
    fn pair_call<'a>(run: &'a gate_router::RemoteRun, siblings: Vec<Sibling<'a>>, log: &'a Path) -> RemoteCall<'a> {
        RemoteCall {
            root: &run.root,
            root_id: run.root_id,
            siblings,
            interrupt: None,
            ..call(log)
        }
    }

    /// The outputs-pending marker says the PC's outputs may be wrong, whichever builder ran: one a `mac-1` run leaves
    /// makes the next run of the worktree on `host` ask for every output that differs (`OutputsFull`).
    #[test]
    fn an_outputs_pending_marker_a_macos_run_leaves_makes_the_next_host_run_send_full() {
        let folder = crate::test_support::temp_dir().join(format!("gate-client-pending-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder an earlier run left can be removed");
        }
        let worktree = folder.join("app");
        fs::create_dir_all(worktree.join("bin")).expect("the worktree can be made");
        fs::write(worktree.join("bin").join("app.dll"), "built").expect("an output can be written");
        let state = folder.join("state");
        let log = folder.join("out.log");
        let root = worktree.to_string_lossy().into_owned();
        let globs = ["**/bin/**".to_owned()];
        let mac = gate_router::Builder {
            alias: "mac-1".to_owned(),
            os: BuilderOs::Macos,
            root: Some("/Users/b/gate-remote".to_owned()),
            slots: 1,
            id: Some("1f0efc62".to_owned()),
            data: None,
        };
        let on_mac = RemoteCall {
            builder: &mac,
            root: &root,
            outputs: &globs,
            state_dir: &state,
            interrupt: None,
            ..call(&log)
        };
        let transport = nowhere();
        // The record of the worktree's last complete outputs pull names the builder each run goes to, so the test's
        // subject stays the marker: a worktree with no record, or one of another builder, sends full on its own.
        let record = gate_state::v1::outputs_from_path(&state, "app-1a2b3c4d");
        fs::create_dir_all(record.parent().expect("the record has a folder")).expect("the state's remote folder can be made");
        fs::write(&record, "mac-1").expect("the record can be written");

        let left = outputs::sent(&mut attempt(&on_mac, &transport)).expect("the mac run's manifest is built");
        outputs::marked(&left).expect("the mac run's marker can be written");
        let on_host = RemoteCall {
            builder: &crate::test_support::HOST,
            ..on_mac
        };
        // The host is the builder this run goes to, so no switch makes it full: the marker alone does.
        fs::write(&record, &crate::test_support::HOST.alias).expect("the record can be rewritten");
        let next = outputs::sent(&mut attempt(&on_host, &transport)).expect("the host run's manifest is built");

        assert!(!left.full, "the mac run found no marker");
        assert!(next.full, "the host run sees the marker the mac run left");
        fs::remove_dir_all(&folder).expect("the test's folder can be removed");
    }

    /// A pair run holds its folders before anything else ([`super::Held::hold`]): a sibling made a junction after
    /// routing keeps the run here naming it, before any snapshot or call to the builder.
    #[test]
    fn a_pair_run_holds_its_folders_first_and_a_sibling_made_a_junction_stays_here() {
        let pair = Pair::new("client-junction", &["lib"]);
        let run = pair.run(&pair.root, &["lib"]);
        let moved = pair.folder("lib-moved");
        fs::rename(pair.folder("lib"), &moved).expect("the sibling can be moved");
        junction(&pair.folder("lib"), &moved);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);

        let ended = attempt.phases();

        drop(attempt);
        fs::remove_dir(pair.folder("lib")).expect("the junction can be removed");
        let link = format!("gate: not routed: sibling lib {}", crate::route::SIBLING_LINK);
        assert!(matches!(&ended, Err(End::NotRouted(line)) if *line == link), "{ended:?}");
    }

    /// A main tree that could not be listed, past the git output cap or holding a path that is not UTF-8, which
    /// [`Attempt::tree`] gives as `None`, has no LFS phase: no git call about its pointers and no `lfs` refusal.
    #[test]
    fn a_main_tree_that_could_not_be_listed_has_no_lfs_phase() {
        let log = crate::test_support::temp_dir().join(format!("gate-client-lfs-unlisted-{}.log", std::process::id()));
        let call = RemoteCall {
            interrupt: None,
            ..call(&log)
        };
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let repos = [super::Repo {
            name: "app".to_owned(),
            root: call.root.to_owned(),
            on_builder: r"C:\builder\work\app-1a2b3c4d".to_owned(),
            bare: r"C:\builder\app.git".to_owned(),
        }];

        let sending = attempt.lfs_sending(&repos, &[snapshot()], &[None]);

        assert!(matches!(sending, Ok(None)), "no LFS phase and no refusal");
        assert!(attempt.lines.is_empty(), "nothing said: {:?}", attempt.lines);
        assert!(!log.exists(), "nothing logged");
    }

    /// A sibling whose worktree lock another run's sync-back holds past the lock wait keeps the run here naming it.
    #[test]
    fn a_pair_run_whose_sibling_lock_is_held_past_the_wait_stays_here() {
        let pair = Pair::new("client-busy", &["lib"]);
        let run = pair.run(&pair.root, &["lib"]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let (release, holder) = held_elsewhere(&lock_name(&STATE, &run.siblings[0].root));
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        attempt.lock_wait = Duration::ZERO;

        let ended = attempt.hold_run();

        release.send(()).expect("the other run is still there");
        holder.join().expect("the other run ends");
        drop(attempt);
        let busy = "gate: not routed: sibling lib is busy with another routed run";
        assert!(matches!(&ended, Err(End::NotRouted(line)) if line == busy), "{ended:?}");
    }

    /// Runs git in `folder` with `args`, which must pass; its standard output, trimmed.
    #[expect(clippy::disallowed_methods, reason = "a test's git takes no spawn lock")]
    fn git_at(folder: &Path, args: &[&str]) -> String {
        let ran = std::process::Command::new("git")
            .arg("-C")
            .arg(folder)
            .args([
                "-c",
                "user.name=gate",
                "-c",
                "user.email=gate@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("git starts");
        assert!(ran.status.success(), "git {args:?} in {}: {ran:?}", folder.display());
        String::from_utf8_lossy(&ran.stdout).trim().to_owned()
    }

    /// Makes the root `app` of `pair` and each of its siblings `names` a repository of its own.
    fn repositories(pair: &Pair, names: &[&str]) {
        for name in names.iter().chain(&["app"]) {
            git_at(&pair.folder(name), &["init", "--quiet"]);
        }
    }

    /// Makes each of `names` beside the root of `pair` a linked worktree of the repository `lib-main` under its top
    /// folder, made first with one commit; the repository's folder.
    fn linked_worktrees(pair: &Pair, names: &[&str]) -> PathBuf {
        let main = pair.top.join("lib-main");
        fs::create_dir_all(&main).expect("the linked worktrees' repository can be made");
        git_at(&main, &["init", "--quiet"]);
        git_at(&main, &["commit", "--quiet", "--allow-empty", "-m", "base"]);
        for name in names {
            fs::remove_dir_all(pair.folder(name)).expect("the plain folder can be removed");
            git_at(&main, &["worktree", "add", "--quiet", "--detach", &pair.folder(name).to_string_lossy()]);
        }
        main
    }

    /// The end of the phases of the pair run of `pair` carrying the siblings `names`, every call to the builder made
    /// through [`nowhere`], with the tamper seam's change `tamper`.
    fn pair_phases(pair: &Pair, names: &[&str], tamper: Option<&str>) -> Result<End, End> {
        let run = pair.run(&pair.root, names);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        attempt.tamper = tamper;
        attempt.phases()
    }

    /// A pair changed between routing and its hold (`--test-remote-tamper before-hold:…`) stays here naming the folder
    /// before any snapshot or call to the builder: a sibling whose on-disk name differs in case, a sibling replaced by a
    /// new folder, whose id is another, and the worktree root replaced so.
    #[test]
    fn a_pair_changed_between_routing_and_its_hold_stays_here_naming_the_folder() {
        for (tamper, line) in [
            ("before-hold:case", "gate: not routed: sibling lib differs in case from its declared name"),
            (
                "before-hold:replace",
                "gate: not routed: sibling lib is not the folder the route was decided for",
            ),
            (
                "before-hold:replace-root",
                "gate: not routed: app is not the folder the route was decided for",
            ),
        ] {
            let pair = Pair::new(&format!("client-{}", tamper.replace(':', "-")), &["lib"]);

            let ended = pair_phases(&pair, &["lib"], Some(tamper));

            assert!(matches!(&ended, Err(End::NotRouted(said)) if said == line), "{tamper}: {ended:?}");
        }
    }

    /// A single-repository run whose root was replaced by a new folder between routing and its hold stays here naming
    /// it, as a pair's root does: the folder's id is read against the route's before the root's lock is taken.
    #[test]
    fn a_root_replaced_between_routing_and_its_lock_is_refused_at_the_lock() {
        let pair = Pair::new("client-root-replaced", &[]);
        let run = pair.run(&pair.root, &[]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, Vec::new(), &log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        attempt.tamper = Some("before-hold:replace-root");

        let ended = attempt.phases();

        drop(attempt);
        let refused = "gate: not routed: app is not the folder the route was decided for";
        assert!(matches!(&ended, Err(End::NotRouted(line)) if line == refused), "{ended:?}");
    }

    /// A single-repository run whose root's lock another run's sync-back holds past the lock wait stays here with the
    /// main repository's busy line, which names it by its folder name alone.
    #[test]
    fn a_run_whose_root_lock_is_held_past_the_wait_stays_here() {
        let pair = Pair::new("client-root-busy", &[]);
        let run = pair.run(&pair.root, &[]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, Vec::new(), &log);
        let (release, holder) = held_elsewhere(&lock_name(&STATE, &run.root));
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        attempt.lock_wait = Duration::ZERO;

        let ended = attempt.hold_run();

        release.send(()).expect("the other run is still there");
        holder.join().expect("the other run ends");
        drop(attempt);
        let busy = "gate: not routed: app is busy with another routed run";
        assert!(matches!(&ended, Err(End::NotRouted(line)) if line == busy), "{ended:?}");
    }

    /// A sibling whose git config names another work tree is not the top folder of its repository as git finds it, so
    /// the run stays here naming the folder git names, before any call to the builder.
    #[test]
    fn a_sibling_whose_config_names_another_work_tree_stays_here() {
        let pair = Pair::new("client-core-worktree", &["lib"]);
        repositories(&pair, &["lib"]);
        let canary = pair.folder("lib-canary");
        fs::create_dir_all(&canary).expect("the canary folder can be made");
        git_at(&pair.folder("lib"), &["config", "core.worktree", &canary.to_string_lossy()]);

        let ended = pair_phases(&pair, &["lib"], None);

        let start = "gate: not routed: sibling lib is not the top folder of its git repository: git names ";
        assert!(
            matches!(&ended, Err(End::NotRouted(line)) if line.starts_with(start) && line.ends_with("/lib-canary")),
            "{ended:?}"
        );
    }

    /// Two siblings the box would lay out in one folder, `x.y` and `x-y`, keep the run here before any call to the
    /// builder, the sibling the box refuses named; of `b`, `a.b` and `a-b`, the one named is `a-b`, not `b`, whose name
    /// the refusal's line also holds.
    #[test]
    fn a_pair_the_box_cannot_lay_out_stays_here_naming_the_sibling() {
        for (test, names, named) in [
            ("client-layout", &["x.y", "x-y"][..], "x-y"),
            ("client-layout-longest", &["b", "a.b", "a-b"][..], "a-b"),
        ] {
            let pair = Pair::new(test, names);
            repositories(&pair, names);

            let ended = pair_phases(&pair, names, None);

            let start = format!("gate: not routed: sibling {named} cannot be laid out on the builder: ");
            assert!(
                matches!(&ended, Err(End::NotRouted(line)) if line.starts_with(&start)),
                "{names:?}: {ended:?}"
            );
        }
    }

    /// The attempt of a pair run of `call` through [`nowhere`], its folders held and its repositories' order read.
    fn held_pair<'c, 'a>(call: &'c RemoteCall<'a>, transport: &'c Local) -> (Attempt<'c, 'a>, Vec<super::Repo>) {
        let mut attempt = attempt(call, transport);
        attempt.hold_run().expect("the pair is held");
        let repos = attempt.repos();
        (attempt, repos)
    }

    /// Makes the held sibling `lib` of `attempt` seem to have moved ([`super::Held::misplace`]).
    fn misplaced(attempt: &mut Attempt<'_, '_>, pair: &Pair) {
        let held = attempt.held_run.as_mut().and_then(|run| run.held.as_mut()).expect("the pair is held");
        let elsewhere = gate_router::sibling_root(&pair.root, "elsewhere");
        assert!(held.misplace("sibling lib", &elsewhere), "the sibling is held");
    }

    /// The pair's folders are read again after each snapshot: a sibling whose path stops leading to its held folder
    /// once the main repository's snapshot is taken keeps the run here naming it.
    #[test]
    fn a_sibling_that_moves_while_the_snapshots_are_taken_stays_here() {
        let pair = Pair::new("client-moved-snapshot", &["lib"]);
        repositories(&pair, &["lib"]);
        let run = pair.run(&pair.root, &["lib"]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let transport = nowhere();
        let (mut attempt, repos) = held_pair(&call, &transport);
        misplaced(&mut attempt, &pair);

        let ended = attempt.snapshots(&repos).map(|_| ());

        drop(attempt);
        let moved = "gate: not routed: sibling lib moved while it was held".to_owned();
        assert_eq!(ended.map_err(|end| format!("{end:?}")), Err(format!("{:?}", End::NotRouted(moved))));
    }

    /// The pair's folders are read again as they are let go, before the run call: a sibling whose path stops leading
    /// to its held folder after the snapshots keeps the run here naming it.
    #[test]
    fn a_sibling_that_moves_before_the_pair_is_let_go_stays_here() {
        let pair = Pair::new("client-moved-release", &["lib"]);
        repositories(&pair, &["lib"]);
        let run = pair.run(&pair.root, &["lib"]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let transport = nowhere();
        let (mut attempt, repos) = held_pair(&call, &transport);
        attempt.snapshots(&repos).expect("the snapshots are taken");
        misplaced(&mut attempt, &pair);

        let ended = attempt.release_run();

        drop(attempt);
        let moved = "gate: not routed: sibling lib moved while it was held".to_owned();
        assert_eq!(ended.map_err(|end| format!("{end:?}")), Err(format!("{:?}", End::NotRouted(moved))));
    }

    /// A sibling that is a linked worktree is snapshotted, and its git folder, outside the pair's parent, is held
    /// while the pair is: the system refuses its rename (a sharing violation, OS error 32).
    #[test]
    fn a_linked_worktree_sibling_is_snapshotted_and_its_git_folder_held() {
        let pair = Pair::new("client-linked", &["lib"]);
        repositories(&pair, &[]);
        let main = linked_worktrees(&pair, &["lib"]);
        let run = pair.run(&pair.root, &["lib"]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let transport = nowhere();
        let (mut attempt, repos) = held_pair(&call, &transport);

        let taken = attempt.snapshots(&repos).map(|snapshots| snapshots.len());
        let git_dir = main.join(".git").join("worktrees").join("lib");
        let renamed = fs::rename(&git_dir, main.join(".git").join("worktrees").join("lib-away"));

        drop(attempt);
        assert_eq!(taken.map_err(|end| format!("{end:?}")), Ok(2), "both repositories are snapshotted");
        assert_eq!(renamed.map_err(|error| error.raw_os_error()), Err(Some(32)), "the git folder is held");
    }

    /// Two siblings that are linked worktrees of one repository share its common folder and its refs, so the run stays
    /// here naming the second. Git prints a common folder by its final path, letter case and links resolved, so the
    /// folder's id is what tells two of them apart.
    #[test]
    fn two_siblings_sharing_one_common_folder_stay_here() {
        let pair = Pair::new("client-shared-common", &["b", "c"]);
        repositories(&pair, &[]);
        linked_worktrees(&pair, &["b", "c"]);

        let ended = pair_phases(&pair, &["b", "c"], None);

        let line = "gate: not routed: sibling c shares its git repository with b";
        assert!(matches!(&ended, Err(End::NotRouted(said)) if said == line), "{ended:?}");
    }

    /// A pair repository's own config cannot turn its push into a protocol helper: with `protocol.ext.allow=always`
    /// and a `pushInsteadOf` that makes the push's URL an `ext::` command writing a marker file, the push fails and the
    /// command never runs.
    #[test]
    fn a_pair_repository_s_config_cannot_turn_its_push_into_a_protocol_helper() {
        let pair = Pair::new("client-push-helper", &["lib"]);
        repositories(&pair, &["lib"]);
        let app = pair.folder("app");
        let marker = pair.top.join("helper-ran");
        git_at(&app, &["config", "protocol.ext.allow", "always"]);
        let helper = format!("url.ext::touch {} .pushInsteadOf", marker.to_string_lossy().replace('\\', "/"));
        let run = pair.run(&pair.root, &["lib"]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let transport = nowhere();
        let target = transport
            .push_target(&transport.profile(call.builder), call.repo)
            .expect("the main repository has a push target");
        git_at(&app, &["config", &helper, &target.url]);
        let (mut attempt, repos) = held_pair(&call, &transport);
        let snapshots = attempt.snapshots(&repos).expect("the snapshots are taken");

        let ended = attempt.pushes(&repos, &snapshots, &[]);

        drop(attempt);
        assert!(matches!(ended, Err(End::Refused(FallbackReason::Unreachable))), "{ended:?}");
        assert!(!marker.exists(), "the protocol helper ran");
    }

    /// While a pair is held, a root spelled other than the one its git location was recorded under at its check has no
    /// location to give git, so a git call about it fails before git starts rather than run with the ceiling alone.
    #[test]
    fn a_root_spelled_other_than_at_its_check_fails_while_the_pair_is_held() {
        let pair = Pair::new("client-unrecorded", &["lib"]);
        repositories(&pair, &["lib"]);
        let run = pair.run(&pair.root, &["lib"]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let transport = nowhere();
        let (mut attempt, repos) = held_pair(&call, &transport);
        attempt.snapshots(&repos).expect("the snapshots are taken");
        let spelled = PathBuf::from(run.root.to_uppercase());
        let git = Git::find(&spelled).expect("git is found");

        let listed = attempt.git_bytes_in(&spelled, &git, &["ls-files"]);

        drop(attempt);
        let line = format!("the git location of {} was not recorded at its check", spelled.display());
        assert!(matches!(&listed, Err(GitError::Failed(why)) if *why == line), "{listed:?}");
    }

    #[test]
    fn a_routed_hint_said_before_a_failed_wait_is_not_said_again() {
        let log = Path::new(r"C:\gate-client-tests-no-such-folder\out.log");
        let call = call(log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let line = "gate: still running after 3 s, 3x its usual p90 of 1 s".to_owned();
        let watching = Watching {
            ended: Err(End::Failed("cannot wait for ssh".to_owned())),
            hinted: Some(Ok(line.clone())),
        };

        let ended = attempt.watched_out(watching);

        assert!(matches!(ended, Err(End::Failed(_))), "the failed watch stays the call's end");
        assert!(attempt.hang_said, "the hint said before the failure counts as said");
        assert_eq!(attempt.lines, [line], "the attempt's lines keep the hint said");
    }

    /// A transport whose every call is made through a stand-in that does not exist, so a call made by mistake fails to
    /// start rather than running anything.
    fn nowhere() -> Local {
        Local::new(r"local:C:\builder", Some(Path::new(r"C:\gate-client-tests-no-such-folder\fake-ssh.exe"))).expect("a local transport")
    }

    /// The location of a run call no test makes.
    fn location() -> Location {
        Location::under(
            &Profile::new("builder", BuilderOs::Windows, r"C:\builder"),
            "app",
            "app-1a2b3c4d",
            "0123456789ab",
        )
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
            outputs: None,
            back_commits: None,
            names_skipped: false,
            cross_not_brought_back: false,
            started: false,
            refused: None,
            drain: None,
            box_killed: None,
            adjusted: None,
            written: None,
            received: None,
            channel: None,
            channel_sent: None,
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

    /// A sink that notes, at each write, whether the call's progress was marked already and whether the call counts as
    /// having written on standard output.
    struct MarkedAtWrite<'s> {
        shared: &'s Shared,
        seen: Vec<(bool, bool)>,
    }

    impl Write for MarkedAtWrite<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let output_seen = held(&self.shared.heard).output_seen;
            self.seen.push((self.shared.progress.heard_any(), output_seen));
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A raw copy marks a chunk only once it is written into the sink, which counts it, so the interrupt raised at the
    /// call's first mark finds that chunk counted.
    #[test]
    fn a_raw_copy_marks_a_chunk_only_once_it_is_written() {
        let shared = Shared::new();
        let mut sink = MarkedAtWrite {
            shared: &shared,
            seen: Vec::new(),
        };

        relay_raw(&b"archive bytes"[..], &mut sink, "the archive", &shared);

        assert_eq!(sink.seen, [(false, false)], "the one chunk is written before any mark");
        assert!(shared.progress.heard_any(), "then marked");
        assert!(
            held(&shared.heard).output_seen,
            "and the call counts as having written on standard output"
        );
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
        assert_eq!(shared.progress.received(), 0, "a copy that writes nothing counts nothing");
    }

    /// The call's progress counts the bytes the copy wrote into the archive, and none it kept out past the cap.
    #[test]
    fn a_raw_copy_counts_the_bytes_it_wrote_into_its_sink() {
        let cap = NonZeroU64::new(1_000).expect("the cap is above 0");
        let shared = Arc::new(Shared::new());
        let mut sink = Capped {
            inner: Vec::new(),
            cap,
            shared: Arc::clone(&shared),
            written: 0,
        };

        relay_raw(&mut Cursor::new(vec![b'x'; 100]), &mut sink, "the archive", &shared);
        let under = shared.progress.received();
        relay_raw(&mut Cursor::new(vec![b'x'; 3_000]), &mut sink, "the archive", &shared);

        assert_eq!(under, 100, "every byte under the cap is counted");
        assert_eq!(shared.progress.received(), cap.get(), "no byte past the cap is counted");
    }

    /// An outputs call the watchdog stopped once its raw copy had written 100 bytes into the archive says them, with
    /// the seconds the call ran and the rate over them.
    #[test]
    fn an_outputs_call_the_watchdog_stopped_says_what_its_copy_had_written() {
        let log = unwritten_log();
        let mut given = call(&log);
        given.interrupt = None;
        let transport = nowhere();
        let mut attempt = attempt(&given, &transport);
        let archive = outputs::Archive::new(
            Path::new(r"C:\gate-client-tests-no-such-folder"),
            PathBuf::from(r"C:\gate-client-tests-no-such-folder\app.tar.zst"),
        );
        let mut stalled = ended_with(KILLED, "");
        stalled.cut = Some(Cut::Silent(Duration::from_millis(300)));
        stalled.received = Some(Received {
            bytes: 100,
            after: Duration::from_millis(2_500),
        });

        let end = outputs::judged(
            &mut attempt,
            (&archive, outputs::Bringing::Outputs),
            (&stalled, false),
            End::Status(0),
            false,
        );

        assert!(matches!(end, End::Killed), "{end:?}");
        assert_eq!(
            attempt.lines,
            [
                "gate: STALLED: no output or remote CPU for 1 s",
                "gate: remote outputs not brought back: the fetch was stopped by the watchdog after 100 B in 2 s (50 B/s)"
            ]
        );
    }

    /// A Ctrl+C come before the outputs call leaves no figure of an earlier raw copy (an inputs or LFS diff) as the pull's:
    /// the call makes nothing, and the pull has received nothing.
    #[test]
    fn an_outputs_call_a_ctrl_c_ends_before_it_starts_has_received_nothing() {
        let log = unwritten_log();
        let given = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&given, &transport);
        attempt.pulled_bytes = 300_000;
        attempt.pulled_last = Some(Instant::now());
        let mut archive = outputs::Archive::new(
            Path::new(r"C:\gate-client-tests-no-such-folder"),
            PathBuf::from(r"C:\gate-client-tests-no-such-folder\app.tar.zst"),
        );
        let pull = super::Pull {
            bringing: outputs::Bringing::Outputs,
            total: None,
            started: Instant::now(),
        };

        let called = attempt.outputs_call(&location(), "app.tar.zst", &mut archive, Watch::Fetch(Limits::for_tests(), None), pull);

        assert!(matches!(called, Err(End::Interrupted)), "the raised Ctrl+C ends it");
        assert_eq!(attempt.pulled_bytes, 0, "no earlier call's bytes");
        assert_eq!(attempt.pulled_last, None, "no earlier call's last byte");
    }

    /// What came over a refused data channel counts once in the pull's total, whether the ssh fallback ended before its
    /// watch ran, leaving the figure alone, or after, setting it to the ssh call's own.
    #[test]
    fn the_channel_s_bytes_count_once_in_the_pull_whatever_way_the_ssh_fallback_ends() {
        let log = unwritten_log();
        let given = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&given, &transport);

        attempt.pulled_bytes = 300;
        attempt.counted_with_channel(|_| ());
        assert_eq!(attempt.pulled_bytes, 300, "a fallback that ended before its watch adds nothing");

        attempt.pulled_bytes = 300;
        attempt.counted_with_channel(|attempt| attempt.pulled_bytes = 4096);
        assert_eq!(attempt.pulled_bytes, 4396, "the channel's bytes and the ssh call's");
    }

    /// An outputs call refused by the box's `channel` tag whose PC side ended as `channel`, the box having said
    /// `box_why` on its not-completed line, or nothing.
    fn channel_refused_with(channel: super::channel::Dialled, box_why: Option<&str>) -> Called {
        let mut called = Called::exited_for_tests(0);
        called.channel = Some(channel);
        called.lines = box_why.map(gate_wire::channel_not_completed_line).into_iter().collect();
        called
    }

    /// The not-used reason takes the PC's own failure first, then a missing size, then the box's text, then a channel
    /// that did not end, then that the box did not complete it; a PC side the call's end stopped has no reason of its own.
    #[test]
    fn a_refused_channel_s_reason_ranks_the_pc_s_failure_the_size_the_box_s_text_and_a_channel_that_did_not_end() {
        use super::channel::Dialled;

        let box_why = Some("no connection from the PC within 10000 ms");
        let why = |channel, said| super::channel_why(&channel_refused_with(channel, said));
        assert_eq!(why(Dialled::Failed("the PC's own".to_owned()), box_why), "the PC's own");
        assert_eq!(why(Dialled::NoSize, box_why), "the box said no archive size");
        assert_eq!(why(Dialled::CallEnded, box_why), "no connection from the PC within 10000 ms");
        assert_eq!(why(Dialled::NotEnded, box_why), "no connection from the PC within 10000 ms");
        assert_eq!(why(Dialled::NoPort, box_why), "no connection from the PC within 10000 ms");
        assert_eq!(why(Dialled::NotEnded, None), "the channel did not end");
        assert_eq!(why(Dialled::CallEnded, None), "the box did not complete it");
    }

    /// The box's data channel line is taken only whole, for this call's nonce, with the port as a number is written.
    #[test]
    fn the_channel_port_line_is_taken_only_whole_without_a_leading_zero() {
        let nonce = "0123456789abcdef0123456789abcdef";
        let lines = super::ProtocolLines::tags(nonce);
        assert_eq!(lines.channel_port(&gate_wire::channel_line(nonce, 47100)), Some(47100));
        assert_eq!(lines.channel_port(&gate_wire::channel_line(nonce, 0)), Some(0));
        let prefix = gate_wire::channel_prefix(nonce);
        for refused in ["047100", "00", "", "70000", "47100 ", "4710x"] {
            assert_eq!(lines.channel_port(&format!("{prefix}{refused}")), None, "{refused:?}");
        }
        let other = gate_wire::channel_line("fedcba9876543210fedcba9876543210", 47100);
        assert_eq!(lines.channel_port(&other), None, "another call's line");
    }

    /// A pull says each progress period once across its calls: the call made again after one that said period 1 finds
    /// no line due at the same elapsed time.
    #[test]
    fn a_call_made_again_says_no_progress_period_an_earlier_call_of_the_pull_said() {
        let log = Path::new(r"C:\no-such-log.log");
        let given = call(log);
        let transport = nowhere();
        let mut attempt = attempt(&given, &transport);
        attempt.outputs_progress = Duration::from_secs(1);
        attempt.pulling = Some(super::Pull {
            bringing: outputs::Bringing::Outputs,
            total: None,
            started: Instant::now(),
        });
        let past_period_1 = Duration::from_millis(1_500);

        let first = attempt.pull_watch().expect("an outputs call watches its pull").due(past_period_1);
        let again = attempt.pull_watch().expect("so does the call made again").due(past_period_1);

        assert_eq!([first, again], [true, false], "period 1 is said once across the pull's calls");
    }

    #[test]
    fn a_put_is_cut_as_silent_only_once_its_feeder_stops_writing() {
        let second = Duration::from_secs(1);
        let begun = Instant::now();
        let limits = Limits {
            started: begun,
            stall: 10 * second,
            command: 60 * second,
            backstop: 360 * second,
            transfer: 1800 * second,
            bound: 30 * second,
            transfer_from: None,
        };
        let put = Watch::Put(limits);
        let mut sending = super::Sending::new(begun);
        // Its output says nothing the whole time; only the bytes its feeder wrote tell a live put from a dead one.
        for look in 1..=60_u32 {
            let now = begun + look * 5 * second;
            let quiet = sending.quiet(u64::from(look) * 65_536, now);
            assert_eq!(
                put.cut(now, begun, quiet, Raised::default(), None),
                None,
                "the feeder still writes at look {look}"
            );
        }
        let last = begun + 300 * second;
        let before = last + 9 * second;
        assert_eq!(put.cut(before, begun, sending.quiet(60 * 65_536, before), Raised::default(), None), None);
        let past = last + 10 * second;
        assert_eq!(
            put.cut(past, begun, sending.quiet(60 * 65_536, past), Raised::default(), None),
            Some(Cut::Silent(10 * second)),
            "a feeder that stopped writing is a stalled put"
        );
    }

    #[test]
    fn a_put_counts_its_output_while_it_feeds_and_no_silence_once_fed() {
        let second = Duration::from_secs(1);
        let begun = Instant::now();
        let limits = Limits {
            started: begun,
            stall: 10 * second,
            command: 60 * second,
            backstop: 360 * second,
            transfer: 1800 * second,
            bound: 30 * second,
            transfer_from: None,
        };
        let put = Watch::Put(limits);
        let mut sending = super::Sending::new(begun);
        let at = begun + 30 * second;
        let heard_lately = sending.idle(0, false, 2 * second, at);
        assert_eq!(heard_lately, 2 * second, "the box's output keeps a put whose feeder is blocked alive");
        assert_eq!(put.cut(at, begun, heard_lately, Raised::default(), None), None);
        let both_silent = sending.idle(0, false, 30 * second, at);
        assert_eq!(
            put.cut(at, begun, both_silent, Raised::default(), None),
            Some(Cut::Silent(10 * second)),
            "silent both ways while feeding is a stalled put"
        );
        let installing = begun + 1700 * second;
        let fed = sending.idle(0, true, 1700 * second, installing);
        assert_eq!(fed, Duration::ZERO, "a fed put is never silent");
        assert_eq!(put.cut(installing, begun, fed, Raised::default(), None), None);
        let late = begun + 1800 * second;
        assert_eq!(
            put.cut(late, begun, sending.idle(0, true, 1800 * second, late), Raised::default(), None),
            Some(Cut::Ceiling(1800 * second)),
            "the transfer ceiling alone watches a fed put"
        );
    }

    /// A writer that takes `room` bytes, then fails each write as a closed pipe does.
    struct ClosesAfter {
        room: usize,
    }

    impl Write for ClosesAfter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.room == 0 {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            let taken = buffer.len().min(self.room);
            self.room -= taken;
            Ok(taken)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_feeder_ends_quietly_on_a_pipe_closed_in_its_archive_but_not_in_its_head() {
        let folder = crate::test_support::temp_dir().join(format!("gate-client-feeder-{}", std::process::id()));
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        let path = folder.join("archive");
        fs::write(&path, vec![b'a'; 200_000]).expect("the archive can be written");
        let streamed = |head: &str| {
            let file = fs::File::open(&path).expect("the archive can be opened");
            super::Input::Streamed(Arc::from(head), super::Archived::new(file, 200_000))
        };
        // Each through the writer stack a feeder writes through, its buffer included.
        let fed = |room: usize| {
            let sent = std::sync::atomic::AtomicU64::new(0);
            super::fed_with(&mut super::feeder_writer(ClosesAfter { room }, &sent), streamed("request\n"))
        };

        let in_head = fed(4);
        let in_archive = fed(100);

        assert_eq!(in_head.map_err(|error| error.kind()), Err(io::ErrorKind::BrokenPipe));
        assert_eq!(in_archive.map_err(|error| error.kind()), Ok(()));
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    #[test]
    fn two_put_attempts_over_one_handle_each_read_the_whole_archive() {
        let folder = crate::test_support::temp_dir().join(format!("gate-client-archived-{}", std::process::id()));
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        let path = folder.join("archive");
        let archive: Vec<u8> = (0..100_000_u32).map(|at| at.to_le_bytes()[0] ^ at.to_le_bytes()[1]).collect();
        fs::write(&path, &archive).expect("the archive can be written");
        let held = fs::File::open(&path).expect("the archive can be opened");
        let packed = 90_000;
        let clone = || held.try_clone().expect("the handle can be cloned");
        let mut first = super::Archived::new(clone(), packed);
        let mut second = super::Archived::new(clone(), packed);
        let (mut read_first, mut read_second) = (Vec::new(), Vec::new());

        let mut chunk = [0_u8; 7_000];
        loop {
            let one = first.read(&mut chunk).expect("the first attempt reads");
            read_first.extend_from_slice(chunk.get(..one).unwrap_or_default());
            let two = second.read(chunk.get_mut(..3_000).unwrap_or_default()).expect("the second attempt reads");
            read_second.extend_from_slice(chunk.get(..two).unwrap_or_default());
            if one == 0 && two == 0 {
                break;
            }
        }

        assert_eq!(
            Some(read_first.as_slice()),
            archive.get(..90_000),
            "the first attempt reads the packed length from the start"
        );
        assert_eq!(
            Some(read_second.as_slice()),
            archive.get(..90_000),
            "and so does the second, read between its reads"
        );
        drop(held);
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    #[test]
    fn a_feeder_counts_every_byte_it_writes() {
        let sent = std::sync::atomic::AtomicU64::new(0);
        let mut sink = Vec::new();
        let mut counted = super::Counted {
            inner: &mut sink,
            sent: &sent,
        };

        counted.write_all(b"request\n").expect("the head is written");
        io::copy(&mut &b"0123456789"[..], &mut counted).expect("the file is copied");

        assert_eq!(sent.load(Ordering::Relaxed), 18);
        assert_eq!(sink, b"request\n0123456789");
    }

    #[test]
    fn a_call_that_passed_a_raw_copy_s_cap_is_cut_for_it_even_after_its_exit() {
        let second = Duration::from_secs(1);
        let started = Instant::now();
        let limits = Limits {
            started,
            stall: 10 * second,
            command: 60 * second,
            backstop: 360 * second,
            transfer: 60 * second,
            bound: 30 * second,
            transfer_from: None,
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

    /// The line of an outputs call cut for the archive it passed, at the cap a 256 KiB budget gives.
    const CAPPED_LINE: &str = "gate: remote outputs not brought back: the archive passed 256.0 KiB";

    /// An empty state folder of the test `name`'s own, which a cap hit writes its outputs cap record under.
    fn fresh_state(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-client-state-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a state folder left by an earlier run can be removed");
        }
        folder
    }

    /// The outputs cap record of the unit call's key under `state`.
    fn capped_record(state: &Path) -> PathBuf {
        gate_state::v1::outputs_capped_path(state, None, "app-1a2b3c4d")
    }

    /// An outputs call cut for the archive it passed, judged for a passing run whose state folder is `state`: its end
    /// and the lines it said.
    fn cap_hit(state: &Path) -> (End, Vec<String>) {
        let log = unwritten_log();
        let mut call = call(&log);
        // No interrupt is raised: this cut is judged by the cap record, unlike the failed fetch a Ctrl+C ends.
        call.interrupt = None;
        call.state_dir = state;
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let archive = outputs::Archive::new(
            Path::new(r"C:\gate-client-tests-no-such-folder"),
            PathBuf::from(r"C:\gate-client-tests-no-such-folder\app.tar.zst"),
        );
        let mut cut = ended_with(0, "");
        cut.cut = Some(Cut::ArchiveTooBig(262_144));
        let end = outputs::judged(&mut attempt, (&archive, outputs::Bringing::Outputs), (&cut, false), End::Status(0), false);
        (end, std::mem::take(&mut attempt.lines))
    }

    #[test]
    fn an_outputs_call_cut_for_the_archive_it_passed_keeps_the_run_s_status() {
        let state = fresh_state("first-cap");

        let (end, lines) = cap_hit(&state);

        assert!(matches!(end, End::Status(0)), "the run keeps its status: {end:?}");
        assert_eq!(lines, [CAPPED_LINE]);
        let bytes = fs::read(capped_record(&state)).expect("the record is written");
        assert_eq!(String::from_utf8_lossy(&bytes), r#"{"v":1,"hits":1}"#);
    }

    #[test]
    fn a_second_outputs_call_cut_for_the_archive_in_a_row_fails_the_run_and_so_does_a_third() {
        let state = fresh_state("caps-in-a-row");

        let (first, _) = cap_hit(&state);
        let (second, second_lines) = cap_hit(&state);
        let (third, third_lines) = cap_hit(&state);

        assert!(matches!(first, End::Status(0)), "{first:?}");
        assert!(matches!(second, End::Status(1)), "{second:?}");
        let again = "gate: the builder's outputs passed the cap again, 2 runs in a row; this run fails";
        assert_eq!(second_lines, [CAPPED_LINE, again]);
        assert!(matches!(third, End::Status(1)), "{third:?}");
        let again = "gate: the builder's outputs passed the cap again, 3 runs in a row; this run fails";
        assert_eq!(third_lines, [CAPPED_LINE, again]);
        let bytes = fs::read(capped_record(&state)).expect("the record is kept");
        assert_eq!(String::from_utf8_lossy(&bytes), r#"{"v":1,"hits":3}"#);
    }

    #[test]
    fn an_outputs_cap_record_that_cannot_be_read_counts_no_hit() {
        let state = fresh_state("unreadable-cap");
        let record = capped_record(&state);
        fs::create_dir_all(record.parent().expect("the record has a folder")).expect("the folder can be made");
        fs::write(&record, "not a record").expect("the record can be spoiled");

        let (end, lines) = cap_hit(&state);

        assert!(matches!(end, End::Status(0)), "{end:?}");
        assert_eq!(lines, [CAPPED_LINE]);
        assert_eq!(fs::read_to_string(&record).expect("the record is rewritten"), r#"{"v":1,"hits":1}"#);
    }

    /// A folder stands at the record's name: it reads as no record and cannot be written over.
    #[test]
    fn an_outputs_cap_record_that_cannot_be_written_is_said_and_the_count_read_decides() {
        let state = fresh_state("unwritable-cap");
        let record = capped_record(&state);
        fs::create_dir_all(&record).expect("a folder can stand at the record's name");

        let (end, lines) = cap_hit(&state);

        assert!(matches!(end, End::Status(0)), "the first hit read keeps the pass: {end:?}");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], CAPPED_LINE);
        let cannot = format!("gate: cannot write the outputs cap record {}: ", record.display());
        assert!(lines[1].starts_with(&cannot), "{lines:?}");
    }

    #[test]
    fn clearing_the_outputs_cap_record_removes_it_and_says_one_it_cannot_remove() {
        let state = fresh_state("clear-cap");
        let record = capped_record(&state);
        let log = unwritten_log();
        let mut call = call(&log);
        call.interrupt = None;
        call.state_dir = &state;
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let (hit, _) = cap_hit(&state);

        outputs::uncapped(&mut attempt);
        let removed = !record.exists();
        outputs::uncapped(&mut attempt);
        let quiet = attempt.lines.is_empty();
        fs::create_dir_all(record.join("held")).expect("a folder can stand at the record's name");
        outputs::uncapped(&mut attempt);

        assert!(matches!(hit, End::Status(0)), "{hit:?}");
        assert!(removed, "the record is removed");
        assert!(quiet, "a record already gone is removed silently");
        let cannot = format!("gate: cannot remove the outputs cap record {}: ", record.display());
        assert_eq!(attempt.lines.len(), 1, "{:?}", attempt.lines);
        assert!(attempt.lines[0].starts_with(&cannot), "{:?}", attempt.lines);
    }

    #[test]
    fn an_outputs_archive_past_the_fetch_cap_is_removed_when_its_cut_is_judged() {
        let state = fresh_state("cap-removes-archive");
        let log = unwritten_log();
        let mut call = call(&log);
        call.interrupt = None;
        call.state_dir = &state;
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let folder = crate::test_support::temp_dir().join(format!("gate-client-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("the folder can be made");
        let path = folder.join(".tmp").join("app.tar.zst");
        let mut archive = outputs::Archive::new(&folder, path.clone());
        let mut sink = archive.create(NonZeroU64::MAX).expect("the archive is made");
        sink.file.write_all(b"partial").expect("the file can be written");
        drop(sink);
        let mut cut = ended_with(0, "");
        cut.cut = Some(Cut::ArchiveTooBig(262_144));

        let end = outputs::judged(&mut attempt, (&archive, outputs::Bringing::Outputs), (&cut, false), End::Status(0), false);

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

        let end = outputs::judged(
            &mut attempt,
            (&archive, outputs::Bringing::Outputs),
            (&failed, false),
            End::Status(0),
            true,
        );

        assert!(matches!(end, End::Interrupted), "{end:?}");
        assert_eq!(attempt.lines, Vec::<String>::new());
    }

    /// A worktree folder of the test `name`'s own, made empty, holding the output `bin/a.txt` when `held`.
    fn outputs_worktree(name: &str, held: bool) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-client-worktree-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a worktree left by an earlier run can be removed");
        }
        fs::create_dir_all(folder.join("bin")).expect("the worktree can be made");
        if held {
            fs::write(folder.join("bin").join("a.txt"), "pc").expect("the output can be written");
        }
        folder
    }

    /// An outputs step as [`brought_back_after`] ends it: the worktree its manifest is built from, whether the run has
    /// output globs, the run call's status and the one the sync-back left, what the box said it packed, whether it said
    /// it skipped outputs for names and that the cross build brought nothing back, and whether the call sent one.
    #[expect(
        clippy::struct_excessive_bools,
        reason = "each flag is an independent fact of the step a case sets by name; no two form a state machine"
    )]
    struct Step<'s> {
        root: &'s Path,
        globs: bool,
        status: i64,
        synced: i64,
        brought: Option<outputs::Brought>,
        names_skipped: bool,
        cross_not_brought_back: bool,
        crossed: bool,
        bins_withheld: bool,
    }

    /// The outputs step of a passing run with output globs in `root` that sent no cross build, the box having said it
    /// packed `brought` and, when `names_skipped`, that it skipped outputs for names.
    fn passed(root: &Path, brought: Option<outputs::Brought>, names_skipped: bool) -> Step<'_> {
        Step {
            root,
            globs: true,
            status: 0,
            synced: 0,
            brought,
            names_skipped,
            cross_not_brought_back: false,
            crossed: false,
            bins_withheld: false,
        }
    }

    /// Whether an attempt's outputs are brought back once its outputs step ended as `step` says ([`ended_after`]).
    fn brought_back_after(name: &str, step: &Step<'_>) -> Option<bool> {
        ended_after(name, step).1
    }

    /// The end of an attempt's outputs step that ended as `step` says, and whether its outputs are brought back
    /// ([`said_after`]).
    fn ended_after(name: &str, step: &Step<'_>) -> (End, Option<bool>) {
        let (end, brought, _) = said_after(name, step);
        (end, brought)
    }

    /// The end of an attempt's outputs step that ended as `step` says, whether its outputs are brought back, and the
    /// lines the attempt said, through a transport whose every call fails to start, the state in a fresh folder of the
    /// test `name`'s own; the call sends a cross build when `step.crossed`.
    fn said_after(name: &str, step: &Step<'_>) -> (End, Option<bool>, Vec<String>) {
        let state = fresh_state(name);
        let log = unwritten_log();
        let globs = ["**/bin/**".to_owned()];
        let cross = gate_router::CrossBuild {
            rid: "win-x64".to_owned(),
            words: vec!["dotnet".to_owned(), "build".to_owned()],
        };
        let mut call = call(&log);
        call.interrupt = None;
        call.root = step.root.to_str().expect("a test worktree's path is Unicode");
        call.state_dir = &state;
        call.outputs = if step.globs { &globs[..] } else { &[] };
        call.cross_build = step.crossed.then_some(&cross);
        call.bins_withheld = step.bins_withheld;
        let transport = nowhere();
        let mut attempt = Attempt::for_tests(&call, &transport);
        // The attempt is past its snapshot, as one that reaches its outputs is, so a fetch is tried.
        attempt.snapshot_taken = Some(std::time::SystemTime::now());
        let sent = outputs::sent(&mut attempt).expect("no interrupt ends the manifest's build");
        let location = location();
        let ran = outputs::Ran {
            location: &location,
            nonce: "0123456789ab",
            status: step.status,
            names_skipped: step.names_skipped,
            cross_not_brought_back: step.cross_not_brought_back,
        };
        let synced = crate::remote::syncback::Synced {
            end: End::Status(step.synced),
            reason: None,
        };
        let end = outputs::ended(&mut attempt, &sent, &ran, synced, step.brought.as_ref());
        (end, attempt.brought_back, attempt.lines.clone())
    }

    #[test]
    fn a_passing_run_whose_cross_build_brought_nothing_back_keeps_its_status_and_brings_nothing_back() {
        let held = outputs_worktree("cross-not-back", true);
        let crossed = Step {
            cross_not_brought_back: true,
            crossed: true,
            ..passed(&held, None, false)
        };
        let (end, brought) = ended_after("cross-not-back", &crossed);
        assert!(matches!(end, End::Status(0)), "the command's own status: {end:?}");
        assert_eq!(brought, Some(false), "the outputs stay not brought back");
        let (end, _) = ended_after("cross-not-back-other", &passed(&held, None, false));
        assert!(matches!(end, End::Status(1)), "any other missing outputs line ends 1: {end:?}");
    }

    #[test]
    fn ended_sets_brought_back_for_each_outputs_end() {
        let held = outputs_worktree("held", true);
        let empty = outputs_worktree("empty", false);
        let unbuilt = Path::new(r"C:\gate-client-tests-no-such-folder");
        let archive = || {
            Some(outputs::Brought::Archive {
                count: 1,
                name: "app-1a2b3c4d-0123456789ab.tar.zst".to_owned(),
                size: None,
            })
        };
        let nothing = || Some(outputs::Brought::Nothing);
        let rows = [
            (
                "no output globs",
                Step {
                    globs: false,
                    ..passed(&held, None, false)
                },
                Some(false),
            ),
            (
                "a failed run",
                Step {
                    status: 3,
                    synced: 3,
                    ..passed(&held, None, false)
                },
                Some(false),
            ),
            (
                "a worktree changed during the run",
                Step {
                    synced: 1,
                    ..passed(&held, nothing(), false)
                },
                Some(false),
            ),
            ("no manifest", passed(unbuilt, nothing(), false), Some(false)),
            ("no outputs line", passed(&held, None, false), Some(false)),
            ("nothing packed for a PC holding outputs", passed(&held, nothing(), false), Some(true)),
            (
                "nothing packed for a PC holding outputs, its bin globs withheld",
                Step {
                    bins_withheld: true,
                    ..passed(&held, nothing(), false)
                },
                Some(false),
            ),
            ("nothing packed for a PC holding none", passed(&empty, nothing(), false), Some(false)),
            ("nothing packed, names skipped", passed(&held, nothing(), true), Some(false)),
            ("an archive whose fetch failed", passed(&held, archive(), false), Some(false)),
        ];
        let log = unwritten_log();
        let call = call(&log);
        let transport = nowhere();

        let unreached = Attempt::for_tests(&call, &transport).brought_back;
        let ended: Vec<(&str, Option<bool>, Option<bool>)> = rows
            .into_iter()
            .enumerate()
            .map(|(index, (end, step, want))| (end, brought_back_after(&format!("brought-back-{index}"), &step), want))
            .collect();

        assert_eq!(unreached, None, "an attempt that never reached the outputs step cannot tell");
        for (end, got, want) in ended {
            assert_eq!(got, want, "{end}");
        }
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
        assert_eq!(
            called.outputs.as_ref().map(|heard| heard.line.as_str()),
            Some(line),
            "the last standard error outputs line is kept"
        );
    }

    #[test]
    fn the_last_standard_error_back_commits_line_alone_is_kept() {
        let mut heard = Heard::default();
        let said = |sha: char| format!("gate: remote back commits: app {}, lib none", sha.to_string().repeat(40));
        heard.record(&said('a'), true, None);
        heard.record(&said('b'), true, None);
        heard.record(&said('c'), false, None);
        heard.record("gate: remote said something else", true, None);

        let called = called_of(CallEnd::Exited(0), Raised::default(), heard).expect("a call that ended");

        assert_eq!(
            called.back_commits.as_deref(),
            Some(said('b').as_str()),
            "the last standard error back commits line is kept, and a standard output one is not"
        );
    }

    #[test]
    fn only_a_standard_error_line_counts_as_a_names_skip() {
        let line = "gate: remote outputs: skipped 1 file(s) with names this PC cannot take: bin/m~10.txt";
        let mut printed = Heard::default();
        printed.record(line, false, None);
        let mut said = Heard::default();
        said.record(line, true, None);

        let printed = called_of(CallEnd::Exited(0), Raised::default(), printed).expect("a call that ended");
        let said = called_of(CallEnd::Exited(0), Raised::default(), said).expect("a call that ended");

        assert!(!printed.names_skipped, "a standard output line is the command's own");
        assert!(said.names_skipped, "the box says it on standard error");
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
                log_cap: LOG_CAP,
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
    fn a_fetch_ends_at_the_stall_window_or_its_transfer_ceiling_from_its_own_start_never_at_the_command_s() {
        let second = Duration::from_secs(1);
        let started = Instant::now();
        let limits = Limits {
            started,
            stall: 10 * second,
            command: 60 * second,
            backstop: 360 * second,
            transfer: 90 * second,
            bound: 30 * second,
            transfer_from: None,
        };
        let begun = started + 40 * second;
        let fetch = Watch::Fetch(limits, None);
        assert_eq!(
            fetch.cut(started + 60 * second, begun, 9 * second, Raised::default(), None),
            None,
            "the command's ceiling, counted from the run's start, does not cut a fetch"
        );
        assert_eq!(
            fetch.cut(begun + 89 * second, begun, 9 * second, Raised::default(), None),
            None,
            "neither yet"
        );
        assert_eq!(
            fetch.cut(started + 50 * second, begun, 10 * second, Raised::default(), None),
            Some(Cut::Silent(10 * second)),
            "silent for the stall window"
        );
        assert_eq!(
            fetch.cut(begun + 90 * second, begun, second, Raised::default(), None),
            Some(Cut::Ceiling(90 * second)),
            "a live fetch at its transfer ceiling, counted from its own start"
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
            Watch::Run(limits).cut(begun + 60 * second, begun, second, Raised::default(), None),
            None,
            "the box counts the command's ceiling, so the run call is not cut at it"
        );
        assert_eq!(
            Watch::Run(limits).cut(started + 360 * second, begun, second, Raised::default(), None),
            None,
            "the backstop does not count the time before the run call"
        );
        assert_eq!(
            Watch::Run(limits).cut(begun + 360 * second, begun, second, Raised::default(), None),
            Some(Cut::Watchdog(Kill::Backstop)),
            "the run call at its backstop, counted from its own start"
        );
        assert_eq!(
            Watch::Setup(limits).cut(started + 60 * second, begun, second, Raised::default(), None),
            None,
            "a setup call is not cut at the command's ceiling"
        );
        assert_eq!(
            Watch::Setup(limits).cut(started + 90 * second, begun, second, Raised::default(), None),
            Some(Cut::Ceiling(90 * second)),
            "a setup call at the transfer ceiling, counted from the attempt's start"
        );
        assert_eq!(
            Watch::Setup(limits).cut(started + second, begun, 10 * second, Raised::default(), None),
            Some(Cut::Watchdog(Kill::Stalled)),
            "a silent setup call stalls"
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
            command: 60 * second,
            backstop: 360 * second,
            transfer: 60 * second,
            bound: 30 * second,
            transfer_from: None,
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
            log_cap: LOG_CAP,
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
        run_call_ended(stdout, stderr, 75, test)
    }

    /// As [`run_call_said`], the call having exited `status`.
    fn run_call_ended(stdout: &str, stderr: &str, status: i32, test: &str) -> (Called, String) {
        let log = crate::test_support::temp_dir().join(format!("gate-client-protocol-{}-{test}.log", std::process::id()));
        let shared = Shared::new();
        let reading = Reading {
            echo: Echo::LogOnly,
            capped: false,
            lines: Some(std::sync::Arc::new(ProtocolLines::new(NONCE))),
            log_cap: LOG_CAP,
        };
        let file = std::fs::File::create(&log).expect("the log can be made");
        let other = file.try_clone().expect("the log can be shared");
        relay(stdout.as_bytes(), Reader::new(false, reading.clone(), Some(other)), &shared);
        relay(stderr.as_bytes(), Reader::new(true, reading, Some(file)), &shared);
        let heard = std::mem::take(&mut *held(&shared.heard));
        let called = called_of(CallEnd::Exited(status), Raised::default(), heard).expect("a call that ended");
        let said = crate::remote::log_texts(&log);
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
        let full_tag = refused_line(NONCE, Refusal::Full);
        let full = gate_wire::slots::full_line(2, 2);
        let cases: [(&str, String, String, Option<FallbackReason>); 11] = [
            ("busy", String::new(), format!("{busy_tag}\n{busy}\n"), Some(FallbackReason::Busy)),
            ("full", String::new(), format!("{full_tag}\n{full}\n"), Some(FallbackReason::Full)),
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

    /// The box's `drain` refusal of the call whose nonce is [`NONCE`], as `boxside::drain::admission_said` says it: the
    /// tag, then the place and barrier lines of `boxside::drain::refused_lines`.
    fn drain_refusal(place: &str, barrier: &str) -> String {
        format!(
            "{}\ngate: drain place {place}\ngate: drain barrier {barrier}\n",
            refused_line(NONCE, Refusal::Drain)
        )
    }

    #[test]
    fn a_drain_tag_is_never_a_fallback_reason() {
        let (called, _) = run_call_said("", &drain_refusal("2", "100 1900"), "drain-tag");
        assert_eq!(called.refusal(), None, "the drain names no fallback reason");
        let drain = Drain {
            place: Some(2),
            raised: 100,
            lapses: 1900,
        };
        assert!(
            matches!(status_end(&called, 0..0), End::Draining(read) if read == drain),
            "a 75 with the tag drains"
        );

        let log = unwritten_log();
        let call = call(&log);
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let outcome = attempt(&call, &transport).outcome(End::Draining(drain));
        assert_eq!((outcome.status, outcome.reason), (75, None), "{outcome:?}");
        assert_eq!(outcome.fallback(), None, "a drain is no fallback");
        assert_eq!(outcome.draining, Some(drain));
        assert!(!log.exists(), "nothing is said");

        let (unplaced, _) = run_call_said("", &drain_refusal("none", "none"), "drain-none");
        assert!(
            matches!(
                status_end(&unplaced, 0..0),
                End::Draining(Drain {
                    place: None,
                    raised: 0,
                    lapses: 0
                })
            ),
            "no place and no barrier"
        );
        let marker = started_line(NONCE);
        let (started, _) = run_call_said("", &format!("{marker}\n{}", drain_refusal("2", "100 1900")), "drain-after-marker");
        assert!(
            matches!(status_end(&started, 0..0), End::Status(75)),
            "a tag after the marker is the command's own line"
        );
    }

    #[test]
    fn the_drain_tag_is_read_before_the_version_refusal() {
        let other = format!("{VERSION}9.9.9\n");
        let (drained, _) = run_call_said(&other, &drain_refusal("1", "100 1900"), "drain-before-version");
        assert!(
            matches!(preflight_ended("builder", r"C:\builder", &drained), Err((None, End::Draining(_)))),
            "a drained builder of another version waits, saying nothing"
        );

        let (passed, _) = run_call_ended(&other, "", 0, "version-without-drain");
        assert!(
            matches!(
                preflight_ended("builder", r"C:\builder", &passed),
                Err((Some(_), End::Refused(FallbackReason::Version)))
            ),
            "without the tag the version is refused"
        );
    }

    #[test]
    fn a_preflight_refused_full_ends_refused_full_so_the_run_goes_to_the_next_builder() {
        let version = format!("{VERSION}{}\n", gate_wire::build_version());
        let full = format!("{}\n{}\n", refused_line(NONCE, Refusal::Full), gate_wire::slots::full_line(1, 1));
        let (refused, _) = run_call_ended(&version, &full, 75, "preflight-full");
        assert!(
            matches!(
                preflight_ended("builder", r"C:\builder", &refused),
                Err((None, End::Refused(FallbackReason::Full)))
            ),
            "a full builder's preflight is refused for its slots"
        );

        let (failed, _) = run_call_ended(&version, &full, 1, "preflight-full-status-1");
        assert!(
            matches!(preflight_ended("builder", r"C:\builder", &failed), Err((Some(_), End::Status(1)))),
            "the tag counts only with the refusal's status"
        );
    }

    #[test]
    fn the_drain_place_and_barrier_lines_are_kept_out_of_the_log_and_the_lines() {
        let (drained, said) = run_call_said("", &format!("{}after\n", drain_refusal("3", "100 1900")), "drain-hidden");
        assert_eq!(said, "after\n", "neither the tag nor its lines are passed on");
        assert_eq!(drained.lines, ["after"], "nor kept");

        let place = "gate: drain place 3\n";
        let (untagged, said) = run_call_said("", place, "drain-untagged");
        assert_eq!(said, place, "a place line with no tag before it is a line like any other");
        assert_eq!(untagged.lines, ["gate: drain place 3"]);
    }

    #[test]
    fn the_adjusted_line_is_read_from_this_run_s_line_on_standard_error_alone_with_finite_seconds() {
        let huge = format!("{}.000", "9".repeat(400));
        let cases: [(&str, String, String, Option<f64>); 9] = [
            ("adjusted", String::new(), format!("{}\n", adjusted_line(NONCE, 72.25)), Some(72.25)),
            (
                "the-last-wins",
                String::new(),
                format!("{}\n{}\n", adjusted_line(NONCE, 1.0), adjusted_line(NONCE, 2.5)),
                Some(2.5),
            ),
            ("stdout", format!("{}\n", adjusted_line(NONCE, 1.0)), String::new(), None),
            (
                "other-nonce",
                String::new(),
                format!("{}\n", adjusted_line("fedcba9876543210fedcba9876543210", 1.0)),
                None,
            ),
            ("no-nonce", String::new(), "gate-remote-adjusted: 1.000\n".to_owned(), None),
            ("two-decimals", String::new(), format!("gate-remote-adjusted: {NONCE} 1.00\n"), None),
            ("negative", String::new(), format!("gate-remote-adjusted: {NONCE} -1.000\n"), None),
            ("infinite", String::new(), format!("gate-remote-adjusted: {NONCE} {huge}\n"), None),
            ("padded", String::new(), format!("{} \n", adjusted_line(NONCE, 1.0)), None),
        ];
        for (name, stdout, stderr, adjusted) in cases {
            let (called, said) = run_call_said(&stdout, &stderr, name);
            assert_eq!(called.adjusted, adjusted, "{name}");
            if adjusted.is_some() {
                assert_eq!(said, "", "this run's adjusted line is not passed on: {name}");
                assert!(called.lines.is_empty(), "nor kept: {name}");
            } else {
                assert_eq!(said, format!("{stdout}{stderr}"), "any other line is passed on: {name}");
            }
        }
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
        let log = crate::test_support::temp_dir().join(format!("gate-client-long-line-{}.log", std::process::id()));

        let shared = read_through(input.as_bytes(), false, Some(&log));

        let written = crate::remote::log_texts(&log);
        std::fs::remove_file(&log).expect("the log can be removed");
        assert!(written == input, "the log holds every byte, the character whole");
        assert!(!shared.progress.too_long(), "an uncapped reading is never cut");
        let heard = std::mem::take(&mut *held(&shared.heard));
        assert_eq!(heard.first_error.as_deref().map(str::len), Some(LINE_CAP), "the first line's head");
        assert_eq!(heard.last_error.as_deref(), Some("next"));
        let lines = heard.kept.into_lines();
        assert_eq!(lines.iter().map(String::len).collect::<Vec<_>>(), [LINE_CAP, 4]);
    }

    fn unwritten_log() -> std::path::PathBuf {
        crate::test_support::temp_dir().join("gate-client-tests-no-such-folder").join("out.log")
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
            objects: PathBuf::from(r"C:\app\.git\objects"),
        };
        let location = Location::under(
            &Profile::new("builder", BuilderOs::Windows, r"C:\builder"),
            "app",
            "app-1a2b3c4d",
            "0123456789ab",
        );

        assert!(matches!(attempt.preflight(&location, &[]), Err(End::Interrupted)), "the preflight");
        let repos = attempt.repos();
        let push = Push {
            at: 0,
            repo: &repos[0],
            snapshot: &snapshot,
            created: false,
        };
        let watch = Watch::Setup(attempt.limits_for("push"));
        assert!(matches!(attempt.push(&push, watch), Err(End::Interrupted)), "the push");
        assert!(
            matches!(attempt.run_call(&location, &payload::input("e30=", None), NONCE), Err(End::Interrupted)),
            "the run call"
        );
        let outcome = attempt.outcome(End::Interrupted);
        assert_eq!(outcome.status, i64::from(CONTROL_C_EXIT.cast_signed()), "{outcome:?}");
        assert!(outcome.lines.is_empty(), "an interrupt says no failure line: {outcome:?}");
        assert_eq!(outcome.fallback(), None, "an interrupt is no fallback");
        assert!(!log.exists(), "nothing is said");
    }

    #[test]
    fn a_call_dropped_before_the_banner_through_a_builder_stall_is_made_again_until_it_is_answered() {
        let log = unwritten_log();
        let mut given = call(&log);
        given.interrupt = None;
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let mut attempt = attempt(&given, &transport);
        attempt.pause = Duration::ZERO;
        let mut made = 0;

        let retried = attempt.with_retry(SSH_FAILED, |_| {
            made += 1;
            Ok(if made < 3 {
                ended_with(SSH_FAILED, "Connection timed out during banner exchange")
            } else {
                ended_with(0, "the command's line")
            })
        });

        let (called, again) = retried.expect("a call that ends without an interrupt");
        assert_eq!(made, 3, "the builder's ssh answered the third call");
        assert_eq!(called.status, 0, "the answered call is the one returned");
        assert_eq!(called.first_error.as_deref(), Some("the command's line"));
        assert!(!again, "the call that counts did not fail before the banner");
    }

    #[test]
    fn a_call_that_never_passes_the_banner_is_made_four_times_and_flagged() {
        let log = crate::test_support::temp_dir().join(format!("gate-client-banner-never-{}.log", std::process::id()));
        let mut given = call(&log);
        given.interrupt = None;
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let mut attempt = attempt(&given, &transport);
        attempt.pause = Duration::ZERO;
        let mut made = 0;

        let retried = attempt.with_retry(SSH_FAILED, |_| {
            made += 1;
            Ok(ended_with(SSH_FAILED, "Connection timed out during banner exchange"))
        });

        let (called, again) = retried.expect("a call that ends without an interrupt");
        assert_eq!(made, 1 + PRE_BANNER_RETRIES, "the call and its retries");
        assert!(again, "the last call failed before the banner too");
        assert_eq!(called.status, SSH_FAILED);
        let said = crate::remote::log_texts(&log);
        let want = [
            "gate: builder did not answer the ssh banner; trying again (1 of 3)",
            "gate: builder did not answer the ssh banner; trying again (2 of 3)",
            "gate: builder did not answer the ssh banner; trying again (3 of 3)",
        ];
        assert_eq!(said.lines().collect::<Vec<_>>(), want, "{said}");
        std::fs::remove_file(&log).expect("the log can be removed");
    }

    #[test]
    fn a_sync_back_git_call_dropped_before_the_banner_is_made_again_until_its_retries_run_out() {
        // The local transport runs git against a bare repository path, never through the ssh stand-in, so the only way
        // to reach the sync-back's retry is the decision itself, as a call that failed before the banner.
        let log = crate::test_support::temp_dir().join(format!("gate-client-sync-back-retry-{}.log", std::process::id()));
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
        assert_eq!(made, 1 + PRE_BANNER_RETRIES, "the git call and its retries");
        assert!(again, "the last call failed before the banner too");
        assert_eq!(called.status, GIT_SSH_FAILED);
        let said = std::fs::read_to_string(&log).expect("the retry lines were written");
        let retries = said.matches("gate: builder did not answer the ssh banner; trying again (").count();
        assert_eq!(retries, PRE_BANNER_RETRIES, "{said}");
        assert!(said.contains("(3 of 3)"), "the last retry is said: {said}");
        std::fs::remove_file(&log).expect("the log can be removed");
    }

    /// A fetch that failed as if before the banner but whose job wrote more than 1 MiB is not made again, so a builder
    /// cannot buy a second fetch of a fresh count: ssh's own failure writes one line.
    #[test]
    fn a_call_whose_job_wrote_more_than_1_mib_is_not_made_again_after_a_failure_before_the_banner() {
        const MIB: u64 = 1 << 20;
        let log = crate::test_support::temp_dir().join(format!("gate-client-retry-written-{}.log", std::process::id()));
        let mut given = call(&log);
        given.interrupt = None;
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let mut attempt = attempt(&given, &transport);
        attempt.pause = Duration::ZERO;
        for (written, times) in [(Some(MIB + 1), 1), (Some(MIB), 1 + PRE_BANNER_RETRIES), (None, 1 + PRE_BANNER_RETRIES)] {
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
    fn refused_line_names_desktop() {
        assert_eq!(refused_line(NONCE, Refusal::Desktop), format!("gate-remote-refused: {NONCE} desktop"));
    }

    #[test]
    fn a_desktop_refusal_ends_the_run_refused_for_the_desktop() {
        let tag = refused_line(NONCE, Refusal::Desktop);
        let (called, said) = run_call_said("", &format!("{tag}\n"), "desktop-refusal");

        assert_eq!(called.refusal(), Some(FallbackReason::Desktop), "{said:?}");
        assert!(matches!(status_end(&called, 0..0), End::Refused(FallbackReason::Desktop)), "{said:?}");
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
            Some("gate: not routed: builder runs gate 0.2.0, this PC 0.1.0; run gate push".to_owned())
        );
        assert_eq!(their_version(&lines(&["gate: remote boot x"])), None);
        assert_eq!(
            version_refusal("builder", None, "0.1.0"),
            Some("gate: not routed: builder runs gate unknown, this PC 0.1.0; run gate push".to_owned())
        );
    }

    #[test]
    fn a_box_that_refuses_its_own_install_is_not_routed_with_its_why() {
        let not_routed = |why: &str| {
            format!("gate: not routed: builder's gate is not installed where only administrators can change it ({why}); run gate check --builder")
        };
        let why = r"C:\gate\gate.exe is owned by S-1-5-21-1, not SYSTEM";
        let refusal = format!("{INSTALL_UNTRUSTED}{why}");
        let said = [r"gate: remote created D:\gate-remote\app.git", refusal.as_str()];

        assert_eq!(install_refusal("builder", 1, said), Some(not_routed(why)));
        assert_eq!(install_refusal("builder", 2, said), None, "only status 1 is the refusal");
        assert_eq!(
            install_refusal("builder", 1, ["gate: remote failed"]),
            None,
            "another failure keeps its line"
        );
        let control = format!("{INSTALL_UNTRUSTED}a\u{1b}b");
        assert_eq!(
            install_refusal("builder", 1, [control.as_str()]),
            Some(not_routed(r"a\u{1b}b")),
            "the box's why is escaped"
        );
    }

    #[test]
    fn an_install_refusal_counts_only_before_the_start_marker() {
        let refusal = install_untrusted_line("the helper is owned by S-1-5-21-1");
        let (before, _) = run_call_ended("", &format!("{refusal}\n"), 1, "install-before");
        let (after, _) = run_call_ended("", &format!("{}\n{refusal}\n", started_line(NONCE)), 1, "install-after");

        assert!(install_refused("builder", &before).is_some(), "a refusal before the start falls back");
        assert_eq!(
            install_refused("builder", &after),
            None,
            "a command's own line after the start is its output"
        );
    }

    /// A preflight whose box refused its work root, unsafe or busy, at status 1 and before any start marker is not
    /// routed, saying the box's why escaped; any other status, or a line after the marker, is no root refusal.
    #[test]
    fn a_box_that_refuses_its_work_root_is_not_routed_with_its_why() {
        let unsafe_line = root_unsafe_line(r"D:\gate-remote lets S-1-5-11 add files to it", WORK_ROOT_STEP);
        let busy_line = root_busy_line(r"D:\gate-remote is open in another process; retry when it is closed");
        let (unsafe_root, _) = run_call_ended("", &format!("{unsafe_line}\n"), 1, "root-unsafe");
        let (busy_root, _) = run_call_ended("", &format!("{busy_line}\n"), 1, "root-busy");
        let (other_status, _) = run_call_ended("", &format!("{unsafe_line}\n"), 2, "root-other-status");
        let (after, _) = run_call_ended("", &format!("{}\n{unsafe_line}\n", started_line(NONCE)), 1, "root-after");
        let (control, _) = run_call_ended("", &format!("{ROOT_UNSAFE}a\u{1b}b\n"), 1, "root-control");

        assert_eq!(
            root_refused("builder", &unsafe_root),
            Some(
                "gate: not routed: builder's work root is not locked down: D:\\gate-remote lets S-1-5-11 add files to it; \
                 run the installer's \"Gate work root\" step (install-toolchains.ps1 -Only \"Gate work root\")"
                    .to_owned()
            )
        );
        assert_eq!(
            root_refused("builder", &busy_root),
            Some(r"gate: not routed: builder's work root is busy: D:\gate-remote is open in another process; retry when it is closed".to_owned())
        );
        assert_eq!(root_refused("builder", &other_status), None, "only status 1 is the refusal");
        assert_eq!(
            root_refused("builder", &after),
            None,
            "a command's own line after the start is its output"
        );
        assert_eq!(
            root_refused("builder", &control),
            Some(r"gate: not routed: builder's work root is not locked down: a\u{1b}b".to_owned()),
            "the box's why is escaped"
        );
    }

    /// A preflight whose box said both its install refusal and a root refusal falls back for its install, read first.
    #[test]
    fn the_install_refusal_is_read_before_the_root_refusal() {
        let install = install_untrusted_line("the helper is owned by S-1-5-21-1");
        let root = root_unsafe_line(r"D:\gate-remote is a reparse point", WORK_ROOT_STEP);
        let (both, _) = run_call_ended("", &format!("{root}\n{install}\n"), 1, "root-and-install");
        let (root_alone, _) = run_call_ended("", &format!("{root}\n"), 1, "root-alone");

        assert_eq!(
            preflight_refusal("builder", &both).map(|(_, reason)| reason),
            Some(FallbackReason::Install)
        );
        assert_eq!(
            preflight_refusal("builder", &root_alone).map(|(_, reason)| reason),
            Some(FallbackReason::Root)
        );
    }

    /// On the run call, the plain line counts only as the call's one line: the box says it so when it refuses before
    /// reading the request, and relayed checkout, restore or hook output before the marker cannot forge it.
    #[test]
    fn relayed_output_before_the_run_s_marker_is_not_an_install_refusal() {
        let refusal = install_untrusted_line("the helper is owned by S-1-5-21-1");
        let (alone, _) = run_call_ended("", &format!("{refusal}\n"), 1, "install-alone");
        let (relayed, _) = run_call_ended("Restored app.csproj\n", &format!("{refusal}\n"), 1, "install-relayed");
        let (relayed_last, _) = run_call_ended("", &format!("{refusal}\ngate: remote checkout failed\n"), 1, "install-relayed-last");

        assert!(run_install_refused("builder", &alone).is_some(), "the tripwire's one line falls back");
        assert_eq!(run_install_refused("builder", &relayed), None, "a line with other output is relayed");
        assert_eq!(run_install_refused("builder", &relayed_last), None, "a line the box followed is relayed");
        assert!(
            install_refused("builder", &relayed).is_some(),
            "a preflight or inputs call reads any of its lines"
        );
    }

    /// The box's refusal tag for its install, bound to the run's nonce, is the run's `install` refusal at its 75, and
    /// the client points at `gate check --builder` just as it does for the plain refusal.
    #[test]
    fn the_run_s_install_tag_is_an_install_refusal() {
        let tag = refused_line(NONCE, Refusal::Install);
        assert_eq!(tag, format!("gate-remote-refused: {NONCE} install"));
        let why = "the helper is owned by S-1-5-21-1";
        let line = install_untrusted_line(why);
        let (called, _) = run_call_said("", &format!("{tag}\n{line}\n"), "install-tag");

        assert_eq!(called.refusal(), Some(FallbackReason::Install));
        assert!(matches!(status_end(&called, 0..0), End::Refused(FallbackReason::Install)));
        assert_eq!(
            install_tag_refusal("builder", called.lines.iter().chain(&called.last_error).map(String::as_str)),
            Some(format!(
                "gate: not routed: builder's gate is not installed where only administrators can change it ({why}); run gate check --builder"
            ))
        );
    }

    #[test]
    fn a_builder_s_why_is_cut_to_the_failure_bytes() {
        let long = format!("{INSTALL_UNTRUSTED}{}", "x".repeat(git::FAILURE_BYTES + 10));
        assert_eq!(install_why(1, [long.as_str()]).map(str::len), Some(git::FAILURE_BYTES));
    }

    #[test]
    fn a_ctrl_c_by_the_time_the_run_call_refused_its_install_ends_the_run_interrupted() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let line = install_untrusted_line("the helper is owned by S-1-5-21-1");

        let ended = attempt.run_call_made(&location(), Limits::for_tests(), |_| Ok(ended_with(1, &line)));

        assert!(
            matches!(ended, Err(End::Interrupted)),
            "an interrupt, not the install fallback: {ended:?}"
        );
        assert!(
            !stopped(&attempt),
            "a refused install started nothing, so nothing is stopped: {:?}",
            attempt.lines
        );
    }

    /// The warm-up file hash the warm cases send, and another.
    const WARM_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_HASH: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    #[test]
    fn the_warm_line_is_taken_only_right_after_the_version_line() {
        let version = "gate: remote version 0.1.0";
        let boot = "gate: remote boot 2026-10-10T08:00:00.0000000Z";
        let hash = format!("gate: remote warm {WARM_HASH}");
        let upper = format!("gate: remote warm {}", WARM_HASH.to_uppercase());
        let short = format!("gate: remote warm {}", &WARM_HASH[..63]);
        let long = format!("{hash}0");
        let padded = format!("{hash} ");
        let cr = format!("{hash}\r");
        let warm = || Warm::Hash(WARM_HASH.to_owned());
        let cases: [(&str, Vec<String>, Warm); 15] = [
            ("none", lines(&[version, "gate: remote warm none"]), Warm::NoHash),
            (
                "hash",
                lines(&["gate: remote created D:\\gate-remote\\app.git", version, &hash, boot]),
                warm(),
            ),
            ("upper-case", lines(&[version, &upper]), Warm::Unseen),
            ("63-digits", lines(&[version, &short]), Warm::Unseen),
            ("65-digits", lines(&[version, &long]), Warm::Unseen),
            ("trailing-space", lines(&[version, &padded]), Warm::Unseen),
            ("a-carriage-return-left-in-the-line", lines(&[version, &cr]), Warm::Unseen),
            ("after-boot", lines(&[version, boot, &hash]), Warm::Unseen),
            ("before-the-version", lines(&[&hash, version]), Warm::Unseen),
            (
                "two-versions-the-last-counts",
                lines(&[version, "gate: remote warm none", version, &hash]),
                warm(),
            ),
            ("two-versions-none-after-the-last", lines(&[version, &hash, version]), Warm::Unseen),
            ("no-version", lines(&[&hash]), Warm::Unseen),
            (
                "a-warm-up-line",
                lines(&[version, "gate: remote warm-up stopped after its first command"]),
                Warm::Unseen,
            ),
            ("empty-after-the-prefix", lines(&[version, "gate: remote warm "]), Warm::Unseen),
            ("none-padded", lines(&[version, "gate: remote warm none "]), Warm::Unseen),
        ];
        for (name, preflight, expected) in cases {
            assert_eq!(warm_of(&preflight), expected, "{name}");
        }
    }

    #[test]
    fn the_new_work_tree_line_marks_the_run_cold() {
        let version = "gate: remote version 0.1.0";
        let boot = "gate: remote boot 2026-10-10T08:00:00.0000000Z";
        let new_tree = "gate: remote new work tree";
        let hash = format!("gate: remote warm {WARM_HASH}");
        let cases: [(&str, Vec<String>); 4] = [
            ("after-warm-none", lines(&[version, "gate: remote warm none", new_tree, boot])),
            (
                "after-a-created-line",
                lines(&[
                    "gate: remote created D:\\gate-remote\\app.git",
                    version,
                    "gate: remote warm none",
                    new_tree,
                ]),
            ),
            ("after-a-warm-hash", lines(&[version, &hash, new_tree])),
            (
                "two-versions-the-last-counts",
                lines(&[version, "gate: remote warm none", version, "gate: remote warm none", new_tree]),
            ),
        ];
        for (name, preflight) in cases {
            assert!(new_work_tree(&preflight), "{name}");
        }
    }

    #[test]
    fn no_new_work_tree_line_is_not_cold() {
        let version = "gate: remote version 0.1.0";
        let warm = "gate: remote warm none";
        let new_tree = "gate: remote new work tree";
        let cases: [(&str, Vec<String>); 7] = [
            (
                "an-older-box-that-never-says-it",
                lines(&[version, warm, "gate: remote boot 2026-10-10T08:00:00.0000000Z"]),
            ),
            ("no-version", lines(&[warm, new_tree])),
            ("no-warm-line-before-it", lines(&[version, new_tree])),
            ("before-the-version", lines(&[new_tree, version, warm])),
            ("after-the-boot-line", lines(&[version, warm, "gate: remote boot x", new_tree])),
            ("two-versions-none-after-the-last", lines(&[version, warm, new_tree, version, warm])),
            ("padded", lines(&[version, warm, "gate: remote new work tree "])),
        ];
        for (name, preflight) in cases {
            assert!(!new_work_tree(&preflight), "{name}");
        }
    }

    #[test]
    fn the_cold_note_names_gate_warm_only_for_a_repo_with_a_warm_up() {
        assert_eq!(
            cold_line(60, 120, true),
            "gate: remote cold: this worktree's work folder on the builder is new or was cleared; \
             the ceiling is raised from 60 s to 120 s; \
             prewarm the next new worktree with gate warm"
        );
        assert_eq!(
            cold_line(3600, 5400, false),
            "gate: remote cold: this worktree's work folder on the builder is new or was cleared; \
             the ceiling is raised from 3600 s to 5400 s; \
             a .gate/warmup.toml lets gate warm prewarm the next new worktree"
        );
    }

    #[test]
    fn a_cold_run_call_watches_for_no_hang_hint() {
        let log = Path::new(r"C:\no-such-log.log");
        let call = RemoteCall {
            usual: Some(crate::durations::Usual { p90: 30.0, family: false }),
            ..call(log)
        };
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        assert!(attempt.run_hang(true).is_some(), "a warm run call compares with its usual p90");
        assert!(attempt.run_hang(false).is_none(), "no other call says the hint");
        attempt.raised = Some(120);
        assert!(attempt.run_hang(true).is_none(), "a cold run call compares with none");
    }

    /// The pull's hang hint counts the run from the box's start marker, as the run call's does; only
    /// `--test-remote-hint-from-pull` moves it to the pull's start, and leaves the run call none.
    #[test]
    fn the_pull_s_hang_hint_counts_from_the_start_marker_unless_the_seam_says_the_pull_s_start() {
        let log = Path::new(r"C:\no-such-log.log");
        let call = RemoteCall {
            usual: Some(crate::durations::Usual { p90: 30.0, family: false }),
            ..call(log)
        };
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let marker = Instant::now().checked_sub(Duration::from_secs(10)).expect("the clock is past 10 s");
        let pull_started = Instant::now();
        attempt.command_started = Some(marker);
        attempt.pulling = Some(super::Pull {
            bringing: outputs::Bringing::Outputs,
            total: None,
            started: pull_started,
        });
        let counted = |attempt: &Attempt<'_, '_>| match attempt.run_hang(false).map(|hang| hang.phase) {
            Some(super::HangPhase::Pulling { counted_from, .. }) => Some(counted_from),
            _ => None,
        };

        assert_eq!(counted(&attempt), Some(marker), "without the seam: from the start marker");
        assert!(attempt.run_hang(true).is_some(), "without the seam the run call watches for it");
        attempt.hint_from_pull = true;
        assert_eq!(counted(&attempt), Some(pull_started), "with the seam: from the pull's start");
        assert!(attempt.run_hang(true).is_none(), "with the seam the run call watches for none");
    }

    /// The pull's hang hint names its phase's time from the pull's start, a retry's pause in it, never from the start of
    /// the call watched, which a retry makes anew: with the pull begun 8 s before a call that starts now, the phase is
    /// at least 8 s and longer than the call has run.
    #[test]
    fn the_pull_s_hang_hint_counts_its_phase_from_the_pull_s_start_not_the_call_s() {
        let log = Path::new(r"C:\no-such-log.log");
        let call = RemoteCall {
            usual: Some(crate::durations::Usual { p90: 30.0, family: false }),
            ..call(log)
        };
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let paused = Duration::from_secs(8);
        let pull_started = Instant::now()
            .checked_sub(paused)
            .expect("this PC's clock reads at least 8 s, so the pull can start 8 s back");
        attempt.command_started = Some(pull_started);
        attempt.pulling = Some(super::Pull {
            bringing: outputs::Bringing::Outputs,
            total: None,
            started: pull_started,
        });
        let phase = attempt.run_hang(false).expect("the pull watches for the hint").phase;
        let call_started = super::Progress::new();

        // Read before the phase's time, so the call's own time can only be the shorter however the two reads are spaced.
        let call_ran = call_started.begun.elapsed();
        let (_, named, during) = phase.times(&call_started).expect("the pull's phase has its times");

        assert_eq!(named, "pulling outputs from");
        assert!(during >= paused, "the phase counts from the pull's start, the pause in it: {during:?}");
        assert!(
            call_ran < during,
            "not from the call's start: the call ran {call_ran:?}, the phase {during:?}"
        );
    }

    /// The hang hint of a failed run's pull of its test reports names them where a passing run's names its outputs.
    #[test]
    fn the_hang_hint_of_a_reports_pull_names_the_failed_run_s_test_reports() {
        let log = Path::new(r"C:\no-such-log.log");
        let call = RemoteCall {
            usual: Some(crate::durations::Usual { p90: 30.0, family: false }),
            ..call(log)
        };
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let started = Instant::now();
        attempt.command_started = Some(started);
        attempt.pulling = Some(super::Pull {
            bringing: outputs::Bringing::Reports(outputs::Narrowed::TestReports),
            total: None,
            started,
        });
        let phase = attempt.run_hang(false).expect("the pull watches for the hint").phase;

        let (_, named, _) = phase.times(&super::Progress::new()).expect("the pull's phase has its times");

        assert_eq!(named, "pulling the failed run's test reports from");
    }

    /// How the warm-up of [`WARM_HASH`] ended whose run call said `stdout` and `stderr` and exited `status`, as the run
    /// call ends one that neither a kill nor an interrupt ended.
    fn warm_said(stdout: &str, stderr: &str, status: i32, test: &str) -> WarmEnd {
        let (called, _) = run_call_ended(stdout, stderr, status, test);
        warm_end(WARM_HASH, called.started, &status_end(&called, 0..0), called.last_error.as_deref())
    }

    fn failed(failure: WarmFailure, reason: &str) -> WarmEnd {
        WarmEnd::Failed {
            failure,
            reason: reason.to_owned(),
        }
    }

    const UNCONFIRMED: &str = "the box did not confirm the warm hash";

    #[test]
    fn a_warm_run_is_warm_only_on_exit_0_the_marker_and_the_exact_last_line() {
        let marker = started_line(NONCE);
        let line = format!("gate: remote warm {WARM_HASH}");
        let other = format!("gate: remote warm {OTHER_HASH}");
        let adjusted = adjusted_line(NONCE, 3.5);
        let unconfirmed = failed(WarmFailure::Commands, UNCONFIRMED);
        let not_begun = failed(WarmFailure::Refused, "the builder ended the warm-up before its commands with status 0");
        let cases: [(&str, String, String, WarmEnd); 10] = [
            ("warm", String::new(), format!("{marker}\nbuilt\n{line}\n"), WarmEnd::Warm),
            ("warm-crlf", String::new(), format!("{marker}\r\n{line}\r\n"), WarmEnd::Warm),
            (
                "a-later-heartbeat",
                String::new(),
                format!("{marker}\n{line}\ngate-remote-heartbeat: 12\n"),
                WarmEnd::Warm,
            ),
            (
                "a-later-adjusted-line",
                String::new(),
                format!("{marker}\n{line}\n{adjusted}\n"),
                WarmEnd::Warm,
            ),
            ("no-marker", String::new(), format!("{line}\n"), not_begun),
            ("wrong-hash", String::new(), format!("{marker}\n{other}\n"), unconfirmed.clone()),
            ("not-last", String::new(), format!("{marker}\n{line}\nlater\n"), unconfirmed.clone()),
            ("on-stdout", format!("{line}\n"), format!("{marker}\n"), unconfirmed.clone()),
            ("padded", String::new(), format!("{marker}\n{line} \n"), unconfirmed.clone()),
            ("no-line", String::new(), format!("{marker}\n"), unconfirmed),
        ];
        for (name, stdout, stderr, expected) in cases {
            assert_eq!(warm_said(&stdout, &stderr, 0, &format!("warm-{name}")), expected, "{name}");
        }
    }

    #[test]
    fn a_warm_run_s_refusals_and_failures_are_classed_by_the_marker() {
        let marker = started_line(NONCE);
        let tag = refused_line(NONCE, Refusal::Busy);
        let busy = busy_line("app-1a2b3c4d");
        let refused = |reason: &str| failed(WarmFailure::Refused, reason);
        let commands = |reason: &str| failed(WarmFailure::Commands, reason);
        assert_eq!(
            warm_said("", &format!("{tag}\n{busy}\n"), 75, "warm-refused"),
            refused("the builder refused: busy"),
            "a tag and 75 before the marker"
        );
        for status in [124, 2, 127, 75] {
            assert_eq!(
                warm_said("", &format!("{marker}\n{tag}\n{busy}\n"), status, &format!("warm-status-{status}")),
                commands(&format!("command stopped with status {status}")),
                "status {status} after the marker"
            );
        }
        let lost = "the connection to the builder was lost";
        assert_eq!(warm_said("", &format!("{marker}\n"), 255, "warm-lost-after"), commands(lost));
        assert_eq!(warm_said("", "", 255, "warm-lost-before"), refused(lost));
        let killed = "the warm-up was killed: it ran out of its budget or went silent";
        assert_eq!(warm_end(WARM_HASH, true, &End::Killed, None), commands(killed));
        assert_eq!(warm_end(WARM_HASH, false, &End::Killed, None), refused(killed));
        for started in [false, true] {
            assert_eq!(warm_end(WARM_HASH, started, &End::Interrupted, None), WarmEnd::Deleted, "{started}");
        }
        assert_eq!(
            warm_end(WARM_HASH, false, &End::NotRouted("gate: not routed: sibling lib x".to_owned()), None),
            refused("the client could not send the warm-up")
        );

        let log = unwritten_log();
        let warm_up = [vec!["./tool.exe".to_owned()]];
        let warm_call = RemoteCall {
            warmth: Warmth::WarmUp {
                commands: &warm_up,
                hash: WARM_HASH,
            },
            ..call(&log)
        };
        let transport = nowhere();
        let mut confirmed_none = attempt(&warm_call, &transport);
        assert!(confirmed_none.warm_ran(&ran(End::Status(0), true, None)), "a warm-up's end is kept");
        let outcome = confirmed_none.outcome(End::Status(0));
        assert_eq!(
            (outcome.status, outcome.warm),
            (1, Some(commands(UNCONFIRMED))),
            "a 0 the box did not confirm is no pass"
        );
        let refused_early = attempt(&warm_call, &transport).outcome(End::Refused(FallbackReason::Version));
        assert_eq!(
            refused_early.warm,
            Some(refused("the builder refused: version")),
            "a refusal before the run call"
        );
        let interrupted = attempt(&warm_call, &transport).outcome(End::Interrupted);
        assert_eq!(interrupted.warm, Some(WarmEnd::Deleted));
        let plain = call(&log);
        assert_eq!(attempt(&plain, &transport).outcome(End::Status(0)).warm, None, "a run is no warm-up");
    }

    /// A run call's end `end`, the box having said the start marker when `started`, its last error line `last_error`.
    fn ran(end: End, started: bool, last_error: Option<String>) -> RunEnd {
        RunEnd {
            end,
            outputs: None,
            back_commits: None,
            names_skipped: false,
            cross_not_brought_back: false,
            started,
            last_error,
        }
    }

    #[test]
    fn a_warm_run_syncs_nothing_back() {
        let log = unwritten_log();
        let warm_up = [vec!["./tool.exe".to_owned()]];
        let warm_call = RemoteCall {
            warmth: Warmth::WarmUp {
                commands: &warm_up,
                hash: WARM_HASH,
            },
            interrupt: None,
            ..call(&log)
        };
        let transport = nowhere();
        let mut attempt_of_warm_up = attempt(&warm_call, &transport);
        let warm = ran(End::Status(0), true, Some(format!("gate: remote warm {WARM_HASH}")));

        assert!(attempt_of_warm_up.warm_ran(&warm), "a warm-up's end is kept");
        let ended = attempt_of_warm_up.warm_ended(warm, &location(), None);

        assert!(matches!(ended, End::Status(0)), "{ended:?}");
        assert_eq!(attempt_of_warm_up.warm, Some(WarmEnd::Warm));
        assert_eq!(attempt_of_warm_up.brought_back, None, "no outputs step");
        assert_eq!(
            (attempt_of_warm_up.split.sync_back, attempt_of_warm_up.split.outputs),
            (None, None),
            "no sync-back and no outputs fetch"
        );
        let plain = RemoteCall {
            interrupt: None,
            ..call(&log)
        };
        let mut run = attempt(&plain, &transport);
        assert!(!run.warm_ran(&ran(End::Status(0), true, None)), "a run's end goes on to the sync-back");
        assert_eq!(run.warm, None);
    }

    /// A call of the work-folder key under `state`, logging to `log`, whose interrupt is `interrupt`, which only the
    /// test raises: [`call`] raises its own.
    fn key_call<'a>(log: &'a Path, state: &'a StateRoot, interrupt: Option<Interrupt>) -> RemoteCall<'a> {
        RemoteCall {
            cross_build: None,
            builder: &crate::test_support::HOST,
            key: "app-1a2b3c4d",
            session: "0123456789abcdef0123456789abcdef",
            root: r"C:\no-such-worktree",
            root_id: gate_router::FolderId { volume: 0, file: [0; 16] },
            leaf: "app",
            siblings: Vec::new(),
            repo: "app",
            subfolder: "",
            words: &[],
            outputs: &[],
            outputs_withheld: false,
            bins_withheld: false,
            failure_outputs: &[],
            dropped_no_build: false,
            send: None,
            desktop: false,
            inputs: &[],
            log,
            timeout: 60,
            usual: None,
            stall: 60,
            tail: 0,
            no_markers: false,
            bring_tracked: &[],
            env: &[],
            interrupt,
            state,
            state_dir: Path::new(r"C:\gate-client-tests-state"),
            warmth: Warmth::Unasked,
        }
    }

    /// How a wait for the key ended, as a word the assertions compare; the line of one that failed.
    fn hold_of(hold: Result<KeyHold, String>) -> String {
        match hold {
            Ok(KeyHold::Held(_)) => "held".to_owned(),
            Ok(KeyHold::Busy) => "busy".to_owned(),
            Ok(KeyHold::Interrupted) => "interrupted".to_owned(),
            Err(line) => line,
        }
    }

    #[test]
    fn a_second_holder_of_the_key_waits_and_takes_it_only_once_the_first_lets_go() {
        let state = StateRoot::new(&format!(r"C:\gate-client-tests-key-wait-{}", std::process::id()), None);
        let log = unwritten_log();
        let quiet = key_call(&log, &state, None);
        let interrupt = Interrupt::install().expect("the console handler is installed");
        let interruptible = key_call(&log, &state, Some(interrupt));
        let first = hold_key(&quiet).expect("the key is free");
        thread::scope(|scope| {
            let second = scope.spawn(|| hold_of(hold_key_waiting(&quiet, Duration::from_millis(250))));
            assert_eq!(second.join().expect("the second holder ends"), "busy", "it waits out its wait, not held");
            // The wait is far longer than any test: it ends on the interrupt, raised while it waits.
            let stopped = scope.spawn(|| hold_of(hold_key_waiting(&interruptible, LONG)));
            interrupt.raise();
            assert_eq!(stopped.join().expect("the interrupted holder ends"), "interrupted");
            let waiter = scope.spawn(|| hold_of(hold_key_waiting(&quiet, LONG)));
            drop(first);
            assert_eq!(waiter.join().expect("the waiter ends"), "held", "it takes the key once the first lets go");
        });
    }

    #[test]
    fn a_warm_up_touches_no_outputs_state() {
        let state_dir = crate::test_support::temp_dir().join(format!("gate-client-warm-outputs-{}", std::process::id()));
        if state_dir.exists() {
            fs::remove_dir_all(&state_dir).expect("an earlier run's folder is removed");
        }
        fs::create_dir_all(&state_dir).expect("the state folder can be made");
        let log = state_dir.join("out.log");
        let globs = ["**/bin/**".to_owned()];
        let warm_up = [vec!["./tool.exe".to_owned()]];
        let warm_call = RemoteCall {
            outputs: &globs,
            outputs_withheld: true,
            bins_withheld: false,
            failure_outputs: &[],
            dropped_no_build: false,
            send: None,
            desktop: false,
            warmth: Warmth::WarmUp {
                commands: &warm_up,
                hash: WARM_HASH,
            },
            interrupt: None,
            state_dir: &state_dir,
            ..call(&log)
        };
        let transport = nowhere();
        let mut attempt_of_warm_up = attempt(&warm_call, &transport);

        let sent = super::outputs_sent(&mut attempt_of_warm_up).expect("a warm-up's outputs need nothing");
        let marked = outputs::marked(&sent);

        let left: Vec<String> = fs::read_dir(&state_dir)
            .expect("the state folder can be read")
            .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
            .collect();
        drop(attempt_of_warm_up);
        fs::remove_dir_all(&state_dir).expect("the state folder can be removed");
        assert!(sent.globs.is_empty() && !sent.full, "no globs, no full send");
        assert!(sent.manifest.is_none(), "no manifest line on the run input");
        assert!(marked.is_ok(), "nothing to mark");
        assert!(left.is_empty(), "no hash cache and no pending marker: {left:?}");
    }

    #[test]
    fn a_warm_up_whose_client_failed_after_the_marker_failed_its_commands() {
        let log = unwritten_log();
        let warm_up = [vec!["./tool.exe".to_owned()]];
        let warm_call = RemoteCall {
            warmth: Warmth::WarmUp {
                commands: &warm_up,
                hash: WARM_HASH,
            },
            interrupt: None,
            ..call(&log)
        };
        let transport = nowhere();
        let failed_after = |marker_seen: bool| {
            let mut attempt_of_warm_up = attempt(&warm_call, &transport);
            attempt_of_warm_up.marker_seen = marker_seen;
            attempt_of_warm_up.outcome(End::Failed("a reader failed".to_owned())).warm
        };

        assert_eq!(
            failed_after(true),
            Some(failed(WarmFailure::Commands, "the client failed")),
            "commands may have run"
        );
        assert_eq!(
            failed_after(false),
            Some(failed(WarmFailure::Refused, "the client failed")),
            "no command ran"
        );
    }

    #[test]
    fn a_warm_up_whose_lost_connection_ends_on_an_interrupt_is_deleted() {
        let log = unwritten_log();
        let warm_up = [vec!["./tool.exe".to_owned()]];
        // The call's interrupt is raised, so the connection's loss ends on it.
        let warm_call = RemoteCall {
            warmth: Warmth::WarmUp {
                commands: &warm_up,
                hash: WARM_HASH,
            },
            ..call(&log)
        };
        let transport = nowhere();
        let mut attempt_of_warm_up = attempt(&warm_call, &transport);
        let lost = ran(End::Lost(0..0), true, None);
        assert!(attempt_of_warm_up.warm_ran(&lost), "a warm-up's end is kept");

        let ended = attempt_of_warm_up.warm_ended(lost, &location(), Some(BOOTED));

        assert!(matches!(ended, End::Interrupted), "{ended:?}");
        assert_eq!(attempt_of_warm_up.warm, Some(WarmEnd::Deleted), "an interrupt leaves no record");
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

    fn entry(mode: &str, kind: &str, path: &str) -> Entry {
        Entry {
            mode: mode.to_owned(),
            kind: kind.to_owned(),
            oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            size: None,
            path: path.to_owned(),
        }
    }

    #[test]
    fn a_listing_reads_each_entry_s_mode_type_object_size_and_path() {
        let listing = "100644 blob 0123456789abcdef0123456789abcdef01234567      21\t.gate/toolchain.toml\0\
                       040000 tree 0123456789abcdef0123456789abcdef01234567       -\tsrc dir\0";
        assert_eq!(
            entries_of(listing),
            Some(vec![
                Entry {
                    size: Some(21),
                    ..entry("100644", "blob", ".gate/toolchain.toml")
                },
                entry("040000", "tree", "src dir")
            ])
        );
        assert_eq!(entries_of(""), Some(Vec::new()));
        assert_eq!(entries_of("100644 blob\0"), None, "no tab");
        assert_eq!(entries_of("100644 blob x\ta\0"), None, "three words");
        assert_eq!(entries_of("100644 blob x y\ta\0"), None, "a size that is no number");
        assert_eq!(entries_of("100644 blob x 1 z\ta\0"), None, "five words");
    }

    #[test]
    fn the_lfs_listing_takes_small_regular_blobs_and_sees_any_attributes_file() {
        let sized = |mode: &str, kind: &str, size: Option<u64>, path: &str| Entry {
            size,
            ..entry(mode, kind, path)
        };
        let entries = [
            sized("100644", "blob", Some(1024), "a.bin"),
            sized("100644", "blob", Some(1025), "big.bin"),
            sized("120000", "blob", Some(5), "link.bin"),
            sized("160000", "commit", None, "sub"),
            sized("040000", "tree", None, "deep"),
        ];
        let small = small_blobs_of(&entries);
        let paths: Vec<&str> = small.blobs.iter().map(|blob| blob.path.as_str()).collect();
        assert_eq!(paths, ["a.bin"]);
        assert!(!small.attributes);
        let with = [sized("100644", "blob", Some(4000), "deep/.gitattributes")];
        assert!(small_blobs_of(&with).attributes, "an attributes file of any size, at any depth");
    }

    /// The manifest is found in one recursive listing that names folders too, as `git ls-tree -r -t -z` gives it: the
    /// exact spelling at the repo root is read, a case variant alone is, two at once differ only in letter case, a
    /// `.gate` that is no folder holds no manifest at all, a folder at the manifest's path is no file, and a
    /// `.gate/toolchain.toml` below the root is not the repo's.
    #[test]
    fn the_manifest_is_found_in_one_recursive_listing() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        // As `git ls-tree -l` lists them: a blob's size, padded, and `-` for a folder.
        let record = |mode: &str, kind: &str, name: &str| {
            let size = if kind == "blob" { "3" } else { "-" };
            format!("{mode} {kind} {oid} {size:>7}\t{name}\0")
        };
        let tree = |name: &str| record("040000", "tree", name);
        let blob = |name: &str| record("100644", "blob", name);
        let found = |listing: &str| -> Option<Result<Entry, String>> {
            let entries = entries_of(listing).expect("the listing reads");
            manifest_entry(&entries).map(|matched| matched.map(|hit| entry(&hit.mode, &hit.kind, &hit.path)))
        };

        let exact = format!("{}{}{}", blob("a.txt"), tree(".gate"), blob(".gate/toolchain.toml"));
        assert_eq!(
            found(&exact),
            Some(Ok(entry("100644", "blob", ".gate/toolchain.toml"))),
            "the folder and the name as spelled"
        );

        let variant = format!("{}{}", tree(".Gate"), blob(".Gate/Toolchain.TOML"));
        assert_eq!(
            found(&variant),
            Some(Ok(entry("100644", "blob", ".Gate/Toolchain.TOML"))),
            "a case variant alone is read too"
        );

        let both = format!("{}{}{}", tree(".Gate"), blob(".Gate/toolchain.toml"), blob(".Gate/Toolchain.TOML"));
        assert_eq!(
            found(&both),
            Some(Err(".Gate/toolchain.toml and .Gate/Toolchain.TOML differ only in letter case".to_owned()))
        );

        assert_eq!(found(&blob(".gate")), None, "a .gate that is no folder holds no manifest");

        let folder = format!("{}{}{}", tree(".gate"), tree(".gate/toolchain.toml"), blob(".gate/toolchain.toml/x"));
        assert_eq!(found(&folder), Some(Err(".gate/toolchain.toml is a folder, not a file".to_owned())));

        let nested = format!("{}{}{}", tree("sub"), tree("sub/.gate"), blob("sub/.gate/toolchain.toml"));
        assert_eq!(found(&nested), None, "a manifest below the root is not the repo's");
    }

    #[test]
    fn the_manifest_is_a_regular_file_at_the_root_whatever_its_letter_case() {
        let file = [entry("100644", "blob", "a.txt"), entry("100644", "blob", ".gate/toolchain.toml")];
        assert_eq!(manifest_entry(&file), Some(Ok(&file[1])));
        let executable = [entry("100755", "blob", ".Gate/Toolchain.TOML")];
        assert_eq!(manifest_entry(&executable), Some(Ok(&executable[0])));
        assert_eq!(
            manifest_entry(&[entry("100644", "blob", "x/.gate/toolchain.toml")]),
            None,
            "not at the root"
        );
        assert_eq!(manifest_entry(&[]), None);
    }

    #[test]
    fn a_manifest_that_is_a_submodule_a_symlink_or_a_folder_is_not_read() {
        for (mode, kind, why) in [
            ("160000", "commit", ".gate/toolchain.toml is a submodule, not a file"),
            ("120000", "blob", ".gate/toolchain.toml is a symbolic link, not a file"),
            ("040000", "tree", ".gate/toolchain.toml is a folder, not a file"),
            ("100664", "blob", ".gate/toolchain.toml is not a regular file (mode 100664)"),
        ] {
            let entries = [entry(mode, kind, ".gate/toolchain.toml")];
            assert_eq!(manifest_entry(&entries), Some(Err(why.to_owned())), "{mode} {kind}");
        }
    }

    #[test]
    fn two_manifests_that_differ_only_in_case_are_not_read_unless_one_is_spelled_exactly() {
        let two = [
            entry("100644", "blob", ".Gate/toolchain.toml"),
            entry("100644", "blob", ".gate/Toolchain.toml"),
        ];
        assert_eq!(
            manifest_entry(&two),
            Some(Err(".Gate/toolchain.toml and .gate/Toolchain.toml differ only in letter case".to_owned()))
        );
        let exact = [
            entry("100644", "blob", ".GATE/toolchain.toml"),
            entry("100644", "blob", ".gate/toolchain.toml"),
        ];
        assert_eq!(manifest_entry(&exact), Some(Ok(&exact[1])), "the exact spelling wins");
        let exact_link = [
            entry("100644", "blob", ".GATE/toolchain.toml"),
            entry("120000", "blob", ".gate/toolchain.toml"),
        ];
        assert_eq!(
            manifest_entry(&exact_link),
            Some(Err(".GATE/toolchain.toml and .gate/toolchain.toml differ only in letter case".to_owned())),
            "an exact spelling that is no regular file wins nothing"
        );
    }

    /// The pin the version text `text` spells, as a repo writes one.
    fn pin_of(text: &str) -> Pin {
        Pin::parse(text).expect("a pin")
    }

    /// A repo's project files are the tree's entries whose name ends in `.csproj`, at any depth and in either letter
    /// case — a folder named like one is no file at all and is left out — and each names the `Godot.NET.Sdk` version
    /// the root `Sdk` attribute pins; the same version twice is one pin.
    #[test]
    fn csproj_pins_are_read_from_the_tree() {
        let entries = [
            entry("100644", "blob", "a.txt"),
            entry("100644", "blob", "src/App/App.csproj"),
            entry("100644", "blob", "Sub/Game.CSProj"),
            entry("040000", "tree", "folder.csproj"),
            entry("100644", "blob", "x.csproj/y.txt"),
        ];
        let mut asked: Vec<String> = Vec::new();
        let (pins, skips) = csproj_pins(
            &entries,
            |files| Ok(files.iter().map(|_| 40_u64).collect()),
            |files| {
                asked.extend(files.iter().map(|file| file.path.clone()));
                Ok(files.iter().map(|_| b"<Project Sdk=\"Godot.NET.Sdk/4.7.2\">".to_vec()).collect())
            },
        )
        .expect("no git failure");

        assert_eq!(
            asked,
            ["src/App/App.csproj", "Sub/Game.CSProj"],
            "any depth, either letter case, no folder, nothing else"
        );
        assert_eq!(pins, [pin_of("4.7.2")], "the same version pinned twice is one");
        assert_eq!(skips, Vec::<String>::new());
    }

    /// What the sizes say decides what is read: a project file past [`CSPROJ_BYTES`] is skipped with its line and never
    /// handed to the read, where a file that is no file at all and one whose text is not UTF-8 are skipped with theirs,
    /// and the file beside them that reads still pins.
    #[test]
    fn a_csproj_over_its_cap_is_skipped_with_its_line() {
        let entries = [
            entry("100644", "blob", "big.csproj"),
            entry("100644", "blob", "odd.csproj"),
            entry("120000", "blob", "link.csproj"),
            entry("100644", "blob", "ok.csproj"),
        ];
        let mut asked: Vec<String> = Vec::new();
        let (pins, skips) = csproj_pins(
            &entries,
            |files| {
                assert_eq!(files.len(), 3, "a file that is no regular file is not weighed");
                Ok(vec![CSPROJ_BYTES as u64 + 1, 4, 20])
            },
            |files| {
                asked.extend(files.iter().map(|file| file.path.clone()));
                Ok(vec![vec![0xff, 0xfe], b"<Project Sdk=\"Godot.NET.Sdk/4.7.2\">".to_vec()])
            },
        )
        .expect("no git failure");

        assert_eq!(asked, ["odd.csproj", "ok.csproj"], "the file past its cap is never read");
        assert_eq!(pins, [pin_of("4.7.2")], "the file that reads still pins");
        assert_eq!(skips.len(), 3, "{skips:?}");
        assert_eq!(
            skips[0],
            "gate: cannot read the Godot pin of link.csproj: it is a symbolic link, not a file; not checking it"
        );
        assert_eq!(
            skips[1],
            "gate: cannot read the Godot pin of big.csproj: its text is more than 256 KiB; not checking it"
        );
        assert_eq!(
            skips[2],
            "gate: cannot read the Godot pin of odd.csproj: not valid UTF-8; not checking it"
        );
    }

    /// Only the first sixty-four project files are read, and one line names how many were left past the cap.
    #[test]
    fn at_most_64_csproj_files_are_read() {
        let entries: Vec<Entry> = (0..65).map(|at| entry("100644", "blob", &format!("p{at:02}.csproj"))).collect();
        let mut read = 0;
        let (pins, skips) = csproj_pins(
            &entries,
            |files| Ok(files.iter().map(|_| 40_u64).collect()),
            |files| {
                read += files.len();
                Ok(files.iter().map(|_| Vec::<u8>::new()).collect())
            },
        )
        .expect("no git failure");

        assert_eq!(read, CSPROJ_FILES, "sixty-four is the most");
        assert_eq!(pins, Vec::<Pin>::new());
        assert_eq!(skips, ["gate: 1 more project files not read for a Godot pin; not checking them"]);
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
            Some(r"gate: not routed: builder runs gate 0.2.0\u{1b}[31m\u{202e}, this PC 0.1.0; run gate push".to_owned())
        );
        let long = "9".repeat(5000);
        assert_eq!(
            version_refusal("builder", Some(&long), "0.1.0"),
            Some(format!(
                "gate: not routed: builder runs gate {}, this PC 0.1.0; run gate push",
                "9".repeat(4096)
            )),
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
                    log_cap: LOG_CAP,
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
            Watch::Setup(Limits::for_tests()),
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

    #[test]
    fn the_ssh_client_has_the_builder_layout_root() {
        let ssh = Ssh {
            program: PathBuf::from(r"C:\Program Files\OpenSSH\ssh.exe"),
        };

        let mut client = RustClient::over(Some(Box::new(ssh)), &TestSeams::default());

        assert_eq!(
            super::found(&mut client.transport).expect("the transport is given").root(),
            r"D:\gate-remote",
            "every work folder and bare repository lives there"
        );
        assert_eq!(client.git, GitBounds::DEFAULT, "a request with no seams keeps the constants");
        assert_eq!(client.dotnet_limit, DOTNET_LIMIT, "a request with no seams keeps the constants");
    }

    #[test]
    fn the_git_limit_and_drain_seams_bound_the_attempt_s_git_calls() {
        let seams = TestSeams {
            git_limit_ms: Some(1_800_000),
            git_drain_ms: Some(600_000),
            ..TestSeams::default()
        };
        let log = crate::test_support::temp_dir().join(format!("gate-client-git-bounds-{}.log", std::process::id()));
        let call = call(&log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        attempt.git = RustClient::over(None, &seams).git;

        let asked = attempt.git_call(Path::new(r"C:\work\app"), &["write-tree"], &[], None);

        let (limit, drain) = (Duration::from_mins(30), Duration::from_mins(10));
        assert_eq!(
            (asked.limit, asked.drain),
            (limit, drain),
            "the attempt's git call is bounded by both seams"
        );
    }

    #[test]
    fn a_call_runs_in_the_system_folder_and_finds_no_program_in_the_working_folder() {
        use std::ffi::{OsStr, OsString};

        use crate::remote::git::NO_CURRENT_FOLDER;
        use crate::remote::transport::Program;

        let program = Program {
            program: PathBuf::from(r"C:\Program Files\OpenSSH\ssh.exe"),
            args: vec![OsString::from("--"), OsString::from("builder")],
        };

        let command = super::call_command(program, Path::new(r"C:\Windows\System32"));

        assert_eq!(command.get_program(), OsStr::new(r"C:\Program Files\OpenSSH\ssh.exe"));
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--", "builder"]);
        assert_eq!(command.get_current_dir(), Some(Path::new(r"C:\Windows\System32")));
        let set: Vec<Option<&OsStr>> = command
            .get_envs()
            .filter(|(name, _)| *name == OsStr::new(NO_CURRENT_FOLDER))
            .map(|(_, value)| value)
            .collect();
        assert_eq!(set, [Some(OsStr::new("1"))], "ssh and what it starts skip the working folder");
    }

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A snapshot of [`COMMIT`] with no bridge.
    fn snapshot() -> Snapshot {
        Snapshot {
            commit: COMMIT.to_owned(),
            bridge: None,
            objects: PathBuf::from(r"C:\app\.git\objects"),
        }
    }

    /// A pair's pushes are watched by one transfer clock, started before the first: with `--test-remote-watch-call
    /// push`, whose ceiling starts a clock at each reading of the push's limits, both pushes still count from one start.
    #[test]
    fn a_pair_s_pushes_share_one_transfer_clock() {
        let pair = Pair::new("client-one-clock", &["lib"]);
        let run = pair.run(&pair.root, &["lib"]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        attempt.one_call = Some(OneCall {
            name: "push",
            stall: None,
            ceiling: Some(Duration::from_secs(3600)),
        });
        let repos = attempt.repos();
        let mut starts = Vec::new();

        let pushed = attempt.each_push(&repos, &[snapshot(), snapshot()], &[], |_, _, watch| {
            let Watch::Setup(limits) = watch else {
                return Err(End::Failed(format!("a push watched as {watch:?}")));
            };
            // The clock moves past this push's start before the next push, so a clock of its own would start later.
            while Instant::now() <= limits.started {
                std::hint::spin_loop();
            }
            starts.push(limits.started);
            Ok(())
        });

        assert!(pushed.is_ok(), "{pushed:?}");
        assert_eq!(starts.len(), 2, "both repositories are pushed");
        assert_eq!(starts.first(), starts.last(), "the second push counts from the first one's start");
    }

    /// A pair's push time is the sum of its pushes' times, each push said with its own.
    #[test]
    fn a_pair_s_push_time_is_the_sum_of_its_pushes() {
        let pair = Pair::new("client-push-sum", &["lib"]);
        let run = pair.run(&pair.root, &["lib"]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let transport = nowhere();
        let mut attempt = attempt(&call, &transport);
        let repos = attempt.repos();

        attempt.pushed(&repos[0], COMMIT, "1 KiB", Duration::from_millis(1200));
        attempt.pushed(&repos[1], COMMIT, "2 KiB", Duration::from_millis(2500));

        assert_eq!(attempt.split.push, Duration::from_millis(3700));
        assert_eq!(
            attempt.lines,
            [
                "gate: remote pushed snapshot app 01234567 in 1.2 s (1 KiB)",
                "gate: remote pushed snapshot lib 01234567 in 2.5 s (2 KiB)",
            ]
        );
    }

    /// An ssh found inside a sibling's root starts nothing, refused as one inside the worktree root is.
    #[test]
    fn an_ssh_inside_a_sibling_s_root_starts_no_run() {
        let pair = Pair::new("client-ssh-in-sibling", &["lib"]);
        let program = pair.folder("lib").join("ssh.exe");
        fs::write(&program, b"").expect("the stand-in ssh can be written");
        let run = pair.run(&pair.root, &["lib"]);
        let log = pair.top.join("out.log");
        let call = pair_call(&run, carried(&run.root, &run.siblings).expect("the run's siblings check"), &log);
        let ssh = Ssh { program: program.clone() };
        let mut client = RustClient::over(Some(Box::new(ssh)), &TestSeams::default());

        let outcome = client.run(&call);

        let why = format!("cannot run ssh: {} lies inside the worktree", program.display());
        assert_eq!(outcome.reason, Some(FallbackReason::Client), "{outcome:?}");
        assert_eq!(outcome.detail, Some(why), "{outcome:?}");
    }

    /// A run call's lines as a stamping box sends them: protocol lines matched as they came, and progress marked per
    /// line, never for a process line.
    mod stamped {
        use std::collections::VecDeque;
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        use gate_wire::stamp::{self, Tag};
        use gate_wire::{CROSS_BUILD_START, CROSS_NOT_BROUGHT_BACK};

        use super::super::{BoxKill, Echo, End, ProtocolLines, Reader, Reading, Shared, held, killed_line, relay};
        use super::{Chunks, Step, UNMARKED, outputs_worktree, passed, said_after};
        use crate::remote::outputs::NO_OUTPUTS_LINE;
        use crate::remote::watchdog::LOG_CAP;

        /// The nonce of the run these cases read the lines of.
        const NONCE: &str = "n1";

        /// How a run call's reader takes its lines: matched against the protocol lines of [`NONCE`], logged nowhere.
        fn run_reading() -> Reading {
            Reading {
                echo: Echo::LogOnly,
                capped: false,
                lines: Some(Arc::new(ProtocolLines::new(NONCE))),
                log_cap: LOG_CAP,
            }
        }

        #[test]
        fn a_kill_tag_the_box_stamped_as_the_command_s_line_does_not_kill_the_run() {
            let tag = killed_line(NONCE, BoxKill::TimedOut);
            let forged = stamp::stamped(&stamp::now(), Tag::Err, &tag);
            let shared = Shared::new();

            relay(format!("{forged}\n").as_bytes(), Reader::new(true, run_reading(), None), &shared);

            let heard = held(&shared.heard);
            assert_eq!(heard.box_killed, None, "the command's own line is no kill tag");
            assert_eq!(
                heard.last_error.as_deref(),
                Some(tag.as_str()),
                "kept as the command's text, without its stamp"
            );
            drop(heard);
            let shared = Shared::new();
            relay(format!("{tag}\n").as_bytes(), Reader::new(true, run_reading(), None), &shared);
            assert_eq!(
                held(&shared.heard).box_killed,
                Some(BoxKill::TimedOut),
                "the box's own tag, bare, kills it"
            );
        }

        /// The line the box says when a run's cross build brought no Windows outputs back.
        fn cross_line() -> String {
            format!("{CROSS_BUILD_START}win-x64 failed with exit 1{CROSS_NOT_BROUGHT_BACK}")
        }

        /// Whether a run call that heard `line` on standard error heard the box say its cross build brought nothing back.
        fn heard_cross_not_back(line: &str) -> bool {
            let shared = Shared::new();
            relay(format!("{line}\n").as_bytes(), Reader::new(true, run_reading(), None), &shared);
            held(&shared.heard).cross_not_brought_back
        }

        #[test]
        fn only_the_box_s_own_stamped_cross_build_line_is_heard() {
            let at = stamp::now();
            assert!(heard_cross_not_back(&stamp::stamped(&at, Tag::Gat, &cross_line())), "the box's own line");
            assert!(!heard_cross_not_back(&stamp::stamped(&at, Tag::Err, &cross_line())), "the command's copy");
            assert!(!heard_cross_not_back(&cross_line()), "an unstamped copy");
        }

        #[test]
        fn a_cross_build_line_on_a_run_that_sent_no_cross_build_still_ends_1_with_no_outputs_line() {
            let held_tree = outputs_worktree("cross-line-uncrossed", true);
            let heard = heard_cross_not_back(&stamp::stamped(&stamp::now(), Tag::Gat, &cross_line()));
            let step = Step {
                cross_not_brought_back: heard,
                ..passed(&held_tree, None, false)
            };
            let (end, _, lines) = said_after("cross-line-uncrossed", &step);
            assert!(matches!(end, End::Status(1)), "{end:?}");
            assert!(lines.iter().any(|line| line.contains(NO_OUTPUTS_LINE)), "{lines:?}");
        }

        #[test]
        fn an_unstamped_cross_build_line_on_a_cross_build_run_still_ends_1() {
            let held_tree = outputs_worktree("cross-line-unstamped", true);
            let step = Step {
                cross_not_brought_back: heard_cross_not_back(&cross_line()),
                crossed: true,
                ..passed(&held_tree, None, false)
            };
            let (end, _, lines) = said_after("cross-line-unstamped", &step);
            assert!(matches!(end, End::Status(1)), "{end:?}");
            assert!(lines.iter().any(|line| line.contains(NO_OUTPUTS_LINE)), "{lines:?}");
        }

        #[test]
        fn a_run_call_that_hears_only_process_lines_marks_no_progress() {
            let at = stamp::now();
            let spawn = stamp::stamped(&at, Tag::Gat, &stamp::spawn_text(7, Some(3), Some("tool.exe")));
            let exit = stamp::stamped(&at, Tag::Gat, &stamp::exit_text(7, stamp::Ended::Code(0), None));
            let said = stamp::stamped(&at, Tag::Err, "said there");
            let shared = Shared::new();
            shared.progress.last_byte.store(UNMARKED, Ordering::Relaxed);
            let chunks: VecDeque<Vec<u8>> = [spawn, exit, said].into_iter().map(|line| format!("{line}\n").into_bytes()).collect();
            let mut stream = Chunks {
                left: chunks,
                shared: &shared,
                seen: Vec::new(),
            };

            relay(&mut stream, Reader::new(true, run_reading(), None), &shared);

            let marked: Vec<bool> = stream.seen.iter().map(|(marked, _)| *marked).collect();
            assert_eq!(marked, [false, false, false, true], "only the command's line marks the call's progress");
            let heard = held(&shared.heard);
            assert_eq!(heard.first_error.as_deref(), Some("said there"), "no process line is kept");
        }
    }
}
