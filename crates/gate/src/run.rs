//! `gate run`: a log inside the state root refused, the parent gate's slot marker read and a critical gate under a heavy
//! or light one refused, a gate under an orphaned script refused, the program found and the words written into its
//! command line, a batch word refused, the log's mutex taken so no other gate writes the same log, the log readied, a
//! slot queued for and taken unless a parent's slot covers the gate, the command started in its job with its environment
//! and priority and waited for, the log completed, the failure markers scanned, the tail and the verdict line said, and
//! the slot released.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::windows::ffi::OsStrExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{fs, io};

use gate_wire::stamp::{self, Tag};

use gate_args::{
    Ceiling, ProgramSearch, RunRequest, Slot, Spelling, batch_command_line, batch_refusal, command_line, is_batch_target, resolve_program,
};
use gate_inputs::glob::names_tmp;
use gate_router::warm::{RecordRead, RecordState};
use gate_router::{
    BuildSettings, BuildTarget, DEFAULT_OUTPUTS, LaterCandidate, PinRefusal, Place, RemoteRun, Route, build_settings, builds_in_place, dotnet_builds,
    dotnet_target, sdk_build_target, without_no_build, work_folder_key,
};
use gate_state::{
    Estimate, FallbackReason, JournalEvent, KernelObject, RebuildOwner, RemoteTag, RoutingFacts, RunFacts, RunTrace, StateRoot, Verdict, key_hash,
    log_hash, v1,
};
use gate_watchdog::{Cores, Hint, SlotKind, hint};
use gate_win32::{
    AcquireError, Interrupt, Job, JobEvent, Launch, LaunchError, LaunchOutput, NamedMutex, OpenedJob, OwnedMutexGuard, PriorityClass, Running,
    StoppableProcess, Suspended,
};
use jiff::{SignedDuration, Timestamp};
use proc_table::{ProcessTree, Win32ProcessTable};
use serde_json::{Map, Value};

use crate::acquire;
use crate::answer::USAGE_STATUS;
use crate::boxside::forward::{DRAIN, Forwarders, OutputBytes, Sink, Stamping};
use crate::commit;
use crate::console::Console;
use crate::covered::Covered;
use crate::durations::{
    Resolved, RunDurations, RunIdentity, Usual, ceiling_hint, expected_field, expected_line, raised_ceiling_line, resolve_ceiling, span_text,
};
use crate::environment::{self, BOX_JOB_VARIABLE, COMMIT_SLOT_VARIABLE, SLOT_HELD_VARIABLE, Variables};
use crate::escaped::{self, Found, Reach, Reached, Recorded};
use crate::journal::{self, RunJournal};
use crate::late::{Late, LateOf, LateSlot};
use crate::load::{self, Measuring, When};
use crate::log::Log;
use crate::log_writer::LogWriter;
use crate::machine::{self, Machine, MachineWait, MachineWaited};
use crate::nesting::{Nesting, Parent, covers, enclosing_of};
use crate::no_build::{self, NothingBack, Unfound, Worktree};
use crate::orphan::{self, Orphan};
use crate::outputs::{stale, walk::walk};
use crate::remote::drain::{self, Drain, utc_clock};
use crate::remote::{self, KEY_WAIT_SLICE, KeyWait, RemoteCall, RemoteClient, RemoteOutcome, Split, git};
use crate::resolve::{Environment, ResolvedRoot};
use crate::restore::{self, NotRestoring, RestorePlan};
use crate::route::{self, Placed};
use crate::slots::{self, BuilderPool, ClaimOf, LendMarker, Pool, Survey, Taken, Takes, ThisGate, epoch_ms};
use crate::stopping::failure;
use crate::tailscale;
use crate::wait::{self, Arrival, Host, UnderParent, Wait, Waited};
use crate::warm;
use crate::watch::{Start, Watched, Watcher, Watching};
use crate::worktree_record;

/// The status of a run the gate itself failed.
const GATE_FAILED: i64 = 1;
/// The status of a command that exited 0 with a failure marker in its log.
const LOG_FAILURE: i64 = 1;
/// The status of a command that could not be started.
const NOT_STARTED: i64 = 127;
/// The exit code the job's processes are ended with when the gate stops the command.
const STOPPED: u32 = 124;
/// `STATUS_CONTROL_C_EXIT`: the status of a process Ctrl+C ended, which the gate exits with when interrupted.
pub(crate) const CONTROL_C_EXIT: u32 = 0xC000_013A;
/// The Win32 error of a file not found, which a program found nowhere is reported as.
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
/// How long the gate waits for the command to end once it has terminated its job, or once a Ctrl+C came.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// The image of a console host, which the run's end never ends.
const CONSOLE_HOST_IMAGE: &str = "conhost.exe";
/// How many passes over its job the run's end makes to end what the command left there, each ending what the last
/// one's processes started meanwhile.
const END_PASSES: usize = 5;
/// The wall time under which a passed run is too short to judge its slot by, so it gets no cores line.
const HINT_MIN_WALL: Duration = Duration::from_secs(10);
/// The unpaused wall time under which a passed light run is too short to ask whether its command would route on heavy.
const ROUTE_HINT_MIN_WALL: Duration = Duration::from_secs(30);
/// How long a routed run waits for its worktree's work folder while another of its runs holds it, before it falls back
/// here as `busy` ([`Run::take_key`]); also the bound of a finished local build's one waiting take of the key, which no
/// drain marker holds off ([`KeyTake::Bounded`]).
const KEY_WAIT: Duration = Duration::from_secs(90);
/// How long a passing build here retries the work folder's key to write its built-here record, as another write here
/// holds it for milliseconds ([`Run::try_folder_key`]).
const BRIEF_KEY_WAIT: Duration = Duration::from_secs(2);

/// How long [`Run::try_folder_key`] waits for the work folder's key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyTake {
    /// [`BRIEF_KEY_WAIT`], for a built-here write.
    Brief,
    /// [`KEY_WAIT`], bounded hard: a live drain marker does not hold it off.
    Bounded,
    /// [`KEY_WAIT`], which a live drain marker of any builder holds off.
    HeldOff,
}

/// Why [`Run::try_folder_key`] did not take the work folder's key.
enum Untaken {
    /// Another holder kept it for the whole bound, this long.
    Held(Duration),
    /// A Ctrl+C ended the take.
    Interrupted,
    /// The lock could not be opened or taken, or the drain markers probed: the line's text.
    Failed(String),
}

impl Untaken {
    /// Why, as a log line's end says it: `held for {ms} ms`, `interrupted`, or the failure's text.
    fn why(self) -> String {
        match self {
            Untaken::Held(bound) => format!("held for {} ms", bound.as_millis()),
            Untaken::Interrupted => "interrupted".to_owned(),
            Untaken::Failed(why) => why,
        }
    }
}

/// Runs `request`'s command, logging to `log` (absolute) under `root`, routing a heavy one by `environment`'s routing
/// file and git hook, saying each line as it goes; returns the gate's exit status.
///
/// The status is the command's own whole 32-bit code, 1 for a failure marker in its log or a failure of the gate after
/// it started, 124 for a command killed as stalled, timed out or past the backstop, 127 when it could not be started,
/// 0xC000013A for a Ctrl+C in the queue, before the command started, or with the command still running 5 s after it,
/// or 2 for a log inside the state root, a critical gate under a heavy or light parent's slot, a critical or `--alone`
/// gate inside a commit run's hooks, a gate started under a script whose caller is gone ([`orphan_refusal`]), a batch
/// word refused before anything was queued, a `--builder` pin refused before anything was queued or an `--alone` gate
/// whose `--no-build` would be dropped ([`alone_refused`]). A run
/// pinned with `--builder` that its builder did not run ends with 1.
pub(crate) fn run(request: &RunRequest, log: &Path, root: &ResolvedRoot, environment: &Environment, console: &mut Console) -> u32 {
    let (nesting, asked) = match admitted(request, log, root, environment, console) {
        Ok(admitted) => admitted,
        Err(status) => return status,
    };
    if let Some(orphan) = orphan_refusal(&nesting) {
        console.err(&orphan.line());
        return u32::from(USAGE_STATUS);
    }
    let request = &*asked;
    let gate = match ThisGate::read(request) {
        Ok(gate) => gate,
        Err(line) => {
            console.err(&line);
            return exit_status(GATE_FAILED);
        }
    };
    let log = Log::new(log);
    let at = Where {
        root,
        gate: &gate,
        nesting: &nesting,
    };
    let (plan, client) = match ready(request, &at, console) {
        Ok(ready) => ready,
        Err(NotRun { status, line }) => {
            console.err(&line);
            return status;
        }
    };
    // Held until the run has released its slot, so no other gate readies or writes this log meanwhile.
    let _log_guard = match take_log(&root.root, log.path()) {
        Ok(guard) => guard,
        Err(line) => {
            console.err(&line);
            return exit_status(GATE_FAILED);
        }
    };
    if readies_log_first(request)
        && let Err(status) = prepare_log(&log, console)
    {
        return status;
    }
    // The request and nesting of a run here that dropped its `--no-build`, and the nesting a gate a commit run's slot
    // covers routes under, beside that run.
    let (mut heavy, mut beside) = (None, None);
    let mut run = Run {
        request,
        root,
        nesting: &nesting,
        survey: Survey::new(&gate),
        journal: RunJournal::open(root, &gate),
        gate,
        log,
        host: host(console),
        client,
        tag: None,
        routing: RoutingFacts::default(),
        warm_hash: None,
        cold: false,
        tailscale_read: None,
        would: None,
        arrived: epoch_ms(Timestamp::now()),
        queued_at: None,
        lent: false,
        lend: None,
        covered: None,
        late: None,
        durations: None,
        load_blocks: Vec::new(),
        here_nesting: None,
        heavy_store: Some(&mut heavy),
        trace: RunTrace::default(),
        writer: None,
        streams: None,
        rerouted_for: None,
        reroute_tried: false,
        restore_after: None,
        closing: Vec::new(),
        builder_call: BuilderCall::NotEnded,
    };
    let (route, placed) = run.route(environment, console);
    if let Some(status) = ended_at_route(&route, request, &run.log, console) {
        return status;
    }
    run.route_beside_commit(&route, &mut beside);
    let local = match run.take_local(&route, &placed, console) {
        Ok(local) => local,
        Err(status) => return status,
    };
    say_nesting(run.nesting, run.request, console);
    run.queue(&route, console);
    let mut hold = Hold::nothing();
    let status = match panic::catch_unwind(AssertUnwindSafe(|| run.through(&route, plan, local, &mut hold, console))) {
        Ok(Some((ended, here))) => run.ended_through(hold, &ended, here, console),
        Ok(None) => CONTROL_C_EXIT,
        Err(payload) => {
            run.after_panic(hold, console);
            panic::resume_unwind(payload);
        }
    };
    run.finish_log(console);
    run.sweep_after();
    status
}

/// Whether a routed run's builder call ended, passing or failing its command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuilderCall {
    /// The run made no builder call, or one that was refused or interrupted before it started.
    NotEnded,
    /// The builder ran the command to its end.
    Ended,
}

impl Run<'_> {
    /// Starts the background prune sweep ([`start_prune_sweep`]) after a run whose builder call ended, unless a Ctrl+C
    /// came or the gate runs inside a box run.
    fn sweep_after(&self) {
        let interrupted = self.host.interrupt.is_some_and(Interrupt::is_raised);
        if self.builder_call == BuilderCall::Ended && !interrupted && !matches!(self.nesting, Nesting::InBox) {
            start_prune_sweep(self.request, &self.root.path);
        }
    }
}

/// How long a background prune sweep's stamp stands before a routed run starts the next sweep.
const PRUNE_SWEEP_EVERY: SignedDuration = SignedDuration::from_hours(6);
/// How far in the future a sweep stamp may be, for a clock set back a little, before it counts as stale.
const PRUNE_SWEEP_SKEW: SignedDuration = SignedDuration::from_mins(5);
/// The variable that names the state root, which the background sweep is given the run's in.
const STATE_DIR_VARIABLE: &str = "GATE_STATE_DIR";
/// The most bytes a sweep stamp is read to.
const PRUNE_SWEEP_STAMP_CAP: usize = 4096;

/// Starts the background prune sweep, `gate prune --builders --quiet` detached from this run, when its stamp under the
/// state root `state` is missing, unreadable, six hours old or more, or more than five minutes in the future, and this
/// process's job lets it break away: the stamp is written first, and removed again when the sweep cannot be started.
/// Whatever fails is dropped unsaid: the sweep never changes the run's status or output.
fn start_prune_sweep(request: &RunRequest, state: &Path) {
    if !prune_sweep_on(request) || !gate_win32::breakaway_refused().is_ok_and(|refused| !refused) {
        return;
    }
    let stamp = v1::prune_sweep_stamp_path(state);
    let now = Timestamp::now();
    if !prune_sweep_due(&stamp, now) || v1::write_prune_sweep_stamp(&stamp, &v1::PruneSweepStamp { at: now }).is_err() {
        return;
    }
    if prune_sweep_started(request, state).is_err() {
        // A stamp left would hold the next sweep back six hours for one that never ran; one that cannot be removed
        // leaves nothing more to do.
        let _ = fs::remove_file(&stamp);
    }
}

/// Whether the background sweep is due at `now` by the stamp at `path`: when it cannot be read as a stamp, or its `at`
/// is [`PRUNE_SWEEP_EVERY`] or more before `now` or more than [`PRUNE_SWEEP_SKEW`] after it.
fn prune_sweep_due(path: &Path, now: Timestamp) -> bool {
    let read = worktree_record::read_regular_within(path, PRUNE_SWEEP_STAMP_CAP)
        .ok()
        .and_then(|bytes| v1::prune_sweep_stamp(&bytes).ok());
    let Some(stamp) = read else {
        return true;
    };
    let age = now.duration_since(stamp.at);
    age >= PRUNE_SWEEP_EVERY || age < -PRUNE_SWEEP_SKEW
}

/// Starts this gate's own exe as `prune --builders --quiet` detached ([`gate_win32::start_detached`]), with this
/// process's environment and `GATE_STATE_DIR` set to `state`, in `state`, its output replacing the sweep log there;
/// its pid, or why it was not started.
fn prune_sweep_started(request: &RunRequest, state: &Path) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let output = fs::File::create(v1::prune_sweep_log_path(state)).map_err(|error| error.to_string())?;
    let mut args: Vec<OsString> = ["prune", "--builders", "--quiet"].map(OsString::from).to_vec();
    args.extend(prune_sweep_seams(request));
    let variables = std::env::vars_os()
        .filter(|(name, _)| !name.eq_ignore_ascii_case(STATE_DIR_VARIABLE))
        .chain([(OsString::from(STATE_DIR_VARIABLE), state.as_os_str().to_owned())]);
    let block = gate_win32::EnvironmentBlock::new(variables).map_err(|error| error.to_string())?;
    gate_win32::start_detached(&exe, &args, &block, state, &output).map_err(|error| error.to_string())
}

/// Whether a routed run whose builder call ended starts the background sweep: in a gate built with the `test-seams`
/// feature, only when the run carries `--test-prune-sweep`.
#[cfg(feature = "test-seams")]
fn prune_sweep_on(request: &RunRequest) -> bool {
    request.test_seams.prune_sweep
}

/// Whether a routed run whose builder call ended starts the background sweep: always.
#[cfg(not(feature = "test-seams"))]
fn prune_sweep_on(_request: &RunRequest) -> bool {
    true
}

/// The run's remote transport seams, `--test-remote-transport` and `--test-remote-ssh`, as the sweep's own words.
#[cfg(feature = "test-seams")]
fn prune_sweep_seams(request: &RunRequest) -> Vec<OsString> {
    let mut words = Vec::new();
    if let Some(transport) = &request.test_seams.remote_transport {
        words.extend([OsString::from("--test-remote-transport"), OsString::from(transport)]);
    }
    if let Some(ssh) = &request.test_seams.remote_ssh {
        words.extend([OsString::from("--test-remote-ssh"), ssh.as_os_str().to_owned()]);
    }
    words
}

/// No seams: a gate built without the `test-seams` feature has none.
#[cfg(not(feature = "test-seams"))]
fn prune_sweep_seams(_request: &RunRequest) -> Vec<OsString> {
    Vec::new()
}

/// The machine the run waits and runs on: its processors, and the Ctrl+C handler, installed now; one that cannot be
/// installed is said, and Ctrl+C then ends the gate at once.
fn host(console: &mut Console) -> Host {
    let interrupt = Interrupt::install()
        .inspect_err(|error| {
            console.err(&format!(
                "gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: {error}"
            ));
        })
        .ok();
    Host {
        processors: wait::processors(console),
        interrupt,
    }
}

/// Has `run` run the words `local` left when it dropped its `--no-build` ([`Run::local_words`]), and says the line saying
/// so: a critical run on its own critical slot ([`Run::run_as_critical`]), any other as a run here that asks for a heavy
/// slot ([`Run::run_as_heavy`]).
fn take_dropped(run: &mut Run<'_>, local: Option<&HereWords>, console: &mut Console) {
    let Some(HereWords {
        words,
        dropped: Some(reason),
        ..
    }) = local
    else {
        return;
    };
    if run.request.slot == Slot::Critical {
        run.run_as_critical(words);
    } else {
        run.run_as_heavy(words, console);
    }
    console.err(&run.dropped_line(reason));
}

/// Whether an `--alone` run whose `local` words dropped their `--no-build` ([`Run::local_words`]) is refused: it measures
/// the command as given, so it is refused before it takes a slot or readies its log, its line said.
fn alone_refused(local: Option<&HereWords>, console: &mut Console) -> bool {
    let Some(reason) = local.and_then(|here| here.dropped.as_deref()) else {
        return false;
    };
    console.err(&format!(
        "gate: refused: --no-build cannot stand: {reason}; an --alone run measures the command as given, so build first \
         (a passing local build of the project clears the record) and run it again"
    ));
    true
}

/// Whether a run of `request` readies its log before it is routed: not one pinned with `--builder`, which readies it
/// once routing accepted the pin ([`ended_at_route`]), nor an `--alone` run, which readies it once its `--no-build` is
/// known to stand ([`Run::take_local`]), so a refused run leaves an earlier log alone.
fn readies_log_first(request: &RunRequest) -> bool {
    request.builder.is_none() && !request.alone
}

/// The nesting `request` runs under, or the status of a run refused before anything is taken or written, its lines
/// said: a log inside the state root, or a gate its nesting refuses ([`refusal`]), which leaves an earlier log in
/// place. Any other nesting's lines are said once the run knows the slot it asks for
/// ([`say_nesting`]); a commit run asked for on no slot says [`commit::NO_SLOT_LINE`] now when it takes its light slot
/// itself. With the nesting, `request` as the gate runs it: a commit run on no slot runs on a light one
/// ([`commit::as_run`]), and a critical gate inside a box run runs as heavy ([`Nesting::requested`]).
fn admitted<'r>(
    request: &'r RunRequest,
    log: &Path,
    root: &ResolvedRoot,
    environment: &Environment,
    console: &mut Console,
) -> Result<(Nesting, Cow<'r, RunRequest>), u32> {
    if log_in_state_root(log, &root.path) {
        console.err(&format!(
            "gate: the log {} is inside the gate's state folder {}",
            log.display(),
            root.path.display()
        ));
        return Err(u32::from(USAGE_STATUS));
    }
    let (request, no_slot_commit) = commit::as_run(request);
    let nesting = Nesting::read(environment, request.slot, root, request.test_seams.parent_job_trusted, console);
    if let Some(status) = refusal(&nesting, &request, console) {
        return Err(status);
    }
    if no_slot_commit && takes_own_slot(&nesting, false) {
        console.err(commit::NO_SLOT_LINE);
    }
    let slot = nesting.requested(request.slot);
    let request = if slot == request.slot {
        request
    } else {
        Cow::Owned(RunRequest {
            slot,
            ..request.into_owned()
        })
    };
    Ok((nesting, request))
}

/// The status of a gate of `request` the `nesting` it runs under refuses, its lines said: a critical or `--alone` gate
/// inside a commit run's hooks; an `--alone` gate under a parent whose slot covers it or refuses it or inside a box run;
/// a critical gate under a heavy or light parent's slot; and a gate pinned with `--builder` that a parent's slot covers
/// or that runs inside a box run, which can never route. `None` for a gate it lets run.
fn refusal(nesting: &Nesting, request: &RunRequest, console: &mut Console) -> Option<u32> {
    if (request.slot == Slot::Critical || request.alone) && nesting.in_commit() {
        console.err(commit::IN_COMMIT_LINE);
        return Some(u32::from(USAGE_STATUS));
    }
    if request.alone && matches!(nesting, Nesting::Covered(_) | Nesting::Refused(_) | Nesting::InBox) {
        let flag = match request.spelling {
            Spelling::Cli => "--alone",
            Spelling::Compat => "-Alone",
        };
        console.err(&format!("gate: {flag} cannot hold the machine alone from inside a gate's command"));
        return Some(u32::from(USAGE_STATUS));
    }
    if matches!(nesting, Nesting::Refused(_)) {
        say_nesting(nesting, request, console);
        return Some(u32::from(USAGE_STATUS));
    }
    if let Some(host) = &request.builder
        && nesting.routes_as_covered()
    {
        console.err(&PinRefusal::covered(host).line());
        return Some(u32::from(USAGE_STATUS));
    }
    None
}

/// The orphaned script a gate under `nesting` refuses to start under ([`orphan::orphaned_caller`]), looked for only when
/// no live parent gate holds this one: a gate a parent covers, lends to or stands beside, or one inside a box run, is
/// started by that gate's command, and a refused one has already ended.
fn orphan_refusal(nesting: &Nesting) -> Option<Orphan> {
    match nesting {
        Nesting::Own | Nesting::Malformed(_) | Nesting::Ended { .. } | Nesting::Unbound { .. } => orphan::orphaned_caller(),
        Nesting::Covered(_) | Nesting::Refused(_) | Nesting::Under(_) | Nesting::Beside(_) | Nesting::InBox => None,
    }
}

/// Says the lines of the `nesting` a gate of `request` runs under, once it is final: the line of a marker whose binding
/// could not be judged first, then the line of an ignored marker, a covering parent or a refusing one. An uncovered
/// gate says so only as it goes on to take a local slot.
fn say_nesting(nesting: &Nesting, request: &RunRequest, console: &mut Console) {
    if let Some(line) = nesting.unjudged_line() {
        console.err(&line);
    }
    let said = match nesting {
        Nesting::Own | Nesting::Under(_) | Nesting::Beside(_) => None,
        Nesting::Malformed(_) | Nesting::Ended { .. } | Nesting::Unbound { .. } | Nesting::Covered(_) | Nesting::Refused(_) | Nesting::InBox => {
            nesting.line(request.slot, request.spelling)
        }
    };
    if let Some(line) = said {
        console.err(&line);
    }
}

/// The run's plan and its remote client, or the failure that stops it before its log is taken: the command as given,
/// and the client a routed run calls.
fn ready(request: &RunRequest, at: &Where<'_>, console: &mut Console) -> Result<(Plan, Box<dyn RemoteClient>), NotRun> {
    let plan = Plan::prepare(request, &request.command, at, console)?;
    let client = remote::chosen(&request.test_seams).map_err(NotRun::failed)?;
    Ok((plan, client))
}

/// Takes the mutex of the log at `path` under `root` without waiting, so no two gates write one log at once; a mutex a
/// killed gate left held is taken too. Else the line saying why the run cannot go on.
fn take_log(root: &StateRoot, path: &Path) -> Result<OwnedMutexGuard, String> {
    let name = root.object_name(KernelObject::Log {
        hash: log_hash(&path.to_string_lossy()),
    });
    let cannot = |error: io::Error| format!("gate: cannot take the mutex {name} of the log {}: {error}", path.display());
    match NamedMutex::open(&name).map_err(cannot)?.acquire_owned(Duration::ZERO) {
        Ok(guard) => Ok(guard),
        Err(AcquireError::Timeout) => Err(format!("gate: the log {} is in use by another gate", path.display())),
        Err(AcquireError::Failed(error)) => Err(cannot(error)),
    }
}

/// Whether the log at `log` (absolute and normalised) would be written inside the state root `root`: each path read
/// through the links and junctions of the folders that exist so far ([`resolved`]), the log's own name as given.
fn log_in_state_root(log: &Path, root: &Path) -> bool {
    let log = match (log.parent(), log.file_name()) {
        (Some(folder), Some(name)) => resolved(folder).join(name),
        _ => resolved(log),
    };
    is_within(&log, &resolved(root))
}

/// `path` with the nearest of itself and its folders that can be made canonical made so, following links and
/// junctions, and the rest joined on as written; `path` as written when no part of it can be made canonical.
pub(crate) fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut existing = path;
    loop {
        // A part that cannot be made canonical, missing or not, is joined on as written, as the parts after it are.
        if let Ok(canonical) = fs::canonicalize(existing) {
            return rest.iter().rev().fold(canonical, |joined, part| joined.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

/// Whether `path` is `folder` or lies inside it, both absolute and normalised: `folder`'s components are the first of
/// `path`'s, each compared whole and without regard to case, as Windows compares file names.
pub(crate) fn is_within(path: &Path, folder: &Path) -> bool {
    let mut own = path.components();
    folder.components().all(|part| own.next().is_some_and(|mine| same_name(mine, part)))
}

/// Whether two path components name the same thing to Windows, which ignores case.
fn same_name(left: Component<'_>, right: Component<'_>) -> bool {
    left.as_os_str().to_string_lossy().to_lowercase() == right.as_os_str().to_string_lossy().to_lowercase()
}

/// The gate's exit status for `status`, which holds a 32-bit exit code as the signed number it is shown as.
fn exit_status(status: i64) -> u32 {
    // Every status is an `i32` exit code or one of the gate's own small ones; the fallback still fails.
    i32::try_from(status).map_or(u32::MAX, i32::cast_unsigned)
}

/// A run that ends before it queues: its status and its line.
struct NotRun {
    status: u32,
    line: String,
}

impl NotRun {
    fn failed(line: String) -> NotRun {
        NotRun {
            status: exit_status(GATE_FAILED),
            line,
        }
    }
}

/// Under which state root a planned command runs, which gate runs it, and under which parent's slot.
struct Where<'a> {
    root: &'a ResolvedRoot,
    gate: &'a ThisGate,
    nesting: &'a Nesting,
}

/// The command as it will be started, or the start failure found before any call.
struct Plan {
    /// The command's first word, as given.
    word: String,
    /// The file the first word was found as, as a full path.
    resolved: Option<String>,
    launch: Result<Launch, String>,
}

/// How a found program is started.
pub(crate) enum Written {
    /// The application and its command line.
    Line(PathBuf, Vec<u16>),
    /// A batch word cmd.exe would split or expand: the refusal's line.
    Refused(String),
    /// A batch target with no cmd.exe to run it: why.
    NoShell(String),
}

impl Plan {
    /// Finds the program of `words`, writes the command line and builds the environment, in the caller's working
    /// folder.
    ///
    /// # Errors
    ///
    /// A refused batch word (status 2), or a working folder or environment the gate cannot read or pass on (status 1).
    fn prepare(request: &RunRequest, words: &[OsString], at: &Where<'_>, console: &mut Console) -> Result<Plan, NotRun> {
        Plan::prepare_without(request, words, at, &[], console)
    }

    /// [`Plan::prepare`], with the variables named `withheld` left out of the command's environment.
    ///
    /// # Errors
    ///
    /// As [`Plan::prepare`].
    fn prepare_without(request: &RunRequest, words: &[OsString], at: &Where<'_>, withheld: &[&str], console: &mut Console) -> Result<Plan, NotRun> {
        let Where { root, gate, nesting } = *at;
        let folder = std::env::current_dir().map_err(|error| NotRun::failed(format!("gate: cannot read the working directory: {error}")))?;
        // A covered gate hands on the marker of the parent whose slot its command runs under. Any other hands on its own,
        // a gate its light parent lends its slot included: that gate's marker names it as a heavy holder, so every gate
        // nested in its command runs covered by the lend.
        let marker = match nesting.covering() {
            Some(parent) => parent.marker.clone(),
            None => environment::slot_marker(request.slot.as_str(), gate, &root.root),
        };
        let variables = Variables::for_command(root, gate, &marker, &request.env);
        // A gate inside a box run hands on no marker, so a gate nested in its command is judged by the box job alone;
        // any other hands on no box job, which it ignored or was not given, so no gate nested in its command says so again.
        let variables = match nesting {
            Nesting::InBox => variables.without(SLOT_HELD_VARIABLE),
            _ => variables.without(BOX_JOB_VARIABLE),
        };
        // A commit run that hands on its own marker names it as a commit run's, so a gate in its hooks its slot covers
        // still routes ([`Nesting::under_commit`]); a gate that commit covers hands that name on with its marker. Any
        // other hands on no commit's name, so it never reaches a gate the named commit does not cover.
        let hands_on_own = nesting.covering().is_none() && !matches!(nesting, Nesting::InBox);
        let variables = if hands_on_own && gate.commit() {
            variables.with(COMMIT_SLOT_VARIABLE, &marker)
        } else if nesting.under_commit().is_some() {
            variables
        } else {
            variables.without(COMMIT_SLOT_VARIABLE)
        };
        let variables = withheld.iter().fold(variables, |variables, name| variables.without(name));
        let environment = variables
            .block()
            .map_err(|error| NotRun::failed(format!("gate: cannot build the command's environment: {error}")))?;
        let Some((first, rest)) = words.split_first() else {
            return Err(NotRun::failed("gate: the run has no command".to_owned()));
        };
        let word = first.to_string_lossy().into_owned();
        let program = match find_program(&word, &variables, &folder) {
            Ok(program) => program,
            Err(message) => {
                let launch = Err(message);
                return Ok(Plan {
                    word,
                    resolved: None,
                    launch,
                });
            }
        };
        let launch = match written(&program, rest, &variables) {
            Written::Refused(line) => {
                let status = u32::from(USAGE_STATUS);
                return Err(NotRun { status, line });
            }
            Written::NoShell(message) => Err(message),
            Written::Line(application, command_line) => Ok(Launch {
                application,
                command_line,
                directory: folder,
                environment,
                output: LaunchOutput::Pipes,
                priority: priority(nesting.runs_as(request.slot), console),
            }),
        };
        Ok(Plan {
            word,
            resolved: Some(program.to_string_lossy().into_owned()),
            launch,
        })
    }
}

/// The file `word` names, looked for along the command's `PATH` and `PATHEXT`, read against `folder` and made a full
/// path as Windows reads it, `.` and `..` folded; else the start failure's message.
pub(crate) fn find_program(word: &str, variables: &Variables, folder: &Path) -> Result<PathBuf, String> {
    // An unset PATH or PATHEXT has no folder or extension to try.
    let path = variables
        .get("PATH")
        .map_or_else(String::new, |value| value.to_string_lossy().into_owned());
    let pathext = variables
        .get("PATHEXT")
        .map_or_else(String::new, |value| value.to_string_lossy().into_owned());
    let folder = folder.to_string_lossy();
    let search = ProgramSearch {
        path: &path,
        pathext: &pathext,
        folder: &folder,
    };
    let found =
        resolve_program(word, &search, |candidate| Path::new(candidate).is_file()).ok_or_else(|| gate_win32::win32_message(ERROR_FILE_NOT_FOUND))?;
    std::path::absolute(&found).map_err(|error| format!("cannot make {found} a full path: {error}"))
}

/// The application and command line of `program` with the words `rest`: a batch target runs through the system
/// folder's cmd.exe, its words checked against what cmd.exe would split or expand.
pub(crate) fn written(program: &Path, rest: &[OsString], variables: &Variables) -> Written {
    let mut words: Vec<Vec<u16>> = vec![program.as_os_str().encode_wide().collect()];
    words.extend(rest.iter().map(|word| word.encode_wide().collect()));
    if !is_batch_target(&program.to_string_lossy()) {
        return Written::Line(program.to_owned(), command_line(&words));
    }
    if let Some(refusal) = batch_refusal(&words, |name| variables.is_set(name)) {
        return Written::Refused(refusal.to_string());
    }
    match gate_win32::system_directory() {
        Ok(system) => {
            let shell = system.join("cmd.exe");
            let line = batch_command_line(&shell.as_os_str().encode_wide().collect::<Vec<u16>>(), &words);
            Written::Line(shell, line)
        }
        Err(error) => Written::NoShell(format!("cannot read the system folder, which holds cmd.exe: {error}")),
    }
}

/// The priority class of a command run as `slot`: high for a critical gate and a gate under a critical parent's slot;
/// otherwise below normal, unless the gate's own class is already below normal or idle, which the command then inherits.
fn priority(slot: Slot, console: &mut Console) -> Option<PriorityClass> {
    if slot == Slot::Critical {
        return Some(PriorityClass::High);
    }
    match gate_win32::runs_below_normal() {
        Ok(true) => None,
        Ok(false) => Some(PriorityClass::BelowNormal),
        Err(error) => {
            console.err(&format!(
                "gate: cannot read this gate's priority class, so the command runs below normal: {error}"
            ));
            Some(PriorityClass::BelowNormal)
        }
    }
}

/// One run under way: what it was asked, where it journals and logs, and what it reads of the other gates.
struct Run<'a> {
    request: &'a RunRequest,
    root: &'a ResolvedRoot,
    /// The parent gate's slot the run is under, read once before it started.
    nesting: &'a Nesting,
    /// The nesting read for a gate a commit run's slot covers while it routes beside that run, which it runs under
    /// again once it falls back here ([`Run::route_beside_commit`]); `None` for every other run.
    here_nesting: Option<&'a Nesting>,
    /// Where the request and nesting of the run go once it drops its `--no-build` ([`Run::run_as_heavy`],
    /// [`Run::run_as_critical`]), which a run does at most once; `None` once it has.
    heavy_store: Option<&'a mut Option<(RunRequest, Nesting)>>,
    gate: ThisGate,
    log: Log,
    journal: RunJournal,
    survey: Survey,
    /// The machine's processors and the console interrupt, read once for the run.
    host: Host,
    /// The remote client a routed run calls.
    client: Box<dyn RemoteClient>,
    /// The remote tag of a routed run, which its journal lines and remote claim carry until its fallback.
    tag: Option<RemoteTag>,
    /// The routing facts the run's `queue`, `fallback` and `take` lines say, which its `release` line copies so the
    /// archive keeps them ([`Run::queue`], [`Run::give_up`]).
    routing: RoutingFacts,
    /// The hash of the warm-up file a routed run's work folder must be warm with, which its call asks the builder for;
    /// `None` when the route asks no warmth. Set once, by [`Run::route`].
    warm_hash: Option<String>,
    /// Whether the routed run's call ran in a work tree the builder found new, its ceiling raised, which its `release`
    /// line says; false for every other run, one that fell back among them.
    cold: bool,
    /// The read of Tailscale's path to the builder a routed run's call went to, started once the call returned, which
    /// its `release` line takes if it has finished ([`Run::tailscale_path`]); `None` for a run that made no builder
    /// call.
    tailscale_read: Option<tailscale::PendingPath>,
    /// What the routing decision saw of the run, kept for a passed light run's route line, which asks the decision again
    /// as a heavy gate would ([`route::would_route_on_heavy`]). `None` for a run whose light parent lent it its slot,
    /// which never asked.
    would: Option<route::Decision>,
    /// The run's first arrival in Unix epoch milliseconds, which every ticket it writes keeps.
    arrived: u64,
    /// The moment of the run's `queue` line: the moment the local `take` of a run the full remote pool sent back counts
    /// its wait from, while every other wait counts from its own take. A light run the critical barrier held back keeps
    /// the instant of its first queue too, for its later waits here and its remote take. `None` for a run that is not
    /// routed and was not held back, and for one sent back for any other reason.
    queued_at: Option<Instant>,
    /// Whether the run's light parent lent it its slot while a critical request drained, before it queued or once it
    /// journalled `parent` as it waited, so it takes, journals and releases no slot.
    lent: bool,
    /// The lend marker of a run its light parent lent its slot, from the lend to its release, which a critical gate's
    /// drain waits for ([`LendMarker`]).
    lend: Option<LendMarker>,
    /// The covered marker of a run a parent's slot covers, from its `covered` line to its release.
    covered: Option<Covered>,
    /// The slot a run a parent's slot covers took when that parent ended as its command ran ([`Late`]), until its
    /// release.
    late: Option<LateSlot>,
    /// The durations of a run that takes a slot of its own ([`Run::open_durations`]).
    durations: Option<RunDurations>,
    /// Each block of load lines a run holding the machine alone wrote to its log ([`Run::log_load`]), which the log's
    /// tail leaves out, so they reach the log only.
    load_blocks: Vec<Vec<String>>,
    /// Where the run's time went, which its `release` line says: what the watch of a command run here found
    /// ([`Watcher::trace`]), or the parts of a routed run the client timed; empty for a run that reached neither.
    trace: RunTrace,
    /// The log's writer from the run here's start ([`Run::here`]) until the run closes it ([`Run::close_log`]).
    writer: Option<Arc<LogWriter>>,
    /// The readers of the started command's output and the consumer of its job's process events, until the run's end
    /// ends them ([`Run::end_streams`]).
    streams: Option<Streams>,
    /// The critical request whose barrier sent this light run to a builder, from its route on; `None` for every other
    /// run.
    rerouted_for: Option<u32>,
    /// Whether the light run has asked to route once a critical request held it back; it asks at most once.
    reroute_tried: bool,
    /// The routed dotnet build that passed and brought everything back, whose target a restore here may need once the
    /// run has let its remote hold go ([`Run::restore_and_close`]); `None` for every other run.
    restore_after: Option<RoutedBuild>,
    /// The verdict's closing lines, held while a restore here is still to run so they stay the run's last lines.
    closing: Vec<Said>,
    /// Whether the routed run's builder call ended, passing or failing its command, rather than being refused or
    /// interrupted before it started: such a run may start the background prune sweep ([`start_prune_sweep`]).
    builder_call: BuilderCall,
}

/// A routed dotnet build that passed and brought everything back: its words, its worktree's root, the routed command's
/// ceiling and when the routed call started, from which that ceiling counts for a restore here.
struct RoutedBuild {
    words: Vec<String>,
    root: PathBuf,
    ceiling: Duration,
    started: Instant,
}

/// A line the verdict says: on stderr, or as a note on stdout ([`Console::note_out`]).
enum Said {
    Err(String),
    NoteOut(String),
}

/// Says `lines` on `console`, each as it was meant.
fn say(lines: Vec<Said>, console: &mut Console) {
    for line in lines {
        match line {
            Said::Err(line) => console.err(&line),
            Said::NoteOut(line) => console.note_out(&line),
        }
    }
}

/// What writes a started command's lines to its log: the readers of its two pipes, once started, and the thread that
/// turns its job's events into process lines.
struct Streams {
    forwarders: Option<Forwarders>,
    consumer: JoinHandle<()>,
}

/// How long the run's end waits for the members of the command's job still running to end, so each gets its exit
/// line: the rest get a `left` line.
const EVENTS_WAIT: Duration = Duration::from_secs(2);

/// Starts the thread that writes each of `events` to `writer` as a process line, until the job drops its sender.
fn spawn_consumer(writer: Arc<LogWriter>, events: Receiver<JobEvent>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name("job-events".to_owned()).spawn(move || {
        for event in events {
            let mut line = event_line(event);
            line.push('\n');
            writer.command(line.as_bytes());
        }
    })
}

/// `event` as the log's process line: a spawn stamped at the process's kernel create time, an exit at its kernel exit
/// time when known, every other line now.
fn event_line(event: JobEvent) -> String {
    let (at, text) = match event {
        JobEvent::Spawned { pid, ppid, image, at } => (stamp::local(at), stamp::spawn_text(pid, ppid, image.as_deref())),
        JobEvent::Exited { pid, code, at, ran } => {
            let ended = code.map_or(stamp::Ended::Unknown, |code| stamp::Ended::Code(i64::from(code)));
            (at.map_or_else(stamp::now, stamp::local), stamp::exit_text(pid, ended, ran))
        }
        JobEvent::Missed { pid, why } => (stamp::now(), stamp::missed_text(pid, &why)),
        JobEvent::Left { pid } => (stamp::now(), stamp::left_text(pid)),
        JobEvent::Stopped { why } => (stamp::now(), stamp::stopped_text(&why)),
    };
    stamp::stamped(&at, Tag::Gat, &text)
}

/// The pid of the process that started this gate, as the system's process table names it.
fn gate_parent_pid() -> Option<u32> {
    let me = std::process::id();
    Win32ProcessTable
        .snapshot_without_command_lines()
        .into_iter()
        .find(|process| process.pid == me)
        .map(|process| process.parent_pid)
}

/// What a take of a pool's slot is journalled and claimed with: the run's state root, gate, request, ceiling and log,
/// its remote tag while it has one, its journal, the parent gate its `take` names, and what the durations store expects
/// of the run, which its claim carries.
pub(crate) struct Taker<'t> {
    pub root: &'t ResolvedRoot,
    pub gate: &'t ThisGate,
    pub request: &'t RunRequest,
    pub ceiling: u64,
    pub log: &'t Log,
    pub tag: Option<&'t RemoteTag>,
    pub journal: &'t mut RunJournal,
    pub parent_pid: Option<u32>,
    pub expected: Option<&'t Map<String, Value>>,
}

impl Taker<'_> {
    /// Writes the claim of `taken`, a slot of `pool` the run waited `waited` seconds for, then journals its take, so a
    /// reader that sees the take finds its claim, unless the claim could not be written. It says and names the slot when
    /// a killed gate had left it held, lets go of the beside mutex a light gate behind a critical request took the slot
    /// under, and wakes the waiters. The gate's ticket is already deleted.
    pub(crate) fn took(&mut self, taken: &mut Taken, pool: Pool, waited: f64, survey: &mut Survey, console: &mut Console) {
        let name = taken.held.name().to_owned();
        let mut abandoned = Vec::new();
        if taken.abandoned {
            acquire::say_taken_abandoned(console, &name);
            abandoned.push(name.clone());
        }
        let of = ClaimOf {
            gate: self.gate,
            ceiling: self.ceiling,
            log: self.log.path(),
            remote: self.tag,
            reason: self.request.reason.as_deref(),
            expected: self.expected,
        };
        if taken.beside.is_some()
            && let Some(go) = &self.request.test_seams.beside_claim_wait
        {
            // The arrival file says the gate reached the seam, so a case can check what is not written yet.
            let mut reached = go.as_os_str().to_owned();
            reached.push(".reached");
            let reached = PathBuf::from(reached);
            if let Err(error) = fs::write(&reached, "") {
                console.err(&format!("gate: cannot write the seam's arrival {}: {error}", reached.display()));
            }
            while !go.exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        slots::write_claim(&taken.held, pool.claim_kind(), &of, console);
        self.journal
            .write(&take_event(vec![name], waited, abandoned, self.parent_pid), self.tag, console);
        taken.beside = None;
        slots::wake_waiters(self.root, survey, console);
    }
}

/// Says the lines of the slot just taken, for its `estimate`: the expected-time line, then the raised-ceiling line when
/// history raised the run's auto ceiling above its floor to `resolved`, then the ceiling hint when `resolved` is below
/// the load-adjusted p90; nothing for a key without history.
fn say_expected(estimate: Option<&Estimate>, resolved: Resolved, console: &mut Console) {
    let Some(estimate) = estimate else {
        return;
    };
    console.err(&expected_line(estimate));
    say_ceiling(estimate, resolved, console);
}

/// Says the ceiling lines of a take for its `estimate`: the raised-ceiling line when history raised the run's auto
/// ceiling above its floor to `resolved`, then the ceiling hint when `resolved` is below the load-adjusted p90.
fn say_ceiling(estimate: &Estimate, resolved: Resolved, console: &mut Console) {
    if let Some((p99, floor)) = resolved.raised {
        console.err(&raised_ceiling_line(resolved.seconds, p99, floor, estimate.family));
    }
    if let Some(hint) = ceiling_hint(resolved.seconds, estimate) {
        console.err(&hint);
    }
}

/// The figures a run runs under, which its claims, its watch, its remote call and its passed line read: its ceiling in
/// whole seconds, and the usual p90 of the slot it took, which the hang hint reads, the local watch's or the remote
/// client's; `None` without one.
#[derive(Clone, Copy)]
struct Figures {
    ceiling: u64,
    usual: Option<Usual>,
}

impl Figures {
    /// The figures of a run that takes no slot of its own: its `ceiling`'s floor, and no p90.
    fn floor(ceiling: Ceiling) -> Figures {
        Figures {
            ceiling: ceiling.floor(),
            usual: None,
        }
    }

    /// The figures a take resolves for `ceiling` from the `estimate` of the slot it takes, with how it resolved the
    /// ceiling.
    fn resolved(ceiling: Ceiling, estimate: Option<&Estimate>) -> (Figures, Resolved) {
        let resolved = resolve_ceiling(ceiling, estimate);
        let figures = Figures {
            ceiling: resolved.seconds,
            usual: estimate.and_then(Usual::of),
        };
        (figures, resolved)
    }
}

/// The parts of a run its client's call is made of, borrowed apart from the run's client.
#[derive(Clone, Copy)]
struct CallParts<'r> {
    request: &'r RunRequest,
    /// The figures of the run's take.
    figures: Figures,
    log: &'r Log,
    host: &'r Host,
    root: &'r ResolvedRoot,
    /// What the call knows of the work folder's warm-up, or asks of it.
    warmth: remote::Warmth<'r>,
    /// The variables the call sends the builder ([`routed_env`]).
    env: &'r [(String, String)],
    /// The run's drain session, made once before its first attempt.
    session: &'r str,
}

/// The outcome a routed run of `request` goes on with once `first`, the outcome of its `call`, ended: `first` itself,
/// unless the builder's drain refused it. Then the run waits behind the drain, its key and remote slot kept: it says
/// the waiting line and the still-waiting line every heartbeat, and polls the builder ([`RemoteClient::poll_drain`])
/// every drain poll, each poll's barrier the last one seen. An unreachable poll is still waiting, said in the log only,
/// until that barrier lapses, and is the outcome after. Any other poll the drain does not refuse starts a fresh attempt,
/// whose outcome it is, unless the drain refused that attempt too, which says the waiting line again and waits again
/// from the poll; a poll whose preflight passed is the builder's admission, and says the run's admitted line first.
/// While it waits it holds its drain marker ([`drain::Marker`]), let go once the wait ends. `None` for a Ctrl+C during
/// the wait.
fn behind_drain(
    client: &mut dyn RemoteClient,
    call: &RemoteCall<'_>,
    first: RemoteOutcome,
    request: &RunRequest,
    console: &mut Console,
) -> Option<Behind> {
    let Some(mut drain) = first.draining else {
        return Some(Behind {
            outcome: first,
            admitted: None,
        });
    };
    let poll = request.test_seams.drain_poll_ms.map_or(wait::DRAIN_POLL, Duration::from_millis);
    let mut beat = Beat::new(request.test_seams.heartbeat_ms.map_or(wait::HEARTBEAT, Duration::from_millis));
    loop {
        console.err(&drain_waiting_line(drain));
        let marker = held_marker(call, console);
        let waited = waited_out(client, call, drain, (poll, &mut beat), console);
        end_marker(marker, console);
        let passed = match waited? {
            WaitEnd::Polled { last, passed } => {
                drain = last;
                passed
            }
            WaitEnd::Ended(outcome) => {
                return Some(Behind {
                    outcome: *outcome,
                    admitted: None,
                });
            }
        };
        if call.interrupt.is_some_and(Interrupt::is_raised) {
            return None;
        }
        if passed {
            console.err(&drain_admitted_line(drain, Timestamp::now().as_second()));
        }
        let admitted = Instant::now();
        let ran = client.run(call);
        match ran.draining {
            Some(again) => drain = again,
            None => {
                return Some(Behind {
                    outcome: ran,
                    admitted: Some(admitted),
                });
            }
        }
    }
}

/// What a routed run's waits after its call may use: the run's request, whose seams set each wait's poll
/// (`--test-drain-poll-ms`) and heartbeat (`--test-heartbeat-ms`), what is left of the run's remote budget for its waits
/// for a builder slot, and the time the run's remote waits have taken, which those waits add to.
struct SlotWait<'w> {
    request: &'w RunRequest,
    left: Duration,
    waited: &'w mut Duration,
}

/// The outcome a routed run of `call` goes on with once `first`, the outcome of its call, ended. Each call of the
/// attempt is judged in turn: one the builder's drain refused waits behind the drain ([`behind_drain`]), unbounded by
/// the remote budget; one the builder refused `full` while some of the budget is `left` waits for a slot there
/// ([`behind_full`]), whose own time alone counts into the `waited` of `wait`; any other is the outcome. A slot wait a
/// poll ends calls the builder once more, and the builder admitted the run just before the last such call, or the
/// drain wait's admitted one. `None` for a Ctrl+C during either wait.
fn after_waits(
    client: &mut dyn RemoteClient,
    call: &RemoteCall<'_>,
    first: RemoteOutcome,
    wait: SlotWait<'_>,
    console: &mut Console,
) -> Option<Behind> {
    let SlotWait { request, left, waited } = wait;
    let (mut outcome, mut admitted, mut spent) = (first, None, Duration::ZERO);
    loop {
        let behind = behind_drain(client, call, outcome, request, console)?;
        admitted = behind.admitted.or(admitted);
        outcome = behind.outcome;
        let rest = left.saturating_sub(spent);
        if !waits_for_slot(&outcome, rest, call) {
            return Some(Behind { outcome, admitted });
        }
        let begun = Instant::now();
        let ended = behind_full(client, call, outcome, (request, rest), console);
        let took = begun.elapsed();
        spent += took;
        *waited += took;
        match ended? {
            // A run that falls back from its wait was never admitted, whatever call an earlier poll admitted.
            SlotEnd::Spent(refused) => {
                return Some(Behind {
                    outcome: refused,
                    admitted: None,
                });
            }
            SlotEnd::Drained(polled) => outcome = polled,
            SlotEnd::Polled => {
                admitted = Some(Instant::now());
                outcome = client.run(call);
            }
        }
    }
}

/// Whether the run of `call` waits for a slot on its builder after `outcome`: the builder refused it `full`, some of the
/// run's remote budget is `left`, and no Ctrl+C was raised during the call, which then ends the run as it would end it.
fn waits_for_slot(outcome: &RemoteOutcome, left: Duration, call: &RemoteCall<'_>) -> bool {
    builder_full(outcome) && !left.is_zero() && !call.interrupt.is_some_and(Interrupt::is_raised)
}

/// How a routed run's wait for a slot on its builder ([`behind_full`]) ended, when no Ctrl+C ended it.
enum SlotEnd {
    /// A poll neither the builder's pool nor its drain refused: the run calls the builder again.
    Polled,
    /// A poll the builder's drain refused, this one: the run waits behind the drain.
    Drained(RemoteOutcome),
    /// The budget was spent: the run is refused `full`, with the outcome of the call that started the wait.
    Spent(RemoteOutcome),
}

/// Waits for a slot on the builder of `call`, which refused the run `full` in `first`, its own slots all held, for what
/// is `left` of the run's remote budget, its remote slot here kept, the poll and the heartbeat read from the seams of
/// `request` as [`SlotWait`] says. It says the waiting line, polls the builder's preflight alone
/// ([`RemoteClient::poll_drain`]) every drain poll, and says the still-waiting line every heartbeat. A poll refused
/// `full` keeps waiting; any other ends the wait as [`SlotEnd`] says. `None` for a Ctrl+C during the wait.
fn behind_full(
    client: &mut dyn RemoteClient,
    call: &RemoteCall<'_>,
    first: RemoteOutcome,
    (request, left): (&RunRequest, Duration),
    console: &mut Console,
) -> Option<SlotEnd> {
    let alias = &call.builder.alias;
    console.err(&format!("gate: waiting for a slot on the builder {alias} (its slots are all held)"));
    let poll = request.test_seams.drain_poll_ms.map_or(wait::DRAIN_POLL, Duration::from_millis);
    let mut beat = Beat::new(request.test_seams.heartbeat_ms.map_or(wait::HEARTBEAT, Duration::from_millis));
    let begun = Instant::now();
    loop {
        let rest = left.saturating_sub(begun.elapsed());
        if rest.is_zero() {
            return Some(SlotEnd::Spent(first));
        }
        if !slept(poll.min(rest), call.interrupt) {
            return None;
        }
        if let Some(waited) = beat.due() {
            console.err(&format!("gate: still waiting for a slot on the builder {alias} ({} s)", waited.as_secs()));
        }
        let polled = client.poll_drain(call);
        if call.interrupt.is_some_and(Interrupt::is_raised) {
            return None;
        }
        if polled.draining.is_some() {
            return Some(SlotEnd::Drained(polled));
        }
        if !builder_full(&polled) {
            return Some(SlotEnd::Polled);
        }
    }
}

/// Whether `outcome` is the builder's own `full` refusal: its slots were all held.
fn builder_full(outcome: &RemoteOutcome) -> bool {
    outcome.fallback() == Some(FallbackReason::Full)
}

/// How a routed run goes on after its call: the outcome of its last attempt, and when the builder admitted that
/// attempt after a wait for a slot there or behind its drain, `None` for a run that never waited and for one that fell
/// back from its wait.
struct Behind {
    outcome: RemoteOutcome,
    admitted: Option<Instant>,
}

/// How one stretch of a wait behind the builder drain ended, when no Ctrl+C ended it.
enum WaitEnd {
    /// A poll the drain did not refuse ended the wait, the last drain seen being `last`: the builder admitted the run
    /// when the poll's preflight `passed`, and the run's attempt meets whatever else ended the poll on its own.
    Polled { last: Drain, passed: bool },
    /// The builder could not be reached after the last barrier lapsed: the run falls back with this outcome.
    Ended(Box<RemoteOutcome>),
}

/// Waits behind the builder drain from `drain`, the last one seen, until a poll admits the run of `call` or ends its
/// wait: polls the builder every `poll` and says the still-waiting line whenever `beat` says it is due; `None` once a
/// Ctrl+C ends the wait.
fn waited_out(
    client: &mut dyn RemoteClient,
    call: &RemoteCall<'_>,
    mut drain: Drain,
    (poll, beat): (Duration, &mut Beat),
    console: &mut Console,
) -> Option<WaitEnd> {
    loop {
        if !slept(poll, call.interrupt) {
            return None;
        }
        if let Some(waited) = beat.due() {
            console.err(&drain_still_waiting_line(waited, drain.place));
        }
        match poll_answer(client.poll_drain(call), drain, console) {
            Answer::Waiting(standing) => drain = standing,
            Answer::Ended(outcome) => return Some(WaitEnd::Ended(outcome)),
            Answer::Polled { passed } => return Some(WaitEnd::Polled { last: drain, passed }),
        }
    }
}

/// The drain marker of the wait of `call` behind its builder's drain, held; `None`, said, when it cannot be, and the
/// run waits without one.
fn held_marker(call: &RemoteCall<'_>, console: &mut Console) -> Option<drain::Marker> {
    match drain::Marker::hold(call.state_dir, call.builder.id.as_deref(), call.session) {
        Ok(marker) => Some(marker),
        Err(error) => {
            let folder = v1::drain_marker_dir(call.state_dir, call.builder.id.as_deref());
            console.err(&format!(
                "gate: cannot hold a drain marker in {}: {}; the run waits without one",
                gate_router::escaped(&folder.to_string_lossy()),
                gate_router::escaped(&error.to_string())
            ));
            None
        }
    }
}

/// Lets the drain marker `marker` go, saying why its file could not be removed.
fn end_marker(marker: Option<drain::Marker>, console: &mut Console) {
    let Some(marker) = marker else {
        return;
    };
    let path = marker.path().to_owned();
    if let Err(error) = marker.end() {
        console.err(&format!(
            "gate: cannot remove the drain marker {}: {}",
            gate_router::escaped(&path.to_string_lossy()),
            gate_router::escaped(&error.to_string())
        ));
    }
}

/// What one drain poll answered a run waiting behind the drain that last said `drain`.
enum Answer {
    /// The drain still holds the run back, as this says.
    Waiting(Drain),
    /// The builder could not be reached after the last barrier lapsed: the run falls back with this outcome.
    Ended(Box<RemoteOutcome>),
    /// The drain no longer refused the poll, whose preflight `passed` when the builder admitted the run.
    Polled { passed: bool },
}

/// What the poll that ended `polled` answered a run whose last drain seen is `drain`: an unreachable builder, or a poll
/// the client killed, is still waiting while that drain's barrier stands, noted in the log alone.
fn poll_answer(polled: RemoteOutcome, drain: Drain, console: &mut Console) -> Answer {
    if let Some(standing) = polled.draining {
        return Answer::Waiting(standing);
    }
    let fallback = polled.fallback();
    if fallback != Some(FallbackReason::Unreachable) {
        return Answer::Polled {
            passed: polled.status == 0 && fallback.is_none(),
        };
    }
    if Timestamp::now().as_second() < drain.lapses {
        console.note(&format!(
            "gate: the builder is unreachable while its drain stands (it lapses at {} UTC); still waiting",
            utc_clock(drain.lapses)
        ));
        return Answer::Waiting(drain);
    }
    Answer::Ended(Box::new(polled))
}

/// When a wait's still-waiting lines are due: every period from the wait's start.
struct Beat {
    begun: Instant,
    period: Duration,
    next: Duration,
}

impl Beat {
    fn new(period: Duration) -> Beat {
        Beat {
            begun: Instant::now(),
            period,
            next: period,
        }
    }

    /// The time waited so far when a still-waiting line is due now, which moves the next one to the first period
    /// after it, skipping any missed; else `None`.
    fn due(&mut self) -> Option<Duration> {
        let waited = self.begun.elapsed();
        if waited < self.next {
            return None;
        }
        while self.next <= waited {
            self.next += self.period;
        }
        Some(waited)
    }
}

/// Sleeps `period` in slices of [`KEY_WAIT_SLICE`], the console `interrupt` looked at before each; whether it slept
/// the whole period, `false` once the interrupt is raised.
fn slept(period: Duration, interrupt: Option<Interrupt>) -> bool {
    let begun = Instant::now();
    loop {
        if interrupt.is_some_and(Interrupt::is_raised) {
            return false;
        }
        let left = period.saturating_sub(begun.elapsed());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(KEY_WAIT_SLICE));
    }
}

/// What a wait for the worktree's key says once its bound passed and a live drain marker holds that bound off.
const KEY_HELD_OFF_LINE: &str = "gate: still waiting for the worktree's key past its bound: a routed run waits behind a builder drain";

/// Says `line` unless `said` says it was, and marks it said.
fn say_once(console: &mut Console, said: &mut bool, line: &str) {
    if !std::mem::replace(said, true) {
        console.err(line);
    }
}

/// The line a routed run says once as it starts to wait behind the builder's `drain`, the barrier's times left out when
/// the box said none stands, and its place when the box gave it none.
fn drain_waiting_line(drain: Drain) -> String {
    let head = match drain.times() {
        Some(times) => format!("gate: waiting behind the builder drain ({times})"),
        None => "gate: waiting behind the builder drain".to_owned(),
    };
    match drain.place {
        Some(place) => format!("{head}; place {place} in line"),
        None => head,
    }
}

/// The line a routed run says every heartbeat of its wait behind the builder drain, `waited` into it, at `place`.
fn drain_still_waiting_line(waited: Duration, place: Option<u32>) -> String {
    let seconds = waited.as_secs();
    match place {
        Some(place) => format!("gate: still waiting behind the builder drain ({seconds} s; place {place})"),
        None => format!("gate: still waiting behind the builder drain ({seconds} s)"),
    }
}

/// The line a routed run says as the builder admits it at the Unix second `now`, the last drain seen being `drain`:
/// the lapse line once that drain's barrier had lapsed by then, else the lift line.
fn drain_admitted_line(drain: Drain, now: i64) -> String {
    if drain.lapses > 0 && now >= drain.lapses {
        return format!(
            "gate: the builder drain lapsed at {} UTC without a lift; starting the routed run",
            utc_clock(drain.lapses)
        );
    }
    "gate: the builder drain lifted; starting the routed run".to_owned()
}

/// The variables a routed run sends the builder: the caller's pairs, then `SourceRevisionId` set to the worktree's
/// `HEAD` sha, so any build the command runs there is pinned to it, unless a caller's pair names it, the letter case
/// ignored, the route read no sha, or the pair would put the variables over the box's cap ([`gate_args::over_cap`]):
/// such a run goes unpinned, saying nothing.
fn routed_env(pairs: &[(String, String)], source_revision: Option<&str>) -> Vec<(String, String)> {
    let mut env = pairs.to_vec();
    let named = pairs.iter().any(|(name, _)| name.eq_ignore_ascii_case(SOURCE_REVISION_VARIABLE));
    let bytes: usize = pairs.iter().map(|(name, value)| name.len() + value.len()).sum();
    let fits = |sha: &str| !gate_args::over_cap(pairs.len() + 1, bytes + SOURCE_REVISION_VARIABLE.len() + sha.len());
    if let Some(sha) = source_revision.filter(|sha| !named && fits(sha)) {
        env.push((SOURCE_REVISION_VARIABLE.to_owned(), sha.to_owned()));
    }
    env
}

/// The variable [`routed_env`] pins a routed run's builds with.
const SOURCE_REVISION_VARIABLE: &str = "SourceRevisionId";

/// Whether the warm-up record at `path` is current for the run: `warm` on `host`, the builder the run went to, for the
/// warm-up file of `hash`. A record of another builder, hash or state, or one that cannot be read, is not, and is left
/// alone.
fn record_is_current(path: &Path, host: &str, hash: &str) -> bool {
    matches!(
        warm::read_warm_record(path),
        RecordRead::Parsed(record) if record.state == RecordState::Warm && record.host == host && record.hash == hash
    )
}

/// What a routed run's call knows of the work folder's warm-up: that the repo's warm-up is set up with the warm-up file
/// of `warm_hash` when the route found one, else nothing.
fn warmth_of(warm_hash: Option<&str>) -> remote::Warmth<'_> {
    warm_hash.map_or(remote::Warmth::Unasked, remote::Warmth::Consented)
}

/// The client's call for `remote`, with the run's log, limits and interrupt, made of the run's `parts` so the run's
/// client can be borrowed apart from them.
///
/// # Errors
///
/// Why a sibling the route carries fails its check again ([`remote::carried`]); no call is made then.
fn call_of<'r>(parts: CallParts<'r>, remote: &'r RemoteRun) -> Result<RemoteCall<'r>, String> {
    let siblings = remote::carried(&remote.root, &remote.siblings)?;
    let CallParts {
        request,
        figures: Figures { ceiling, usual },
        log,
        host,
        root,
        warmth,
        env,
        session,
    } = parts;
    Ok(RemoteCall {
        builder: &remote.builder,
        key: &remote.key,
        session,
        root: &remote.root,
        root_id: remote.root_id,
        leaf: &remote.leaf,
        siblings,
        repo: &remote.repo,
        subfolder: &remote.subfolder,
        words: &remote.words,
        outputs: &remote.outputs,
        outputs_withheld: remote.outputs_withheld,
        bins_withheld: remote.bins_withheld,
        failure_outputs: &remote.failure_outputs,
        dropped_no_build: remote.dropped_no_build,
        send: remote.send.as_ref(),
        cross_build: remote.cross_build.as_ref(),
        inputs: &remote.inputs,
        desktop: remote.desktop,
        log: log.path(),
        timeout: ceiling,
        usual,
        stall: request.stall,
        tail: request.tail,
        no_markers: request.no_markers,
        bring_tracked: &request.bring_tracked,
        env,
        interrupt: host.interrupt,
        state: &root.root,
        state_dir: &root.path,
        warmth,
    })
}

/// The slot a run holds, or the machine a critical run holds, once it has taken it, and when it took it.
struct Hold {
    taken: Option<Taken>,
    machine: Option<Machine>,
    /// When the run took what it holds; `None` once a builder's refusal let the remote slot go, until the run takes
    /// another, so a release journalled then counts no time held.
    took: Option<Instant>,
}

impl Hold {
    /// A hold of nothing yet, its take counted from now until a slot or the machine is taken.
    fn nothing() -> Hold {
        Hold {
            taken: None,
            machine: None,
            took: Some(Instant::now()),
        }
    }

    /// How long the run has held what it holds, in seconds: none once a refusal let the remote slot go
    /// ([`Hold::took`]).
    fn held_seconds(&self) -> f64 {
        self.took.map_or(0.0, |took| took.elapsed().as_secs_f64())
    }
}

/// How a wait for a slot ended.
enum Taking {
    /// The slot was taken at this moment, and the run runs under these figures.
    Took(Instant, Figures),
    /// The run's light parent lent it its slot while a critical request drains; the run left the queue, and runs
    /// under these figures.
    Lent(Figures),
    /// A Ctrl+C ended the wait.
    Interrupted,
    /// The remote pool stayed full past the wait's limit.
    Full,
    /// A critical request, its gate `by`, holds this light run back with no place beside it; the run left the queue it
    /// joined at `queued`.
    HeldBack { by: u32, queued: Instant },
}

/// How one builder's attempt of a routed run ended ([`Run::attempt`]).
enum Attempt {
    /// The run ends as this says.
    Done(Remotely),
    /// The builder sent the run back.
    Refused(Refusal),
}

impl Attempt {
    /// The attempt the builder refused for `reason`, with the client's `outcome` and the remote slot `held`.
    fn refused(reason: FallbackReason, outcome: RemoteOutcome, held: Option<Taken>) -> Attempt {
        Attempt::Refused(Refusal {
            reason,
            outcome: Box::new(outcome),
            held,
        })
    }
}

/// A builder's refusal of a routed run's attempt: the reason, the client's outcome and the remote slot still held.
struct Refusal {
    reason: FallbackReason,
    outcome: Box<RemoteOutcome>,
    held: Option<Taken>,
}

/// Where a routed run goes after a builder's attempt fell back ([`Run::next_candidate`]).
enum Next {
    /// To this later builder.
    Builder(Box<RemoteRun>),
    /// Here.
    Here,
    /// Nowhere: a Ctrl+C ended the walk over the later builders.
    Interrupted,
}

/// How a routed run's remote attempt ended.
enum Remotely {
    /// The client ran the command to its end.
    Ended(Ended),
    /// A Ctrl+C ended the wait for the remote client to start or for a remote slot.
    Interrupted,
    /// The run falls back here.
    Here,
}

/// What a passed line says of how long the command ran: here, by the wall clock and load-adjusted, or on a builder,
/// by the wall clock, the whole routed run's and its parts', and load-adjusted as the builder counted it, `None` when
/// it said no figure.
#[derive(Clone, Copy)]
enum Pass<'h> {
    Here(Timing),
    On(&'h str, Duration, Split, Option<f64>),
}

/// A run whose verdict may pass: how long it ran, as its passed line says, the ceiling it ran under in whole seconds,
/// and the lines a passing routed build says of the folders it left stale ([`stale_lines`]).
#[derive(Clone, Copy)]
struct Passing<'h> {
    pass: Pass<'h>,
    ceiling: u64,
    stale: &'h [String],
}

/// How long a command ran: by the wall clock, in load-adjusted seconds, `None` once the sampler failed and they are
/// unknown, and the part of its wall time paused while a gate under it waited behind a critical request; and the cores
/// it kept busy and found free, `None` when no unpaused sample measured them, the sampler failed, its CPU could not be
/// read at its end, or a gate under it took a slot of its own, which `nested_took_slot` says.
#[derive(Clone, Copy)]
struct Timing {
    wall: Duration,
    adjusted: Option<f64>,
    paused: Duration,
    cores: Option<Cores>,
    nested_took_slot: bool,
}

/// How a watched command ended, as its verdict reads it: its status, how long it ran, and the line of a kill.
struct Outcome<'k> {
    exited: i64,
    timing: Timing,
    kill: Option<&'k str>,
}

/// How the run ended: the gate's status, and the facts its release line carries when its verdict was reached.
struct Ended {
    status: i64,
    facts: Option<RunFacts>,
}

impl<'a> Run<'a> {
    /// Has a routed gate a commit run's slot covers route beside that run, its nesting kept in `beside`: it queues,
    /// takes and releases a remote slot of its own as a gate under no covering parent does, its lines naming the commit
    /// run as its parent. Once it falls back here it runs under the commit run's slot again
    /// ([`Run::back_under_commit`]). Any other run is left as it is.
    fn route_beside_commit(&mut self, route: &Route, beside: &'a mut Option<Nesting>) {
        if !matches!(route, Route::Routed(_)) {
            return;
        }
        let Some(parent) = self.nesting.under_commit() else {
            return;
        };
        self.here_nesting = Some(self.nesting);
        self.nesting = beside.insert(Nesting::Beside(parent.clone()));
    }

    /// Has the run take the request and nesting of a run here that runs `words` and asks for a heavy slot
    /// ([`Run::as_heavy`]), kept in [`Run::heavy_store`]. Its journal lines, ticket and claim then name the heavy kind and
    /// the command it runs. A light parent whose slot no longer covers it is asked again whether it lends that slot
    /// before the run takes one ([`Run::lend_again`]).
    ///
    /// # Panics
    ///
    /// When the run already ran as heavy: a run drops its `--no-build` once, here from the start or after a fallback.
    fn run_as_heavy(&mut self, words: &[String], console: &mut Console) {
        let heavy = self.as_heavy(words, console);
        self.run_as(heavy);
        self.lend_again(console);
    }

    /// Has a critical run that dropped its `--no-build` run `words` on the critical slot it asked for, its request and
    /// nesting as they were but for the command, kept in [`Run::heavy_store`]. Its journal lines, ticket and claim then
    /// name the command it runs.
    ///
    /// # Panics
    ///
    /// As [`Run::run_as_heavy`] does.
    fn run_as_critical(&mut self, words: &[String]) {
        let request = RunRequest {
            command: words.iter().map(OsString::from).collect(),
            ..self.request.clone()
        };
        let nesting = self.nesting.clone();
        self.run_as((request, nesting));
    }

    /// Has the run take `dropped`, the request and nesting of a run here that dropped its `--no-build`, kept in
    /// [`Run::heavy_store`], and open its journal for the gate running that request.
    ///
    /// # Panics
    ///
    /// When the run already took one: a run drops its `--no-build` once, here from the start or after a fallback.
    fn run_as(&mut self, dropped: (RunRequest, Nesting)) {
        #[expect(clippy::expect_used, reason = "a run drops its --no-build once, here from the start or after a fallback")]
        let store = self.heavy_store.take().expect("a run drops its --no-build at most once");
        let (request, nesting) = store.insert(dropped);
        self.request = request;
        self.nesting = nesting;
        self.gate = self.gate.running(request);
        self.journal = RunJournal::open(self.root, &self.gate);
    }

    /// Puts a gate that routed beside a commit run ([`Run::route_beside_commit`]) back under that run's slot as it falls
    /// back here, with no durations, which it opened for its run on the builder. One that drops its `--no-build`
    /// (`dropped` holds the words it runs) where the commit's slot does not cover a heavy gate runs as heavy
    /// ([`Run::run_as_heavy`]), as such a run here from the start does. One the commit's slot then covers says and
    /// journals its cover now, before its plan, so its journal ends whatever comes next. Any other run is left as it is.
    fn back_under_commit(&mut self, dropped: Option<&[String]>, console: &mut Console) {
        let Some(nesting) = self.here_nesting.take() else {
            return;
        };
        self.nesting = nesting;
        self.durations = None;
        let uncovered_heavy = nesting.covering().is_some_and(|parent| !covers(parent.held, Slot::Heavy));
        if let Some(words) = dropped
            && uncovered_heavy
        {
            self.run_as_heavy(words, console);
        }
        if let Some(parent) = self.nesting.covering() {
            if let Some(line) = self.nesting.line(self.request.slot, self.request.spelling) {
                console.err(&line);
            }
            self.cover(parent.pid, console);
        }
    }
}

/// The status a run of `request` ends with as soon as it is routed by `route`: a refusal's usage status, its lines said
/// by the decision, written to the log a run readies before routing so that an earlier file there is replaced by them
/// ([`readies_log_first`]), while a run pinned with `--builder`, whose log is readied only once routing accepted the
/// pin, leaves an earlier file as it was; or, for such a pinned run, the failed status of a log that cannot be readied.
/// `None` for a run that goes on.
fn ended_at_route(route: &Route, request: &RunRequest, log: &Log, console: &mut Console) -> Option<u32> {
    if matches!(route, Route::Refused(_)) {
        if readies_log_first(request)
            && let Err(error) = log.finish(console)
        {
            console.err(&format!("gate: cannot write to {}: {error}", log.path().display()));
        }
        return Some(u32::from(USAGE_STATUS));
    }
    if request.builder.is_some() {
        return prepare_log(log, console).err();
    }
    None
}

/// Readies `log` for the run ([`Log::prepare`]); else says why, and the gate's failed status.
fn prepare_log(log: &Log, console: &mut Console) -> Result<(), u32> {
    log.prepare().map_err(|problem| {
        console.err(&format!("gate: {problem}"));
        exit_status(GATE_FAILED)
    })
}

impl Run<'_> {
    /// Decides where the command runs, and sets the remote tag of a routed run. A gate a parent's slot covers runs
    /// here, as does one whose light parent lends it its slot before it queues, while a critical request drains, which
    /// is not routed ([`Run::on_parent`]); a run pinned with `--builder` asks for no lend, so it routes whatever a
    /// critical request is doing. The route, and what the decision learnt of the gate's place in its worktree.
    /// The hash of the warm-up file the route asks the builder's work folder to be warm with is kept for the call.
    fn route(&mut self, environment: &Environment, console: &mut Console) -> (Route, Placed) {
        if self.request.builder.is_none() && self.lend_before_queue(console) {
            self.lent = true;
            return (Route::Local { reason: None }, Placed::NotAsked);
        }
        // A gate inside a box run stays here as a covered gate does, saying nothing. A gate a commit run's slot covers
        // is asked as one under no parent, so a build in the commit's hooks still routes.
        let covering = self.nesting.routes_as_covered();
        let route::Decided {
            route,
            placed: place,
            warm_hash,
            decision,
        } = route::route(self.request, environment, covering, &self.root.path, console);
        self.warm_hash = warm_hash;
        self.would = Some(decision);
        self.tag = match &route {
            Route::Routed(remote) => Some(RemoteTag {
                host: remote.builder.alias.clone(),
                key: remote.key.clone(),
                critical_pid: None,
            }),
            Route::Local { .. } | Route::Refused(_) => None,
        };
        (route, place)
    }

    /// The durations of a run that takes a slot of its own, read once it knows the command it journals and before it
    /// queues, its folder placed as the routing decision `placed` it when the decision asked git: `None` for a no-slot
    /// run, one a parent's slot covers or lends, and one whose journal lines cannot be written. A gate that routes beside
    /// the commit run whose slot covers it has them for its run on the builder ([`Run::back_under_commit`]).
    fn open_durations(&self, placed: &Placed) -> Option<RunDurations> {
        if matches!(self.request.slot, Slot::None) || !takes_own_slot(self.nesting, self.lent) {
            return None;
        }
        let identity = self.gate.journal_identity().ok()?;
        let run = RunIdentity {
            cwd: &identity.cwd,
            command: &identity.command,
            critical: matches!(self.request.slot, Slot::Critical),
            owner: RebuildOwner {
                pid: self.gate.pid,
                start_ms: self.gate.start_ms,
            },
        };
        Some(RunDurations::open(&self.root.path, &run, placed, &self.request.test_seams))
    }

    /// The estimate of the slot the run is about to take, keyed by where it takes it: on the builder of a routed attempt,
    /// else here. `None` for a run without durations or a key without history.
    fn estimate(&mut self, console: &mut Console) -> Option<Estimate> {
        let host = self.tag.as_ref().map(|tag| tag.host.as_str());
        self.durations.as_mut()?.estimate(host, console)
    }

    /// Journals the run's `release` line of `slots`, held `held_seconds`, with how the run `ended` and the run's remote
    /// tag, and files the run in the durations store in the same hold of the journal mutex when it took a slot of its own
    /// and reached a verdict.
    fn write_release(&mut self, slots: Vec<String>, held_seconds: f64, ended: Option<&Ended>, console: &mut Console) {
        let facts = ended.and_then(|ended| ended.facts);
        let routing = if self.routing.is_empty() {
            None
        } else {
            Some(Box::new(std::mem::take(&mut self.routing)))
        };
        let release = JournalEvent::Release {
            slots,
            held_seconds,
            status: ended.map(|ended| ended.status),
            facts,
            trace: std::mem::take(&mut self.trace),
            cold: self.cold,
            path: self.tailscale_path(console),
            routing,
        };
        let remote = self.tag.as_ref();
        let filing = self
            .durations
            .as_ref()
            .and_then(|durations| durations.filing(remote.map(|tag| tag.host.as_str()), facts, self.cold));
        let filed = self
            .journal
            .write_then(&release, remote, console, |time| filing.as_ref().map(|filing| filing.file(time)));
        if let (Some(durations), Some(Some(filed))) = (&mut self.durations, filed) {
            durations.filed(filed, console);
        }
    }

    /// Starts reading Tailscale's path to the builder of `remote`, whose call returned, on a thread of its own, ended by
    /// the run's Ctrl+C; `None`, with a note in the log, when the thread cannot be started.
    fn start_tailscale_read(&self, remote: &RemoteRun, console: &mut Console) -> Option<tailscale::PendingPath> {
        let seams = &self.request.test_seams;
        let read = tailscale::ReadOf {
            alias: remote.builder.alias.clone(),
            status_flag: seams.tailscale_status.clone(),
            worktree: remote.root.clone(),
            interrupt: self.host.interrupt,
            delay: seams.tailscale_delay_ms.map(Duration::from_millis),
            hold_start: seams.tailscale_hold_start_ms.map(Duration::from_millis),
        };
        tailscale::PendingPath::start(read)
            .map_err(|why| console.note(&format!("gate: tailscale path not read: {why}")))
            .ok()
    }

    /// Tailscale's path to the builder the routed run's call went to, as its `release` line says it: taken only when
    /// its read has finished, never waited on (but for `--test-tailscale-wait-ms`); `None` when unknown and for a run
    /// that made no builder call. The log alone notes `gate: tailscale path not read: <why>` for a read that failed,
    /// timed out or was interrupted, `gate: tailscale path not read in time for the release line` for one not finished,
    /// and `gate: tailscale path read failed` for one whose thread ended without an answer ([`tailscale::Taken::note`]).
    fn tailscale_path(&mut self, console: &mut Console) -> Option<String> {
        let pending = self.tailscale_read.take()?;
        let wait = self.request.test_seams.tailscale_wait_ms.map(Duration::from_millis);
        let taken = pending.taken(wait);
        if let Some(note) = taken.note() {
            console.note(&note);
        }
        taken.journal_word()
    }

    /// Rebuilds the durations store once the run has released, when a read of the run asked for it, unless a Ctrl+C
    /// comes before or during the rebuild.
    fn rebuild_durations(&mut self, console: &mut Console) {
        if let (Some(durations), Some(lock)) = (&mut self.durations, self.journal.mutex()) {
            durations.rebuild(lock, self.host.interrupt, console);
        }
    }

    /// Asks a light parent whose slot does not cover this run, once it dropped its `--no-build` and asks for a heavy
    /// slot, whether it lends that slot before the run queues, as [`Run::route`] asked of the nesting the run was read
    /// with; a run already lent its parent's slot keeps it, and a run pinned with `--builder` asks for none.
    fn lend_again(&mut self, console: &mut Console) {
        if !self.lent && self.request.builder.is_none() {
            self.lent = self.lend_before_queue(console);
        }
    }

    /// Whether the light parent whose slot does not cover the run lends it that slot before it queues
    /// ([`wait::lent_before_queue`]), its lend marker written under the floor of its ceiling holding the beside mutex
    /// the lend was decided under. A marker that cannot be written lends nothing: the run queues.
    fn lend_before_queue(&mut self, console: &mut Console) -> bool {
        let Nesting::Under(parent) = self.nesting else {
            return false;
        };
        let Some(beside) = wait::lent_before_queue(self.root, &mut self.survey, console) else {
            return false;
        };
        self.lend = self.lend_marker(parent.pid, Figures::floor(self.request.timeout).ceiling, None, console);
        drop(beside);
        self.lend.is_some()
    }

    /// The lend marker of this run on the slot of its light parent `parent_pid`, under `ceiling`, with what the
    /// durations store `expected` of the run; `None` when it cannot be written, said.
    fn lend_marker(&self, parent_pid: u32, ceiling: u64, expected: Option<&Map<String, Value>>, console: &mut Console) -> Option<LendMarker> {
        let of = ClaimOf {
            gate: &self.gate,
            ceiling,
            log: self.log.path(),
            remote: None,
            reason: self.request.reason.as_deref(),
            expected,
        };
        LendMarker::write(&self.root.path, parent_pid, &of, console)
    }

    /// Journals the queue of the run `route` sends: with `notRouted` for a heavy gate its caller kept here with
    /// `--local`, with the remote tag for a routed one. A gate a parent's slot covers journals `covered` with the
    /// parent's pid in place of the queue, then writes its covered marker ([`Covered`]). A gate its light parent lent
    /// its slot before it queued journals nothing here. A routed run keeps the moment of its queue line in
    /// [`Run::queued_at`], which its local take counts from when the full remote pool sent it back.
    fn queue(&mut self, route: &Route, console: &mut Console) {
        if self.lent {
            return;
        }
        if matches!(route, Route::Routed(_)) {
            self.queued_at = Some(Instant::now());
        }
        if let Some(parent) = self.nesting.covering() {
            self.cover(parent.pid, console);
            return;
        }
        self.routing.not_routed = route::not_routed(route);
        self.routing.reason = self.request.reason.clone();
        let event = JournalEvent::Queue {
            log: self.log.path().to_string_lossy().into_owned(),
            not_routed: self.routing.not_routed.clone(),
            reason: self.routing.reason.clone(),
            pinned: self.request.builder.is_some(),
        };
        self.journal.write(&event, self.tag.as_ref(), console);
    }

    /// Journals `covered` with the pid of the parent gate `parent_pid` whose slot covers the run, then writes its covered
    /// marker ([`Covered`]).
    fn cover(&mut self, parent_pid: u32, console: &mut Console) {
        self.journal.write(&JournalEvent::Covered { parent_pid }, self.tag.as_ref(), console);
        self.covered = Some(Covered::write(&self.root.path, &self.gate, parent_pid, console));
    }

    /// Runs the command where `route` sends it: on the builder, and here after a fallback; or here ([`Run::here`]), as
    /// the words `local` says when it dropped the command's `--no-build` ([`Run::local_words`]). How the run ended, with
    /// the words a run here ran, whose passing build clears the worktree's nothing-back record
    /// ([`Run::clear_nothing_back`]); `None` when a Ctrl+C ended a wait for a slot or for the remote client to start.
    fn through(
        &mut self,
        route: &Route,
        plan: Plan,
        local: Option<HereWords>,
        hold: &mut Hold,
        console: &mut Console,
    ) -> Option<(Ended, Option<HereWords>)> {
        let planned = match route {
            Route::Local { .. } => self.plan_local(plan, local, console),
            Route::Routed(remote) => match self.routed(remote, hold, console) {
                Remotely::Ended(ended) => return Some((ended, None)),
                Remotely::Interrupted => return None,
                Remotely::Here => self.plan_here(remote, plan, console),
            },
            Route::Refused(_) => unreachable!("run returns a refused pin's usage status before the run goes through"),
        };
        let (plan, here) = match planned {
            Ok(planned) => planned,
            Err(ended) => return Some((ended, None)),
        };
        let ended = self.here(plan, hold, console)?;
        // A light run a critical request sent to a builder ran its words there; one that came back keeps them, since
        // its fallback cleared its tag ([`Run::give_up`]).
        Some((ended, here.filter(|_| self.tag.is_none())))
    }

    /// Runs the command here. A gate a parent's slot covers takes no slot; nor does one whose light parent lends it its
    /// slot while a critical request drains, before it queued or as it waits ([`Run::on_parent`]). Any other gate takes
    /// a slot into `hold`, one whose parent's slot does not cover it saying so first, before any wait line. A light run
    /// a critical request holds back with no place beside it asks once whether its command would route on heavy
    /// ([`route::route_as_heavy`]): when it would, it says so, closes its log and runs on the builder ([`Run::routed`]),
    /// its remote tag naming the request; a refusal or fallback, or a command that would not route, sends it back to
    /// its wait here, keeping the instant it queued. How the run ended; `None` when a Ctrl+C ended the wait for a slot
    /// or for the remote client to start.
    fn here(&mut self, plan: Plan, hold: &mut Hold, console: &mut Console) -> Option<Ended> {
        self.open_log(console);
        let nesting = self.nesting;
        let floor = Figures::floor(self.request.timeout);
        if nesting.covering().is_some() {
            return Some(self.command(plan, floor, console));
        }
        if let Nesting::Under(parent) = nesting {
            if self.lent {
                return Some(self.on_parent(parent, plan, floor, console));
            }
            console.err(&parent.uncovered_line(self.request.slot));
        }
        loop {
            match self.take_own(hold, console) {
                Taking::Took(took, figures) => {
                    hold.took = Some(took);
                    return Some(self.command(plan, figures, console));
                }
                Taking::Lent(figures) => return Some(self.lent_as_it_waited(plan, hold, figures, console)),
                Taking::Interrupted => return None,
                // Only a remote pool has a wait limit, so a local take never times out.
                Taking::Full => unreachable!("a local pool is never full past a limit"),
                Taking::HeldBack { by, queued } => match self.route_held_back(by, queued, hold, console) {
                    Remotely::Ended(ended) => return Some(ended),
                    Remotely::Interrupted => return None,
                    // Back to its wait, in its first place.
                    Remotely::Here => {}
                },
            }
        }
    }

    /// Asks once whether the light run the critical request of gate `by` held back, out of the queue it joined at
    /// `queued`, would route on heavy, and runs it on the builder when it would ([`Run::here`]). How the run ended
    /// there; [`Remotely::Here`] when it goes back to its wait here, its command not routing or its route fallen back.
    /// The lines the route says are held, not written to the log, as a heavy routed run's are before its log opens: a
    /// fallback removes the log the client wrote, and the reopened log takes them, the fallback line after them.
    fn route_held_back(&mut self, by: u32, queued: Instant, hold: &mut Hold, console: &mut Console) -> Remotely {
        self.reroute_tried = true;
        self.queued_at = Some(queued);
        console.detach();
        let routed = self
            .would
            .as_ref()
            .and_then(|decision| route::route_as_heavy(self.request, decision, by, console));
        let Some((remote, warm_hash)) = routed else {
            // It waits on, held back like any light gate, its log taking the notes the decision held.
            if let Some(writer) = &self.writer {
                console.attach(writer);
            }
            return Remotely::Here;
        };
        self.close_log(console);
        self.warm_hash = warm_hash;
        self.rerouted_for = Some(by);
        self.tag = Some(RemoteTag {
            host: remote.builder.alias.clone(),
            key: remote.key.clone(),
            critical_pid: Some(by),
        });
        let remotely = self.routed(&remote, hold, console);
        if matches!(remotely, Remotely::Here) {
            self.rerouted_for = None;
            self.queued_at = Some(queued);
            self.open_log(console);
        }
        remotely
    }

    /// Waits for and takes the run's own slot of its pool into `hold`, or the machine for a critical run; a no-slot run
    /// takes nothing and waits for nothing ([`Run::take_nothing`]).
    fn take_own(&mut self, hold: &mut Hold, console: &mut Console) -> Taking {
        match Takes::of(self.nesting.takes(self.request.slot)) {
            Takes::Pool(pool) => self.take(pool, self.queued_at, None, &mut hold.taken, console),
            Takes::Machine => self.take_machine(&mut hold.machine, console),
            Takes::Nothing => self.take_nothing(console),
        }
    }

    /// Starts a no-slot run at once: no ticket, claim or mutex, and no waiter woken. It journals a `take` of no slot,
    /// waited for nothing, naming the parent gate it runs under when there is one.
    fn take_nothing(&mut self, console: &mut Console) -> Taking {
        let took = Instant::now();
        let parent_pid = self.nesting.uncovering().map(|parent| parent.pid);
        self.routing.parent_pid = parent_pid;
        self.journal
            .write(&take_event(Vec::new(), 0.0, Vec::new(), parent_pid), self.tag.as_ref(), console);
        Taking::Took(took, Figures::floor(self.request.timeout))
    }

    /// Runs the command on the slot of the light parent that lent it as the gate waited ([`Run::on_parent`]), under the
    /// `figures` of its take; `hold`'s time starts at the end of the wait, which a release journalled for a command that
    /// never ran counts from.
    fn lent_as_it_waited(&mut self, plan: Plan, hold: &mut Hold, figures: Figures, console: &mut Console) -> Ended {
        let nesting = self.nesting;
        let Nesting::Under(parent) = nesting else {
            unreachable!("only a gate under a light parent is lent its parent's slot")
        };
        hold.took = Some(Instant::now());
        self.on_parent(parent, plan, figures, console)
    }

    /// Runs the command on the slot of the light parent `parent`, which lends it while a critical request drains:
    /// journals `parent` with the parent's pid, then says so and runs it, so a reader that sees the line finds its
    /// event. The run takes, claims and releases no slot. Its command keeps the marker of this gate as a heavy holder
    /// ([`Plan::prepare`]), so a gate nested in it runs covered by the lend and asks for none of its own. The command
    /// runs under `figures`.
    fn on_parent(&mut self, parent: &Parent, plan: Plan, figures: Figures, console: &mut Console) -> Ended {
        self.lent = true;
        self.journal.write(&JournalEvent::Parent { parent_pid: parent.pid }, None, console);
        console.err(&format!(
            "gate: running under the {} slot of gate {} while a critical request drains; taking no slot",
            parent.held.as_str(),
            parent.pid
        ));
        self.command(plan, figures, console)
    }

    /// Runs the routed command on the builder, and on the later builders of the route after a refusal for a reason
    /// worth another builder ([`RETRIED`]): takes the client's work-folder key lock once, the worktree's on every
    /// builder, held until the run ends, then tries each builder in turn ([`Run::attempt`]), its remote slot taken into
    /// `hold` and freed when it falls back. A key lock another run of the worktree holds is waited for up to its bound,
    /// and one that stays held, like a client not ready, falls back here before any remote slot is taken. Each later
    /// candidate is built when it is tried ([`Run::next_candidate`]) and says its route lines once the fallback line
    /// naming it is said; the run's remote tag names it from its take on. A run whose last attempt fell back runs
    /// here, its local take counting from the run's queue line only when that fallback was the full remote pool; one a
    /// Ctrl+C or the client ended ends as it did.
    fn routed(&mut self, remote: &RemoteRun, hold: &mut Hold, console: &mut Console) -> Remotely {
        let (remotely, last) = self.try_builders(remote, hold, console);
        // Only a run whose last fallback is the full remote pool counts its local take from its own queue line; the
        // local take of every other fallback counts from the take itself, as a run that never fell back does.
        if last != Some(FallbackReason::Full) {
            self.queued_at = None;
        }
        remotely
    }

    /// The attempts of [`Run::routed`], and the reason of the last builder's fallback when a builder's attempt fell back,
    /// a walk over the later builders a Ctrl+C ended after it included, and `inputs` for a built-here record that cannot
    /// be removed before an attempt ([`Run::clear_built_here`]); `None` when the run fell back before any attempt for
    /// another reason, or an attempt ended it.
    fn try_builders(&mut self, remote: &RemoteRun, hold: &mut Hold, console: &mut Console) -> (Remotely, Option<FallbackReason>) {
        // Nothing is held until an attempt takes a remote slot: a pinned run that ends before then counts no time held,
        // and a run that falls back here sets it again at its local take.
        hold.took = None;
        let pinned = self.request.builder.is_some();
        if let Err(why) = self.client.ready() {
            return (self.fell_back(FallbackReason::Client, &RemoteOutcome::not_started(why), console), None);
        }
        // The key lock reads neither figure of the call; no take has resolved them yet. A sibling that fails its check
        // again sends nothing: the run falls back here before it takes the lock or a slot.
        let _key = match self.take_key(remote, console) {
            Ok(key) => key,
            Err(remotely) => return (remotely, None),
        };
        // Under the key lock, once a run: the key is the same for every builder the run tries.
        worktree_record::leave(&self.root.path, &remote.root, &remote.repo, &remote.key, console);
        // One drain session for every attempt and poll of the run.
        let session = remote::payload::run_nonce();
        let mut waited = Duration::ZERO;
        let mut looked = 0;
        let mut later: Option<Box<RemoteRun>> = None;
        loop {
            let current = later.as_deref().unwrap_or(remote);
            // Before each attempt, whose builder may plan another drop, so a run killed on the builder leaves its
            // entries cleared. A record that cannot be removed would outlast the build there: the run falls back here.
            if let Err(line) = self.clear_built_here(current) {
                console.err(&line);
                let reason = FallbackReason::Inputs;
                return (self.fell_back(reason, &RemoteOutcome::not_started(line), console), Some(reason));
            }
            let refusal = match self.attempt(current, hold, (&mut waited, &session), console) {
                Attempt::Done(remotely) => return (remotely, None),
                Attempt::Refused(refusal) => refusal,
            };
            if pinned {
                // A pinned run ends on its builder's refusal, its slot kept in the hold its release lets go; one the
                // full pool turned away took none, so its take is still unset.
                let Refusal { reason, outcome, held } = refusal;
                hold.taken = held;
                let said = self.drop_warm_record(current, reason);
                return (self.pin_ended(reason, &outcome, said.as_deref(), console), Some(reason));
            }
            // The refused attempt's slot is let go below: the run holds nothing until it takes another.
            hold.took = None;
            let reason = refusal.reason;
            match self.after_refusal(remote, current, refusal, &mut looked, console) {
                Ok(next) => later = Some(next),
                Err(remotely) => return (remotely, Some(reason)),
            }
        }
    }

    /// Ends the attempt on `current` that the builder refused ([`Refusal`]): drops the warm-up record a `layout` refusal
    /// leaves stale ([`Run::drop_warm_record`]), gives the attempt up ([`Run::give_up`]), its slot let go and its lines
    /// noted before anything of the next builder is built, then looks for the next builder of the routed run `first`
    /// ([`Run::next_candidate`]). The next builder's run, once the fallback line naming it and its route lines are said
    /// and the run's remote tag names it; else how the run ends: here, saying so, or, when a Ctrl+C ended the walk, as
    /// an interrupt before the command started, saying only why the builder did not run it.
    fn after_refusal(
        &mut self,
        first: &RemoteRun,
        current: &RemoteRun,
        refusal: Refusal,
        looked: &mut usize,
        console: &mut Console,
    ) -> Result<Box<RemoteRun>, Remotely> {
        let Refusal { reason, outcome, held } = refusal;
        let said = self.drop_warm_record(current, reason);
        let host = self.give_up(reason, &outcome, held, console);
        let next = match self.next_candidate(first, reason, looked, console) {
            Next::Interrupted => {
                console.err(&fallback_head(&host, reason, outcome.detail.as_deref()));
                return Err(Remotely::Ended(self.interrupted_before_start(console)));
            }
            Next::Here => None,
            Next::Builder(next) => Some(next),
        };
        let goes = next.as_ref().map(|next| next.builder.alias.as_str());
        console.err(&fallback_line(&host, reason, outcome.detail.as_deref(), goes));
        if let Some(line) = &said {
            console.err(line);
        }
        let next = next.ok_or(Remotely::Here)?;
        next.lines().iter().for_each(|line| console.err(line));
        self.tag = Some(RemoteTag {
            host: next.builder.alias.clone(),
            key: next.key.clone(),
            critical_pid: self.rerouted_for,
        });
        Ok(next)
    }

    /// Where the routed run `first` goes after a fallback for `reason`: the first of its later candidates after the
    /// `looked` already looked at that is built ([`route::later_run`]), each looked at counted, and one skipped for a
    /// reason noted in the log naming it; here when the reason is not worth another builder ([`RETRIED`]) or no
    /// candidate is left. A Ctrl+C raised before a candidate is built, or by the time its build returned, built or
    /// skipped, ends the walk.
    fn next_candidate(&self, first: &RemoteRun, reason: FallbackReason, looked: &mut usize, console: &mut Console) -> Next {
        if !RETRIED.contains(&reason) {
            return Next::Here;
        }
        let Some(decision) = self.would.as_ref() else {
            return Next::Here;
        };
        let interrupted = || self.host.interrupt.is_some_and(Interrupt::is_raised);
        while let Some(candidate) = first.later.get(*looked) {
            *looked += 1;
            if interrupted() {
                return Next::Interrupted;
            }
            let built = self.build_candidate(decision, candidate, console);
            if interrupted() {
                return Next::Interrupted;
            }
            if let Some(run) = built {
                return Next::Builder(Box::new(run));
            }
        }
        Next::Here
    }

    /// The run of the later builder `candidate` ([`route::later_run`]) under the routing `decision`; `None` when it is
    /// skipped, noting in the log why, naming it, when the skip says why.
    fn build_candidate(&self, decision: &route::Decision, candidate: &LaterCandidate, console: &mut Console) -> Option<RemoteRun> {
        let asked = if self.rerouted_for.is_some() { Slot::Heavy } else { self.request.slot };
        let built = route::later_run(self.request, decision, candidate, asked, console);
        // The seam raises the console interrupt once the walk has built a candidate, before the run tries it.
        if self.request.test_seams.interrupt_before_later
            && let Some(interrupt) = self.host.interrupt
        {
            interrupt.raise();
        }
        match built {
            Ok(run) => Some(run),
            Err(why) => {
                if let Some(why) = why {
                    console.note(&format!("gate: not trying {}: {why}", candidate.alias));
                }
                None
            }
        }
    }

    /// One builder's attempt of the routed run `remote`: takes a remote slot of the builder's pool into `hold`, waiting
    /// only for what is left of the run's remote budget ([`wait::remote_budget`]) after the `waited` of its earlier
    /// attempts, which this wait adds to, then calls the client ([`Run::call_builder`]). A full pool is a refusal. A
    /// Ctrl+C ends the run here, with the remote slot released: one raised before the call, or during it over a refusal.
    fn attempt(&mut self, remote: &RemoteRun, hold: &mut Hold, (waited, session): (&mut Duration, &str), console: &mut Console) -> Attempt {
        let left = self.remote_left(*waited);
        let begun = Instant::now();
        // A light run a critical request sent here counts its remote take's wait from its first queue line.
        let since = self.queued_at.filter(|_| self.rerouted_for.is_some());
        let taking = self.take(
            Pool::Remote(BuilderPool::of(&remote.builder)),
            since,
            Some(left),
            &mut hold.taken,
            console,
        );
        *waited += begun.elapsed();
        let figures = match taking {
            Taking::Took(took, figures) => {
                hold.took = Some(took);
                figures
            }
            Taking::Lent(_) => unreachable!("only a local wait is lent a parent's slot"),
            Taking::Interrupted => return Attempt::Done(Remotely::Interrupted),
            Taking::Full => return Attempt::refused(FallbackReason::Full, RemoteOutcome::default(), None),
            Taking::HeldBack { .. } => unreachable!("only a local light wait is held back to route"),
        };
        // A Ctrl+C that came once the remote slot was held, before the client was called, ends the run as an
        // interrupt before a command starts does here; the caller releases the slot with the run's tag.
        if self.host.interrupt.is_some_and(Interrupt::is_raised) {
            return Attempt::Done(Remotely::Ended(self.interrupted_before_start(console)));
        }
        self.call_builder(remote, hold, (figures, session, waited), console)
    }

    /// What is left of the run's remote budget ([`wait::remote_budget`]) after `waited`, the time its remote waits took
    /// so far: none for a light run a critical request sent here, which never waits for the remote.
    fn remote_left(&self, waited: Duration) -> Duration {
        if self.rerouted_for.is_some() {
            Duration::ZERO
        } else {
            wait::remote_budget(self.request).saturating_sub(waited)
        }
    }

    /// Calls the client for the routed run `remote` under `figures`, the remote slot held in `hold`, and ends the run as
    /// the client ended it. A refusal or a client that cannot start is a refusal, with the slot still held. A Ctrl+C
    /// during the call over a refusal ends the run here, with the remote slot released. A call the builder refused
    /// `full` waits for a slot there within what is left of the run's remote budget after `waited`, which the wait adds
    /// to ([`behind_full`]), and a call the builder's drain refused waits behind the drain ([`behind_drain`]), each with
    /// the slot still held; a Ctrl+C during either wait ends the run here as one before the command started. A client
    /// that ended on its own keeps its own status, and the run writes or clears the worktree's nothing-back record
    /// ([`Run::leave_nothing_back`]).
    fn call_builder(
        &mut self,
        remote: &RemoteRun,
        hold: &mut Hold,
        (figures, session, waited): (Figures, &str, &mut Duration),
        console: &mut Console,
    ) -> Attempt {
        let mut started = Instant::now();
        let env = routed_env(&self.request.env, remote.source_revision.as_deref());
        let parts = CallParts {
            request: self.request,
            figures,
            log: &self.log,
            host: &self.host,
            root: self.root,
            warmth: warmth_of(self.warm_hash.as_deref()),
            env: &env,
            session,
        };
        let call = match call_of(parts, remote) {
            Ok(call) => call,
            Err(why) => return Attempt::refused(FallbackReason::Client, RemoteOutcome::not_started(why), hold.taken.take()),
        };
        let first = self.client.run(&call);
        let wait = SlotWait {
            request: self.request,
            left: self.remote_left(*waited),
            waited,
        };
        // A full builder is waited for within the run's remote budget, and a drain's refusal is no fallback: the run
        // waits behind it, and goes on with the outcome of its admitted attempt.
        let Some(Behind { outcome, admitted }) = after_waits(self.client.as_mut(), &call, first, wait, console) else {
            return Attempt::Done(Remotely::Ended(self.interrupted_before_start(console)));
        };
        // A wait for a builder slot or behind the drain is no part of the run's time: its slot's hold and its wall time
        // count from the admission.
        if let Some(admitted) = admitted {
            started = admitted;
            hold.took = Some(admitted);
        }
        let interrupted = self.host.interrupt.is_some_and(Interrupt::is_raised);
        // The path is read for every builder call that returned, a refused one that falls back here included, but for
        // one a Ctrl+C ended.
        if !interrupted {
            self.tailscale_read = self.start_tailscale_read(remote, console);
        }
        let fallback = outcome.fallback();
        // A Ctrl+C during the call wins over a refusal: the command never runs here.
        if fallback.is_some() && interrupted {
            return Attempt::Done(Remotely::Ended(self.interrupted_before_start(console)));
        }
        if let Some(reason) = fallback {
            return Attempt::refused(reason, outcome, hold.taken.take());
        }
        let wall = started.elapsed();
        self.cold = outcome.raised.is_some();
        let words = unicode_words(&self.request.command);
        self.leave_nothing_back(remote, &outcome, words.as_deref(), console);
        // A cold run's line names the ceiling the box was given.
        let ceiling = outcome.raised.unwrap_or(figures.ceiling);
        self.restore_after = restore_candidate(remote, &outcome, words.as_deref(), ceiling, started);
        let stale = stale_lines(remote, &outcome, words.as_deref());
        let passing = Passing {
            pass: Pass::On(&remote.builder.alias, wall, outcome.split, outcome.adjusted),
            ceiling,
            stale: &stale,
        };
        self.builder_call = BuilderCall::Ended;
        Attempt::Done(Remotely::Ended(self.remote_ended(&outcome, passing, wall, console)))
    }

    /// Drops the work folder's warm-up record after the client's refusal for `reason`, while the run still holds the
    /// work folder's key: a `layout` refusal deletes it while it is current for the run, `warm` on the run's builder
    /// for the warm-up file the route found ([`record_is_current`]), one already gone among them, and any other keeps
    /// it. A run of a repo whose warm-up is not set up neither reads nor deletes anything under `remote\warm`.
    /// The line to say once the run has fallen back, when the delete failed: why.
    fn drop_warm_record(&self, remote: &RemoteRun, reason: FallbackReason) -> Option<String> {
        if reason != FallbackReason::Layout {
            return None;
        }
        let path = v1::warm_record_path(&self.root.path, &remote.key);
        let current = self
            .warm_hash
            .as_deref()
            .is_some_and(|hash| record_is_current(&path, &remote.builder.alias, hash));
        match current.then(|| fs::remove_file(&path)) {
            Some(Err(error)) if error.kind() != io::ErrorKind::NotFound => Some(format!(
                "gate: cannot remove the warm-up record {}: {}",
                gate_router::escaped(&path.to_string_lossy()),
                gate_router::escaped(&error.to_string())
            )),
            _ => None,
        }
    }

    /// Updates the worktree's nothing-back record after a routed run that ended on the builder, killed, interrupted or
    /// unreported among them. A dotnet command whose `--no-build` the builder dropped, as the client's outcome says
    /// ([`RemoteOutcome::dropped_no_build`]: a run whose send of the PC's build landed kept it), built there and brings
    /// nothing back: it writes the record when none stands, and never rewrites or clears one. A dotnet build ([`dotnet_builds`])
    /// that passed with its outputs brought back ([`brought_everything_back`]) clears the record, as a passing build here
    /// of its words would ([`Run::clear_nothing_back`]); any other dotnet build writes it, an unknown counting as not
    /// brought back. The record names the builder and the configuration, framework and runtime the words set
    /// ([`build_settings`]), but for the configuration only when the record it replaces names the same one, compared
    /// ignoring letter case with one naming none differing from one naming a value: a record holds the last build's
    /// alone, so a build here of another configuration must not leave a standing record that a later `--no-build` run
    /// of the older configuration would keep its word by. One that cannot be written, or whose presence cannot be read,
    /// is said. The command's
    /// `words`, `None` when one is not valid Unicode ([`unicode_words`]), are the caller's, so one reading of them serves
    /// this record and the stale line.
    fn leave_nothing_back(&self, remote: &RemoteRun, outcome: &RemoteOutcome, words: Option<&[String]>, console: &mut Console) {
        let Some(words) = words else {
            return;
        };
        let path = v1::nothing_back_path(&self.root.path, &remote.key);
        if outcome.dropped_no_build {
            match path.try_exists() {
                Ok(false) => {}
                Ok(true) => return,
                Err(error) => {
                    console.err(&format!("gate: cannot read {}: {error}", path.display()));
                    return;
                }
            }
        } else if !dotnet_builds(words) {
            return;
        } else if brought_everything_back(outcome) {
            self.clear_nothing_back(HereWords::routed(remote, words.to_vec()), console);
            return;
        }
        let settings = build_settings(words);
        let standing = match no_build::read_record(&path) {
            Ok(standing) => standing,
            Err(error) => {
                console.err(&format!("gate: cannot read {}: {error}", path.display()));
                None
            }
        };
        let differs = |old: &Option<String>| match (old, &settings.configuration) {
            (Some(old), Some(new)) => old.to_lowercase() != new.to_lowercase(),
            (None, None) => false,
            _ => true,
        };
        let configuration = match standing.as_ref() {
            Some(record) if differs(&record.settings.configuration) => None,
            _ => settings.configuration.clone(),
        };
        let record = v1::NothingBackRecord {
            host: remote.builder.alias.clone(),
            configuration,
            framework: settings.framework,
            runtime: settings.runtime,
            built: Vec::new(),
        };
        if let Err(error) = v1::write_nothing_back(&path, &record) {
            console.err(&format!("gate: cannot write {}: {error}", path.display()));
        }
    }

    /// Ends a run that has let its slots and its work folder's key go: after a routed dotnet build that passed and
    /// brought everything back ([`restore_candidate`]), runs a `dotnet restore` here of the target the command names
    /// when that target has no `obj\project.assets.json` on this PC ([`restore_due`]), so a later `--no-build`
    /// run here keeps its word; then says the verdict's closing lines it held ([`Run::verdict`]), which stay the run's
    /// last. The restore runs as a run here of its words would, in the command's working folder with its environment,
    /// on no slot, bounded by [`Run::restore_limit`], its output in a side file beside the log ([`restore::side_log`]),
    /// and says how it went ([`restore::said`]). The status `ended` holds was fixed before, the console interrupt read
    /// for it among the rest ([`Run::remote_ended`]): however the restore goes, a Ctrl+C during it included, the run's
    /// status and verdict are the routed command's. A run that did not pass restores nothing and says nothing of it, and
    /// so does a passing run that is no restore candidate, a publish or a build that moves its output among them; one
    /// that cannot restore its target here says why ([`Run::restore_here`]).
    fn restore_and_close(&mut self, ended: &Ended, console: &mut Console) {
        let candidate = self.restore_after.take();
        let closing = std::mem::take(&mut self.closing);
        if let Some(build) = candidate.filter(|_| ended.status == 0) {
            self.restore_here(&build, console);
        }
        say(closing, console);
    }

    /// Ends a run that went through as `ended`, with the words `here` a run here ran: lets its `hold` go, restores here
    /// when a routed build needs it and says the verdict's closing lines ([`Run::restore_and_close`]), updates the
    /// worktree's nothing-back record after a passing run here ([`Run::settle_nothing_back`]) and rebuilds the duration
    /// history; the gate's exit status.
    fn ended_through(&mut self, hold: Hold, ended: &Ended, here: Option<HereWords>, console: &mut Console) -> u32 {
        self.release(hold, Some(ended), console);
        self.restore_and_close(ended, console);
        if ended.status == 0
            && let Some(here) = here
        {
            self.settle_nothing_back(here, console);
        }
        self.rebuild_durations(console);
        exit_status(ended.status)
    }

    /// The restore here of `build` ([`Run::restore_and_close`]), when its target needs one ([`restore_due`]), which says
    /// nothing otherwise. A target that needs one but is no project or solution file ([`restore::names_restorable`]),
    /// a Ctrl+C already pressed, or under [`restore::RESTORE_LEAST_LEFT`] of the routed command's ceiling left, in that
    /// order, skips it with a line saying why ([`restore::not_restoring`]).
    fn restore_here(&self, build: &RoutedBuild, console: &mut Console) {
        let Some(plan) = restore_due(build, console) else {
            return;
        };
        let limit = if !restore::names_restorable(plan.target()) {
            Err(NotRestoring::NotProjectOrSolution)
        } else if self.host.interrupt.is_some_and(Interrupt::is_raised) {
            Err(NotRestoring::Interrupted)
        } else {
            self.restore_limit(build).ok_or(NotRestoring::TooLittleLeft)
        };
        let limit = match limit {
            Ok(limit) => limit,
            Err(why) => {
                console.err(&restore::not_restoring(&plan, &why));
                return;
            }
        };
        let side = restore::side_log(self.log.path());
        let started = Instant::now();
        let ran = self
            .restore_launch(&plan, console)
            .and_then(|launch| restore::run_restore(&launch, &side, limit, self.host.interrupt));
        for line in restore::said(&plan, &side, started.elapsed(), limit, ran) {
            console.err(&line);
        }
    }

    /// How long the restore here of `build` may take: [`restore::RESTORE_LIMIT`], or `--test-restore-limit-ms`, cut to
    /// what is left of the routed command's ceiling counted from the routed call's start; `None` when under
    /// [`restore::RESTORE_LEAST_LEFT`] is left.
    fn restore_limit(&self, build: &RoutedBuild) -> Option<Duration> {
        let limit = self
            .request
            .test_seams
            .restore_limit_ms
            .map_or(restore::RESTORE_LIMIT, Duration::from_millis);
        let left = build.ceiling.saturating_sub(build.started.elapsed());
        (left >= restore::RESTORE_LEAST_LEFT).then(|| limit.min(left))
    }

    /// The launch of `plan`'s words, planned as a run here of them is ([`Plan::prepare`]); else why it cannot be made,
    /// without the `gate: ` its line starts with. It is planned under the nesting a run here has, so a gate that routed
    /// beside the commit run whose slot covers it ([`Run::route_beside_commit`]) plans it under that cover again. A run
    /// no parent covers hands on no slot marker ([`SLOT_HELD_VARIABLE`]), as it released its slot before the restore
    /// starts. A covered run hands on its parent's, as that parent still holds its slot while the restore runs.
    fn restore_launch(&self, plan: &RestorePlan, console: &mut Console) -> Result<Launch, String> {
        let words: Vec<OsString> = plan.words().into_iter().map(OsString::from).collect();
        let nesting = self.here_nesting.unwrap_or(self.nesting);
        let at = Where {
            root: self.root,
            gate: &self.gate,
            nesting,
        };
        let withheld: &[&str] = if nesting.covering().is_none() { &[SLOT_HELD_VARIABLE] } else { &[] };
        let planned = Plan::prepare_without(self.request, &words, &at, withheld, console)
            .map_err(|NotRun { line, .. }| line.strip_prefix("gate: ").map(str::to_owned).unwrap_or(line))?;
        planned.launch
    }

    /// Takes the lock of the call's work-folder key: at once when no other run of the worktree holds it, and else after
    /// waiting up to the run's key-wait bound for the holder to let it go, saying the wait in a line with the holder's
    /// time left when its remote claim carries a usual run time, and a still-waiting line each heartbeat. When a warm-up
    /// of the work folder holds the key, the run falls back at once, as it did before the wait. Else how the run goes on:
    /// here, `busy`, when the lock is held past the bound or cannot be opened or taken, which says why; or interrupted
    /// during the wait, which says so and journals the abandon.
    fn take_key(&mut self, remote: &RemoteRun, console: &mut Console) -> Result<Option<OwnedMutexGuard>, Remotely> {
        // The key lock reads neither figure of the call; no take has resolved them yet. A sibling that fails its check
        // again sends nothing: the run falls back here before it takes the lock or a slot.
        let env = routed_env(&self.request.env, remote.source_revision.as_deref());
        let parts = CallParts {
            request: self.request,
            figures: Figures::floor(self.request.timeout),
            log: &self.log,
            host: &self.host,
            root: self.root,
            warmth: warmth_of(self.warm_hash.as_deref()),
            env: &env,
            // The key lock sends nothing, so it names no session.
            session: "",
        };
        let call = match call_of(parts, remote) {
            Ok(call) => call,
            Err(why) => return Err(self.fell_back(FallbackReason::Client, &RemoteOutcome::not_started(why), console)),
        };
        // The key is the worktree's on every builder: a key held elsewhere is busy on all of them, so the run runs here.
        let busy = |run: &mut Run<'_>, console: &mut Console| run.fell_back(FallbackReason::Busy, &RemoteOutcome::default(), console);
        // The seam raises the console interrupt before the take, so the zero wait meets it as the lock's busy.
        if self.request.test_seams.interrupt_before_key
            && let Some(interrupt) = self.host.interrupt
        {
            interrupt.raise();
        }
        match self.client.hold_key(&call) {
            Ok(held) => return Ok(held),
            // Another run of the worktree holds the key, or the lock could not be taken without a line; the zero wait
            // never reads the interrupt, which the check after it sees.
            Err(None) => {}
            Err(Some(line)) => {
                console.err(&line);
                return Err(busy(self, console));
            }
        }
        // A Ctrl+C raised before the take reads as the lock's busy; it ends the run as an interrupted wait does.
        if self.host.interrupt.is_some_and(Interrupt::is_raised) {
            return Err(self.interrupted_key_wait(remote, Duration::ZERO, console));
        }
        // A light run a critical request sent here never waits for the key.
        if self.rerouted_for.is_some() {
            return Err(busy(self, console));
        }
        let mut probe_said = false;
        let mut marker_said = drain::ProbeSaid::default();
        let mut past_bound_said = false;
        if warm_up_holds(&self.root.path, &remote.key, console, &mut probe_said) {
            return Err(busy(self, console));
        }
        let bound = self.request.test_seams.key_wait_ms.map_or(KEY_WAIT, Duration::from_millis);
        let heartbeat = self.request.test_seams.heartbeat_ms.map_or(wait::HEARTBEAT, Duration::from_millis);
        let mut next = heartbeat;
        let left = holder_time_left(&mut self.survey, &self.root.path, call.key, console);
        console.err(&key_wait_line(&remote.builder.alias, left));
        // The seam raises the console interrupt once the gate has said it waits, the key wait's line among them.
        if self.request.test_seams.interrupt_queued
            && let Some(interrupt) = self.host.interrupt
        {
            interrupt.raise();
        }
        let begun = Instant::now();
        loop {
            match self.client.hold_key_waiting(&call, KEY_WAIT_SLICE) {
                Ok(KeyWait::Held(held)) => return Ok(held),
                Ok(KeyWait::Interrupted) => return Err(self.interrupted_key_wait(remote, begun.elapsed(), console)),
                Ok(KeyWait::Busy) => {}
                Err(line) => {
                    console.err(&line);
                    return Err(busy(self, console));
                }
            }
            // Each slice's wait is the slice itself, whose `Busy` comes before the client reads the interrupt.
            if self.host.interrupt.is_some_and(Interrupt::is_raised) {
                return Err(self.interrupted_key_wait(remote, begun.elapsed(), console));
            }
            let waited = begun.elapsed();
            // A run of the worktree waiting behind a builder's drain may hold the key, which is the worktree's on every
            // builder: a live marker of any of them holds the bound off.
            if waited >= bound {
                if !drain::any_marker_stood(&self.root.path, console, &mut marker_said) {
                    return Err(busy(self, console));
                }
                say_once(console, &mut past_bound_said, KEY_HELD_OFF_LINE);
            }
            if waited >= next {
                // A warm-up that took the key during the wait is not waited for either.
                if warm_up_holds(&self.root.path, &remote.key, console, &mut probe_said) {
                    return Err(busy(self, console));
                }
                console.err(&format!(
                    "gate: still waiting for this worktree's work folder on {} ({} s)",
                    remote.builder.alias,
                    waited.as_secs()
                ));
                while next <= waited {
                    next += heartbeat;
                }
            }
        }
    }

    /// Ends a run whose wait for its work folder's key was interrupted after `waited`: says so with the builder, journals
    /// the abandon with how long it waited and the run's remote tag, and ends the run as an interrupted wait does, with
    /// no ticket and no claim to delete.
    fn interrupted_key_wait(&mut self, remote: &RemoteRun, waited: Duration, console: &mut Console) -> Remotely {
        console.err(&format!(
            "gate: interrupted while waiting for this worktree's work folder on {}",
            remote.builder.alias
        ));
        self.journal.write(
            &JournalEvent::Abandon {
                waited_seconds: waited.as_secs_f64(),
            },
            self.tag.as_ref(),
            console,
        );
        Remotely::Interrupted
    }

    /// Gives up the remote attempt for `reason`, before it took a remote slot ([`Run::give_up`]), and says the run goes
    /// on here; a run pinned with `--builder` ends instead ([`Run::pin_ended`]).
    fn fell_back(&mut self, reason: FallbackReason, outcome: &RemoteOutcome, console: &mut Console) -> Remotely {
        if self.request.builder.is_some() {
            return self.pin_ended(reason, outcome, None, console);
        }
        let host = self.give_up(reason, outcome, None, console);
        console.err(&fallback_line(&host, reason, outcome.detail.as_deref(), None));
        Remotely::Here
    }

    /// Ends a run pinned with `--builder` whose builder did not run it, for `reason`: no other builder is tried, nothing
    /// runs here and no fallback is journaled. Says why with [`PIN_NOT_HERE`] after the fallback head, then `said`, the
    /// line of a warm-up record the refusal dropped, and ends the log; the run ends failed, its release journaled with
    /// the run's remote tag.
    fn pin_ended(&mut self, reason: FallbackReason, outcome: &RemoteOutcome, said: Option<&str>, console: &mut Console) -> Remotely {
        let host = self.tag.as_ref().map_or_else(String::new, |tag| tag.host.clone());
        console.err(&format!("{}; {PIN_NOT_HERE}", fallback_head(&host, reason, outcome.detail.as_deref())));
        if let Some(line) = said {
            console.err(line);
        }
        self.log_owed_end_load(None, console);
        if let Err(error) = self.log.finish(console) {
            console.err(&format!("gate: could not write to {}: {error}", self.log.path().display()));
        }
        Remotely::Ended(Ended {
            status: GATE_FAILED,
            facts: None,
        })
    }

    /// Gives up the remote attempt for `reason`: journals the fallback, the attempt's last line with the remote tag,
    /// while the remote slot `held` is still held, then deletes its claim, lets it go and wakes the waiters, removes the
    /// log so the client's own bytes do not stand beside the notes that say them, and gathers the attempt's lines into
    /// the log's notes unsaid. The host the tag named, which the fallback line names.
    fn give_up(&mut self, reason: FallbackReason, outcome: &RemoteOutcome, held: Option<Taken>, console: &mut Console) -> String {
        self.routing.fell_back.push(reason);
        let tag = self.tag.take();
        self.journal.write(&JournalEvent::Fallback { reason }, tag.as_ref(), console);
        if let Some(taken) = held {
            slots::delete_claim(&taken.held, console);
            drop(taken);
            slots::wake_waiters(self.root, &mut self.survey, console);
        }
        // A run here starts from a log holding none of what the client wrote: the notes below say its lines again.
        if let Err(error) = self.log.discard() {
            console.err(&format!(
                "gate: cannot remove the log {} of the attempt: {error}",
                self.log.path().display()
            ));
        }
        for line in &outcome.lines {
            console.note(line);
        }
        tag.map_or_else(String::new, |tag| tag.host)
    }

    /// The plan of the run here after a fallback of any reason, and the words it runs: the first plan, the command as
    /// given, unless it is a dotnet `--no-build` command that would find no build here ([`Run::no_build_words`]), which
    /// drops the word, says the line a run here that drops it says ([`Run::dropped_line`]), and plans the words it left.
    /// A command with a word that is not valid Unicode is never routed ([`route`]), so the branch that keeps it as given
    /// cannot be reached. A gate that routed beside a commit run is put back under it first ([`Run::back_under_commit`]).
    /// Else how the run ended when the dropped command cannot be planned.
    fn plan_here(&mut self, remote: &RemoteRun, plan: Plan, console: &mut Console) -> Result<(Plan, Option<HereWords>), Ended> {
        let Some(words) = unicode_words(&self.request.command) else {
            self.back_under_commit(None, console);
            return Ok((plan, None));
        };
        let (words, dropped) = self.no_build_words(words, |_| Some(remote.root.clone()), console);
        self.back_under_commit(dropped.as_ref().map(|_| words.as_slice()), console);
        let Some(reason) = dropped else {
            return Ok((plan, Some(HereWords::routed(remote, words))));
        };
        console.err(&self.dropped_line(&reason));
        let plan = self.replan(&words.iter().map(OsString::from).collect::<Vec<_>>(), console)?;
        Ok((plan, Some(HereWords::routed(remote, words))))
    }

    /// The plan of a run here from the start: the first plan, unless `local` dropped the command's `--no-build`
    /// ([`Run::local_words`]); then a plan of the words it left. With the words it runs; else how the run ended when
    /// they cannot be planned.
    fn plan_local(&self, plan: Plan, local: Option<HereWords>, console: &mut Console) -> Result<(Plan, Option<HereWords>), Ended> {
        match local {
            Some(here) if here.dropped.is_some() => {
                let plan = self.replan(&here.words.iter().map(OsString::from).collect::<Vec<_>>(), console)?;
                Ok((plan, Some(here)))
            }
            local => Ok((plan, local)),
        }
    }

    /// The words of a run here from the start ([`Run::local_words`]), once the run took the words it left when it
    /// dropped its `--no-build` ([`take_dropped`]) and read its durations, as it now knows the command it journals
    /// ([`Run::open_durations`]). An `--alone` run not pinned with `--builder` readies its log here, once its
    /// `--no-build` is known to stand ([`readies_log_first`]).
    ///
    /// # Errors
    ///
    /// The usage status of an `--alone` run whose `--no-build` would be dropped, refused with its line said
    /// ([`alone_refused`]), or the failed status of a log that cannot be readied ([`prepare_log`]).
    fn take_local(&mut self, route: &Route, placed: &Placed, console: &mut Console) -> Result<Option<HereWords>, u32> {
        let local = self.local_words(route, placed.place(), console);
        if self.request.alone {
            if alone_refused(local.as_ref(), console) {
                return Err(u32::from(USAGE_STATUS));
            }
            if self.request.builder.is_none() {
                prepare_log(&self.log, console)?;
            }
        }
        take_dropped(self, local.as_ref(), console);
        self.durations = self.open_durations(placed);
        Ok(local)
    }

    /// The words of a run here from the start, its worktree when it is known, and why its `--no-build` was dropped
    /// ([`Run::no_build_words`]). Its worktree comes from the route's `place`, else from one git call, asked only when
    /// the words have a `--no-build` word to drop; a command outside a worktree keeps its words. `None` for a routed
    /// run, and for a command with a word that is not valid Unicode, which runs as it is.
    fn local_words(&self, route: &Route, place: Option<&Place>, console: &mut Console) -> Option<HereWords> {
        if matches!(route, Route::Routed(_)) {
            return None;
        }
        let words = unicode_words(&self.request.command)?;
        let mut found = place.map(|place| Worktree {
            root: place.root.clone(),
            prefix: place.prefix.clone(),
        });
        let (words, dropped) = self.no_build_words(
            words,
            |console| {
                if found.is_none() {
                    found = self.worktree(false, console);
                }
                found.as_ref().map(|worktree| worktree.root.clone())
            },
            console,
        );
        Some(HereWords {
            words,
            worktree: found,
            dropped,
        })
    }

    /// The words a dotnet `--no-build` command of `words` runs in the worktree `root` finds, and why the word was
    /// dropped, `None` when it is kept: each exact `--no-build` word of a dotnet command that builds dropped
    /// ([`without_no_build`]) when the worktree has a nothing-back record or a project without
    /// `obj\project.assets.json` ([`Run::no_build_reason`]), else the words as given. `root` is asked once the words have
    /// one to drop, so a command that drops nothing pays no git call ([`Run::worktree`]).
    fn no_build_words(
        &self,
        words: Vec<String>,
        root: impl FnOnce(&mut Console) -> Option<String>,
        console: &mut Console,
    ) -> (Vec<String>, Option<String>) {
        let dropped = without_no_build(words.clone());
        if dropped.len() == words.len() {
            return (words, None);
        }
        let Some(root) = root(console) else {
            return (words, None);
        };
        match self.no_build_reason(&root, &words, console) {
            Some(reason) => (dropped, Some(reason)),
            None => (words, None),
        }
    }

    /// The request and nesting of this run once its `--no-build` is dropped: the run here is a build, so it asks for a
    /// heavy slot whatever slot the caller named, runs `words`, and is under its parent as a heavy gate is, a parent
    /// that stands in for an ended one looked for again for a heavy gate ([`Nesting::as_heavy`]).
    fn as_heavy(&self, words: &[String], console: &mut Console) -> (RunRequest, Nesting) {
        let request = RunRequest {
            slot: Slot::Heavy,
            command: words.iter().map(OsString::from).collect(),
            ..self.request.clone()
        };
        let trusted = self.request.test_seams.parent_job_trusted;
        let nesting = self.nesting.as_heavy(|| enclosing_of(self.root, Slot::Heavy, &[], trusted, console));
        (request, nesting)
    }

    /// The line a run says as it drops its `--no-build` for `reason`, naming where it builds: on the slot of the parent
    /// gate that covers or lent it, on no slot inside a box run, else on the critical slot of a critical run or a heavy
    /// slot of its own ([`dropped_line`]).
    fn dropped_line(&self, reason: &str) -> String {
        let lent_by = self.lent.then(|| self.nesting.uncovering()).flatten();
        let parent = self.nesting.covering().or(lent_by);
        dropped_line(reason, parent, matches!(self.nesting, Nesting::InBox), self.request.slot)
    }

    /// Why a dotnet `--no-build` run of `words` in the worktree at `root` drops the word, `None` when it keeps it: kept
    /// when the worktree's built-here record says the last build of what the words build ran here under their settings
    /// ([`no_build::last_build_ran_here`], the rule a routed run is kept here by); else dropped when the worktree has a
    /// nothing-back record that does not list the project the words name as built here
    /// ([`no_build::named_project_key`]) under the configuration, framework and runtime the words set
    /// ([`BuildSettings::same_as`]), or a project it builds has no `obj\project.assets.json`
    /// ([`Run::without_assets`]), asked only without such a record ([`no_build::drop_reason`]). A record that names a
    /// configuration other than the one the words set (compared as the send veto compares them, ignoring letter case) is
    /// set aside as no record: it says nothing of this build, so the obj check decides. A record naming no configuration
    /// stands for every run, and words setting no `-c` keep the old rule, dropped by any record that stands. A record
    /// that cannot be read counts as there, said.
    fn no_build_reason(&self, root: &str, words: &[String], console: &mut Console) -> Option<String> {
        if no_build::last_build_ran_here(&self.root.path, root, words).is_some() {
            return None;
        }
        let path = v1::nothing_back_path(&self.root.path, &work_folder_key(root));
        let record = no_build::read_record(&path).unwrap_or_else(|error| {
            console.err(&format!("gate: cannot read {}: {error}", path.display()));
            Some(NothingBack {
                host: None,
                settings: BuildSettings::default(),
                built: Vec::new(),
            })
        });
        let settings = build_settings(words);
        let record = record.filter(|record| {
            let built = no_build::named_project_key(words, Path::new(root)).is_some_and(|key| record.built.contains(&key));
            let same_settings = built && record.settings.same_as(&settings);
            let other_configuration = matches!(
                (&record.settings.configuration, &settings.configuration),
                (Some(left), Some(right)) if left.to_lowercase() != right.to_lowercase()
            );
            !(same_settings || other_configuration)
        });
        no_build::drop_reason(record, || self.without_assets(Path::new(root), words, console))
    }

    /// The first project of the worktree at `root` the dotnet command `words` builds with no `obj\project.assets.json`:
    /// of the project or solution it names ([`no_build::named_without_assets`]), else of the worktree's tracked projects
    /// ([`no_build::first_without_assets`]). `None` when each has it, when the worktree's build files may move it
    /// ([`no_build::moves_intermediates`]), and when the projects cannot be listed or the build files read, which is
    /// said.
    fn without_assets(&self, root: &Path, words: &[String], console: &mut Console) -> Option<String> {
        let cannot = |console: &mut Console, line: String| {
            console.err(&line);
            None
        };
        match no_build::moves_intermediates(root) {
            Ok(true) => return None,
            Ok(false) => {}
            Err(error) => return cannot(console, format!("gate: {error}")),
        }
        if let Some(target) = dotnet_target(words) {
            return no_build::named_without_assets(&target, root).unwrap_or_else(|error| cannot(console, format!("gate: {error}")));
        }
        match no_build::project_listing(root, git::limit(&self.request.test_seams)) {
            Ok(listing) => no_build::first_without_assets(root, &listing),
            Err(error) => cannot(console, format!("gate: cannot list the worktree's projects: {error}")),
        }
    }

    /// The git worktree the gate runs in ([`no_build::worktree`]); `None` outside a repository, and when git cannot be
    /// asked or fails otherwise, which is said, unless `quiet_without_git` and git cannot be found or started.
    fn worktree(&self, quiet_without_git: bool, console: &mut Console) -> Option<Worktree> {
        match no_build::worktree(git::limit(&self.request.test_seams)) {
            Ok(worktree) => worktree,
            Err(Unfound::NoGit(_)) if quiet_without_git => None,
            Err(error) => {
                console.err(&format!("gate: cannot find the command's git worktree: {}", error.text()));
                None
            }
        }
    }

    /// Updates the worktree's built-here and nothing-back records after a run here that passed, once its slot is
    /// released: a dotnet build in place writes the built-here entries of the projects it built
    /// ([`Run::built_here_entry`], [`Run::record_built_here`]); a dotnet build of the whole worktree clears the
    /// nothing-back record ([`Run::clear_nothing_back`]), and a build of one project adds that project to the record's
    /// `built` list ([`Run::record_change`], [`no_build::add_built`]) under the key lock the built-here write took,
    /// never waiting for it a second time. A key that cannot be added is said.
    fn settle_nothing_back(&self, mut here: HereWords, console: &mut Console) {
        let settings = build_settings(&here.words);
        let mut taken = None;
        if let Some((folder, projects)) = self.built_here_entry(&mut here, console) {
            taken = Some((self.record_built_here(&folder, &projects, &settings, console), folder));
        }
        match self.record_change(here, console) {
            Some(RecordChange::Clear(path)) => remove_record(&path, console),
            Some(RecordChange::Add { path, folder, project }) => {
                // The key lock a routed run writes the record under, so a fresh record it writes meanwhile is kept.
                let held = match taken {
                    Some((held, taken_folder)) if taken_folder == folder => held,
                    _ => self.hold_folder_key(&folder, console),
                };
                let added = held
                    .map_err(|reason| format!("gate: cannot add to {}: {reason}", path.display()))
                    .and_then(|_held| no_build::add_built(&path, &project, &settings));
                if let Err(line) = added {
                    console.err(&line);
                }
            }
            None => {}
        }
    }

    /// The work-folder key and the built-here entries ([`v1::built_here_path`]) a passing run here of `here`'s words
    /// writes: for an SDK dotnet build other than `msbuild` ([`sdk_build_target`]) that builds in place
    /// ([`builds_in_place`]), the keys of the projects inside the worktree it builds ([`no_build::built_projects`]).
    /// `here`'s worktree is looked up when it has none and the words may have an entry, git that cannot be found or
    /// started counting as none, unsaid, and kept for the nothing-back record's update. `None` for any other words.
    fn built_here_entry(&self, here: &mut HereWords, console: &mut Console) -> Option<(String, Vec<String>)> {
        let target = sdk_build_target(&here.words).filter(|_| builds_in_place(&here.words))?;
        if target == BuildTarget::Other && !no_build::names_project(&here.words) {
            return None;
        }
        if here.worktree.is_none() {
            here.worktree = self.worktree(true, console);
        }
        let worktree = here.worktree.as_ref()?;
        let (_, projects) = no_build::built_projects(&here.words, Path::new(&worktree.root))?;
        Some((work_folder_key(&worktree.root), projects))
    }

    /// Writes the entries of `projects` under `settings` into the built-here record of the work-folder key `folder`
    /// ([`no_build::record_built_here`]) and removes its outputs-from record ([`v1::outputs_from_path`]), since the PC's
    /// outputs no longer are what a builder last sent, both under the key lock a routed run of the worktree holds. The
    /// lock is taken with a brief retry ([`KeyTake::Brief`]), as a local write holds it for milliseconds: a lock still
    /// held after it writes no entry, saying so in the log, and is then taken once more with a wait bounded hard at the
    /// key-wait bound ([`KeyTake::Bounded`]), said in the log first, for the outputs-from record alone, which a routed
    /// run's pull writes under that lock. A lock not taken either way, for any reason, still removes the outputs-from
    /// record, without it, saying so in the log. A record trimmed to fit its cap is said in the log; a record that
    /// cannot be written or removed, or a lock that cannot be opened or taken, is said.
    ///
    /// # Errors
    ///
    /// Why the lock is not held, so the caller does not wait for it again.
    fn record_built_here(
        &self,
        folder: &str,
        projects: &[String],
        settings: &BuildSettings,
        console: &mut Console,
    ) -> Result<OwnedMutexGuard, String> {
        let path = v1::built_here_path(&self.root.path, folder);
        let outputs_from = v1::outputs_from_path(&self.root.path, folder);
        let taken = match self.try_folder_key(folder, KeyTake::Brief, console) {
            Ok(held) => {
                match no_build::record_built_here(&path, projects, settings) {
                    Ok(Some(left)) => console.note(&format!("gate: built-here record trimmed to {left} entries")),
                    Ok(None) => {}
                    Err(line) => console.err(&line),
                }
                Ok(held)
            }
            Err(Untaken::Held(_)) => {
                console.note(&format!("gate: built-here record not written: work folder {folder} is held"));
                console.note(&format!("gate: waiting for the work folder's key to remove {}", outputs_from.display()));
                self.try_folder_key(folder, KeyTake::Bounded, console).map_err(Untaken::why)
            }
            Err(untaken) => {
                let why = untaken.why();
                console.err(&format!("gate: cannot write {}: {why}", path.display()));
                Err(why)
            }
        };
        remove_record(&outputs_from, console);
        if let Err(why) = &taken {
            console.note(&format!("gate: removed {} without the work folder's key: {why}", outputs_from.display()));
        }
        taken
    }

    /// Removes the built-here record of the routed run `remote`'s work-folder key before its attempt, under the key lock
    /// the caller holds, when its words as sent build: its command as given is a dotnet build of any target
    /// ([`dotnet_builds`]), or the route dropped its `--no-build` with no send planned, since the builder may build the
    /// projects it references too and bring their outputs back ([`no_build::clear_built_here`]). A send that does not
    /// land and runs the dropped words removes it in the client ([`remote::client`]).
    ///
    /// # Errors
    ///
    /// The line saying the record cannot be removed, so the attempt is not sent.
    fn clear_built_here(&self, remote: &RemoteRun) -> Result<(), String> {
        let dropped = remote.dropped_no_build && remote.send.is_none();
        if !dropped && !unicode_words(&self.request.command).is_some_and(|words| dotnet_builds(&words)) {
            return Ok(());
        }
        no_build::clear_built_here(&v1::built_here_path(&self.root.path, &remote.key))
    }

    /// Holds the lock of the work-folder key `folder`, the one a routed run of the worktree holds while it writes the
    /// worktree's nothing-back record ([`Run::take_key`]), waiting for another holder up to the run's key-wait bound,
    /// which a live drain marker holds off ([`KeyTake::HeldOff`]).
    ///
    /// # Errors
    ///
    /// Why the lock is not held: it cannot be opened or taken, another run held it for the whole bound, or a Ctrl+C
    /// ended the wait.
    fn hold_folder_key(&self, folder: &str, console: &mut Console) -> Result<OwnedMutexGuard, String> {
        self.try_folder_key(folder, KeyTake::HeldOff, console).map_err(|untaken| match untaken {
            Untaken::Held(bound) => format!("another run of the worktree held the work folder's key for {} ms", bound.as_millis()),
            Untaken::Interrupted => "Ctrl+C ended the wait for the work folder's key".to_owned(),
            Untaken::Failed(why) => why,
        })
    }

    /// Appends one line to the key-waits test seam's file, when it is set, for a take of the work folder's key that
    /// waits ([`KeyTake::Bounded`], [`KeyTake::HeldOff`]); a file that cannot be written is said.
    fn count_key_wait(&self, console: &mut Console) {
        let Some(path) = self.request.test_seams.key_waits.as_deref() else {
            return;
        };
        let appended = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| io::Write::write_all(&mut file, b"wait\n"));
        if let Err(error) = appended {
            console.err(&format!("gate: cannot write {}: {error}", path.display()));
        }
    }

    /// Takes the lock of the work-folder key `folder` in slices, between which a Ctrl+C ends the take, for up to the
    /// bound `wait` sets ([`KeyTake`]); past a [`KeyTake::HeldOff`] bound, a live drain marker of any builder holds the
    /// bound off, said once, an entry the marker probe skips noted in the log once through `console`. The key-wait test
    /// seam sets every bound.
    ///
    /// # Errors
    ///
    /// Why the lock is not held ([`Untaken`]).
    fn try_folder_key(&self, folder: &str, wait: KeyTake, console: &mut Console) -> Result<OwnedMutexGuard, Untaken> {
        let name = self.root.root.object_name(KernelObject::Routed { hash: key_hash(folder) });
        let unseamed = if wait == KeyTake::Brief { BRIEF_KEY_WAIT } else { KEY_WAIT };
        let bound = self.request.test_seams.key_wait_ms.map_or(unseamed, Duration::from_millis);
        let begun = Instant::now();
        let mut said = drain::ProbeSaid::default();
        let mut past_bound_said = false;
        if wait != KeyTake::Brief {
            self.count_key_wait(console);
        }
        let mut reached = self
            .request
            .test_seams
            .built_here_key_reached
            .as_deref()
            .filter(|_| wait == KeyTake::Brief);
        loop {
            // A wait that times out gives its handle up, so each slice waits on one of its own.
            let mutex = NamedMutex::open(&name).map_err(|error| Untaken::Failed(format!("cannot open the mutex {name}: {error}")))?;
            match mutex.acquire_owned(KEY_WAIT_SLICE) {
                Ok(held) => return Ok(held),
                Err(AcquireError::Timeout) => {
                    if let Some(path) = reached.take()
                        && let Err(error) = fs::write(path, "")
                    {
                        console.err(&format!("gate: cannot write {}: {error}", path.display()));
                    }
                }
                Err(AcquireError::Failed(error)) => return Err(Untaken::Failed(format!("cannot take the mutex {name}: {error}"))),
            }
            if self.host.interrupt.is_some_and(Interrupt::is_raised) {
                return Err(Untaken::Interrupted);
            }
            if begun.elapsed() < bound {
                continue;
            }
            if wait != KeyTake::HeldOff {
                return Err(Untaken::Held(bound));
            }
            // The key is the worktree's on every builder: a live drain marker of any of them holds the bound off.
            let probed = drain::any_marker_stands(&self.root.path, &mut |path, error| said.skipped(console, path, error));
            match probed {
                Ok(true) => say_once(console, &mut past_bound_said, KEY_HELD_OFF_LINE),
                Ok(false) => return Err(Untaken::Held(bound)),
                Err(error) => {
                    return Err(Untaken::Failed(format!(
                        "another run of the worktree held the work folder's key for {} ms, and the drain markers cannot be probed: {error}",
                        bound.as_millis()
                    )));
                }
            }
        }
    }

    /// Removes the worktree's nothing-back record after a routed run that brought its outputs back
    /// ([`Run::leave_nothing_back`]), when the words it ran are a dotnet build of the whole worktree that rebuilt what the
    /// record stands for ([`Run::record_change`]); a build of one project there adds nothing. A record that cannot be
    /// removed is said.
    fn clear_nothing_back(&self, here: HereWords, console: &mut Console) {
        if let Some(RecordChange::Clear(path)) = self.record_change(here, console) {
            remove_record(&path, console);
        }
    }

    /// What a passing run of `here`'s words does to its worktree's nothing-back record (`here`'s worktree, else the one
    /// git gives, git that cannot be found or started counting as none, unsaid), for words that are an SDK dotnet build
    /// other than `msbuild` ([`sdk_build_target`]). One of a solution, or of nothing run at the worktree's root, clears
    /// it when it set the configuration, framework and runtime the record's build set ([`BuildSettings::same_as`]);
    /// one of a single project inside the worktree, built in place ([`builds_in_place`]), adds the project's key
    /// ([`no_build::named_project_key`]), which [`no_build::add_built`] does only under the same settings rule. `None`
    /// for any other words, and when there is no record to clear; a record that cannot be read is said and left. Words
    /// that name something other than a project built in place ([`no_build::names_project`]) look for no worktree, so
    /// they start no git process and say nothing.
    fn record_change(&self, here: HereWords, console: &mut Console) -> Option<RecordChange> {
        let target = sdk_build_target(&here.words)?;
        if target == BuildTarget::Other && !(builds_in_place(&here.words) && no_build::names_project(&here.words)) {
            return None;
        }
        let worktree = here.worktree.or_else(|| self.worktree(true, console))?;
        let folder = work_folder_key(&worktree.root);
        let path = v1::nothing_back_path(&self.root.path, &folder);
        let whole = match target {
            BuildTarget::Solution => true,
            BuildTarget::Unnamed => worktree.prefix.is_empty(),
            BuildTarget::Other => false,
        };
        if !whole {
            let project = builds_in_place(&here.words)
                .then(|| no_build::named_project_key(&here.words, Path::new(&worktree.root)))
                .flatten()?;
            return Some(RecordChange::Add { path, folder, project });
        }
        let record = match no_build::read_record(&path) {
            Ok(record) => record?,
            Err(error) => {
                console.err(&format!("gate: cannot read {}: {error}", path.display()));
                return None;
            }
        };
        record.settings.same_as(&build_settings(&here.words)).then_some(RecordChange::Clear(path))
    }

    /// A plan of `words`, which a run here runs in place of the command as given; else how the run ended when they
    /// cannot be planned.
    fn replan(&self, words: &[OsString], console: &mut Console) -> Result<Plan, Ended> {
        let at = Where {
            root: self.root,
            gate: &self.gate,
            nesting: self.nesting,
        };
        Plan::prepare(self.request, words, &at, console).map_err(|NotRun { status, line }| {
            console.err(&line);
            Ended {
                status: i64::from(status),
                facts: None,
            }
        })
    }

    /// Ends a run the client ran to its end: the verdict with the passed line on the run's host and, when the verdict
    /// passes, the lines naming the folders the run left stale ([`stale_lines`]); its markers not scanned when the
    /// client killed the run or the console was interrupted, and the facts its release carries, with the adjusted
    /// seconds the box said. It is a kill only when the box's kill tag named one, whose kind is the verdict, or when the
    /// client killed it, by the kill line in the log.
    fn remote_ended(&mut self, outcome: &RemoteOutcome, passing: Passing<'_>, wall: Duration, console: &mut Console) -> Ended {
        self.trace.phases = phases(&outcome.split);
        let interrupted = self.host.interrupt.is_some_and(Interrupt::is_raised);
        // A client that wrote nothing left no log; the verdict reads one.
        if let Err(error) = OpenOptions::new().append(true).create(true).open(self.log.path()) {
            let problem = format!("cannot create the log {}: {error}", self.log.path().display());
            return self.gate_failed(&problem, None, console);
        }
        match self.verdict(outcome.status, Some(passing), outcome.killed || interrupted, &[], console) {
            Ok(status) => {
                let verdict = if let Some(kill) = outcome.box_killed {
                    crate::remote::box_kill_verdict(kill)
                } else if outcome.transfer_failed {
                    Verdict::TransferFailed
                } else {
                    let kill = if outcome.killed { self.kill_line(console) } else { None };
                    Verdict::of(kill.as_deref(), outcome.status, status)
                };
                Ended {
                    status,
                    facts: Some(RunFacts {
                        wall_seconds: wall.as_secs_f64(),
                        adjusted_seconds: outcome.adjusted,
                        avg_cores: None,
                        peak_cores: None,
                        free_cores: None,
                        verdict,
                    }),
                }
            }
            Err(problem) => self.gate_failed(&problem, None, console),
        }
    }

    /// The log's last kill line, as the client's watchdog wrote it; `None` when it holds none, or, said, when it cannot
    /// be read.
    fn kill_line(&self, console: &mut Console) -> Option<String> {
        let is_kill = |line: &str| Verdict::of(Some(line), 0, 0) != Verdict::Passed;
        self.log
            .last_line(is_kill)
            .inspect_err(|error| console.err(&format!("gate: cannot read the log {}: {error}", self.log.path().display())))
            .ok()
            .flatten()
    }

    /// What a take of the run's slot is journalled and claimed with, naming `parent_pid` ([`Taker`]), with the survey
    /// the run reads the other gates with, which the caller borrows beside it. The claim carries `expected` and the
    /// run's `ceiling`.
    fn taker<'t>(&'t mut self, parent_pid: Option<u32>, expected: Option<&'t Map<String, Value>>, ceiling: u64) -> (Taker<'t>, &'t mut Survey) {
        let taker = Taker {
            root: self.root,
            gate: &self.gate,
            request: self.request,
            ceiling,
            log: &self.log,
            tag: self.tag.as_ref(),
            journal: &mut self.journal,
            parent_pid,
            expected,
        };
        (taker, &mut self.survey)
    }

    /// The [`Taker`] of a run whose watch may take a slot once a covering parent ends, naming `parent_pid`, with the
    /// parts the caller borrows beside it: where the run keeps that slot ([`Run::late`]), and its covered marker
    /// ([`Run::covered`]). A run a parent's slot covers has no durations, so its late take's claim expects nothing; the
    /// claim carries the run's `ceiling`.
    fn late_taker(&mut self, parent_pid: Option<u32>, ceiling: u64) -> (Taker<'_>, &mut Option<LateSlot>, &mut Option<Covered>) {
        let taker = Taker {
            root: self.root,
            gate: &self.gate,
            request: self.request,
            ceiling,
            log: &self.log,
            tag: self.tag.as_ref(),
            journal: &mut self.journal,
            parent_pid,
            expected: None,
        };
        (taker, &mut self.late, &mut self.covered)
    }

    /// Waits its turn for a slot of `pool` and takes it into `held` the moment it has it, then writes the claim and
    /// journals the take, with the remote tag while the run has one. `since` is the moment the wait counts from, the run's
    /// own `queue` line for a run the full remote pool sent back and for a light run the critical barrier held back, in
    /// its later waits here and its remote take, and `None` for every other wait, which counts from now;
    /// the wait, its still-waiting lines and its journal all count from that one instant. `left` limits a remote wait
    /// to what is left of the run's remote budget, `None` waiting for the whole budget ([`Wait::limit_to`]).
    /// When the slot was taken; else a Ctrl+C ended the wait, which deletes the ticket at once, wakes the waiters and
    /// journals the abandon, or a remote pool stayed full past the wait's limit, which deletes the ticket, or a light
    /// parent lent the run its slot, which deletes the ticket and wakes the waiters.
    fn take(&mut self, pool: Pool, since: Option<Instant>, left: Option<Duration>, held: &mut Option<Taken>, console: &mut Console) -> Taking {
        let queued = since.unwrap_or_else(Instant::now);
        let estimate = self.estimate(console);
        let expected = estimate.as_ref().map(expected_field);
        let arrival = Arrival {
            gate: &self.gate,
            request: self.request,
            queued,
            arrived: self.arrived,
            builder: self.tag.as_ref().map(|tag| tag.host.as_str()),
            after_fallback: since.is_some(),
            under: match self.nesting {
                Nesting::Under(parent) => Some(UnderParent {
                    pid: parent.pid,
                    start_ms: parent.start_ms,
                    lends: true,
                }),
                // A parent that holds no slot has none to lend: it names only the pause marker the gate keeps.
                Nesting::Beside(parent) => Some(UnderParent {
                    pid: parent.pid,
                    start_ms: parent.start_ms,
                    lends: false,
                }),
                _ => None,
            },
            expected: expected.as_ref(),
        };
        let mut wait = Wait::queue(self.root, pool, arrival, self.host, console);
        if let Some(left) = left {
            wait.limit_to(left);
        }
        if matches!(pool, Pool::Local(gate_state::SlotKind::Light)) && self.would.is_some() && !self.reroute_tried && self.request.builder.is_none() {
            wait.may_route();
        }
        if pool.is_remote() && self.rerouted_for.is_some() {
            wait.keep_spare();
        }
        let taken = loop {
            match wait.until_taken(&mut self.survey, console) {
                Waited::Taken(taken) => break held.insert(taken),
                Waited::Lent(beside) => {
                    let Some(figures) = self.lent_as_it_waits(beside, estimate.as_ref(), expected.as_ref(), console) else {
                        wait.refuse_lend();
                        continue;
                    };
                    wait.end(console);
                    slots::wake_waiters(self.root, &mut self.survey, console);
                    return Taking::Lent(figures);
                }
                Waited::Interrupted => {
                    self.abandon(wait, pool, queued, console);
                    return Taking::Interrupted;
                }
                Waited::TimedOut => {
                    wait.end(console);
                    slots::wake_waiters(self.root, &mut self.survey, console);
                    return Taking::Full;
                }
                Waited::HeldBack(by) => {
                    wait.end(console);
                    slots::wake_waiters(self.root, &mut self.survey, console);
                    return Taking::HeldBack { by, queued };
                }
            }
        };
        let waited = queued.elapsed().as_secs_f64();
        wait.end(console);
        let took = Instant::now();
        let parent_pid = self.nesting.uncovering().map(|parent| parent.pid);
        self.routing.parent_pid = parent_pid;
        let (figures, resolved) = Figures::resolved(self.request.timeout, estimate.as_ref());
        let (mut taker, survey) = self.taker(parent_pid, expected.as_ref(), figures.ceiling);
        taker.took(taken, pool, waited, survey, console);
        say_expected(estimate.as_ref(), resolved, console);
        Taking::Took(took, figures)
    }

    /// The figures of a run its light parent lent its slot as it waited, the lend decided holding the beside mutex
    /// `beside`: the command runs on the parent's slot under the ceiling the take's `estimate` resolves, which it says
    /// as a take of its own slot does, with no expected-time line. Its lend marker is written, with what the durations
    /// store `expected`, before the mutex is let go. `None` when the marker cannot be written: the run waits on.
    fn lent_as_it_waits(
        &mut self,
        beside: OwnedMutexGuard,
        estimate: Option<&Estimate>,
        expected: Option<&Map<String, Value>>,
        console: &mut Console,
    ) -> Option<Figures> {
        let Nesting::Under(parent) = self.nesting else {
            unreachable!("only a gate under a light parent is lent its parent's slot")
        };
        let (figures, resolved) = Figures::resolved(self.request.timeout, estimate);
        self.lend = Some(self.lend_marker(parent.pid, figures.ceiling, expected, console)?);
        drop(beside);
        if let Some(estimate) = estimate {
            say_ceiling(estimate, resolved, console);
        }
        Some(figures)
    }

    /// Waits for the machine as a critical gate and takes it into `held` once it holds every heavy slot and its light
    /// slot, then says the holds line, writes the critical claim, journals the take with what it found and wakes the
    /// waiters, so a reader that sees the take finds its claim, unless the claim could not be written. When the machine
    /// was taken; else a Ctrl+C ended the wait, which says so, journals the abandon, then releases what it took so far,
    /// deletes the ticket and wakes the waiters.
    fn take_machine(&mut self, held: &mut Option<Machine>, console: &mut Console) -> Taking {
        let queued = Instant::now();
        let estimate = self.estimate(console);
        let expected = estimate.as_ref().map(expected_field);
        let arrival = Arrival {
            gate: &self.gate,
            request: self.request,
            queued,
            arrived: self.arrived,
            builder: None,
            after_fallback: false,
            under: None,
            expected: expected.as_ref(),
        };
        let wait = MachineWait::queue(self.root, arrival, self.host, console);
        let (machine, snapshot) = match wait.until_held(&mut self.survey, console) {
            MachineWaited::Held(machine, snapshot) => (held.insert(machine), snapshot),
            MachineWaited::Interrupted(machine) => {
                console.err("gate: interrupted while waiting for the machine");
                let waited_seconds = queued.elapsed().as_secs_f64();
                self.journal.write(&JournalEvent::Abandon { waited_seconds }, None, console);
                machine.release(self.root, &mut self.survey, console);
                return Taking::Interrupted;
            }
        };
        let waited = queued.elapsed().as_secs_f64();
        let line = machine::holds_line(Timestamp::now(), waited, &snapshot, machine);
        let parent_pid = self.nesting.uncovering().map(|parent| parent.pid);
        self.routing.parent_pid = parent_pid;
        let take = JournalEvent::Take {
            slots: machine.names(),
            waited_seconds: waited,
            abandoned: machine.abandoned().to_vec(),
            machine: Some(snapshot),
            // A critical gate under a parent whose slot does not cover it is refused, so only a parent that holds no
            // slot is named here.
            parent_pid,
        };
        console.err(&line);
        let took = Instant::now();
        let (figures, resolved) = Figures::resolved(self.request.timeout, estimate.as_ref());
        let of = ClaimOf {
            gate: &self.gate,
            ceiling: figures.ceiling,
            log: self.log.path(),
            remote: None,
            reason: self.request.reason.as_deref(),
            expected: expected.as_ref(),
        };
        machine.write_claim(&of, console);
        self.journal.write(&take, None, console);
        slots::wake_waiters(self.root, &mut self.survey, console);
        say_expected(estimate.as_ref(), resolved, console);
        self.log_load(When::Start, None, console);
        Taking::Took(took, figures)
    }

    /// Writes the machine's load over a window at `when` to the log of a run that holds the machine alone, `job` being
    /// its command's once it has one; any other run writes nothing.
    fn log_load(&mut self, when: When, job: Option<&Job>, console: &mut Console) {
        if !self.request.alone {
            return;
        }
        let measuring = Measuring {
            pid: self.gate.pid,
            start_ms: self.gate.start_ms,
            root: &self.root.path,
            job,
            window: self.request.test_seams.load_window_ms.map_or(load::WINDOW, Duration::from_millis),
            interrupt: self.host.interrupt,
        };
        let block = load::log_load(when, &measuring, self.host.processors, console);
        self.load_blocks.push(block);
    }

    /// Writes the end window of a run that wrote its start window and not yet its end window, `job` being its
    /// command's when one started, so the start block always has its end block.
    fn log_owed_end_load(&mut self, job: Option<&Job>, console: &mut Console) {
        if self.load_blocks.len() == 1 {
            self.log_load(When::End, job, console);
        }
    }

    /// Leaves the queue of `pool` after a Ctrl+C: deletes the ticket, wakes the waiters, says so and journals the
    /// abandon, with the remote tag while the run has one.
    fn abandon(&mut self, wait: Wait<'_>, pool: Pool, queued: Instant, console: &mut Console) {
        wait.end(console);
        slots::wake_waiters(self.root, &mut self.survey, console);
        console.err(&format!("gate: interrupted while waiting for a {} slot", pool.name()));
        let waited_seconds = queued.elapsed().as_secs_f64();
        self.journal.write(&JournalEvent::Abandon { waited_seconds }, self.tag.as_ref(), console);
    }

    /// After a panic from the moment the slot was taken: completes the log as far as it can, the `.err` file and the
    /// notes so far, then releases the slot with the run's end unknown. The job of a started command was terminated
    /// as the panic left its watch.
    fn after_panic(&mut self, hold: Hold, console: &mut Console) {
        // The unwind dropped the command's job, which ended its events, so the consumer ends without a wait on them.
        self.end_streams(None, console);
        if let Err(error) = self.log.finish(console) {
            console.err(&format!(
                "gate: could not write to {} after the failure: {error}",
                self.log.path().display()
            ));
        }
        self.release(hold, None, console);
        self.finish_log(console);
    }

    /// Closes the log's writer ([`Run::close_log`]), then appends the notes held since, as the run's last lines.
    fn finish_log(&mut self, console: &mut Console) {
        self.close_log(console);
        if let Err(error) = self.log.finish(console) {
            console.err(&format!("gate: cannot write to {}: {error}", self.log.path().display()));
        }
    }

    /// Opens the log's writer, writes the gate's own process line to it first, then attaches the console, which
    /// writes the notes it held and every later one; once a run. A log that cannot be opened is said, and the
    /// command then does not start ([`Run::command`]).
    fn open_log(&mut self, console: &mut Console) {
        if self.writer.is_some() {
            return;
        }
        match LogWriter::open(self.log.path()) {
            Ok(writer) => {
                let root = stamp::root_text(self.gate.pid, gate_parent_pid());
                writer.gate_now(&[&root]);
                console.attach(&writer);
                self.writer = Some(writer);
            }
            Err(error) => console.err(&format!("gate: cannot open the log {}: {error}", self.log.path().display())),
        }
    }

    /// Detaches the console and closes the log's writer, so no handle to the log outlives the run's hold on it; a
    /// write to it that failed is said, held for [`Log::finish`].
    fn close_log(&mut self, console: &mut Console) {
        console.detach();
        if let Some(writer) = self.writer.take()
            && let Some(error) = writer.close()
        {
            console.err(&format!("gate: cannot write to {}: {error}", self.log.path().display()));
        }
    }

    /// Wires the command `suspended` to the log before it runs: the job's events to a consumer that writes them as
    /// process lines, and the command's two pipes to readers that write each line stamped and tagged as its stream's;
    /// what they read is counted in the bytes returned. What was started is kept in [`Run::streams`] even when a later
    /// part fails, so [`Run::end_streams`] ends it.
    fn start_streams(&mut self, job: &mut Job, suspended: &mut Suspended) -> Result<OutputBytes, String> {
        let writer = self.writer.clone().ok_or_else(|| "the log is not open".to_owned())?;
        let (stdout, stderr) = suspended
            .take_output()
            .ok_or_else(|| "the command's output pipes were not made".to_owned())?;
        let (events, received) = mpsc::channel();
        job.remember(Some(events))
            .map_err(|error| format!("cannot remember the processes of the command's job: {error}"))?;
        let consumer =
            spawn_consumer(Arc::clone(&writer), received).map_err(|error| format!("cannot start writing the job's process lines: {error}"))?;
        let streams = self.streams.insert(Streams { forwarders: None, consumer });
        let bytes = OutputBytes::default();
        let sink = Sink::Log {
            writer: &writer,
            bytes: &bytes,
        };
        let forwarders = Forwarders::start(stdout, stderr, Stamping::Tags(Tag::Out, Tag::Err), sink)
            .map_err(|error| format!("cannot start reading the command's output: {error}"))?;
        streams.forwarders = Some(forwarders);
        Ok(bytes)
    }

    /// Ends what writes the command's lines to the log, then seals it: the readers get [`DRAIN`] to reach the end of
    /// their pipes, longer while one is only behind; the job's members still running get [`EVENTS_WAIT`] to end, so
    /// their exit lines are written (`job`, `None` once the job is gone); the consumer is joined. After the seal no
    /// line of the command and no process line reaches the log. Nothing is waited for twice.
    fn end_streams(&mut self, job: Option<&Job>, console: &mut Console) {
        if let Some(Streams { forwarders, consumer }) = self.streams.take() {
            if let Some(forwarders) = forwarders
                && !forwarders.drain(DRAIN).all_ended
            {
                console.note(&format!(
                    "gate: the command's output readers did not finish within {} s; their last lines may be missing",
                    DRAIN.as_secs()
                ));
            }
            if let Some(job) = job {
                job.finish_events(EVENTS_WAIT);
            }
            if consumer.join().is_err() {
                console.err("gate: the thread writing the job's process lines failed");
            }
        }
        if let Some(writer) = &self.writer {
            writer.seal();
        }
    }

    /// Deletes the claim, journals the release with how the run `ended` (status and verdict unknown when it did not
    /// end on its own path), filing a run that reached a verdict in the durations store in the same journal hold
    /// ([`Run::write_release`]), and lets the slot go, then wakes the waiters. A critical gate journals the release of every
    /// slot it held first, then lets the machine go ([`Machine::release`]). A gate a parent's slot covers deletes its
    /// covered marker, then lets go the slot it took when that parent ended as its command ran ([`Late`]), as any slot
    /// is let go, its release naming the parent its late `take` named; it took no other slot, and journals no release
    /// when it took none. A gate a light parent lent its slot took no slot and journals no release; it deletes its lend
    /// marker and wakes the waiters.
    fn release(&mut self, hold: Hold, ended: Option<&Ended>, console: &mut Console) {
        if let Some(lend) = self.lend.take() {
            lend.end(self.root, &mut self.survey, console);
        }
        if let Some(mut covered) = self.covered.take() {
            covered.end(console);
        }
        if let Some(LateSlot { taken, took, parent_pid }) = self.late.take() {
            self.routing.parent_pid = parent_pid;
            self.let_go(Some(taken), took.elapsed().as_secs_f64(), ended, console);
        }
        if self.nesting.covering().is_some() || self.lent {
            return;
        }
        let held_seconds = hold.held_seconds();
        let Hold { taken, machine, .. } = hold;
        if let Some(machine) = machine {
            self.write_release(machine.names(), held_seconds, ended, console);
            machine.release(self.root, &mut self.survey, console);
            return;
        }
        self.let_go(taken, held_seconds, ended, console);
    }

    /// Deletes the claim of `taken`, the slot of a pool the run held `held_seconds`, journals the release with how the
    /// run `ended` and lets the slot go, then wakes the waiters; a run that took no slot journals a release of none.
    fn let_go(&mut self, taken: Option<Taken>, held_seconds: f64, ended: Option<&Ended>, console: &mut Console) {
        let slots = match &taken {
            Some(taken) => {
                slots::delete_claim(&taken.held, console);
                vec![taken.held.name().to_owned()]
            }
            None => Vec::new(),
        };
        self.write_release(slots, held_seconds, ended, console);
        if let Some(taken) = taken {
            drop(taken);
            slots::wake_waiters(self.root, &mut self.survey, console);
        }
    }

    /// Starts the command in the gate's job and sees it to its end under `figures`.
    fn command(&mut self, plan: Plan, figures: Figures, console: &mut Console) -> Ended {
        let launch = match plan.launch {
            Ok(launch) => launch,
            Err(message) => return self.not_started(&plan.word, plan.resolved.as_deref(), &message, console),
        };
        if self.host.interrupt.is_some_and(Interrupt::is_raised) {
            return self.interrupted_before_start(console);
        }
        let name = self.root.root.object_name(KernelObject::Job { gate_pid: self.gate.pid });
        let created = if self.request.test_seams.job_breakaway {
            Job::create_breakaway(&name)
        } else {
            Job::create(&name)
        };
        let mut job = match created {
            Ok(job) => job,
            Err(error) => return self.gate_failed(&format!("cannot create the job {name}: {error}"), None, console),
        };
        let start = Start::now();
        let mut suspended = match launch.start_suspended() {
            Ok(suspended) => suspended,
            Err(LaunchError::CreateProcess { message, .. }) => return self.not_started(&plan.word, plan.resolved.as_deref(), &message, console),
            Err(error) => return self.gate_failed(&error.to_string(), None, console),
        };
        // The job remembers every process put in it from the command on, so a kill still reaches what one that has
        // ended started outside it, and tells each one's coming and going to the log.
        let bytes = match self.start_streams(&mut job, &mut suspended) {
            Ok(bytes) => bytes,
            Err(problem) => {
                drop(suspended);
                self.end_streams(Some(&job), console);
                return self.gate_failed(&problem, None, console);
            }
        };
        match suspended.assign_and_resume(&job) {
            Ok(running) => self.watched(&job, &running, &start, &bytes, figures, console),
            Err(error) => {
                self.end_streams(Some(&job), console);
                self.gate_failed(&error.to_string(), None, console)
            }
        }
    }

    /// Says that a Ctrl+C came before the command started, which then never starts, and completes the log; the status
    /// is `STATUS_CONTROL_C_EXIT`, with no verdict.
    fn interrupted_before_start(&mut self, console: &mut Console) -> Ended {
        console.err("gate: interrupted before the command started");
        self.log_owed_end_load(None, console);
        if let Err(error) = self.log.finish(console) {
            console.err(&format!("gate: could not write to {}: {error}", self.log.path().display()));
        }
        Ended {
            status: i64::from(CONTROL_C_EXIT.cast_signed()),
            facts: None,
        }
    }

    /// Watches the started command to its exit, a kill or a Ctrl+C, then reaches the verdict. Should the gate fail
    /// or panic first, the job is terminated, so the command does not outlive the run. The command runs under `figures`.
    fn watched(&mut self, job: &Job, running: &Running, start: &Start, output: &OutputBytes, figures: Figures, console: &mut Console) -> Ended {
        // A copy of the run's own reference, so the stopper holds no borrow of the run while it is watched.
        let root = self.root;
        let recorded = RefCell::new(Recorded::default());
        let started = Started {
            job,
            running,
            recorded: Some(&recorded),
            output,
            figures,
        };
        let mut stopper = Stopper {
            ends: Ender {
                reach: Reach { job, root: &root.root },
                running,
            },
            recorded: &recorded,
            armed: true,
        };
        let ended = match self.watch(started, &recorded, start, console) {
            Ok((watched, timing)) => self.after_watch(watched, timing, started, console),
            Err(problem) => self.gate_failed(&problem, Some(started), console),
        };
        stopper.armed = false;
        ended
    }

    /// Watches the command until it exits, is killed or the console is interrupted; how it ended and how long it ran,
    /// with the cores it kept busy, else the gate's problem. The watch's samples write what they find outside the job to
    /// `recorded`. A heavy or light run a parent's slot covers watches that parent too, and takes a slot of its own kind
    /// into [`Run::late`] when the parent ends first ([`Late`]), its covered marker in hand.
    fn watch(
        &mut self,
        started: Started<'_>,
        recorded: &RefCell<Recorded>,
        start: &Start,
        console: &mut Console,
    ) -> Result<(Watched, Timing), String> {
        let (nesting, host, arrived) = (self.nesting, self.host, self.arrived);
        let Figures { ceiling, usual } = started.figures;
        let covering = nesting.covering();
        let (taker, held, covered) = self.late_taker(covering.map(|parent| parent.pid), ceiling);
        let (root, gate, request) = (taker.root, taker.gate, taker.request);
        let late = match (covering, Takes::of(request.slot), covered) {
            (Some(parent), Takes::Pool(pool), Some(covered)) => {
                let of = LateOf {
                    parent: parent.clone(),
                    pool,
                    processors: host.processors,
                    arrived,
                    taker,
                    held,
                    covered,
                };
                Late::watch(of, console)
            }
            _ => None,
        };
        let watching = Watching {
            job: started.job,
            running: started.running,
            output: started.output.clone(),
            ceiling,
            usual,
            interrupt: host.interrupt,
            runs_as: nesting.runs_as(request.slot),
            root,
            gate,
            hands_on: covering,
            recorded,
            late,
        };
        let mut watcher =
            Watcher::begin(request, watching, start, host.processors).map_err(|error| format!("cannot begin to watch the command: {error}"))?;
        let watched = watcher.until_end(console).map_err(|error| format!("cannot watch the command: {error}"))?;
        let timing = Timing {
            wall: start.started.elapsed(),
            adjusted: watcher.adjusted_seconds(),
            paused: watcher.paused_time(),
            cores: watcher.cores(),
            nested_took_slot: false,
        };
        let trace = watcher.trace();
        drop(watcher);
        self.trace = trace;
        Ok((watched, timing))
    }

    /// Ends the run as the watch ended it: a command that exited has what it left in its job ended but the gates there
    /// ([`end_leftovers`]), then its verdict, so a leftover's last output is in the log the verdict scans and a failure
    /// to end one is said before the verdict line; one killed as stalled, timed out or past the backstop has its
    /// diagnostics written to the log, its kill line and the pointer to them said, its job terminated and the verdict of
    /// status 124; an interrupted one is given 5 s to end before its job is stopped, and has the verdict of its own
    /// status, neither scanned for markers. A gate failure after a command's own exit ends its job, not what it left
    /// running outside it.
    fn after_watch(&mut self, watched: Watched, timing: Timing, started: Started<'_>, console: &mut Console) -> Ended {
        let started = Started {
            recorded: records_after(&watched, started.recorded),
            ..started
        };
        let Started { job, running, recorded, .. } = started;
        match watched {
            Watched::Exited(code) => {
                end_leftovers(job, &self.root.root, &[], console);
                let exited = i64::from(code.cast_signed());
                self.ended(&Outcome { exited, timing, kill: None }, false, &[], started, console)
            }
            Watched::Killed {
                line,
                hint,
                processes,
                diagnostics,
            } => {
                console.note_block(&diagnostics);
                console.err(&line);
                if let Some(hint) = hint {
                    console.err(hint);
                }
                kill(&self.ender(job, running), recorded, processes, console);
                console.err(&format!("gate: diagnostics written to {}", self.log.path().display()));
                let exited = i64::from(STOPPED);
                self.ended(
                    &Outcome {
                        exited,
                        timing,
                        kill: Some(&line),
                    },
                    true,
                    &diagnostics,
                    started,
                    console,
                )
            }
            Watched::Interrupted => {
                let exited = interrupted(&self.ender(job, running), recorded, console);
                self.ended(&Outcome { exited, timing, kill: None }, true, &[], started, console)
            }
        }
    }

    /// Reaches the verdict of `outcome`, its markers not scanned when `unscanned`, and the facts its release carries.
    /// `diagnostics` is the block a kill wrote to the log, whose lines the verdict's tail leaves out.
    fn ended(&mut self, outcome: &Outcome<'_>, unscanned: bool, diagnostics: &[String], started: Started<'_>, console: &mut Console) -> Ended {
        self.end_streams(Some(started.job), console);
        self.log_load(When::End, Some(started.job), console);
        let timing = self.without_nested_cores(outcome.timing, console);
        let passing = Passing {
            pass: Pass::Here(timing),
            ceiling: started.figures.ceiling,
            stale: &[],
        };
        match self.verdict(outcome.exited, Some(passing), unscanned, diagnostics, console) {
            Ok(status) => Ended {
                status,
                facts: Some(RunFacts {
                    wall_seconds: timing.wall.saturating_sub(timing.paused).as_secs_f64(),
                    adjusted_seconds: timing.adjusted,
                    avg_cores: timing.cores.map(|cores| cores.avg),
                    peak_cores: timing.cores.map(|cores| cores.peak),
                    free_cores: timing.cores.map(|cores| cores.free),
                    verdict: Verdict::of(outcome.kill, outcome.exited, status),
                }),
            },
            Err(problem) => self.gate_failed(&problem, Some(started), console),
        }
    }

    /// `timing` with its cores dropped when a gate under this run took a slot of its own during it, whose CPU the job
    /// counted as this run's ([`journal::nested_took_slot`]), or when the journal that tells cannot be read, which is
    /// noted in the log. A run whose cores are unknown reads nothing.
    fn without_nested_cores(&self, mut timing: Timing, console: &mut Console) -> Timing {
        if timing.cores.is_none() {
            return timing;
        }
        match journal::nested_took_slot(&self.root.path, self.gate.pid, self.gate.start) {
            Ok(false) => {}
            Ok(true) => {
                timing.nested_took_slot = true;
                timing.cores = None;
            }
            Err(error) => {
                console.note(&format!(
                    "gate: cannot tell whether a gate under this one took a slot of its own: {error}; its cores are unknown"
                ));
                timing.cores = None;
            }
        }
        timing
    }

    /// Says the start failure of `word`, found as `resolved`, with the system's `message`, and ends with status 127.
    fn not_started(&mut self, word: &str, resolved: Option<&str>, message: &str, console: &mut Console) -> Ended {
        let line = match resolved.filter(|resolved| *resolved != word) {
            Some(resolved) => format!("gate: could not start {word} ({resolved}): {message}"),
            None => format!("gate: could not start {word}: {message}"),
        };
        console.err(&line);
        self.log_owed_end_load(None, console);
        match self.verdict(NOT_STARTED, None, false, &[], console) {
            Ok(status) => Ended { status, facts: None },
            Err(problem) => self.gate_failed(&problem, None, console),
        }
    }

    /// Completes the log with the notes so far, and says the marker lines of a failure, the log's tail and the verdict
    /// line; the gate's status. `exited` is the command's status, and `passing` how long it ran and under what ceiling
    /// for the passed line, `None` when it never started; a run the gate ended itself is `unscanned`, its markers not
    /// read. `diagnostics` is the block a kill wrote to the log: the log keeps it, and the tail leaves exactly its lines
    /// out. A routed run's tail is not said: its command's output was already shown live, as the box sent it, so saying
    /// the last lines again would repeat them.
    fn verdict(
        &mut self,
        exited: i64,
        passing: Option<Passing<'_>>,
        unscanned: bool,
        diagnostics: &[String],
        console: &mut Console,
    ) -> Result<i64, String> {
        let shown = self.log.path().display().to_string();
        self.log
            .finish(console)
            .map_err(|error| format!("cannot complete the log {shown}: {error}"))?;
        let (found, tail) = self
            .read_back(unscanned, diagnostics)
            .map_err(|error| format!("cannot read the log {shown}: {error}"))?;
        let mut status = exited;
        if status == 0 && !found.is_empty() {
            console.err(&format!("gate: command exited 0 but its log reports a failure -> {shown}"));
            status = LOG_FAILURE;
        }
        if status != 0 {
            for (number, line) in &found {
                console.out(&format!("{number}:{line}"));
            }
        }
        let routed = matches!(passing, Some(Passing { pass: Pass::On(..), .. }));
        if !routed {
            for line in &tail {
                console.out(line);
            }
        }
        let closing = match passing {
            Some(passed) if status == 0 => self.passed_lines(passed, &shown),
            _ => vec![Said::Err(format!("gate: FAILED (status {status}). Full output: {shown}"))],
        };
        // A routed build whose restore here is still to run keeps its verdict line for after it ([`Run::restore_and_close`]).
        if self.restore_after.is_some() {
            self.closing = closing;
        } else {
            say(closing, console);
        }
        Ok(status)
    }

    /// What a passed run says: the lines naming the folders it left stale when it has any, the cores hint, the route
    /// line, and the passed line.
    fn passed_lines(&self, passing: Passing<'_>, shown: &str) -> Vec<Said> {
        let Passing { pass, ceiling, stale } = passing;
        let mut lines: Vec<Said> = stale.iter().map(|line| Said::Err(line.clone())).collect();
        lines.extend(self.cores_hint(pass).map(Said::NoteOut));
        lines.extend(self.route_hint(pass).map(Said::NoteOut));
        lines.push(Said::NoteOut(passed_line(pass, ceiling, shown)));
        lines
    }

    /// The cores line of a passed run that ran as `pass` says, when its slot did not fit ([`cores_hint_for`]).
    fn cores_hint(&self, pass: Pass<'_>) -> Option<String> {
        cores_hint_for(Judged {
            passed: true,
            own_slot: takes_own_slot(self.nesting, self.lent),
            slot: self.request.slot,
            commit: self.gate.commit(),
            timing: match pass {
                Pass::Here(timing) => Some(timing),
                Pass::On(..) => None,
            },
        })
    }

    /// The route line of a passed run that ran here as `pass` on a light slot, when its command would route on heavy
    /// ([`route_hint_for`]).
    fn route_hint(&self, pass: Pass<'_>) -> Option<String> {
        let decision = self.would.as_ref()?;
        route_hint_for(
            RouteJudged {
                own_slot: takes_own_slot(self.nesting, self.lent),
                slot: self.request.slot,
                timing: match pass {
                    Pass::Here(timing) => Some(timing),
                    Pass::On(..) => None,
                },
                floor: self
                    .request
                    .test_seams
                    .route_hint_floor_ms
                    .map_or(ROUTE_HINT_MIN_WALL, Duration::from_millis),
            },
            || route::would_route_on_heavy(decision, &self.request.test_seams),
        )
    }

    /// The log's first marker lines, none when the run turned the scan off or is `unscanned`, and its tail: the last
    /// [`RunRequest::tail`] lines of the log with the block `diagnostics` a kill wrote dropped by position, so the
    /// command's own output and the kill's lines fill the tail whatever the block's size.
    fn read_back(&self, unscanned: bool, diagnostics: &[String]) -> io::Result<(Vec<Marker>, Vec<String>)> {
        let found = if self.request.no_markers || unscanned {
            Vec::new()
        } else {
            self.log.markers()?
        };
        let dropped = diagnostics.len() + self.load_blocks.iter().map(Vec::len).sum::<usize>();
        let wanted = self.request.tail.saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
        let mut lines = self.log.tail(wanted)?;
        for block in &self.load_blocks {
            if let Some(at) = last_run(&lines, block) {
                lines.drain(at..at + block.len());
            }
        }
        Ok((found, tail_without_block(lines, diagnostics, self.request.tail)))
    }

    /// What a kill of the command in `job` reaches: the job, and the state root that names a gate's job.
    fn reach<'s>(&'s self, job: &'s Job) -> Reach<'s> {
        Reach { job, root: &self.root.root }
    }

    /// What ends the command `running` in `job`: the job with what its kill reaches outside it, and the command.
    fn ender<'s>(&'s self, job: &'s Job, running: &'s Running) -> Ender<'s> {
        Ender {
            reach: self.reach(job),
            running,
        }
    }

    /// Says the gate's own failure, terminates the job of a command that started, and completes the log; status 1.
    fn gate_failed(&mut self, problem: &str, started: Option<Started<'_>>, console: &mut Console) -> Ended {
        console.err(&format!("gate: the gate failed while running the command: {problem}"));
        let job = started.as_ref().map(|started| started.job);
        if let Some(Started { job, running, recorded, .. }) = started {
            stop(&self.ender(job, running), recorded, console);
            self.end_streams(Some(job), console);
        }
        self.log_owed_end_load(job, console);
        if let Err(error) = self.log.finish(console) {
            console.err(&format!(
                "gate: could not write to {} after the failure: {error}",
                self.log.path().display()
            ));
        }
        Ended {
            status: GATE_FAILED,
            facts: None,
        }
    }
}

/// Terminates the job, and ends the processes that left it, when dropped armed: on a return or a panic the run did not
/// plan for after the command started.
struct Stopper<'a, E: Ends> {
    /// The job and what its stop reaches outside it.
    ends: E,
    /// The processes outside the job the watch recorded, which the stop ends too.
    recorded: &'a RefCell<Recorded>,
    armed: bool,
}

impl<E: Ends> Drop for Stopper<'_, E> {
    fn drop(&mut self) {
        if self.armed {
            // The run is already leaving on a failure, with nowhere left to report a failed terminate to.
            let _ = self.ends.terminate(STOPPED, STOP_WAIT, earlier(Some(self.recorded)));
        }
    }
}

/// What the ends of a run stop: the command's job with the processes that left it, and the command itself.
trait Ends {
    /// What `earlier` found with what a look now finds outside the job ([`Reach::look`]).
    fn look(&self, earlier: Found) -> Found;
    /// Terminates the job with `status` and ends the processes outside it, `earlier`'s with those a look finds now
    /// ([`Reach::terminate`]).
    fn terminate(&self, status: u32, wait: Duration, earlier: Found) -> io::Result<Reached>;
    /// Waits at most `timeout` for the command; its exit code once it has exited, `None` while it still runs.
    fn wait(&self, timeout: Duration) -> io::Result<Option<u32>>;
}

/// The command's job, what its stop reaches outside it, and the command's process.
struct Ender<'a> {
    reach: Reach<'a>,
    running: &'a Running,
}

impl Ends for Ender<'_> {
    fn look(&self, earlier: Found) -> Found {
        self.reach.look(earlier)
    }

    fn terminate(&self, status: u32, wait: Duration, earlier: Found) -> io::Result<Reached> {
        self.reach.terminate(status, wait, earlier)
    }

    fn wait(&self, timeout: Duration) -> io::Result<Option<u32>> {
        self.running.wait(timeout)
    }
}

/// The command's started job and process, the processes outside the job its watch records, which every end of the run
/// but the command's own exit ends: `None` once the command has exited; and the figures it runs under.
#[derive(Clone, Copy)]
struct Started<'a> {
    job: &'a Job,
    running: &'a Running,
    recorded: Option<&'a RefCell<Recorded>>,
    /// The bytes the command's readers have read, which the watch's stall counts.
    output: &'a OutputBytes,
    figures: Figures,
}

/// The processes outside the job the watch `recorded` that an end of the run after `watched` reaches: none after the
/// command's own exit, which leaves what it left running outside its job.
fn records_after<'a>(watched: &Watched, recorded: Option<&'a RefCell<Recorded>>) -> Option<&'a RefCell<Recorded>> {
    match watched {
        Watched::Exited(_) => None,
        Watched::Killed { .. } | Watched::Interrupted => recorded,
    }
}

/// What the watch `recorded` outside the job, as a kill's earlier look: its processes, parents first; nothing when
/// there is no record. A record the watch is still writing, which no end of the run reads while a sample runs, is
/// nothing, so the stop on a panic never panics itself.
fn earlier(recorded: Option<&RefCell<Recorded>>) -> Found {
    recorded
        .and_then(|recorded| recorded.try_borrow().ok())
        .map_or_else(Found::default, |recorded| Found::recorded(&recorded))
}

/// Terminates the job `ends` names after a failure of the gate's own, ends the processes that left it, those the watch
/// `recorded` with them, and gives the command a moment to end.
fn stop(ends: &impl Ends, recorded: Option<&RefCell<Recorded>>, console: &mut Console) {
    match ends.terminate(STOPPED, STOP_WAIT, earlier(recorded)) {
        Ok(reached) => say_problems(&reached, console),
        Err(error) => {
            console.err(&format!("gate: and could not terminate the command's job: {error}"));
            return;
        }
    }
    await_stopped(ends, console);
}

/// Terminates the job `ends` names of a killed command, which held `processes` processes, ends the processes that left
/// it, those the watch `recorded` with them, and gives the command a moment to end.
fn kill(ends: &impl Ends, recorded: Option<&RefCell<Recorded>>, processes: u32, console: &mut Console) {
    match ends.terminate(STOPPED, STOP_WAIT, earlier(recorded)) {
        Ok(reached) => {
            console.err(&escaped::terminated_line(processes, &reached.ended));
            say_problems(&reached, console);
        }
        Err(error) => {
            console.err(&format!("gate: could not terminate the command's job: {error}"));
            return;
        }
    }
    await_stopped(ends, console);
}

/// Looks for the processes that left the command's job as soon as the interrupt arrives, while the in-job process that
/// started them may still run, then waits up to [`STOP_WAIT`] for the command, which had the Ctrl+C too, to end on its
/// own; its own status, with whatever it left in its job terminated and the processes that left it ended: those that
/// first look found and those a second look at the terminate finds. One still running then has its job terminated and
/// those processes ended the same way, and the status is `STATUS_CONTROL_C_EXIT`. The processes the watch `recorded`
/// outside the job are ended with those the first look found.
fn interrupted(ends: &impl Ends, recorded: Option<&RefCell<Recorded>>, console: &mut Console) -> i64 {
    console.err("gate: interrupted; stopping the command");
    let early = ends.look(earlier(recorded));
    match ends.wait(STOP_WAIT) {
        Ok(Some(code)) => {
            // What the command left running in its job, or outside it, does not outlive the gate after a Ctrl+C.
            match ends.terminate(CONTROL_C_EXIT, STOP_WAIT, early) {
                Ok(reached) => say_problems(&reached, console),
                Err(error) => console.err(&format!("gate: could not terminate the command's job: {error}")),
            }
            return i64::from(code.cast_signed());
        }
        Ok(None) => console.err("gate: the command was still running 5 s after the interrupt; terminating its job"),
        Err(error) => console.err(&format!(
            "gate: cannot wait for the command after the interrupt: {error}; terminating its job"
        )),
    }
    match ends.terminate(CONTROL_C_EXIT, STOP_WAIT, early) {
        Ok(reached) => {
            say_problems(&reached, console);
            await_stopped(ends, console);
        }
        Err(error) => console.err(&format!("gate: could not terminate the command's job: {error}")),
    }
    i64::from(CONTROL_C_EXIT.cast_signed())
}

/// Ends what a command that exited left in its `job`, before its verdict is reached: every process in it but the gates
/// it holds, with the processes each one started and those in its job under `root`, since each such gate ends what its
/// own command leaves when that command exits; but the console hosts, which end with their last client; and but the
/// processes of `keep`, which the caller ends itself. It goes over the job in passes ([`in_passes`], [`end_pass`]), so
/// what a leftover starts while one pass ends it is ended by the next. A run that left nothing says nothing; a
/// failure, or processes left after the last pass, is said once, on stderr and in the log, and the run's status stands.
pub(crate) fn end_leftovers(job: &Job, root: &StateRoot, keep: &[u32], console: &mut Console) {
    if let Some(problem) = in_passes(|ending| end_pass(job, root, keep, ending)) {
        console.err(&format!("gate: cannot end the run's job: {problem}"));
    }
}

/// What one pass over the run's job found: how many of its processes it would end, and ended when it was let, and what
/// it could not open, judge or end.
struct EndPass {
    left: usize,
    problems: Vec<String>,
}

/// Runs `pass` over the run's job, ending what it finds (`pass(true)`), until a pass finds nothing left to end, at most
/// [`END_PASSES`] times; one more pass then only counts (`pass(false)`). What to say: why a pass could not run; when
/// the passes left processes, how many, with the first problem of the last pass that ended processes or, after it, of
/// the counting pass, so a terminate refused up to the cap is said; else the first problem of the pass that found
/// nothing left, or `None` when it met none. A problem of an earlier pass is not said once a pass finds nothing left,
/// since the process it was about had gone by then, or was met again.
fn in_passes(mut pass: impl FnMut(bool) -> Result<EndPass, String>) -> Option<String> {
    let mut ending_problems = Vec::new();
    for _ in 0..END_PASSES {
        let found = match pass(true) {
            Ok(found) => found,
            Err(problem) => return Some(problem),
        };
        if found.left == 0 {
            return found.problems.into_iter().next();
        }
        ending_problems = found.problems;
    }
    let counted = match pass(false) {
        Ok(counted) => counted,
        Err(problem) => return Some(problem),
    };
    if counted.left == 0 {
        return counted.problems.into_iter().next();
    }
    let count = format!("{} of its processes still ran after {END_PASSES} passes", counted.left);
    Some(match ending_problems.into_iter().chain(counted.problems).next() {
        Some(first) => format!("{count}; first: {first}"),
        None => count,
    })
}

/// One pass of the run's end over `job`, ending what it finds when `ending`: the job's processes are listed, then the
/// machine's in a snapshot, the gates among them spared with what runs under them and the console hosts
/// ([`spared_gates`]), and the processes of `keep`, and every other one ended that its own handle shows in the job still
/// and still running ([`opened_members`]), a member whose own handle shows it ended not counted, and with the start and
/// the parent the snapshot lists for its pid ([`by_start`]), so a pid freed and taken by a new process since the
/// snapshot, a spared gate's command's child included, is passed over, and left to the next pass to look at again. The
/// processes ended are given [`TERMINATE_CHECK`] together to leave the job before the next pass lists it
/// ([`end_and_wait`]).
///
/// # Errors
///
/// The job's processes or the process snapshot cannot be read: nothing is ended.
fn end_pass(job: &Job, root: &StateRoot, keep: &[u32], ending: bool) -> Result<EndPass, String> {
    let members = job.process_ids().map_err(|error| failure("QueryInformationJobObject", &error))?;
    let mut problems = Vec::new();
    if members.is_empty() {
        return Ok(EndPass { left: 0, problems });
    }
    let tree = ProcessTree::new(Win32ProcessTable.snapshot_without_command_lines());
    let mut spared = spared_gates(&members, &tree, |gate| gate_job(root, gate), &mut problems)?;
    spared.extend(keep);
    let opened = opened_members(job, members.into_iter().filter(|pid| !spared.contains(pid)), &mut problems);
    let found = by_start(started(opened, &mut problems), &tree);
    let left = found.left();
    if ending {
        end_and_wait(&found.ended, &mut problems);
    }
    Ok(EndPass { left, problems })
}

/// Each of `members` that its own handle shows in `job` still, opened to be ended ([`opened_member`]); what cannot be
/// opened or judged is added to `problems`.
fn opened_members(job: &Job, members: impl Iterator<Item = u32>, problems: &mut Vec<String>) -> Vec<(u32, StoppableProcess)> {
    let mut opened = Vec::new();
    for pid in members {
        match opened_member(job, pid) {
            Ok(Some(process)) => opened.push((pid, process)),
            Ok(None) => {}
            Err(problem) => problems.push(problem),
        }
    }
    opened
}

/// A member's start and the id of the process that started it, as its own handle reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Origin {
    start: Timestamp,
    parent: u32,
}

/// Each of `opened` with its [`Origin`]; one whose start or parent cannot be read is left out, the failure added to
/// `problems`.
fn started(opened: Vec<(u32, StoppableProcess)>, problems: &mut Vec<String>) -> Vec<((u32, StoppableProcess), Origin)> {
    let mut started = Vec::new();
    for (pid, process) in opened {
        let start = match process.created() {
            Ok(start) => start,
            Err(error) => {
                problems.push(failure(&format!("GetProcessTimes({pid})"), &error));
                continue;
            }
        };
        match process.parent_pid() {
            Ok(parent) => started.push(((pid, process), Origin { start, parent })),
            Err(error) => problems.push(failure(&format!("reading the parent of {pid}"), &error)),
        }
    }
    started
}

/// What a pass found among the members it opened: those it ends, and how many it passed over as not the process the
/// snapshot listed under their pid.
struct ByStart<T> {
    ended: Vec<T>,
    passed_over: usize,
}

impl<T> ByStart<T> {
    /// How many members the pass leaves for the next one to look at: those it ends, which may not have left the job by
    /// then, and those it passed over, which the next pass's snapshot lists.
    fn left(&self) -> usize {
        self.ended.len() + self.passed_over
    }
}

/// The members of `started`, each a pid with its [`Origin`] as its own handle reads it, that a pass ends: every one the
/// snapshot `tree` lists under its pid with that same start and parent, as [`crate::stopping::judged`] compares them.
/// The others are passed over and counted: a pid the snapshot lacks, or lists with another start or parent, has been
/// taken by a process that started since the snapshot, or since the snapshot read the name and parent of the one
/// before it there, and one whose start the snapshot could not read cannot be judged.
fn by_start<T>(started: Vec<((u32, T), Origin)>, tree: &ProcessTree) -> ByStart<(u32, T)> {
    let listed = |pid: u32| {
        tree.get(pid).and_then(|process| {
            Some(Origin {
                start: process.created?,
                parent: process.parent_pid,
            })
        })
    };
    let (ended, passed_over): (Vec<_>, Vec<_>) = started.into_iter().partition(|((pid, _), origin)| listed(*pid) == Some(*origin));
    ByStart {
        ended: ended.into_iter().map(|(member, _)| member).collect(),
        passed_over: passed_over.len(),
    }
}

/// How long a process whose termination failed is given to end before the failure counts.
const TERMINATE_CHECK: Duration = Duration::from_secs(1);

/// Terminates every one of `doomed`, then gives them [`TERMINATE_CHECK`] together, from the first wait, to leave the
/// job before the next pass lists it, each wait taking what is left of that time ([`escaped::wait_left`]). A process
/// that was ending already counts as terminated; one that refused the terminate is added to `problems` only when its
/// wait has not seen it end.
fn end_and_wait(doomed: &[(u32, impl Endable)], problems: &mut Vec<String>) {
    let refusals: Vec<Option<io::Error>> = doomed.iter().map(|(_, process)| process.terminate(STOPPED).err()).collect();
    let mut deadline = None;
    for ((pid, process), refused) in doomed.iter().zip(refusals) {
        let (end, left) = escaped::wait_left(deadline, TERMINATE_CHECK, Instant::now());
        deadline = Some(end);
        // A failed wait leaves a terminated process to the next pass, which lists it again while it is in the job.
        let ended = matches!(process.wait(left), Ok(true));
        if let Some(error) = refused
            && !ended
        {
            problems.push(failure(&format!("TerminateProcess({pid})"), &error));
        }
    }
}

/// A process the run's end ends: terminated, then waited for.
trait Endable {
    /// Terminates it with `status`.
    fn terminate(&self, status: u32) -> io::Result<()>;
    /// Waits up to `timeout` for it to end; whether it has.
    fn wait(&self, timeout: Duration) -> io::Result<bool>;
}

impl Endable for StoppableProcess {
    fn terminate(&self, status: u32) -> io::Result<()> {
        // A process found ending already counts as terminated.
        StoppableProcess::terminate(self, status).map(|_| ())
    }

    fn wait(&self, timeout: Duration) -> io::Result<bool> {
        StoppableProcess::wait(self, timeout)
    }
}

/// The processes in the job of the gate `gate`, those in the jobs nested inside it included, looked for under `root`,
/// then under the default root ([`job_under_roots`]); `None` when no job has its name, as for a gate whose command has
/// not started.
fn gate_job(root: &StateRoot, gate: u32) -> io::Result<Option<Vec<u32>>> {
    job_under_roots(root, gate, |name| {
        OpenedJob::open_to_list(name).and_then(|opened| opened.map(|opened| opened.process_ids()).transpose())
    })
}

/// What `list` gives for the name of the job of the gate `gate` under `root`, or, when no job has that name and `root`
/// is not the default root, for its name under the default root, where a gate given no `--state-dir` or
/// `GATE_STATE_DIR` names its job. A failure under `root` is returned as it is, and the default root is not looked at.
fn job_under_roots(root: &StateRoot, gate: u32, list: impl Fn(&str) -> io::Result<Option<Vec<u32>>>) -> io::Result<Option<Vec<u32>>> {
    let job = KernelObject::Job { gate_pid: gate };
    let own = list(&root.object_name(job))?;
    if own.is_some() || root.is_default() {
        return Ok(own);
    }
    // A root judged against itself is the default root, whose object names no root path enters.
    let default = StateRoot::new(root.normalized(), Some(root.normalized()));
    list(&default.object_name(job))
}

/// The processes among `members` that a run's end leaves running: each one the snapshot `tree` names as a gate, the
/// processes above it in the job up to the first that is not a member, which launched it and wait on it, the processes
/// under it, and those in its job, as `gate_job` lists them; and each one it names as a console host
/// ([`is_console_host`]), which ends by itself with its console's last client, and whose end would take the console
/// from a spared gate still on it. A gate whose job cannot be listed is spared with what the snapshot shows under it,
/// and the failure added to `problems`.
///
/// # Errors
///
/// A snapshot that listed no process, which says nothing of which members are gates.
fn spared_gates(
    members: &[u32],
    tree: &ProcessTree,
    gate_job: impl Fn(u32) -> io::Result<Option<Vec<u32>>>,
    problems: &mut Vec<String>,
) -> Result<HashSet<u32>, String> {
    if tree.iter().next().is_none() {
        return Err(escaped::EMPTY_SNAPSHOT.to_owned());
    }
    let children = tree.children();
    let image = |pid: u32| tree.get(pid).map(|process| process.name.as_str());
    let mut spared: HashSet<u32> = members.iter().copied().filter(|&pid| image(pid).is_some_and(is_console_host)).collect();
    let gates = members.iter().filter(|&&pid| image(pid).is_some_and(escaped::is_gate));
    for &gate in gates {
        spared.insert(gate);
        let launchers = tree.ancestors(gate).take_while(|process| members.contains(&process.pid));
        spared.extend(launchers.map(|process| process.pid));
        spared.extend(children.descendants(gate).iter().map(|process| process.pid));
        match gate_job(gate) {
            Ok(inside) => spared.extend(inside.into_iter().flatten()),
            Err(error) => problems.push(failure(&format!("listing the job of gate {gate}"), &error)),
        }
    }
    Ok(spared)
}

/// Whether `image` is a console host's, `conhost.exe`, compared as a gate's image is ([`escaped::is_image`]).
fn is_console_host(image: &str) -> bool {
    escaped::is_image(image, CONSOLE_HOST_IMAGE)
}

/// The process `pid`, opened to be ended, when its own handle shows it in `job` and still running; `None` when no
/// process has that pid now, the one that has it is outside the job, or its own handle shows it ended, as the job's list
/// names a process for a while after it ended; the failure, when it cannot be opened or judged.
fn opened_member(job: &Job, pid: u32) -> Result<Option<StoppableProcess>, String> {
    let process = match StoppableProcess::open(pid) {
        Ok(Some(process)) => process,
        Ok(None) => return Ok(None),
        Err(error) => return Err(failure(&format!("OpenProcess({pid})"), &error)),
    };
    match process.in_job(job) {
        Ok(true) => {}
        Ok(false) => return Ok(None),
        Err(error) => return Err(failure(&format!("IsProcessInJob({pid})"), &error)),
    }
    // The job's list still names a member for a while after its handle is signalled; one that has ended is not left.
    match process.wait(Duration::ZERO) {
        Ok(true) => Ok(None),
        Ok(false) => Ok(Some(process)),
        Err(error) => Err(failure(&format!("WaitForSingleObject({pid})"), &error)),
    }
}

/// Says each problem ending the processes that left the job.
fn say_problems(reached: &Reached, console: &mut Console) {
    for problem in &reached.problems {
        console.err(problem);
    }
}

/// Waits up to [`STOP_WAIT`] for the command `ends` names, whose job was terminated; its exit code, or `None`, said,
/// when it did not end or could not be waited for.
fn await_stopped(ends: &impl Ends, console: &mut Console) -> Option<u32> {
    match ends.wait(STOP_WAIT) {
        Ok(Some(code)) => Some(code),
        Ok(None) => {
            console.err("gate: the command was still running 5 s after its job was terminated");
            None
        }
        Err(error) => {
            console.err(&format!("gate: cannot wait for the command after terminating its job: {error}"));
            None
        }
    }
}

/// The tail of `lines` for a kill that wrote `block`: the last contiguous run equal to `block` is dropped by position,
/// and the last `count` lines of what is left are the tail. Nothing is dropped by content, so a command line equal to a
/// block line is still shown.
fn tail_without_block(mut lines: Vec<String>, block: &[String], count: u64) -> Vec<String> {
    if let Some(at) = last_run(&lines, block) {
        lines.drain(at..at + block.len());
    }
    let wanted = usize::try_from(count).unwrap_or(usize::MAX);
    let extra = lines.len().saturating_sub(wanted);
    lines.drain(..extra);
    lines
}

/// The index the last run of `lines` equal to `block` starts at, or `None` when `block` occurs nowhere.
fn last_run(lines: &[String], block: &[String]) -> Option<usize> {
    (0..=lines.len().checked_sub(block.len())?)
        .rev()
        .find(|&at| &lines[at..at + block.len()] == block)
}

/// The passed line of a run that ran as `pass`, its ceiling as `ceiling` whole seconds and its log shown as `shown`.
///
/// A run here whose clock has reached its ceiling says so, naming the ceiling it ran past: the load-adjusted clock the
/// ceiling kill is enforced on, or the wall time the watch falls back to once the sampler has failed. The comparison is
/// the kill's own, so the line says it ran past exactly when the kill would have fired. A run on a builder says the
/// same of the clock the builder counted, then its parts.
fn passed_line(pass: Pass<'_>, ceiling: u64, shown: &str) -> String {
    match pass {
        Pass::Here(timing) => format!(
            "gate: passed in {} s ({}). Full output: {shown}",
            whole_seconds(timing.wall.as_secs_f64()),
            against_ceiling(timing.adjusted, timing.wall, ceiling)
        ),
        Pass::On(host, wall, split, adjusted) => format!(
            "gate: passed in {} s on {host} ({}; {}). Full output: {shown}",
            whole_seconds(wall.as_secs_f64()),
            against_ceiling(adjusted, split.command, ceiling),
            split_said(split)
        ),
    }
}

/// What a passed line says of the command's clock against its ceiling of `ceiling` whole seconds: its `adjusted`
/// seconds, or that they are unknown as the sampler failed, and the ceiling, or that the clock ran past it: the
/// load-adjusted clock, or the command's `wall` time once the sampler failed.
#[expect(
    clippy::cast_precision_loss,
    reason = "the ceiling is read as the double the clock counts in, as the ceiling kill reads it"
)]
fn against_ceiling(adjusted: Option<f64>, wall: Duration, ceiling: u64) -> String {
    let (adjusted, over) = match adjusted {
        Some(adjusted) => (format!("{} s", whole_seconds(adjusted)), adjusted >= ceiling as f64),
        None => ("unknown: the sampler failed".to_owned(), wall.as_secs_f64() >= ceiling as f64),
    };
    if over {
        format!("load-adjusted {adjusted}, past its ceiling of {ceiling} s")
    } else {
        format!("load-adjusted {adjusted}, ceiling {ceiling} s")
    }
}

/// One part of a routed run's time, as its passed line and its release's `phases` both name it.
struct Part {
    /// Its name in the passed line, such as `sent build`.
    said: &'static str,
    /// Its name in the release's `phases`, such as `sentBuild`.
    journalled: &'static str,
    /// How long it took.
    took: Duration,
}

/// The parts of a routed run's time, `split`, in the order they ran: the command's own time, then each transfer the run
/// made, the push always. The one list the passed line ([`split_said`]) and the release ([`phases`]) both read.
fn split_parts(split: Split) -> Vec<Part> {
    let part = |said, journalled, took| Part { said, journalled, took };
    let optional = [
        ("inputs", "inputs", split.inputs),
        ("lfs", "lfs", split.lfs),
        ("sent build", "sentBuild", split.sent_build),
        ("sync-back", "syncBack", split.sync_back),
        ("outputs", "outputs", split.outputs),
    ];
    let mut parts = vec![part("command", "command", split.command), part("push", "push", split.push)];
    parts.extend(
        optional
            .into_iter()
            .filter_map(|(said, journalled, took)| took.map(|took| part(said, journalled, took))),
    );
    parts
}

/// What a routed run's passed line says of its parts, `split` ([`split_parts`]), each in whole seconds.
fn split_said(split: Split) -> String {
    let parts: Vec<String> = split_parts(split)
        .into_iter()
        .map(|part| format!("{} {} s", part.said, whole_seconds(part.took.as_secs_f64())))
        .collect();
    parts.join(", ")
}

/// The `phases` a routed run's release gives of its parts, `split`: the parts its passed line says ([`split_parts`]),
/// in its order, by their journal names, in seconds; none for a client that timed none of them.
fn phases(split: &Split) -> Vec<(&'static str, f64)> {
    if *split == Split::default() {
        return Vec::new();
    }
    split_parts(*split)
        .into_iter()
        .map(|part| (part.journalled, part.took.as_secs_f64()))
        .collect()
}

/// `seconds` in whole seconds, rounded half to even as the PowerShell gate's `[math]::Round` rounds.
fn whole_seconds(seconds: f64) -> f64 {
    seconds.round_ties_even()
}

/// What the cores line of a run is decided from: whether it passed, whether it took its own slot (not covered by a
/// parent's, nor lent one), the slot kind it asked for, whether it is a commit run ([`ThisGate::commit`]), and
/// how long it ran here, `None` for a run on a builder.
#[derive(Clone, Copy)]
struct Judged {
    passed: bool,
    own_slot: bool,
    slot: Slot,
    commit: bool,
    timing: Option<Timing>,
}

/// The line a run says as it drops its `--no-build` for `reason`: building on the slot of `parent`, the parent gate that
/// covers or lent it, on no slot when it runs inside a box run (`in_box`), else on its critical slot when the run is of
/// the `slot` kind critical, and on a heavy slot of its own otherwise.
fn dropped_line(reason: &str, parent: Option<&Parent>, in_box: bool, slot: Slot) -> String {
    let building = match parent {
        Some(parent) => format!("building on the parent gate's {} slot", parent.held.as_str()),
        None if in_box => "building on no slot".to_owned(),
        None if slot == Slot::Critical => "building on its critical slot".to_owned(),
        None => "building on a heavy slot".to_owned(),
    };
    format!("gate: dropped --no-build: {reason}; {building}")
}

/// Whether a run under `nesting` took a slot of its own: not when a parent's slot covers it, inside a box run, nor when
/// a light parent lent it its slot (`lent`). A run here after a fallback from a builder takes its own local slot like any
/// other.
fn takes_own_slot(nesting: &Nesting, lent: bool) -> bool {
    nesting.covering().is_none() && !matches!(nesting, Nesting::InBox) && !lent
}

/// The cores line of `judged`, when its slot did not fit: only a passed run that ran here on its own light or heavy
/// slot, or on no slot, for at least [`HINT_MIN_WALL`] of unpaused wall time, with no gate under it that took a slot of
/// its own, is judged, and only when its cores are known.
fn cores_hint_for(judged: Judged) -> Option<String> {
    let timing = judged.timing.filter(|_| judged.passed && judged.own_slot)?;
    if timing.nested_took_slot || timing.wall.saturating_sub(timing.paused) < HINT_MIN_WALL {
        return None;
    }
    let slot = match judged.slot {
        Slot::Light => SlotKind::Light,
        Slot::Heavy => SlotKind::Heavy,
        Slot::Critical => SlotKind::Critical,
        Slot::None => SlotKind::None,
    };
    let cores = timing.cores?;
    hint(cores, slot).map(|misfit| cores_line(cores, misfit, judged.commit))
}

/// The line that tells a run its slot did not fit, `misfit` as `cores` show it: the average and free cores to one
/// decimal, the peak to a whole number, halves to even. A run on no slot is told without the free cores. A commit run
/// (`commit`) is told of its hooks and of the `--slot` to pass.
fn cores_line(cores: Cores, misfit: Hint, commit: bool) -> String {
    let who = if commit { "the commit's hooks used" } else { "used" };
    let used = format!("gate: {who} {:.1} cores on average (peak {}", cores.avg, cores.peak.round_ties_even());
    let free = cores.free;
    match (misfit, commit) {
        (Hint::Hot, false) => format!("{used}, {free:.1} free) on a light slot; work that keeps more than 2 busy belongs on heavy"),
        (Hint::Hot, true) => format!("{used}, {free:.1} free) on a light slot; hooks that keep more than 2 busy belong on --slot heavy"),
        (Hint::Cold, false) => format!("{used}, {free:.1} free) on a heavy slot; work that keeps under 1.5 busy fits on light"),
        (Hint::Cold, true) => format!("{used}, {free:.1} free) on a heavy slot; hooks that keep under 1.5 busy fit on --slot light"),
        (Hint::Busy, _) => format!("{used}) on no slot; work that keeps 1.5 or more busy belongs on light or heavy"),
    }
}

/// What the route line of a run is decided from: whether it took its own slot (not covered by a parent's, nor lent
/// one), the slot kind it asked for, how long it ran here (`None` for a run on a builder), and the floor under which a
/// run is too short to ask whether its command would route on heavy.
#[derive(Clone, Copy)]
struct RouteJudged {
    own_slot: bool,
    slot: Slot,
    timing: Option<Timing>,
    floor: Duration,
}

/// The route line of `judged`, when its command would have routed on heavy: only a run that ran here on its own light
/// slot, for at least its floor of unpaused wall time, is asked, and only then does it ask `would_route`, so no other
/// run reads the routing file or git.
fn route_hint_for(judged: RouteJudged, would_route: impl FnOnce() -> bool) -> Option<String> {
    if !judged.own_slot || judged.slot != Slot::Light {
        return None;
    }
    let timing = judged.timing?;
    let unpaused = timing.wall.saturating_sub(timing.paused);
    if unpaused < judged.floor || !would_route() {
        return None;
    }
    Some(route_hint_line(unpaused))
}

/// The line that tells a passed light run its command would route on heavy, after `unpaused` of unpaused wall time:
/// whole seconds, rounded down.
fn route_hint_line(unpaused: Duration) -> String {
    format!(
        "gate: ran {} s on a light slot; on heavy, remote.json would send this command to a builder",
        unpaused.as_secs()
    )
}

/// The line a routed run says as it starts waiting for its worktree's work folder on `host`, which another of its runs
/// holds, with `left`, the holder's time left by its remote claim, when that claim carries a usual run time.
fn key_wait_line(host: &str, left: Option<String>) -> String {
    let line = format!("gate: waiting for this worktree's work folder on {host}, which another of its runs holds");
    match left {
        Some(left) => format!("{line} (about {left} left)"),
        None => line,
    }
}

/// How long the holder of the work folder of `key` under `state` has left by its remote claim, by the gate's figure
/// rule, for the key wait's first line: `None` when no live claim names `key`, when its claim carries no usual run time,
/// or once that time is at or past.
fn holder_time_left(survey: &mut Survey, state: &Path, key: &str, console: &mut Console) -> Option<String> {
    let claims = survey.live_claims(state, console).ok()?;
    key_wait_left(holder_claim(&claims, key)?, Timestamp::now())
}

/// The live claim of the work folder of `key` among `claims` with the latest `started`, when one names it: a holder
/// that let the key go keeps its claim until its run ends, so the latest one is the run that holds it now.
fn holder_claim<'c>(claims: &'c [v1::Claim], key: &str) -> Option<&'c v1::Claim> {
    claims
        .iter()
        .filter(|claim| claim.key.as_deref() == Some(key))
        .max_by_key(|claim| claim.started)
}

/// What the key wait's first line says of `claim` at `now`: the holder's time left by its usual run time, by the gate's
/// figure rule ([`span_text`]), or `None` when the claim carries no usual run time or that time is at or past.
fn key_wait_left(claim: &v1::Claim, now: Timestamp) -> Option<String> {
    let left = wait::usual(claim, now)?.left;
    (left > 0.0).then(|| span_text(left))
}

/// Whether a warm-up of the work folder of `key` under `state` holds the key's lock, by the warm-up lock beside it: a
/// run whose holder is a warm-up falls back at once rather than wait for a warm-up to end. A lock that cannot be
/// probed reads as no warm-up, said once through `said`, and the wait goes on.
fn warm_up_holds(state: &Path, key: &str, console: &mut Console, said: &mut bool) -> bool {
    let path = v1::warm_lock_path(state, key);
    match warm::lock_held(&path) {
        Ok(held) => held,
        Err(error) => {
            if !*said {
                *said = true;
                console.err(&format!(
                    "gate: cannot probe the warm-up lock {}: {}",
                    gate_router::escaped(&path.to_string_lossy()),
                    gate_router::escaped(&error.to_string())
                ));
            }
            false
        }
    }
}

/// The line a routed run says as it falls back for `reason` from the builder `host`: to the `next` builder it tries,
/// `trying <next>`, else here, `running here`; a client that could not start says why, `detail`.
fn fallback_line(host: &str, reason: FallbackReason, detail: Option<&str>, next: Option<&str>) -> String {
    let goes = next.map_or_else(|| "running here".to_owned(), |next| format!("trying {next}"));
    format!("{}; {goes}", fallback_head(host, reason, detail))
}

/// Why the builder `host` did not run the command, as a fallback line says it before where the run goes
/// ([`fallback_line`]): `gate: remote <host> <reason>`, or for a client that could not start, why, `detail`.
fn fallback_head(host: &str, reason: FallbackReason, detail: Option<&str>) -> String {
    match reason {
        FallbackReason::Client => format!("gate: remote {host} client could not start: {}", detail.unwrap_or("no reason given")),
        _ => format!("gate: remote {host} {}", reason.as_str()),
    }
}

/// What a run pinned with `--builder` says after its builder's fallback head, in place of where the run goes.
const PIN_NOT_HERE: &str = "--builder given, so not running here";

/// The fallback reasons worth another builder: a builder that is full, busy, cannot be reached, or lacks the version,
/// parity, install or tools the run needs may be one another builder has. Every other reason runs here: the PC's own
/// failures (`client`, `inputs`, `lfs`) and those of the worktree, the desktop or a lost run.
const RETRIED: [FallbackReason; 7] = [
    FallbackReason::Full,
    FallbackReason::Busy,
    FallbackReason::Unreachable,
    FallbackReason::Version,
    FallbackReason::Parity,
    FallbackReason::Install,
    FallbackReason::Tools,
];

/// A line holding a failure marker: its number, counted from 1, and its text as printed.
type Marker = (usize, String);

/// The `take` of a slot of a pool, naming `parent_pid` when the gate runs under a parent whose slot did not cover it.
fn take_event(slots: Vec<String>, waited_seconds: f64, abandoned: Vec<String>, parent_pid: Option<u32>) -> JournalEvent {
    JournalEvent::Take {
        slots,
        waited_seconds,
        abandoned,
        machine: None,
        parent_pid,
    }
}

/// The words a run here runs, its worktree when it is already known, and why the command's `--no-build` was dropped
/// from them when it was: what plans a run here from the start ([`Run::local_words`]) and clears the worktree's
/// nothing-back record once the run passes ([`Run::clear_nothing_back`]).
struct HereWords {
    words: Vec<String>,
    worktree: Option<Worktree>,
    dropped: Option<String>,
}

impl HereWords {
    /// `words`, in the worktree and folder `remote`'s route placed the run in.
    fn routed(remote: &RemoteRun, words: Vec<String>) -> HereWords {
        HereWords {
            words,
            worktree: Some(Worktree {
                root: remote.root.clone(),
                prefix: remote.subfolder.clone(),
            }),
            dropped: None,
        }
    }
}

/// What a passing run does to its worktree's nothing-back record at the path each holds ([`Run::record_change`]).
enum RecordChange {
    /// Removes it.
    Clear(PathBuf),
    /// Adds the key of the project built to its `built` list, under the lock of the worktree's work-folder key.
    Add { path: PathBuf, folder: String, project: String },
}

/// Removes the nothing-back record at `path`; one that cannot be removed is said, one already gone is not.
fn remove_record(path: &Path, console: &mut Console) {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            console.err(&format!("gate: cannot remove {}: {error}", path.display()));
        }
        _ => {}
    }
}

/// Whether a routed run's `outcome` lets it clear the worktree's nothing-back record: it passed, as a run here must to
/// clear it, and its outputs are known to be brought back.
fn brought_everything_back(outcome: &RemoteOutcome) -> bool {
    outcome.status == 0 && outcome.brought_back == Some(true)
}

/// The routed run `remote`'s dotnet build of the command's `words` that builds in place ([`builds_in_place`]), passed
/// and brought everything back ([`brought_everything_back`]), whose target a restore here may need
/// ([`Run::restore_and_close`]), under the routed command's `ceiling` in whole seconds, counted from the routed call's
/// start `started`; `None` for any other run, a publish, a pack or a build whose output or assets go elsewhere among
/// them, and for words that are not all valid Unicode.
fn restore_candidate(remote: &RemoteRun, outcome: &RemoteOutcome, words: Option<&[String]>, ceiling: u64, started: Instant) -> Option<RoutedBuild> {
    let words = words?;
    (builds_in_place(words) && brought_everything_back(outcome)).then(|| RoutedBuild {
        words: words.to_vec(),
        root: PathBuf::from(&remote.root),
        ceiling: Duration::from_secs(ceiling),
        started,
    })
}

/// The restore `build` needs ([`restore::restore_words`], read against the working folder): one whose target, the
/// one the command names, has no `obj\project.assets.json` ([`no_build::named_without_assets`]). `None` for a
/// command that names no target or is no direct dotnet command, a target with its assets file, and a worktree whose
/// build files may move it ([`no_build::moves_intermediates`]); a working folder, build file or solution that cannot
/// be read is said.
fn restore_due(build: &RoutedBuild, console: &mut Console) -> Option<RestorePlan> {
    let folder = match std::env::current_dir() {
        Ok(folder) => folder,
        Err(error) => {
            console.err(&restore::cannot_run(&format!("cannot read the working directory: {error}")));
            return None;
        }
    };
    let plan = restore::restore_words(&build.words, &folder)?;
    let missing = no_build::moves_intermediates(&build.root).and_then(|moves| {
        if moves {
            Ok(None)
        } else {
            no_build::named_without_assets(plan.check(), &build.root)
        }
    });
    match missing {
        Ok(missing) => missing.map(|_| plan),
        Err(error) => {
            console.err(&format!("gate: {error}"));
            None
        }
    }
}

/// The lines a passing routed build says of the folders it left stale, none when it says none: the folders in the
/// worktree the run's judging globs stop at that the run did not bring back. Only a dotnet build the gate treats as one
/// ([`dotnet_builds`]) that ended on the builder has any, and the run's verdict decides whether it is said
/// ([`Run::verdict`]), so a failed build, one whose log reports a failure among them, and a fallback (whose build ran
/// here) say nothing. The judging globs are [`judging_globs`]; a file the run brought back is no stale folder's. A pair's siblings are judged by
/// the same globs, and named after the main's on the same line, relative to the main worktree; a sibling's output is
/// never brought back. A tree that cannot be listed says why it could not, on a line of its own ([`stale::stale_lines`]).
fn stale_lines(remote: &RemoteRun, outcome: &RemoteOutcome, words: Option<&[String]>) -> Vec<String> {
    let Some(words) = words else {
        return Vec::new();
    };
    if outcome.status != 0 || !dotnet_builds(words) {
        return Vec::new();
    }
    let judging = judging_globs(&remote.outputs);
    let brought_back = if brought_everything_back(outcome) {
        remote.outputs.clone()
    } else {
        Vec::new()
    };
    let siblings: Vec<(&str, &str)> = remote
        .siblings
        .iter()
        .map(|sibling| (sibling.name.as_str(), sibling.root.as_str()))
        .collect();
    stale::stale_lines(&remote.root, &siblings, &judging, &brought_back, |root, globs| {
        walk(Path::new(root), globs).map(|found| found.into_iter().map(|file| file.path).collect())
    })
}

/// The globs [`stale_lines`] judges a tree's folders by: the run's `outputs` but those that name a `.tmp` folder
/// ([`names_tmp`]), so no `.tmp` folder is ever judged stale, and the defaults beside them, even in a repo whose
/// `outputs` list is empty ([`DEFAULT_OUTPUTS`]).
fn judging_globs(outputs: &[String]) -> Vec<String> {
    let mut judging: Vec<String> = outputs.iter().filter(|glob| !names_tmp(glob)).cloned().collect();
    judging.extend(DEFAULT_OUTPUTS.iter().map(|glob| (*glob).to_owned()));
    judging
}

/// The command's words as text, `None` when one of them is not valid Unicode.
fn unicode_words(command: &[OsString]) -> Option<Vec<String>> {
    command.iter().map(|word| word.to_str().map(str::to_owned)).collect()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders must fail loudly")]
#[expect(
    clippy::disallowed_methods,
    reason = "a unit test starts cmd.exe directly, with no launch of its own open"
)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::io;
    use std::process::Command;

    use std::time::Duration;

    use gate_args::Slot;
    use gate_watchdog::{Cores, Hint};

    use super::{
        Ends, Judged, Pass, ROUTE_HINT_MIN_WALL, RouteJudged, Stopper, Timing, brought_everything_back, cores_hint_for, cores_line,
        drain_waiting_line, dropped_line, earlier, holder_claim, interrupted, judging_globs, key_wait_left, key_wait_line, kill, log_in_state_root,
        passed_line, records_after, route_hint_for, route_hint_line, routed_env, stop, takes_own_slot,
    };
    use crate::console::Console;
    use crate::escaped::{Found, Look, Reached, Recorded};
    use crate::nesting::{Nesting, Parent};
    use crate::remote::drain::Drain;
    use crate::remote::{RemoteOutcome, Split};
    use crate::watch::Watched;
    use gate_router::DEFAULT_OUTPUTS;
    use gate_state::v1::{Claim, ClaimKind};
    use jiff::Timestamp;
    use serde_json::{Map, Value};

    #[test]
    fn stale_folders_are_not_judged_by_a_glob_naming_tmp() {
        let outputs: Vec<String> = [".tmp/x.json", "bin/**", "**/.TMP/dumps/*.dmp", "src/.tmp/a.txt", "out/*.tmp"]
            .iter()
            .map(|glob| (*glob).to_owned())
            .collect();
        let mut expected = vec!["bin/**".to_owned(), "out/*.tmp".to_owned()];
        expected.extend(DEFAULT_OUTPUTS.iter().map(|glob| (*glob).to_owned()));
        assert_eq!(
            judging_globs(&outputs),
            expected,
            "every glob naming a .tmp folder is left out; the rest, and the defaults, judge"
        );
        assert_eq!(judging_globs(&[]), expected[2..], "the defaults judge a repo with no outputs");
    }

    #[test]
    fn a_drain_with_no_barrier_says_its_waiting_line_without_the_times() {
        let none = Drain {
            place: Some(3),
            raised: 0,
            lapses: 0,
        };
        assert_eq!(drain_waiting_line(none), "gate: waiting behind the builder drain; place 3 in line");
        assert_eq!(
            drain_waiting_line(Drain { place: None, ..none }),
            "gate: waiting behind the builder drain"
        );
        assert_eq!(
            drain_waiting_line(Drain {
                raised: 100,
                lapses: 1900,
                ..none
            }),
            "gate: waiting behind the builder drain (raised 00:01 UTC, lapses 00:31 UTC); place 3 in line"
        );
    }

    /// `count` caller pairs of distinct names.
    fn caller_pairs(count: usize) -> Vec<(String, String)> {
        (0..count).map(|index| (format!("V{index}"), "x".to_owned())).collect()
    }

    #[test]
    fn the_source_revision_pair_is_added_while_the_variables_stay_under_the_box_s_cap() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let under = caller_pairs(gate_args::MOST_ENV_NAMES - 1);
        let env = routed_env(&under, Some(sha));
        assert_eq!(env.len(), gate_args::MOST_ENV_NAMES, "the pair fills the last name");
        assert_eq!(env.last(), Some(&("SourceRevisionId".to_owned(), sha.to_owned())));

        let full = caller_pairs(gate_args::MOST_ENV_NAMES);
        assert_eq!(routed_env(&full, Some(sha)), full, "a full list goes unpinned");

        let big = vec![("BIG".to_owned(), "x".repeat(gate_args::MOST_ENV_BYTES - "BIG".len()))];
        assert_eq!(routed_env(&big, Some(sha)), big, "a list at the byte cap goes unpinned");
    }

    /// A remote claim of the work folder of `key`, started at `started`, whose usual run time is `p50` when one.
    fn claim_of(key: &str, started: Timestamp, p50: Option<f64>) -> Claim {
        let expected: Option<Map<String, Value>> = p50.map(|p50| [("p50".to_owned(), Value::from(p50))].into_iter().collect());
        Claim {
            kind: ClaimKind::Remote,
            index: 0,
            pid: 1,
            process_start: started,
            started,
            cwd: String::new(),
            command: String::new(),
            log: String::new(),
            timeout_seconds: 900.0,
            remote: Some(true),
            host: Some("builder".to_owned()),
            key: Some(key.to_owned()),
            reason: None,
            expected,
        }
    }

    #[test]
    fn the_key_wait_names_the_latest_claim_of_the_work_folder() {
        let base = Timestamp::from_second(1_792_000_000).expect("a timestamp");
        let later = Timestamp::from_second(base.as_second() + 5).expect("a timestamp");
        let claims = [
            claim_of("k", base, Some(600.0)),
            claim_of("k", later, Some(600.0)),
            claim_of("other", base, Some(600.0)),
        ];

        let found = holder_claim(&claims, "k").expect("a claim names the key");

        assert_eq!(found.started, later, "the latest claim of the key, not the first the folder lists");
        assert!(holder_claim(&claims, "none").is_none(), "no claim names the key");
    }

    #[test]
    fn the_key_wait_time_left_is_the_holders_usual_time_by_the_figure_rule() {
        let base = Timestamp::from_second(1_792_000_000).expect("a timestamp");
        let at = |seconds: i64| Timestamp::from_second(base.as_second() + seconds).expect("a timestamp");
        let claim = |p50: Option<f64>| claim_of("k", base, p50);

        assert!(key_wait_left(&claim(None), at(0)).is_none(), "a claim with no usual run time names none");
        assert!(
            key_wait_left(&claim(Some(10.0)), at(20)).is_none(),
            "at or past the usual, the line leaves the time left out"
        );
        assert_eq!(
            key_wait_left(&claim(Some(10.4)), at(10)),
            Some("1 s".to_owned()),
            "0.4 s left is never 0 s"
        );
        assert_eq!(
            key_wait_left(&claim(Some(100.5)), at(11)),
            Some("2 min".to_owned()),
            "89.5 s left is whole minutes"
        );
        assert_eq!(key_wait_left(&claim(Some(610.0)), at(10)), Some("10 min".to_owned()), "600 s left");
    }

    #[test]
    fn the_key_wait_line_says_the_time_left_only_when_there_is_one() {
        let line = "gate: waiting for this worktree's work folder on builder, which another of its runs holds";

        assert_eq!(key_wait_line("builder", None), line);
        assert_eq!(key_wait_line("builder", Some("10 min".to_owned())), format!("{line} (about 10 min left)"));
    }

    #[test]
    fn only_a_passing_routed_run_that_brought_its_outputs_back_clears_the_record() {
        let outcome = |status, brought_back| RemoteOutcome {
            status,
            brought_back,
            ..RemoteOutcome::default()
        };

        assert!(
            brought_everything_back(&outcome(0, Some(true))),
            "a passing run that brought everything back"
        );
        assert!(
            !brought_everything_back(&outcome(1, Some(true))),
            "a run whose status was forced to fail after a complete install"
        );
        assert!(!brought_everything_back(&outcome(0, Some(false))), "outputs not brought back");
        assert!(!brought_everything_back(&outcome(0, None)), "a client that cannot tell");
    }

    /// Ends that note the pids of the processes outside the job each terminate is given, whose look finds nothing more
    /// than it is given, and whose command has exited 0.
    struct Noted<'a>(&'a RefCell<Vec<Vec<u32>>>);

    impl Ends for Noted<'_> {
        fn look(&self, earlier: Found) -> Found {
            earlier
        }

        fn terminate(&self, _status: u32, _wait: Duration, earlier: Found) -> io::Result<Reached> {
            self.0.borrow_mut().push(earlier.pids());
            Ok(Reached {
                ended: Vec::new(),
                problems: Vec::new(),
            })
        }

        fn wait(&self, _timeout: Duration) -> io::Result<Option<u32>> {
            Ok(Some(0))
        }
    }

    #[test]
    fn a_stall_ceiling_or_backstop_kill_passes_the_records_to_terminate() {
        let (recorded, terminated) = (recorded(), RefCell::new(Vec::new()));

        kill(&Noted(&terminated), Some(&recorded), 3, &mut Console::new());

        assert_eq!(terminated.into_inner(), vec![vec![2, 3]]);
    }

    #[test]
    fn a_ctrl_c_passes_the_records_to_terminate() {
        let (recorded, terminated) = (recorded(), RefCell::new(Vec::new()));

        let exited = interrupted(&Noted(&terminated), Some(&recorded), &mut Console::new());

        assert_eq!(exited, 0, "the command's own status");
        assert_eq!(terminated.into_inner(), vec![vec![2, 3]]);
    }

    #[test]
    fn a_gate_failure_passes_the_records_to_terminate() {
        let (recorded, terminated) = (recorded(), RefCell::new(Vec::new()));

        stop(&Noted(&terminated), Some(&recorded), &mut Console::new());

        assert_eq!(terminated.into_inner(), vec![vec![2, 3]]);
    }

    #[test]
    fn the_stopper_on_a_panic_passes_the_records_to_terminate() {
        let (recorded, terminated) = (recorded(), RefCell::new(Vec::new()));

        drop(Stopper {
            ends: Noted(&terminated),
            recorded: &recorded,
            armed: true,
        });

        assert_eq!(terminated.into_inner(), vec![vec![2, 3]]);
    }

    #[test]
    fn a_gate_failure_after_the_command_s_own_exit_passes_no_records() {
        let (recorded, terminated) = (recorded(), RefCell::new(Vec::new()));
        let killed = Watched::Killed {
            line: String::new(),
            hint: None,
            processes: 1,
            diagnostics: Vec::new(),
        };

        stop(
            &Noted(&terminated),
            records_after(&Watched::Exited(0), Some(&recorded)),
            &mut Console::new(),
        );

        assert_eq!(
            terminated.into_inner(),
            vec![Vec::<u32>::new()],
            "a normal exit leaves what it leaves running"
        );
        assert!(records_after(&killed, Some(&recorded)).is_some(), "a kill keeps them");
        assert!(records_after(&Watched::Interrupted, Some(&recorded)).is_some(), "a Ctrl+C keeps them");
    }

    #[test]
    fn only_an_end_of_the_run_that_passes_the_records_looks_under_the_job_s_ended_members() {
        let recorded = recorded();
        let killed = Watched::Killed {
            line: String::new(),
            hint: None,
            processes: 1,
            diagnostics: Vec::new(),
        };

        let after_exit = earlier(records_after(&Watched::Exited(0), Some(&recorded)));
        let after_kill = earlier(records_after(&killed, Some(&recorded)));
        let after_interrupt = earlier(records_after(&Watched::Interrupted, Some(&recorded)));

        assert!(!after_exit.reaches_departed(), "a gate failure after the command's own exit leaves them");
        assert!(after_kill.reaches_departed(), "a kill reaches them");
        assert!(after_interrupt.reaches_departed(), "a Ctrl+C reaches them");
    }

    /// What a watch recorded of member 1's process 2 outside the job, which started 3.
    fn recorded() -> RefCell<Recorded> {
        let process = |pid: u32, parent_pid: u32, second: i64| proc_table::ProcessInfo {
            pid,
            parent_pid,
            name: "pwsh.exe".to_owned(),
            command_line: None,
            created: jiff::Timestamp::from_second(1_790_000_000 + second).ok(),
        };
        let tree = proc_table::ProcessTree::new(vec![process(1, 0, 0), process(2, 1, 1), process(3, 2, 2)]);
        let look = Look {
            before: vec![1],
            after: vec![1],
            since: jiff::Timestamp::UNIX_EPOCH,
            tree: &tree,
            departed: Vec::new(),
        };
        let recorded = RefCell::new(Recorded::default());
        recorded.borrow_mut().record(look);
        recorded
    }

    #[test]
    fn a_kill_a_gate_failure_and_a_panic_reach_what_the_watch_recorded_parents_first() {
        let recorded = recorded();

        assert_eq!(earlier(Some(&recorded)).pids(), vec![2, 3]);
        assert_eq!(earlier(None).pids(), Vec::<u32>::new(), "no record is nothing");
        let writing = recorded.borrow_mut();
        assert_eq!(
            earlier(Some(&recorded)).pids(),
            Vec::<u32>::new(),
            "a record a sample is still writing is nothing, never a panic"
        );
        drop(writing);
    }

    #[test]
    fn only_a_run_neither_covered_nor_lent_takes_its_own_slot() {
        let parent = Parent {
            held: Slot::Light,
            pid: 4242,
            start_ms: 1,
            marker: "light:4242:1".to_owned(),
            unjudged: None,
            stand_in: false,
            commit: false,
        };
        let rows: [(&str, Nesting, bool, bool); 7] = [
            ("covered by its parent's slot", Nesting::Covered(parent.clone()), false, false),
            ("inside a box run, which takes no slot", Nesting::InBox, false, false),
            ("lent its light parent's slot", Nesting::Under(parent.clone()), true, false),
            ("under a parent whose slot does not cover it", Nesting::Under(parent), false, true),
            ("under no parent, here after a fallback or from the start", Nesting::Own, false, true),
            ("with an ignored marker", Nesting::Malformed("x".to_owned()), false, true),
            (
                "under a marker it does not run inside the command of",
                Nesting::Unbound {
                    marker: "light:4242:1".to_owned(),
                    pid: 4242,
                },
                false,
                true,
            ),
        ];
        for (name, nesting, lent, want) in rows {
            assert_eq!(takes_own_slot(&nesting, lent), want, "{name}");
        }
    }

    #[test]
    fn a_dropped_no_build_inside_a_box_run_builds_on_no_slot() {
        let reason = r"app/App.csproj has no obj\project.assets.json here";
        let parent = Parent {
            held: Slot::Light,
            pid: 4242,
            start_ms: 1,
            marker: "light:4242:1".to_owned(),
            unjudged: None,
            stand_in: false,
            commit: false,
        };
        assert_eq!(
            dropped_line(reason, None, true, Slot::Heavy),
            format!("gate: dropped --no-build: {reason}; building on no slot")
        );
        assert_eq!(
            dropped_line(reason, None, false, Slot::Heavy),
            format!("gate: dropped --no-build: {reason}; building on a heavy slot")
        );
        assert_eq!(
            dropped_line(reason, Some(&parent), false, Slot::Heavy),
            format!("gate: dropped --no-build: {reason}; building on the parent gate's light slot")
        );
    }

    const HOT: Cores = Cores {
        avg: 3.0,
        peak: 4.0,
        free: 12.0,
    };
    const COLD: Cores = Cores {
        avg: 0.5,
        peak: 1.0,
        free: 12.0,
    };

    /// A passed run here on its own `slot` slot for `wall_s` seconds, `paused_s` of them paused, with `cores`.
    fn judged(slot: Slot, wall_s: u64, paused_s: u64, cores: Cores) -> Judged {
        Judged {
            passed: true,
            own_slot: true,
            slot,
            commit: false,
            timing: Some(Timing {
                wall: Duration::from_secs(wall_s),
                adjusted: Some(1.0),
                paused: Duration::from_secs(paused_s),
                cores: Some(cores),
                nested_took_slot: false,
            }),
        }
    }

    #[test]
    fn the_cores_line_is_said_only_for_a_misfit_of_a_judged_run() {
        let hot = judged(Slot::Light, 12, 0, HOT);
        let nested = Judged {
            timing: hot.timing.map(|timing| Timing {
                nested_took_slot: true,
                ..timing
            }),
            ..hot
        };
        let commit = |judged: Judged| Judged { commit: true, ..judged };
        let rows: [(&str, Judged, Option<&str>); 15] = [
            ("failed", Judged { passed: false, ..hot }, None),
            ("routed", Judged { timing: None, ..hot }, None),
            ("not on its own slot", Judged { own_slot: false, ..hot }, None),
            ("critical", judged(Slot::Critical, 12, 0, HOT), None),
            ("under 10 s unpaused", judged(Slot::Light, 12, 3, HOT), None),
            ("a gate under it took a slot", nested, None),
            ("hot", hot, Some("on a light slot")),
            ("cold", judged(Slot::Heavy, 10, 0, COLD), Some("on a heavy slot")),
            ("fitted", judged(Slot::Heavy, 12, 0, HOT), None),
            ("no slot, busy", judged(Slot::None, 12, 0, HOT), Some("on no slot")),
            ("no slot, quiet", judged(Slot::None, 12, 0, COLD), None),
            ("a hot commit", commit(hot), Some("the commit's hooks used 3.0 cores")),
            (
                "a cold commit",
                commit(judged(Slot::Heavy, 10, 0, COLD)),
                Some("hooks that keep under 1.5 busy fit on --slot light"),
            ),
            ("a fitted commit", commit(judged(Slot::Heavy, 12, 0, HOT)), None),
            ("a commit under 10 s unpaused", commit(judged(Slot::Light, 12, 3, HOT)), None),
        ];
        for (name, judged, want) in rows {
            let line = cores_hint_for(judged);
            match want {
                Some(part) => assert!(line.as_deref().is_some_and(|line| line.contains(part)), "{name}: {line:?}"),
                None => assert_eq!(line, None, "{name}"),
            }
        }
    }

    #[test]
    fn the_cores_line_says_each_misfit_with_its_rounding() {
        let hot = Cores {
            avg: 7.44,
            peak: 14.6,
            free: 9.26,
        };
        assert_eq!(
            cores_line(hot, Hint::Hot, false),
            "gate: used 7.4 cores on average (peak 15, 9.3 free) on a light slot; work that keeps more than 2 busy belongs on heavy"
        );
        assert_eq!(
            cores_line(hot, Hint::Hot, true),
            concat!(
                "gate: the commit's hooks used 7.4 cores on average (peak 15, 9.3 free) on a light slot; ",
                "hooks that keep more than 2 busy belong on --slot heavy"
            )
        );
        let cold = Cores {
            avg: 0.04,
            peak: 0.5,
            free: 12.0,
        };
        assert_eq!(
            cores_line(cold, Hint::Cold, false),
            "gate: used 0.0 cores on average (peak 0, 12.0 free) on a heavy slot; work that keeps under 1.5 busy fits on light"
        );
        assert_eq!(
            cores_line(cold, Hint::Cold, true),
            concat!(
                "gate: the commit's hooks used 0.0 cores on average (peak 0, 12.0 free) on a heavy slot; ",
                "hooks that keep under 1.5 busy fit on --slot light"
            )
        );
        let busy = Cores {
            avg: 1.54,
            peak: 2.5,
            free: 3.0,
        };
        assert_eq!(
            cores_line(busy, Hint::Busy, false),
            "gate: used 1.5 cores on average (peak 2) on no slot; work that keeps 1.5 or more busy belongs on light or heavy"
        );
    }

    /// A `Timing` of `wall_s` wall seconds, `paused_s` of them paused, with no cores sampled.
    fn timing(wall_s: f64, paused_s: f64) -> Timing {
        Timing {
            wall: Duration::from_secs_f64(wall_s),
            adjusted: None,
            paused: Duration::from_secs_f64(paused_s),
            cores: None,
            nested_took_slot: false,
        }
    }

    #[test]
    fn the_route_hint_line_says_the_whole_seconds() {
        assert_eq!(
            route_hint_line(Duration::from_secs_f64(42.9)),
            "gate: ran 42 s on a light slot; on heavy, remote.json would send this command to a builder"
        );
    }

    /// A run on its own `slot` slot for `wall_s` seconds, `paused_s` of them paused, with `floor` as the route hint's
    /// floor.
    fn route_judged(slot: Slot, own_slot: bool, wall_s: f64, paused_s: f64, floor: Duration) -> RouteJudged {
        RouteJudged {
            own_slot,
            slot,
            timing: Some(timing(wall_s, paused_s)),
            floor,
        }
    }

    /// Asserts that `route_hint_for` says nothing for `judged` and never asks `would_route`.
    fn refused_without_asking(judged: RouteJudged) {
        let asked = Cell::new(false);
        let line = route_hint_for(judged, || {
            asked.set(true);
            true
        });
        assert_eq!(line, None);
        assert!(!asked.get(), "the route is asked of a run it must not judge");
    }

    #[test]
    fn a_light_pass_under_the_floor_gets_no_route_hint() {
        refused_without_asking(route_judged(Slot::Light, true, 29.0, 0.0, ROUTE_HINT_MIN_WALL));
    }

    #[test]
    fn a_light_pass_whose_paused_time_leaves_it_under_the_floor_gets_no_route_hint() {
        refused_without_asking(route_judged(Slot::Light, true, 40.0, 15.0, ROUTE_HINT_MIN_WALL));
    }

    #[test]
    fn a_run_not_on_its_own_slot_gets_no_route_hint() {
        refused_without_asking(route_judged(Slot::Light, false, 60.0, 0.0, ROUTE_HINT_MIN_WALL));
    }

    #[test]
    fn a_heavy_or_no_slot_run_gets_no_route_hint() {
        for slot in [Slot::Heavy, Slot::Critical, Slot::None] {
            refused_without_asking(route_judged(slot, true, 60.0, 0.0, ROUTE_HINT_MIN_WALL));
        }
    }

    #[test]
    fn a_light_pass_of_exactly_the_route_hint_floor_is_hinted() {
        let line = route_hint_for(route_judged(Slot::Light, true, 30.0, 0.0, ROUTE_HINT_MIN_WALL), || true);
        assert_eq!(
            line.as_deref(),
            Some("gate: ran 30 s on a light slot; on heavy, remote.json would send this command to a builder")
        );
    }

    /// A `Pass::Here` of a run of `wall_s` wall seconds and `adjusted` load-adjusted seconds, `None` once the sampler
    /// has failed.
    fn here(wall_s: f64, adjusted: Option<f64>) -> Pass<'static> {
        Pass::Here(Timing {
            wall: Duration::from_secs_f64(wall_s),
            adjusted,
            paused: Duration::ZERO,
            cores: None,
            nested_took_slot: false,
        })
    }

    #[test]
    fn a_passed_line_says_a_run_here_ran_past_a_ceiling_its_clock_reached() {
        let rows: [(&str, Pass<'_>, &str); 5] = [
            (
                "so far past the ceiling the kill could not have missed it",
                here(41.0, Some(41.0)),
                "gate: passed in 41 s (load-adjusted 41 s, past its ceiling of 1 s). Full output: out.log",
            ),
            (
                "at the ceiling, which the kill reaches at once",
                here(41.0, Some(1.0)),
                "gate: passed in 41 s (load-adjusted 1 s, past its ceiling of 1 s). Full output: out.log",
            ),
            (
                "under the ceiling",
                here(41.0, Some(0.4)),
                "gate: passed in 41 s (load-adjusted 0 s, ceiling 1 s). Full output: out.log",
            ),
            (
                "the sampler failed with the wall time past the ceiling",
                here(41.0, None),
                "gate: passed in 41 s (load-adjusted unknown: the sampler failed, past its ceiling of 1 s). Full output: out.log",
            ),
            (
                "the sampler failed with the wall time under the ceiling",
                here(0.4, None),
                "gate: passed in 0 s (load-adjusted unknown: the sampler failed, ceiling 1 s). Full output: out.log",
            ),
        ];
        for (name, pass, want) in rows {
            assert_eq!(passed_line(pass, 1, "out.log"), want, "{name}");
        }
    }

    #[test]
    fn a_passed_line_on_a_builder_splits_the_command_s_time_from_each_transfer_it_made() {
        let second = Duration::from_secs(1);
        let pushed = Split {
            command: Duration::from_millis(76_400),
            push: 224 * second,
            inputs: None,
            lfs: None,
            sent_build: None,
            sync_back: None,
            outputs: None,
        };
        let rows = [
            (
                "the push alone",
                pushed,
                Some(70.4),
                "gate: passed in 301 s on builder (load-adjusted 70 s, ceiling 300 s; command 76 s, push 224 s). Full output: out.log",
            ),
            (
                "the outputs fetched",
                Split {
                    outputs: Some(Duration::from_millis(1_600)),
                    ..pushed
                },
                Some(70.4),
                "gate: passed in 301 s on builder (load-adjusted 70 s, ceiling 300 s; command 76 s, push 224 s, outputs 2 s). Full output: out.log",
            ),
            (
                "the inputs and the LFS content sent, in the order they ran",
                Split {
                    inputs: Some(2 * second),
                    lfs: Some(Duration::from_millis(4_400)),
                    ..pushed
                },
                Some(70.4),
                "gate: passed in 301 s on builder (load-adjusted 70 s, ceiling 300 s; command 76 s, push 224 s, inputs 2 s, lfs 4 s). \
                 Full output: out.log",
            ),
            (
                "the inputs and the PC's build sent, in the order they ran",
                Split {
                    inputs: Some(2 * second),
                    sent_build: Some(5 * second),
                    ..pushed
                },
                Some(70.4),
                "gate: passed in 301 s on builder (load-adjusted 70 s, ceiling 300 s; command 76 s, push 224 s, inputs 2 s, sent build 5 s). \
                 Full output: out.log",
            ),
            (
                "the sync-back and the outputs fetched, in the order they ran",
                Split {
                    sync_back: Some(3 * second),
                    outputs: Some(Duration::ZERO),
                    ..pushed
                },
                Some(70.4),
                "gate: passed in 301 s on builder (load-adjusted 70 s, ceiling 300 s; command 76 s, push 224 s, sync-back 3 s, outputs 0 s). \
                 Full output: out.log",
            ),
            (
                "the box's clock at the ceiling",
                pushed,
                Some(300.0),
                "gate: passed in 301 s on builder (load-adjusted 300 s, past its ceiling of 300 s; command 76 s, push 224 s). Full output: out.log",
            ),
            (
                "no figure from the box, the command's wall time under the ceiling",
                pushed,
                None,
                "gate: passed in 301 s on builder (load-adjusted unknown: the sampler failed, ceiling 300 s; command 76 s, push 224 s). \
                 Full output: out.log",
            ),
            (
                "no figure from the box, the command's wall time past the ceiling",
                Split {
                    command: 300 * second,
                    ..pushed
                },
                None,
                "gate: passed in 301 s on builder (load-adjusted unknown: the sampler failed, past its ceiling of 300 s; command 300 s, push \
                 224 s). Full output: out.log",
            ),
        ];
        for (name, split, adjusted, want) in rows {
            assert_eq!(
                passed_line(Pass::On("builder", 301 * second, split, adjusted), 300, "out.log"),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn a_log_reached_through_a_junction_into_the_state_root_is_inside_it() {
        let folder = crate::test_support::temp_dir().join(format!("gate-run-junction-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        let state = folder.join("state");
        fs::create_dir_all(&state).expect("the state folder can be made");
        let link = folder.join("link");
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&state)
            .output()
            .expect("cmd.exe runs");
        assert!(made.status.success(), "a junction needs no privilege: {made:?}");

        assert!(log_in_state_root(&link.join("x.log"), &state), "through the junction");
        assert!(log_in_state_root(&link.join("new").join("x.log"), &state), "into a folder not made yet");
        assert!(log_in_state_root(&folder.join("STATE").join("x.log"), &state), "in another case");
        assert!(
            !log_in_state_root(&folder.join("stateful.log"), &state),
            "a name that only starts the same"
        );
        assert!(!log_in_state_root(&folder.join("x.log"), &state), "beside the state folder");
        fs::remove_dir_all(&folder).expect("the test folder can be removed");
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test timestamp that does not parse must fail loudly")]
mod end_tests {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::io;
    use std::time::Duration;

    use jiff::Timestamp;
    use proc_table::{ProcessInfo, ProcessTree};

    use gate_state::{KernelObject, StateRoot};

    use super::{END_PASSES, EndPass, Endable, Origin, by_start, end_and_wait, in_passes, job_under_roots, opened_member, spared_gates};
    use crate::escaped::EMPTY_SNAPSHOT;

    /// `ERROR_ACCESS_DENIED`, as a refused terminate reports it.
    const ACCESS_DENIED: i32 = 5;

    /// A process a pass ends, which records each call made to it in `calls`: it refuses the terminate when `refuses`,
    /// and its waits see it ended when `ends`.
    struct Fake<'a> {
        pid: u32,
        refuses: bool,
        ends: bool,
        calls: &'a RefCell<Vec<String>>,
    }

    impl Endable for Fake<'_> {
        fn terminate(&self, _status: u32) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("terminate {}", self.pid));
            if self.refuses {
                Err(io::Error::from_raw_os_error(ACCESS_DENIED))
            } else {
                Ok(())
            }
        }

        fn wait(&self, _timeout: Duration) -> io::Result<bool> {
            self.calls.borrow_mut().push(format!("wait {}", self.pid));
            Ok(self.ends)
        }
    }

    fn at(second: i64) -> Timestamp {
        Timestamp::from_second(1_791_900_000 + second).expect("a test timestamp is in range")
    }

    fn listed(pid: u32, parent_pid: u32, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            name: name.to_owned(),
            command_line: None,
            created: Some(at(i64::from(pid))),
        }
    }

    #[expect(clippy::unnecessary_wraps, reason = "every pass a case fakes is one that ran")]
    fn pass(left: usize, problems: &[&str]) -> Result<EndPass, String> {
        Ok(EndPass {
            left,
            problems: problems.iter().map(|&problem| problem.to_owned()).collect(),
        })
    }

    #[test]
    fn an_empty_snapshot_spares_nothing_and_ends_nothing() {
        let mut problems = Vec::new();

        let spared = spared_gates(&[2, 3], &ProcessTree::new(Vec::new()), |_| Ok(None), &mut problems);

        assert_eq!(
            spared,
            Err(EMPTY_SNAPSHOT.to_owned()),
            "a pass with no snapshot fails before it ends anything"
        );
    }

    #[test]
    fn a_gate_is_spared_with_what_runs_under_it_and_in_its_job() {
        let tree = ProcessTree::new(vec![
            listed(1, 0, "fake-tool.exe"),
            listed(2, 1, "gate.exe"),
            listed(3, 2, "fake-tool.exe"),
            listed(4, 1, "VBCSCompiler.exe"),
        ]);
        let mut problems = Vec::new();

        let spared = spared_gates(&[2, 3, 4, 9], &tree, |gate| Ok((gate == 2).then(|| vec![3, 9])), &mut problems);

        assert_eq!(
            spared,
            Ok(HashSet::from([2, 3, 9])),
            "the gate, its child, and its job's orphan; not the server"
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_gate_s_launcher_in_the_job_is_spared_with_it() {
        let tree = ProcessTree::new(vec![
            listed(1, 0, "fake-tool.exe"),
            listed(2, 1, "cmd.exe"),
            listed(3, 2, "gate.exe"),
            listed(4, 1, "fake-tool.exe"),
        ]);
        let mut problems = Vec::new();

        let spared = spared_gates(&[2, 3, 4], &tree, |_| Ok(None), &mut problems);

        assert_eq!(
            spared,
            Ok(HashSet::from([2, 3])),
            "the gate and its launcher in the job; not the command outside the job above them, nor the sibling leftover"
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// What [`job_under_roots`] finds for the gate 7 under `root` among `jobs`, each a job name with the processes in
    /// it, or `None` for a name another object holds, which fails to open; and the names it looked up, in order.
    fn looked_up(root: &StateRoot, jobs: &[(&str, Option<Vec<u32>>)]) -> (Result<Option<Vec<u32>>, String>, Vec<String>) {
        let looked = RefCell::new(Vec::new());
        let found = job_under_roots(root, 7, |name| {
            looked.borrow_mut().push(name.to_owned());
            match jobs.iter().find(|(held, _)| *held == name) {
                None => Ok(None),
                Some((_, Some(inside))) => Ok(Some(inside.clone())),
                Some((_, None)) => Err(io::Error::other("the name is held by another object")),
            }
        });
        (found.map_err(|error| error.to_string()), looked.into_inner())
    }

    #[test]
    fn a_spared_gate_s_job_is_found_under_the_default_root() {
        let default_path = r"C:\Users\me\AppData\Local\gate\v1";
        let custom = StateRoot::new(r"D:\state", Some(default_path));
        let default = StateRoot::new(default_path, Some(default_path));
        let own = custom.object_name(KernelObject::Job { gate_pid: 7 });
        let under_default = r"Local\gate-job-7";

        assert_eq!(
            looked_up(&custom, &[(under_default, Some(vec![8]))]),
            (Ok(Some(vec![8])), vec![own.clone(), under_default.to_owned()]),
            "a nested gate under the default root has its job found"
        );
        assert_eq!(
            looked_up(&default, &[]),
            (Ok(None), vec![under_default.to_owned()]),
            "an ender under the default root looks once"
        );
        assert_eq!(
            looked_up(&custom, &[(&own, Some(vec![9])), (under_default, Some(vec![8]))]),
            (Ok(Some(vec![9])), vec![own.clone()]),
            "a job found under the ender's root is not looked for again"
        );
        assert_eq!(
            looked_up(&custom, &[(&own, None)]),
            (Err("the name is held by another object".to_owned()), vec![own.clone()]),
            "a failure under the ender's root is said, and the default root is not looked at"
        );
    }

    #[test]
    fn a_console_host_in_the_job_is_never_ended() {
        let tree = ProcessTree::new(vec![
            listed(1, 0, "fake-tool.exe"),
            listed(2, 1, "ConHost.exe"),
            listed(3, 1, "conhost.exe"),
            listed(4, 1, "VBCSCompiler.exe"),
        ]);
        let mut problems = Vec::new();

        let spared = spared_gates(
            &[2, 3, 4],
            &tree,
            |pid| Err(io::Error::other(format!("the job of {pid} was looked for"))),
            &mut problems,
        );

        assert_eq!(spared, Ok(HashSet::from([2, 3])), "each console host, in any letter case; not the server");
        assert!(
            problems.is_empty(),
            "a console host is no gate, so no job of its own is looked for: {problems:?}"
        );
    }

    #[test]
    fn a_console_host_name_is_compared_ascii_case_insensitively() {
        let tree = ProcessTree::new(vec![
            listed(1, 0, "fake-tool.exe"),
            listed(2, 1, "CONHOST.EXE"),
            listed(3, 1, "conho\u{17F}t.exe"),
            listed(4, 1, "GATE.EXE"),
        ]);
        let mut problems = Vec::new();

        let spared = spared_gates(&[2, 3, 4], &tree, |_| Ok(None), &mut problems);

        assert_eq!(
            spared,
            Ok(HashSet::from([2, 4])),
            "the console host and the gate in ASCII capitals; not a long s standing in for an s"
        );
    }

    #[test]
    fn a_refused_terminate_does_not_delay_the_others() {
        let calls = RefCell::new(Vec::new());
        let fake = |pid, refuses, ends| Fake {
            pid,
            refuses,
            ends,
            calls: &calls,
        };
        let doomed = [(7, fake(7, true, false)), (8, fake(8, false, true)), (9, fake(9, true, true))];
        let mut problems = Vec::new();

        end_and_wait(&doomed, &mut problems);

        assert_eq!(
            calls.into_inner(),
            ["terminate 7", "terminate 8", "terminate 9", "wait 7", "wait 8", "wait 9"],
            "every member is terminated before any is waited for"
        );
        assert_eq!(problems.len(), 1, "only the refusal of one still running is a problem: {problems:?}");
        assert!(
            problems.first().is_some_and(|problem| problem.starts_with("TerminateProcess(7) failed")),
            "{problems:?}"
        );
    }

    /// The member `pid` whose own handle reads its start as second `second` and its parent as `parent`.
    fn member(pid: u32, second: i64, parent: u32) -> ((u32, ()), Origin) {
        ((pid, ()), Origin { start: at(second), parent })
    }

    #[test]
    fn a_member_the_listing_lacks_is_passed_over() {
        let tree = ProcessTree::new(vec![listed(2, 0, "fake-tool.exe"), listed(3, 0, "fake-tool.exe")]);
        let starts = vec![member(2, 2, 0), member(3, 3, 0), member(4, 21, 0)];

        let found = by_start(starts, &tree);

        assert_eq!(found.ended, [(2, ()), (3, ())], "a pid the snapshot does not list is left running");
        assert_eq!(found.passed_over, 1);
    }

    #[test]
    fn a_member_whose_start_differs_from_the_listing_is_passed_over() {
        let mut unread = listed(4, 0, "fake-tool.exe");
        unread.created = None;
        let tree = ProcessTree::new(vec![listed(2, 0, "fake-tool.exe"), listed(3, 0, "fake-tool.exe"), unread]);
        let starts = vec![member(2, 2, 0), member(3, 20, 0), member(4, 4, 0)];

        let found = by_start(starts, &tree);

        assert_eq!(
            found.ended,
            [(2, ())],
            "a pid taken in the snapshot's tick by a process with another start is left running, as is one whose listed \
             start is unknown"
        );
        assert_eq!(found.passed_over, 2);
    }

    #[test]
    fn a_member_whose_parent_differs_from_the_listing_is_passed_over() {
        let tree = ProcessTree::new(vec![listed(2, 1, "fake-tool.exe"), listed(3, 1, "fake-tool.exe")]);
        let starts = vec![member(2, 2, 1), member(3, 3, 9)];

        let found = by_start(starts, &tree);

        assert_eq!(
            found.ended,
            [(2, ())],
            "a pid taken by a process with the listed start but another parent is left running"
        );
        assert_eq!(found.passed_over, 1);
    }

    #[test]
    fn a_pass_that_passes_over_a_new_member_is_followed_by_another() {
        let calls = RefCell::new(Vec::new());
        let tree = ProcessTree::new(vec![listed(2, 0, "fake-tool.exe")]);
        let mut listings = [vec![member(4, 21, 0)], vec![]].into_iter();

        let said = in_passes(|ending| {
            calls.borrow_mut().push(ending);
            let found = by_start(listings.next().expect("no more passes than planned"), &tree);
            pass(found.left(), &[])
        });

        assert_eq!(said, None, "the next pass found nothing left");
        assert_eq!(
            calls.into_inner(),
            [true, true],
            "a pass that only passed over a new member is followed by another"
        );
    }

    #[test]
    fn passes_at_the_cap_say_the_first_problem_with_the_count() {
        let said = in_passes(|_| pass(3, &["OpenProcess(7) failed", "OpenProcess(8) failed"]));

        assert_eq!(
            said,
            Some(format!(
                "3 of its processes still ran after {END_PASSES} passes; first: OpenProcess(7) failed"
            ))
        );
    }

    #[test]
    fn passes_go_on_while_each_finds_something_left_to_end() {
        let calls = RefCell::new(Vec::new());
        let mut left = [2, 1, 0].into_iter();

        let said = in_passes(|ending| {
            calls.borrow_mut().push(ending);
            pass(left.next().expect("no more passes than planned"), &[])
        });

        assert_eq!(said, None, "everything went, so nothing is said");
        assert_eq!(calls.into_inner(), [true, true, true]);
    }

    #[test]
    fn passes_stop_at_the_cap_and_say_how_many_processes_are_left() {
        let calls = RefCell::new(Vec::new());

        let said = in_passes(|ending| {
            calls.borrow_mut().push(ending);
            pass(3, &[])
        });

        assert_eq!(said, Some(format!("3 of its processes still ran after {END_PASSES} passes")));
        let mut expected = vec![true; END_PASSES];
        expected.push(false);
        assert_eq!(calls.into_inner(), expected, "five passes end, the last only counts");
    }

    #[test]
    fn a_pass_that_cannot_run_or_end_a_process_is_said_once() {
        let failed = in_passes(|_| Err::<EndPass, _>("the snapshot failed".to_owned()));
        let mut passes = [pass(1, &[]), pass(0, &["OpenProcess(7) failed", "OpenProcess(8) failed"])].into_iter();
        let unopened = in_passes(|_| passes.next().expect("two passes"));

        assert_eq!(failed, Some("the snapshot failed".to_owned()));
        assert_eq!(unopened, Some("OpenProcess(7) failed".to_owned()), "the first problem, once");
    }

    #[test]
    fn a_refused_terminate_that_lasts_to_the_cap_is_said_with_the_count() {
        let refusing = |ending: bool| {
            let refused: &[&str] = if ending { &["TerminateProcess(7) failed"] } else { &[] };
            pass(1, refused)
        };
        let mut left = [1; END_PASSES].into_iter().chain([0]);

        let lasting = in_passes(refusing);
        let gone_by_the_count = in_passes(|ending| {
            refusing(ending).map(|found| EndPass {
                left: left.next().expect("no more passes than planned"),
                ..found
            })
        });

        assert_eq!(
            lasting,
            Some(format!(
                "1 of its processes still ran after {END_PASSES} passes; first: TerminateProcess(7) failed"
            )),
            "the last ending pass's refusal, which only an ending pass can meet, is said with the count"
        );
        assert_eq!(gone_by_the_count, None, "a refused process gone by the counting pass is not said");
    }

    #[test]
    fn a_problem_of_an_earlier_pass_is_not_said_when_the_last_pass_found_none() {
        let mut passes = [pass(1, &["OpenProcess(7) failed"]), pass(0, &[])].into_iter();

        let said = in_passes(|_| passes.next().expect("two passes"));

        assert_eq!(said, None, "the process the first pass could not open had gone by the last");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot make its job or start its member must fail loudly")]
    fn a_member_that_has_exited_is_not_opened_to_be_ended() {
        let system = std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set"));
        let launch = gate_win32::Launch {
            application: system.join(r"System32\cmd.exe"),
            command_line: "cmd.exe /d /c exit 0".encode_utf16().collect(),
            directory: system,
            environment: gate_win32::EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
            output: gate_win32::LaunchOutput::Piped,
            priority: None,
        };
        // Kill-on-close, so a failing case leaves nothing in it running.
        let job = gate_win32::Job::anonymous_kill_on_close().expect("the job is created");
        let running = launch
            .start_suspended()
            .expect("the member starts")
            .assign_and_resume(&job)
            .expect("the member is put in the job and resumed");
        // A hang guard, not a bound the case asserts on.
        assert_eq!(running.wait(Duration::from_secs(600)).expect("the member is waited for"), Some(0));

        // `running` holds the member's handle, as the gate holds its command's, so it still opens and still shows in the
        // job: the job's list names such a member for a while after its handle is signalled, longer than the run end's
        // passes on a busy machine.
        let opened = opened_member(&job, running.pid());

        assert!(
            matches!(opened, Ok(None)),
            "an exited member is nothing to end: {:?}",
            opened.map(|process| process.is_some())
        );
    }
}
