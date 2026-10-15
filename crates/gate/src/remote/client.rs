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
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::BoxVerb;
#[cfg(feature = "test-seams")]
use gate_args::RunRequest;
use gate_args::TestSeams;
use gate_state::{FallbackReason, KernelObject, key_hash};
use gate_win32::{AcquireError, Interrupt, Job, NamedMutex, OwnedMutexGuard, PipedConsole, Running};
use jiff::{SignedDuration, Timestamp};
use regex::Regex;

use super::git::{self, Git, GitError, NO_CURRENT_FOLDER, last_update};
use super::launch::{DRAIN, KILL_WAIT, KILLED, call_job, launched, piped_launch};
use super::outputs;
use super::parity;
use super::payload::{self, Location, RunCall};
use super::retry::{self, Ended, GIT_SSH_FAILED, SSH_FAILED};
use super::snapshot::{self, Snapshot};
use super::syncback;
use super::toolchain::{self, Manifest, Reported};
#[cfg(feature = "test-seams")]
use super::transport::Local;
use super::transport::ssh::Ssh;
use super::transport::{Program, Transport, Wire};
use super::watchdog::{self, Cut, Kept, Kill, LINE_CAP};
use super::{
    BoxKill, FAILED, FAILED_RUN, REFUSED, Refusal, RemoteCall, RemoteClient, RemoteOutcome, Split, adjusted_prefix, killed_line, refused_line,
    started_line,
};
use crate::boxside::outputs::{NAMES_SKIPPED, OUTPUTS_BUDGET};
use crate::boxside::payload::escaped;
use crate::durations::{Usual, hang_hint};
use crate::environment::Variables;
use crate::outputs::archive::sent_limit;
#[cfg(feature = "test-seams")]
use crate::run::find_program;
use crate::run::{CONTROL_C_EXIT, is_within, resolved};
use crate::version::build_version;

/// The statuses of a preflight whose builder has no `gate box` that takes the run: cmd's for a program it cannot find,
/// and a gate's usage error for a payload it does not read.
pub(crate) const NOT_RECOGNIZED: i32 = 9009;
pub(crate) const USAGE: i32 = 2;
/// The most bytes a call's job may have written and still be made once more after a failure before the banner: ssh's
/// own failure writes one line, and a second fetch from a builder would start a fresh count against its cap.
const RETRY_WRITTEN: u64 = 1 << 20;
/// How often the retry pause looks at the console interrupt.
const PAUSE_STEP: Duration = Duration::from_millis(20);
/// The box's heartbeat, `gate-remote-heartbeat: <digits>` on standard error: progress, kept out of the log.
const HEARTBEAT: &str = "gate-remote-heartbeat: ";
/// The preflight's lines the client reads.
const VERSION: &str = "gate: remote version ";
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

/// The Rust client: over ssh, or over the local transport of `--test-remote-transport`.
pub(crate) struct RustClient {
    /// The way to the builder: `None` for the ssh transport until a routed attempt first needs it ([`found`]), so a
    /// run that never routes never looks for ssh.
    transport: Option<Box<dyn Transport>>,
    /// How long each git call of the snapshot and the saved ref may take.
    limit: Duration,
    /// The pause before a retry.
    pause: Duration,
    /// How long a bounded call, the `stop` or the `boot`, may take.
    bound: Duration,
    /// `--test-stall-ms`: the stall window, given as it is, with no floor.
    stall_ms: Option<u64>,
    /// `--test-remote-timeout-ms`: with `--test-remote-watch-call` naming a call other than `run`, that call's transfer
    /// ceiling, or the `ls-remote` deadline, in milliseconds.
    timeout_ms: Option<u64>,
    /// `--test-remote-backstop-ms`: the run call's backstop, in milliseconds, in place of five ceilings and 60 s.
    backstop_ms: Option<u64>,
    /// `--test-remote-watch-call`: the one call `--test-stall-ms` applies to, when it is not the whole run's, and
    /// `--test-remote-timeout-ms` too, which names a call other than `run`; the backstop seam is the run call's alone.
    watch_call: Option<String>,
    /// `--test-remote-interrupt-at`: the call after whose first output byte the client raises the console interrupt.
    interrupt_at: Option<String>,
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
    /// The most bytes of one call's output written to the gate's log: `--test-remote-log-cap-bytes`, else 256 MiB.
    log_cap: u64,
    /// `--test-own-start-fails`: this gate's start, which names the sync-back's quarantine, cannot be read.
    own_start_fails: bool,
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
    pub(crate) fn local(request: &RunRequest, transport: &str) -> Result<RustClient, String> {
        let seams = &request.test_seams;
        let local = local_transport(transport, seams.remote_ssh.as_deref())?;
        Ok(RustClient::over(Some(Box::new(local)), seams))
    }

    /// The client over `transport`, the ssh one looked for later when `None`, each limit its constant unless `seams`
    /// gives one in its place, as only a gate built with the test seams can.
    fn over(transport: Option<Box<dyn Transport>>, seams: &TestSeams) -> RustClient {
        RustClient {
            transport,
            limit: git::limit(seams),
            pause: retry::pause(seams),
            bound: retry::bound(seams),
            stall_ms: seams.stall_ms,
            timeout_ms: seams.remote_timeout_ms,
            backstop_ms: seams.remote_backstop_ms,
            watch_call: seams.remote_watch_call.clone(),
            interrupt_at: seams.remote_interrupt_at.clone(),
            local_sdks: seams.local_sdks.clone(),
            dotnet_limit: dotnet_limit(seams),
            fetch_cap: seams.remote_fetch_cap_bytes.unwrap_or(watchdog::FETCH_CAP),
            apply_cap: seams.remote_apply_cap_bytes.unwrap_or(watchdog::APPLY_CAP),
            outputs_budget: seams.remote_outputs_budget_bytes.unwrap_or(OUTPUTS_BUDGET),
            log_cap: seams.remote_log_cap_bytes.unwrap_or(watchdog::LOG_CAP),
            own_start_fails: seams.own_start_fails,
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
    let name = call.state.object_name(KernelObject::Routed { hash: key_hash(call.key) });
    let mutex = NamedMutex::open(&name).map_err(|error| Some(format!("gate: cannot open the mutex {name}: {error}")))?;
    match mutex.acquire_owned(Duration::ZERO) {
        Ok(held) => Ok(held),
        Err(AcquireError::Timeout) => Err(None),
        Err(AcquireError::Failed(error)) => Err(Some(format!("gate: cannot take the mutex {name}: {error}"))),
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

    /// Looks for ssh on the routed attempt's first need of it, so a run that cannot find it falls back `client` before
    /// it takes a key lock, a ticket or a remote slot.
    fn ready(&mut self) -> Result<(), String> {
        found(&mut self.transport).map(drop)
    }

    fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
        if let Some(sibling) = call.siblings.first() {
            return pairs_off(call, sibling.name());
        }
        let transport = match found(&mut self.transport) {
            Ok(transport) => transport,
            Err(why) => return RemoteOutcome::not_started(why),
        };
        // The worktree is known only now: a transport whose program lies inside it starts nothing.
        if let Err(why) = transport.judge(Path::new(call.root)) {
            return RemoteOutcome::not_started(why);
        }
        let started = Instant::now();
        let one = self.watch_call.as_deref();
        // With `--test-remote-watch-call`, the stall seam is that one call's, and the run keeps its own.
        let stall_ms = if one.is_some() { None } else { self.stall_ms };
        let (stall, raised) = watchdog::stall_window(call.stall, stall_ms);
        let mut attempt = Attempt {
            call,
            transport: &*transport,
            limit: self.limit,
            lock_wait: self.limit,
            pause: self.pause,
            limits: Limits {
                started,
                stall,
                command: Duration::from_secs(call.timeout),
                backstop: self.backstop_ms.map_or_else(|| watchdog::backstop(call.timeout), Duration::from_millis),
                transfer: TRANSFER_CEILING,
                bound: self.bound,
            },
            one_call: one.map(|name| OneCall {
                name,
                stall: self.stall_ms.map(Duration::from_millis),
                ceiling: self.timeout_ms.map(Duration::from_millis),
            }),
            interrupt_at: self.interrupt_at.as_deref(),
            local_sdks: self.local_sdks.as_deref(),
            dotnet_limit: self.dotnet_limit,
            fetch_cap: self.fetch_cap,
            apply_cap: self.apply_cap,
            outputs_budget: self.outputs_budget,
            log_cap: self.log_cap,
            own_start_fails: self.own_start_fails,
            brought_back: None,
            split: Split::default(),
            lines: Vec::new(),
            problem: None,
            hang_said: false,
            adjusted: None,
            box_killed: None,
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

/// What the fallback line of a call carrying siblings says the client could not start for.
const PAIRS_OFF: &str = "sibling pairs are off";

/// The outcome of the call `call`, which carries the sibling `name` first, while the builder takes no sibling pairs: it
/// falls back `client` before any folder read, ssh call or push, saying `gate: not routed: sibling {name} the builder
/// does not take sibling pairs yet`.
fn pairs_off(call: &RemoteCall<'_>, name: &str) -> RemoteOutcome {
    let line = format!("gate: not routed: sibling {name} the builder does not take sibling pairs yet");
    let detail = match super::say(call.log, &[&line], io::stderr().lock()) {
        Ok(()) => PAIRS_OFF.to_owned(),
        Err(why) => why,
    };
    RemoteOutcome {
        lines: vec![line],
        ..RemoteOutcome::not_started(detail)
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
    /// The watchdog killed the run call, and the command was stopped, or it killed a fetch after it, the sync-back's or
    /// the outputs call, at its transfer ceiling or, the outputs call, for its silence: status 124.
    Killed,
    /// The run call's connection was lost: it exited 255, ssh's own failure status, other than before the banner. The
    /// run call returns it before anything is stopped, with where the run call's own lines are among the attempt's, so
    /// a fallback can leave them out. Nothing is synced back: status 255.
    Lost(Range<usize>),
}

/// What the preflight found: whether it created the bare repository just now, the builder's boot time, `None` when it
/// said none, the SDKs it lists, `None` when it said no SDK line, the tools its `Needs` named, and what the builder
/// said of each of them, in that order, a tool it said nothing of left out.
struct Prepared {
    created: bool,
    boot: Option<String>,
    sdks: Option<Vec<String>>,
    sent: Vec<String>,
    tools: Vec<(String, Reported)>,
}

/// What the snapshot's tree gives the parity check, read before the preflight: the git that listed it, the names of
/// its files and submodules, and its `.gate/toolchain.toml`, `None` when it has none, `Err` with why when it cannot be
/// read or is no regular file.
struct Tree {
    git: Git,
    names: Vec<String>,
    manifest: Option<Result<Manifest, String>>,
}

/// The repo's toolchain manifest, as its path at the repo root is spelled.
const MANIFEST_PATH: &str = ".gate/toolchain.toml";
/// The folder at the repo root that holds the manifest, matched ignoring ASCII case.
const GATE_FOLDER: &str = ".gate";
/// The manifest's name inside [`GATE_FOLDER`], matched ignoring ASCII case.
const MANIFEST_NAME: &str = "toolchain.toml";

/// One entry of the snapshot's tree as `git ls-tree -z` lists one: the mode, the object type (`blob`, `tree` or
/// `commit`), the object id and the path.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    mode: String,
    kind: String,
    oid: String,
    path: String,
}

/// The entries of a `git ls-tree -z` listing, each `<mode> <type> <oid>\t<path>` ended by a NUL; `None` when an entry
/// is not of that form.
fn entries_of(listing: &str) -> Option<Vec<Entry>> {
    listing
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let (meta, path) = record.split_once('\t')?;
            let mut parts = meta.split(' ');
            let (mode, kind, oid) = (parts.next()?, parts.next()?, parts.next()?);
            parts.next().is_none().then(|| Entry {
                mode: mode.to_owned(),
                kind: kind.to_owned(),
                oid: oid.to_owned(),
                path: path.to_owned(),
            })
        })
        .collect()
}

/// The repo's `.gate` folder among the root listing `root`: the entries named `.gate` ignoring ASCII case whose type is
/// a tree, so one that is no folder names no manifest at all.
fn gate_folders(root: &[Entry]) -> Vec<&Entry> {
    root.iter()
        .filter(|entry| entry.path.eq_ignore_ascii_case(GATE_FOLDER) && entry.kind == "tree")
        .collect()
}

/// The entries of one `.gate` folder's listing `inside` that can be the repo's manifest: each named `toolchain.toml`
/// ignoring ASCII case, its path the folder's path as `gate` spells it joined to its own name, so [`manifest_entry`]
/// weighs it as the file that folder holds at the repo root, whatever the letter case of either.
fn manifest_candidates(gate: &Entry, inside: &[Entry]) -> Vec<Entry> {
    inside
        .iter()
        .filter(|entry| entry.path.eq_ignore_ascii_case(MANIFEST_NAME))
        .map(|entry| Entry {
            mode: entry.mode.clone(),
            kind: entry.kind.clone(),
            oid: entry.oid.clone(),
            path: format!("{}/{}", gate.path, entry.path),
        })
        .collect()
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

impl Tree {
    /// The `Needs` the preflight sends for the tree's manifest; none when it has no manifest that reads.
    fn needs(&self) -> Vec<(String, String)> {
        match &self.manifest {
            Some(Ok(manifest)) => toolchain::needs_of(manifest),
            _ => Vec::new(),
        }
    }
}

/// The ceiling of each transfer, whatever `--timeout` says: the setup up to the end of the push, counted from the
/// attempt's start, and each fetch after the run call, the sync-back's and the outputs', counted from its own start.
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

/// `--test-remote-watch-call`: the one call, `preflight`, `push`, `run`, `ls-remote`, `fetch` or `outputs`, that the
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
    /// start, or once its job wrote more bytes than the cap when it has one: the sync-back's fetch, and the outputs call.
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
                let elapsed = now.saturating_duration_since(begun);
                watchdog::kill(idle, elapsed, limits.stall, limits.backstop).map(Cut::Watchdog)
            }
            Watch::Setup(limits) => limits.setup_cut(now, idle),
            Watch::Fetch(limits, _) if idle >= limits.stall => Some(Cut::Silent(limits.stall)),
            Watch::Fetch(limits, _) => (now.saturating_duration_since(begun) >= limits.transfer).then_some(Cut::Ceiling(limits.transfer)),
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
            Watch::Run(_) | Watch::Setup(_) | Watch::Deadline(_) | Watch::Bounded(_) => None,
        }
    }

    /// How long the client waits between two looks at a call watched so.
    fn tick(self) -> Duration {
        match self {
            Watch::Run(limits) | Watch::Setup(limits) | Watch::Fetch(limits, _) => watchdog::tick(limits.stall),
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
    /// Its last standard error line that is the box's back commits line, however many lines came after it.
    back_commits: Option<String>,
    /// Whether it said on standard error that the box skipped outputs for names this PC cannot take.
    names_skipped: bool,
    /// Whether the box said the run's start marker on standard error.
    started: bool,
    /// The reason the box's refusal tag for the run named on standard error, the first when it said more than one.
    refused: Option<FallbackReason>,
    /// The kill the box's kill tag for the run named on standard error, the last when it said more than one.
    box_killed: Option<BoxKill>,
    /// The load-adjusted seconds the box's last adjusted line on standard error said.
    adjusted: Option<f64>,
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
    /// How long the sync-back waits for a worktree's lock another run holds: the git limit, which the sync-back's tests
    /// shorten without shortening its git calls.
    pub(super) lock_wait: Duration,
    pause: Duration,
    pub(super) limits: Limits,
    one_call: Option<OneCall<'c>>,
    /// The call, `preflight`, `run` or `outputs`, after whose first output byte the console interrupt is raised, as a
    /// Ctrl+C would; or `install`, once the install has moved its first file and before it moves a second, so its line
    /// reads `1 of {m}`: an install of a single file ends before the seam is reached; or `stop`, as `run` and then once
    /// more while the stop call that Ctrl+C leads to runs; or `apply`, once a pair's sync-back has checked every patch
    /// and before it applies the first.
    pub(super) interrupt_at: Option<&'c str>,
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
    /// The most bytes of one call's output, its two streams counted together, written to the gate's log.
    log_cap: u64,
    /// `--test-own-start-fails`: this gate's start, which names the sync-back's quarantine, cannot be read.
    pub(super) own_start_fails: bool,
    /// Whether the run's outputs were brought back, as its outputs step ended ([`outputs::ended`]); `None` for a run
    /// that never reached it.
    pub(super) brought_back: Option<bool>,
    /// How long the run call and each transfer took, as far as the attempt got.
    pub(super) split: Split,
    lines: Vec<String>,
    problem: Option<String>,
    /// Whether the run call said its hang hint, which an attempt says once.
    hang_said: bool,
    /// The load-adjusted seconds the box said it counted for the command, `None` until the run call that counts said
    /// them.
    adjusted: Option<f64>,
    /// The kill the box's kill tag named, set only when the run call ended as the box's own kill.
    box_killed: Option<BoxKill>,
}

/// How a run call ended ([`Attempt::run_call`]): its end, the last standard error lines of the call that counts that
/// are the box's outputs line and its back commits line, and whether that call said on standard error that the box
/// skipped outputs for names this PC cannot take.
struct RunEnd {
    end: End,
    outputs_line: Option<String>,
    back_commits: Option<String>,
    names_skipped: bool,
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
            timeout: self.call.timeout,
            host: self.call.host,
            sibling_snapshots: &[],
        };
        let run_input = payload::input(&payload::run_word(&location, &run), sent.manifest.as_deref());
        let tree = self.tree(&snapshot)?;
        let needs = tree.as_ref().map(Tree::needs).unwrap_or_default();
        let prepared = self.preflight(&location, &needs)?;
        self.parity(&snapshot, tree.as_ref(), &prepared)?;
        self.push(&snapshot, prepared.created)?;
        // After the push: a run refused before it, at the preflight or by the parity check, leaves no marker.
        outputs::marked(&sent)?;
        let called = self.run_call(&location, &run_input, &nonce)?;
        // The sync-back runs after a run call that ended with the command's own status; a refusal, an interrupt, a
        // kill, a lost connection or a failed call ended the attempt before it, so a box refusal's 75 never syncs back.
        // The pending marker stays on every end but a passing run whose box packed nothing.
        match called.end {
            End::Status(status) => {
                let back_commits = called.back_commits.as_deref().and_then(syncback::back_commits);
                // The run sends no sibling snapshots, as `sibling_snapshots` above says: a run carrying siblings applies
                // nothing, and one carrying none syncs back its main repository.
                let synced = match syncback::targets(self.call, &snapshot, &[]) {
                    Some(targets) => syncback::sync_back(self, &targets, back_commits.as_deref(), status)?,
                    None => syncback::unmatched(self, status),
                };
                let brought = outputs::accepted(called.outputs_line.as_deref(), self.call.key, &nonce);
                let ran = outputs::Ran {
                    location: &location,
                    nonce: &nonce,
                    status,
                    names_skipped: called.names_skipped,
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

    /// The preflight: readies the work folder, asks the builder to probe the tools of `needs`, and checks the builder
    /// runs this gate's version. Whether it created the bare repository just now, the builder's boot time, its SDKs and
    /// what it said of each tool sent. A builder with no `gate box` that takes the run is not routed, as one of another
    /// version is not.
    fn preflight(&mut self, location: &Location, needs: &[(String, String)]) -> Result<Prepared, End> {
        self.go_on()?;
        let input = payload::input(&payload::preflight_word(location, needs), None);
        let sent: Vec<String> = needs.iter().map(|(name, _)| name.clone()).collect();
        let watch = Watch::Setup(self.limits_for("preflight"));
        let raise = self.raises_at("preflight");
        let (mut called, _) = self.with_retry(SSH_FAILED, |attempt| {
            attempt.ssh(BoxVerb::Preflight, Echo::LogOnly, watch, raise, CallOnly::plain(Arc::clone(&input)))
        })?;
        let theirs = their_version(&called.lines).map(str::to_owned);
        let created = called.lines.iter().any(|line| line.starts_with(CREATED));
        let boot = boot_time(&called.lines).map(str::to_owned);
        let sdks = sdks_of(&called.lines);
        let tools = toolchain::reported(&called.lines, &sent);
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
        // The builder's whole text must be this build's, so another build of the same package version is refused too.
        if let Some(line) = version_refusal(host, theirs.as_deref(), &build_version()) {
            self.say(line);
            return Err(End::Refused(FallbackReason::Version));
        }
        Ok(Prepared {
            created,
            boot,
            sdks,
            sent,
            tools,
        })
    }

    /// The snapshot's tree as the parity check reads it, listed before the preflight: the names of its files and
    /// submodules ([`Attempt::file_names`]) and its `.gate/toolchain.toml` ([`Attempt::manifest`]), the manifest found
    /// in a listing of the root and one of each `.gate` folder it names ([`manifest_candidates`]), so the names
    /// listing stays as small as one of names alone. A listing past the git output cap, or one that is not UTF-8 or not of
    /// `ls-tree`'s form, is said and gives `None`, which skips the whole check.
    fn tree(&mut self, snapshot: &Snapshot) -> Result<Option<Tree>, End> {
        self.go_on()?;
        let git = Git::find(Path::new(self.call.root)).map_err(End::Failed)?;
        let Some(names) = self.file_names(&git, &snapshot.commit)? else {
            return Ok(None);
        };
        let Some(root) = self.tree_entries(&git, &snapshot.commit)? else {
            return Ok(None);
        };
        let mut candidates = Vec::new();
        for gate in gate_folders(&root) {
            let Some(inside) = self.tree_entries(&git, &gate.oid)? else {
                return Ok(None);
            };
            candidates.extend(manifest_candidates(gate, &inside));
        }
        let manifest = self.manifest(&git, &candidates)?;
        Ok(Some(Tree { git, names, manifest }))
    }

    /// The names of the files and submodules of the snapshot's tree `treeish`, as its listing spells them, one per NUL;
    /// `None`, with a line, when that listing is past the git output cap or is not UTF-8.
    fn file_names(&mut self, git: &Git, treeish: &str) -> Result<Option<Vec<String>>, End> {
        let Some(listing) = self.listed(git, &["ls-tree", "-r", "-z", "--name-only", treeish])? else {
            return Ok(None);
        };
        Ok(Some(listing.split('\0').filter(|name| !name.is_empty()).map(str::to_owned).collect()))
    }

    /// The entries of `git ls-tree -z` over the tree `treeish` ([`entries_of`]), each with its mode, type, object id
    /// and path; `None`, with a line, when that listing is past the git output cap, is not UTF-8 or is not of that form.
    fn tree_entries(&mut self, git: &Git, treeish: &str) -> Result<Option<Vec<Entry>>, End> {
        let Some(listing) = self.listed(git, &["ls-tree", "-z", treeish])? else {
            return Ok(None);
        };
        let Some(entries) = entries_of(&listing) else {
            self.say("gate: cannot list the snapshot's files: an entry is not of git ls-tree's form; skipping the toolchain check".to_owned());
            return Ok(None);
        };
        Ok(Some(entries))
    }

    /// What one listing of `args` answered as text; `None`, with a line, when it is past the git output cap or is not
    /// UTF-8, either of which skips the whole toolchain check. Any other git failure ends the run.
    fn listed(&mut self, git: &Git, args: &[&str]) -> Result<Option<String>, End> {
        self.go_on()?;
        let listing = match self.git_bytes(git, args) {
            Ok(listing) => listing,
            Err(GitError::OverCap(why)) => {
                self.say(format!("gate: cannot list the snapshot's files: {why}; skipping the toolchain check"));
                return Ok(None);
            }
            Err(error) => return Err(git_end(error)),
        };
        let Ok(listing) = String::from_utf8(listing) else {
            self.say("gate: cannot list the snapshot's files: a path is not valid UTF-8; skipping the toolchain check".to_owned());
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

    /// The toolchain parity check, made after the preflight and before the push, so a refusal costs no transfer, over
    /// the snapshot's `tree`, which `None` skips: the builder must have the SDK [`Attempt::sdk_needs`] reads and the
    /// tools the manifest declares, as [`toolchain::judge`] weighs what the `prepared` preflight said of them. The SDK
    /// step's own lines come first, its "cannot read this PC's SDK list" among them; then each manifest problem and
    /// tool not checked, in order; then a builder that lacks any of it says one mismatch line per need, the SDK's
    /// before the tools', and is refused `parity` once.
    fn parity(&mut self, snapshot: &Snapshot, tree: Option<&Tree>, prepared: &Prepared) -> Result<(), End> {
        let Some(tree) = tree else {
            return Ok(());
        };
        self.go_on()?;
        let mut needs = self.sdk_needs(snapshot, tree, prepared.sdks.as_deref())?;
        let judged = toolchain::judge(tree.manifest.as_ref(), &prepared.sent, &prepared.tools, self.call.host);
        for line in judged.skips {
            self.say(line);
        }
        needs.extend(judged.needs);
        if needs.is_empty() {
            return Ok(());
        }
        let sdks = prepared.sdks.as_deref().unwrap_or_default();
        for line in parity::lines(self.call.repo, &needs, self.call.host, sdks) {
            self.say(line);
        }
        Err(End::Refused(FallbackReason::Parity))
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
            Some(name) => match self.git_bytes(&tree.git, &["show", &format!("{}:{name}", snapshot.commit)]) {
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
        let call = git::Call {
            interrupt: self.call.interrupt,
            ..git::Call::plain(Path::new(self.call.root), args, self.limit)
        };
        git.output_bytes(&call)
    }

    /// This PC's newest .NET SDK major, from `--test-local-sdks` when given, else from `dotnet --list-sdks`; 0 when no
    /// dotnet is found or it lists no SDK. A list that cannot be read is said, and checks no major; one whose read
    /// failed while the console interrupt is raised says nothing and checks no major, and [`Attempt::parity`] then ends
    /// the run as an interrupt. `--test-remote-interrupt-at sdks` raises the interrupt just before the read.
    fn local_major(&mut self) -> u64 {
        if let Some(listing) = self.local_sdks {
            return parity::newest_major(parity::listed(listing).iter().map(String::as_str));
        }
        if let Some(interrupt) = self.call.interrupt.filter(|_| self.raises_at("sdks")) {
            interrupt.raise();
        }
        let worktree = Path::new(self.call.root);
        match local_sdk_listing(Variables::from_process(), worktree, self.dotnet_limit, self.call.interrupt) {
            Ok(listing) => parity::newest_major(parity::sdk_versions(&listing)),
            Err(_) if self.call.interrupt.is_some_and(Interrupt::is_raised) => 0,
            Err(error) => {
                let why = String::from(error);
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
        let target = self.transport.push_target(self.call.host, self.call.repo).map_err(End::Failed)?;
        let mut args: Vec<String> = ["push", "--no-verify", "--no-recurse-submodules", "--progress"].map(str::to_owned).into();
        args.extend(target.receive_pack.iter().map(|pack| format!("--receive-pack={pack}")));
        args.extend(["--".to_owned(), target.url.clone(), format!("+{}:refs/gate/{key}", snapshot.commit)]);
        let bridge = snapshot.bridge.as_deref().filter(|_| !created);
        args.extend(bridge.map(|bridge| format!("+{bridge}:refs/gate-bridge/{key}")));
        let git = Git::find(root).map_err(End::Failed)?;
        let started = Instant::now();
        let ssh_command = target.ssh_command.as_deref().map(|ssh| ("GIT_SSH_COMMAND", std::ffi::OsStr::new(ssh)));
        let watch = Watch::Setup(self.limits_for("push"));
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
        self.split.push = started.elapsed();
        self.say(format!("gate: pushed snapshot {short} in {} s ({size})", seconds(self.split.push)));
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

    /// The run call to the builder at `location`: the command's own status; `unreachable` when it failed before the
    /// banner twice, as the command never started; a refusal when its 75 comes with the box's refusal tag for the run
    /// whose nonce is `nonce` and no start marker; a lost connection when it exited 255 otherwise. A watchdog kill says
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
        let mut adjusted = None;
        // The command's time counts from the start of the call that counts, as its backstop does.
        let mut begun = None;
        let made = self.run_call_made(location, limits, |attempt| {
            begun = Some(Instant::now());
            let run = CallOnly {
                input: Arc::clone(input),
                nonce: Some(Arc::clone(&nonce)),
                sink: None,
                wire: Wire::Compressed,
            };
            let called = attempt.ssh(BoxVerb::Run, Echo::Console, Watch::Run(limits), raise, run)?;
            heard.clone_from(&called.outputs_line);
            back_commits.clone_from(&called.back_commits);
            names_skipped = called.names_skipped;
            adjusted = called.adjusted;
            Ok(called)
        });
        self.split.command = begun.map_or(Duration::ZERO, |begun| begun.elapsed());
        self.adjusted = adjusted;
        let end = made?;
        Ok(RunEnd {
            end,
            outputs_line: heard,
            back_commits,
            names_skipped,
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
        let reason = called.refusal();
        let kept = self.lines.len();
        self.lines.append(&mut called.lines);
        let output = kept..self.lines.len();
        if let Some(Cut::Watchdog(kill)) = called.cut {
            self.say(watchdog::kill_line(
                kill,
                limits.stall,
                limits.backstop,
                limits.command.as_secs(),
                self.call.host,
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
            self.stop_after_ctrl_c(location);
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
        let what = format!("asking {} for its boot time", self.call.host);
        let called = self.bounded(BoxVerb::Boot, location, &what)?;
        (called.status == 0 && called.cut.is_none())
            .then(|| boot_time(&called.lines).map(str::to_owned))
            .flatten()
    }

    /// Stops the command the run call started on the builder at `location`, with the `stop` call, a bounded call; the
    /// run's end is the caller's however it went.
    fn stop(&mut self, location: &Location) {
        let what = format!("stopping the command on {}", self.call.host);
        self.bounded(BoxVerb::Stop, location, &what);
    }

    /// [`Attempt::stop`] after a Ctrl+C, saying first how long the stop may take, its bound in whole seconds, so the
    /// wait that follows the Ctrl+C is explained. A Ctrl+C during the stop does not end it and says nothing more.
    fn stop_after_ctrl_c(&mut self, location: &Location) {
        let bound = watchdog::whole_seconds(self.limits.bound);
        self.say(format!("gate: stopping the command on {} (up to {bound} s)", self.call.host));
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
            self.say(retry::retry_line(self.call.host));
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

    /// One call of `gate box` on the builder for `verb`, its line [`payload::line`], fed its request on its standard
    /// input; watched as `watch` says; the console interrupt raised after its first output byte when `raise`; what only
    /// this call gives it, `only`.
    fn ssh(&mut self, verb: BoxVerb, echo: Echo, watch: Watch, raise: bool, only: CallOnly) -> Result<Called, End> {
        let system = gate_win32::system_directory().map_err(|error| End::Failed(format!("cannot read the system folder: {error}")))?;
        let command = call_command(self.transport.call(self.call.host, &payload::line(verb), only.wire), &system);
        let starts = Starts {
            breakaway: self.transport.box_runs_here(),
            raise,
            input: Some(Fed { verb, input: only.input }),
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
        let input = payload::input(&payload::outputs_word(location, name), None);
        let raise = self.raises_at("outputs");
        let (mut called, _) = self.with_retry(SSH_FAILED, |attempt| {
            let cap = sent_limit(attempt.outputs_budget);
            let only = CallOnly {
                sink: Some(archive.create(cap).map_err(End::Failed)?),
                ..CallOnly::plain(Arc::clone(&input))
            };
            attempt.ssh(BoxVerb::Outputs, Echo::LogOnly, Watch::Fetch(limits, None), raise, only)
        })?;
        self.lines.append(&mut called.lines);
        Ok(called)
    }

    /// git with `args` in the worktree, given the variables `set` beside the ones every call is given, among them the ssh
    /// it runs as `GIT_SSH_COMMAND` when there is one, its lines going where `echo` says, watched as `watch` says.
    pub(super) fn git(&mut self, git: &Git, args: &[String], set: &[(&str, &std::ffi::OsStr)], echo: Echo, watch: Watch) -> Result<Called, End> {
        let command = git.command(Path::new(self.call.root));
        self.git_started(command, args, set, echo, watch)
    }

    /// git with `args` in the worktree at `folder`, a pair's sibling's among them, made as [`Attempt::git`] makes its
    /// call, its lines kept out of the log.
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

    /// The git `command` with `args` and the variables `set` started as [`Attempt::git`] starts it.
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
        let (suspended, stdout, stderr, stdin) = launched(&launch, &job, starts.input.is_some()).map_err(|error| cannot_start(error.to_string()))?;
        let running = suspended.assign_and_resume(&job).map_err(|error| cannot_start(error.to_string()))?;
        let feeder = stdin.zip(starts.input).map(|(stdin, input)| fed(stdin, input));
        let shared = Arc::new(Shared::new());
        // Only the run call carries the run's nonce, and only it says the hang hint.
        let run_call = starts.nonce.is_some();
        let (done, readers) = mpsc::channel();
        let reading = Reading {
            echo,
            capped: echo != Echo::Console,
            lines: starts.nonce.as_deref().map(|nonce| Arc::new(ProtocolLines::new(nonce))),
            log_cap: self.log_cap,
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
            hang: self
                .call
                .usual
                .filter(|_| run_call && !self.hang_said)
                .map(|usual| RunHang { usual, log: self.call.log }),
        };
        let raise = self.call.interrupt.filter(|_| starts.raise);
        let watching = watched.watch(watch, self.call.interrupt, raise);
        if let Some(raw) = raw {
            // The call's end has come: the copy writes nothing more into an archive the fetch is about to remove.
            shared.stop();
            let drained = watching.ended.as_ref().is_ok_and(|(ended, _)| ended.drained());
            self.ended_raw(raw, drained);
        }
        let (ended, looks) = self.watched_out(watching)?;
        if let Some(line) = &looks.unread {
            self.logged(line);
        }
        if watch.byte_cap().is_some() && !matches!(ended, CallEnd::Exited(0)) {
            self.emptied(&job, &shown);
        }
        let heard = std::mem::take(&mut *held(&shared.heard));
        match called_of(ended, shared.progress.raised(), heard) {
            Ok(called) => {
                if let Some(feeder) = feeder {
                    self.fed_out(feeder, &called);
                }
                Ok(Called {
                    written: looks.written,
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
            brought_back: self.brought_back,
            split: self.split,
            adjusted: self.adjusted,
            box_killed: self.box_killed,
        }
    }
}

#[cfg(all(test, feature = "test-seams"))]
impl<'c, 'a> Attempt<'c, 'a> {
    /// An attempt of `call` through `transport` with no pause before a retry, as the sync-back's tests drive one.
    pub(super) fn for_tests(call: &'c RemoteCall<'a>, transport: &'c dyn Transport) -> Attempt<'c, 'a> {
        Attempt {
            call,
            transport,
            limit: git::GIT_LIMIT,
            lock_wait: git::GIT_LIMIT,
            pause: Duration::ZERO,
            limits: Limits::for_tests(),
            one_call: None,
            interrupt_at: None,
            local_sdks: None,
            dotnet_limit: git::DOTNET_LIMIT,
            fetch_cap: watchdog::FETCH_CAP,
            apply_cap: watchdog::APPLY_CAP,
            outputs_budget: OUTPUTS_BUDGET,
            log_cap: watchdog::LOG_CAP,
            own_start_fails: false,
            brought_back: None,
            split: Split::default(),
            lines: Vec::new(),
            problem: None,
            hang_said: false,
            adjusted: None,
            box_killed: None,
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
        back_commits: heard.back_commits,
        names_skipped: heard.names_skipped,
        started: heard.started,
        refused: heard.refused,
        box_killed: heard.box_killed,
        adjusted: heard.adjusted,
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

/// What watching a call came to: how it ended, with what the looks at the bytes its job wrote found, or why it could not
/// be watched; and the run call's hang hint once said, or why it could not be said, which stands either way.
struct Watching {
    ended: Result<(CallEnd, Looks), End>,
    hinted: Option<Result<String, String>>,
}

/// The hang hint a run call says once its command has run 3x its usual p90 on the builder: that p90, and the log the
/// hint is written to beside stderr.
#[derive(Clone, Copy)]
struct RunHang<'w> {
    usual: Usual,
    log: &'w Path,
}

/// What the looks at the bytes a capped call's job wrote found: the count at the last look that succeeded, and the
/// line saying the first look that failed.
#[derive(Default)]
struct Looks {
    written: Option<u64>,
    unread: Option<String>,
}

/// A started call the client watches: its process and job, when its streams last sent a byte, the channel each of its
/// two readers says its end on, its program as shown, and the run call's hang hint while it is still to be said.
struct Watched<'w> {
    running: &'w Running,
    job: &'w Job,
    progress: &'w Progress,
    readers: &'w Receiver<()>,
    shown: &'w str,
    hang: Option<RunHang<'w>>,
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
        loop {
            if hinted.is_none() {
                *hinted = self.hint_if_hung();
            }
            if open == 0
                && let Some(code) = self.running.wait(Duration::ZERO).map_err(|error| cannot_wait(self.shown, &error))?
            {
                let over = watch.over(self.written(watch, &mut looks));
                let ended = over.map_or(CallEnd::Exited(code.cast_signed()), CallEnd::ExitedOver);
                return Ok((ended, looks));
            }
            if (self.progress.heard_any() || !watch.interruptible())
                && let Some(interrupt) = raise.take()
            {
                interrupt.raise();
            }
            if watch.interruptible() && interrupt.is_some_and(Interrupt::is_raised) {
                let ended = CallEnd::Interrupted(self.end(open));
                return Ok((ended, looks));
            }
            let written = self.written(watch, &mut looks);
            if let Some(cut) = watch.cut(Instant::now(), begun, self.progress.idle(), self.progress.raised(), written) {
                let ended = CallEnd::Cut(cut, self.end(open));
                return Ok((ended, looks));
            }
            open -= self.readers_ended(open, tick);
        }
    }

    /// Says the run call's hang hint, on stderr and in the log, once its command has been running on the builder 3x its
    /// usual p90, counted from the box's start marker: the line said, or why it could not be said; `None` before then,
    /// and for every other call.
    fn hint_if_hung(&self) -> Option<Result<String, String>> {
        let hang = self.hang?;
        let running = self.progress.since_started()?;
        let line = hang_hint(running.as_secs_f64(), hang.usual)?;
        Some(super::say(hang.log, &[&line], io::stderr().lock()).map(|()| line))
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
/// match the box's start marker and refusal tags by, `None` for every call but the run call; and the sink its standard
/// output is copied to raw, `None` for every call but the outputs call.
struct Starts {
    breakaway: bool,
    raise: bool,
    input: Option<Fed>,
    nonce: Option<Arc<str>>,
    sink: Option<RawSink>,
}

/// What a call of `gate box` is fed on its standard input, `input`, and the `verb` of the call, which the line saying
/// it could not be sent names.
struct Fed {
    verb: BoxVerb,
    input: Arc<str>,
}

/// The thread feeding a call of `gate box` the `verb` its input, which gives back how the write went.
struct Feeder {
    verb: BoxVerb,
    thread: thread::JoinHandle<io::Result<()>>,
}

/// What one call of `gate box` alone gives it: what is fed on its standard input, the request ([`payload::input`])
/// every call carries; the run call the run's nonce and a compressed connection, the outputs call the sink its
/// standard output is copied to raw; neither of them, and a plain connection, for any other call.
struct CallOnly {
    input: Arc<str>,
    nonce: Option<Arc<str>>,
    sink: Option<RawSink>,
    wire: Wire,
}

impl CallOnly {
    /// A call fed `input` alone, over a plain connection.
    fn plain(input: Arc<str>) -> CallOnly {
        CallOnly {
            input,
            nonce: None,
            sink: None,
            wire: Wire::Plain,
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
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Writes what `fed` gives to a call's `stdin` on a thread of its own, then closes it; the thread gives back how the
/// write went, which [`Attempt::fed_out`] reads once the call has ended.
fn fed(mut stdin: File, fed: Fed) -> Feeder {
    let Fed { verb, input } = fed;
    let thread = thread::spawn(move || stdin.write_all(input.as_bytes()));
    Feeder { verb, thread }
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
struct Heard {
    kept: Kept,
    output_seen: bool,
    first_error: Option<String>,
    last_error: Option<String>,
    outputs_line: Option<String>,
    back_commits: Option<String>,
    names_skipped: bool,
    started: bool,
    refused: Option<FallbackReason>,
    box_killed: Option<BoxKill>,
    adjusted: Option<f64>,
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
        self.kept_line(line, error);
    }

    /// Marks what the box's line read as `protocol` says: the run started, its first refusal, its last kill tag, its last
    /// load-adjusted seconds.
    fn marked(&mut self, protocol: Protocol) {
        match protocol {
            Protocol::Started => self.started = true,
            Protocol::Refused(reason) => {
                self.refused.get_or_insert(reason);
            }
            Protocol::Killed(kill) => self.box_killed = Some(kill),
            Protocol::Adjusted(seconds) => self.adjusted = Some(seconds),
        }
    }

    /// Keeps `line`, already cut, of standard error when `error`: the last error line, the outputs line, the back
    /// commits line and the skipped names it may be, and the line among those kept for the log.
    fn kept_line(&mut self, line: &str, error: bool) {
        if error {
            self.last_error = Some(line.to_owned());
        }
        outputs::keep_outputs_line(&mut self.outputs_line, line, error);
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
            started: AtomicU64::new(NOT_STARTED),
        }
    }

    /// Marks a byte that came now.
    fn mark(&self) {
        let millis = u64::try_from(self.begun.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_byte.store(millis, Ordering::Relaxed);
        self.any.store(true, Ordering::Relaxed);
    }

    /// Marks the box's start marker of the run, which came now.
    fn mark_started(&self) {
        let millis = u64::try_from(self.begun.elapsed().as_millis()).unwrap_or(NOT_STARTED - 1);
        self.started.store(millis, Ordering::Relaxed);
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
        if protocol == Some(Protocol::Started) {
            shared.progress.mark_started();
        }
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

    /// Writes `text`, with a line end when `end`, to the log unless the reading keeps it out or the call's output has
    /// passed the log cap, which the log is told once, and on its own stream when it echoes to the console, past the
    /// cap too; the first failure to is kept.
    fn passed(&mut self, text: &str, end: bool, shared: &Shared) {
        let mut said = String::with_capacity(text.len() + 1);
        said.push_str(text);
        if end {
            said.push('\n');
        }
        if let Some(log) = &mut self.log {
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

/// The line the log of a call whose output passed the log cap `cap` gets, once, with its line end.
fn log_cut_line(cap: u64) -> String {
    format!(
        "gate: the builder's output passed {}; the rest is not logged\n",
        watchdog::whole_size(cap)
    )
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

/// Copies `stream` into `sink`, shown as `shown`, byte for byte until it ends, in chunks of at most [`RAW_CHUNK`]: no
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

/// What `dotnet --list-sdks` lists on this PC, as [`git::sdk_listing`] reads it, with the folders inside `worktree`
/// dropped from the `PATH` of `variables`, each compared by its canonical path so a junction does not get around it,
/// and a dotnet found that still lies inside it refused. A `PATH` that cannot be read that way cannot be read at all.
pub(crate) fn local_sdk_listing(variables: Variables, worktree: &Path, limit: Duration, interrupt: Option<Interrupt>) -> Result<String, GitError> {
    let variables = outside_worktree(variables, worktree).map_err(GitError::Failed)?;
    git::sdk_listing(variables, limit, interrupt, Some(worktree))
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
/// said just before the command starts, a refusal tag and the fallback reason it names, a kill tag and the kill it
/// names, or the adjusted line and the load-adjusted seconds it carries.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Protocol {
    Started,
    Refused(FallbackReason),
    /// The box's kill tag, said just after its kill line.
    Killed(BoxKill),
    /// The box's adjusted line, said once the command's readers drained.
    Adjusted(f64),
}

/// The start marker, the three refusal tags, the two kill tags and the adjusted line's prefix of one run, made once for
/// its run call's readers.
struct ProtocolLines {
    started: String,
    busy: String,
    tools: String,
    layout: String,
    timed_out: String,
    backstop: String,
    adjusted: String,
}

impl ProtocolLines {
    /// The lines of the run whose nonce is `nonce`.
    fn new(nonce: &str) -> ProtocolLines {
        ProtocolLines {
            started: started_line(nonce),
            busy: refused_line(nonce, Refusal::Busy),
            tools: refused_line(nonce, Refusal::Tools),
            layout: refused_line(nonce, Refusal::Layout),
            timed_out: killed_line(nonce, BoxKill::TimedOut),
            backstop: killed_line(nonce, BoxKill::Backstop),
            adjusted: adjusted_prefix(nonce),
        }
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
            (&self.timed_out, Protocol::Killed(BoxKill::TimedOut)),
            (&self.backstop, Protocol::Killed(BoxKill::Backstop)),
        ]
        .into_iter()
        .find_map(|(tag, protocol)| (line == tag.as_str()).then_some(protocol))
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

    use super::{
        Attempt, CallEnd, Called, Capped, Cut, Echo, End, GIT_SSH_FAILED, Heard, LINE_CAP, Limits, Loss, NonZeroU64, OUTPUTS_BUDGET, ProtocolLines,
        Raised, Reader, Reading, RustClient, SSH_FAILED, Shared, Split, Ssh, Unfinished, Watch, Watching, after_loss, boot_time, called_of,
        console_of, held, is_heartbeat, last_update, local_sdk_listing, path_outside, paused, push_size, relay, relay_raw, relayed_raw, sdks_of,
        seconds, their_version, version_refusal,
    };
    use super::{Entry, entries_of, gate_folders, manifest_candidates, manifest_entry};
    use crate::environment::Variables;
    use crate::remote::git::{DOTNET_LIMIT, GIT_LIMIT, GitError};
    use crate::remote::outputs;
    use crate::remote::payload;
    use crate::remote::payload::Location;
    use crate::remote::snapshot::Snapshot;
    use crate::remote::transport::Local;
    use crate::remote::watchdog::{self, Kill, LOG_CAP};
    use crate::remote::{Refusal, RemoteCall, adjusted_line, busy_line, refused_line, started_line};
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

    /// A Ctrl+C before this PC's SDK list is read starts no dotnet and ends the read as interrupted: the `dotnet.cmd`
    /// its `PATH` holds writes no mark.
    #[test]
    fn an_interrupted_sdk_list_read_starts_no_dotnet() {
        let folder = std::env::temp_dir().join(format!("gate-client-dotnet-{}", std::process::id()));
        fs::create_dir_all(&folder).expect("the stub's folder can be made");
        let marker = folder.join("ran");
        let stub = format!("@echo off\r\n>\"{}\" echo ran\r\necho 9.0.100 [C:\\stub]\r\n", marker.display());
        fs::write(folder.join("dotnet.cmd"), stub).expect("the stub dotnet can be written");
        let variables = Variables::from_process().with("PATH", &folder.to_string_lossy());
        let worktree = Path::new(env!("CARGO_MANIFEST_DIR"));

        let read = local_sdk_listing(variables, worktree, DOTNET_LIMIT, Some(raised()));

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
        let log = std::env::temp_dir().join(format!("gate-client-log-cap-{}.log", std::process::id()));
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
        let said = fs::read_to_string(&log).expect("the log can be read");
        fs::remove_file(&log).expect("the log can be removed");

        let cut = "gate: the builder's output passed 4 KiB; the rest is not logged";
        assert_eq!(said.lines().filter(|line| *line == cut).count(), 1, "{said}");
        let logged: Vec<&str> = said.lines().filter(|line| *line != cut).collect();
        assert_eq!(logged.len(), 40, "the whole lines that fit under the cap: {said}");
        assert!(logged.iter().all(|line| line.len() == 100), "{said}");
    }

    /// A call whose interrupt is raised, logging to `log`, which no test here writes.
    fn call(log: &Path) -> RemoteCall<'_> {
        RemoteCall {
            host: "builder",
            key: "app-1a2b3c4d",
            root: r"C:\no-such-worktree",
            leaf: "app",
            siblings: Vec::new(),
            repo: "app",
            subfolder: "",
            words: &[],
            outputs: &[],
            outputs_withheld: false,
            log,
            timeout: 60,
            usual: None,
            stall: 60,
            tail: 0,
            no_markers: false,
            bring_tracked: &[],
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
            lock_wait: GIT_LIMIT,
            pause: LONG,
            limits: Limits::for_tests(),
            one_call: None,
            interrupt_at: None,
            local_sdks: None,
            dotnet_limit: DOTNET_LIMIT,
            fetch_cap: watchdog::FETCH_CAP,
            apply_cap: watchdog::APPLY_CAP,
            outputs_budget: OUTPUTS_BUDGET,
            log_cap: LOG_CAP,
            own_start_fails: false,
            brought_back: None,
            split: Split::default(),
            lines: Vec::new(),
            problem: None,
            hang_said: false,
            adjusted: None,
            box_killed: None,
        }
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
            back_commits: None,
            names_skipped: false,
            started: false,
            refused: None,
            box_killed: None,
            adjusted: None,
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
            command: 60 * second,
            backstop: 360 * second,
            transfer: 60 * second,
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

    /// The line of an outputs call cut for the archive it passed, at the cap a 256 KiB budget gives.
    const CAPPED_LINE: &str = "gate: remote outputs not brought back: the archive passed 256.0 KiB";

    /// An empty state folder of the test `name`'s own, which a cap hit writes its outputs cap record under.
    fn fresh_state(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-client-state-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a state folder left by an earlier run can be removed");
        }
        folder
    }

    /// The outputs cap record of the unit call's key under `state`.
    fn capped_record(state: &Path) -> PathBuf {
        gate_state::v1::outputs_capped_path(state, "app-1a2b3c4d")
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
        let end = outputs::judged(&mut attempt, &archive, &cut, End::Status(0));
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

        let end = outputs::judged(&mut attempt, &archive, &cut, End::Status(0));

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

        let end = outputs::judged(&mut attempt, &archive, &failed, End::Status(0));

        assert!(matches!(end, End::Interrupted), "{end:?}");
        assert_eq!(attempt.lines, Vec::<String>::new());
    }

    /// A worktree folder of the test `name`'s own, made empty, holding the output `bin/a.txt` when `held`.
    fn outputs_worktree(name: &str, held: bool) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-client-worktree-{name}-{}", std::process::id()));
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
    /// output globs, the run call's status and the one the sync-back left, what the box said it packed, and whether it
    /// said it skipped outputs for names.
    struct Step<'s> {
        root: &'s Path,
        globs: bool,
        status: i64,
        synced: i64,
        brought: Option<outputs::Brought>,
        names_skipped: bool,
    }

    /// The outputs step of a passing run with output globs in `root`, the box having said it packed `brought` and, when
    /// `names_skipped`, that it skipped outputs for names.
    fn passed(root: &Path, brought: Option<outputs::Brought>, names_skipped: bool) -> Step<'_> {
        Step {
            root,
            globs: true,
            status: 0,
            synced: 0,
            brought,
            names_skipped,
        }
    }

    /// Whether an attempt's outputs are brought back once its outputs step ended as `step` says, through a transport
    /// whose every call fails to start, the state in a fresh folder of the test `name`'s own.
    fn brought_back_after(name: &str, step: &Step<'_>) -> Option<bool> {
        let state = fresh_state(name);
        let log = unwritten_log();
        let globs = ["**/bin/**".to_owned()];
        let mut call = call(&log);
        call.interrupt = None;
        call.root = step.root.to_str().expect("a test worktree's path is Unicode");
        call.state_dir = &state;
        call.outputs = if step.globs { &globs[..] } else { &[] };
        let transport = nowhere();
        let mut attempt = Attempt::for_tests(&call, &transport);
        let sent = outputs::sent(&mut attempt).expect("no interrupt ends the manifest's build");
        let location = location();
        let ran = outputs::Ran {
            location: &location,
            nonce: "0123456789ab",
            status: step.status,
            names_skipped: step.names_skipped,
        };
        let synced = crate::remote::syncback::Synced {
            end: End::Status(step.synced),
            reason: None,
        };
        outputs::ended(&mut attempt, &sent, &ran, synced, step.brought.as_ref());
        attempt.brought_back
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
        assert_eq!(called.outputs_line.as_deref(), Some(line), "the last standard error outputs line is kept");
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
        let log = std::env::temp_dir().join(format!("gate-client-protocol-{}-{test}.log", std::process::id()));
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

        assert!(matches!(attempt.preflight(&location, &[]), Err(End::Interrupted)), "the preflight");
        assert!(matches!(attempt.push(&snapshot, false), Err(End::Interrupted)), "the push");
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
            Some("gate: not routed: builder runs gate 0.2.0, this PC 0.1.0; run gate push".to_owned())
        );
        assert_eq!(their_version(&lines(&["gate: remote boot x"])), None);
        assert_eq!(
            version_refusal("builder", None, "0.1.0"),
            Some("gate: not routed: builder runs gate unknown, this PC 0.1.0; run gate push".to_owned())
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

    fn entry(mode: &str, kind: &str, path: &str) -> Entry {
        Entry {
            mode: mode.to_owned(),
            kind: kind.to_owned(),
            oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            path: path.to_owned(),
        }
    }

    #[test]
    fn a_listing_reads_each_entry_s_mode_type_object_and_path() {
        let listing = "100644 blob 0123456789abcdef0123456789abcdef01234567\t.gate/toolchain.toml\0\
                       040000 tree 0123456789abcdef0123456789abcdef01234567\tsrc dir\0";
        assert_eq!(
            entries_of(listing),
            Some(vec![entry("100644", "blob", ".gate/toolchain.toml"), entry("040000", "tree", "src dir")])
        );
        assert_eq!(entries_of(""), Some(Vec::new()));
        assert_eq!(entries_of("100644 blob\0"), None, "no tab");
        assert_eq!(entries_of("100644 blob x y\ta\0"), None, "four words");
    }

    /// The manifest's candidates are built from one listing of the root and one of each `.gate` folder it names: the
    /// exact spelling at the repo root is read, a case variant alone is, two at once differ only in letter case, and a
    /// `.gate` that is no folder holds no manifest at all.
    #[test]
    fn the_manifest_s_entries_are_built_from_a_root_listing_and_a_gate_listing() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let record = |mode: &str, kind: &str, name: &str| format!("{mode} {kind} {oid}\t{name}\0");
        let candidates = |root: &str, inside: &str| -> Vec<Entry> {
            let root = entries_of(root).expect("the root listing reads");
            let inside = entries_of(inside).expect("the .gate listing reads");
            gate_folders(&root)
                .into_iter()
                .flat_map(|gate| manifest_candidates(gate, &inside))
                .collect()
        };

        let exact = candidates(&record("040000", "tree", ".gate"), &record("100644", "blob", "toolchain.toml"));
        assert_eq!(
            exact,
            vec![entry("100644", "blob", ".gate/toolchain.toml")],
            "the folder and the name as spelled"
        );
        assert_eq!(manifest_entry(&exact), Some(Ok(&exact[0])), "and that is the one read");

        let variant = candidates(&record("040000", "tree", ".Gate"), &record("100644", "blob", "Toolchain.TOML"));
        assert_eq!(variant, vec![entry("100644", "blob", ".Gate/Toolchain.TOML")], "each part as spelled");
        assert_eq!(manifest_entry(&variant), Some(Ok(&variant[0])), "a case variant alone is read too");

        let both = format!(
            "{}{}",
            record("100644", "blob", "toolchain.toml"),
            record("100644", "blob", "Toolchain.TOML")
        );
        assert_eq!(
            manifest_entry(&candidates(&record("040000", "tree", ".Gate"), &both)),
            Some(Err(".Gate/toolchain.toml and .Gate/Toolchain.TOML differ only in letter case".to_owned()))
        );

        assert_eq!(
            candidates(&record("100644", "blob", ".gate"), &record("100644", "blob", "toolchain.toml")),
            Vec::new(),
            "a .gate that is no folder holds no manifest"
        );
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
        assert_eq!(client.limit, GIT_LIMIT, "a request with no seams keeps the constants");
        assert_eq!(client.dotnet_limit, DOTNET_LIMIT, "a request with no seams keeps the constants");
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
}
