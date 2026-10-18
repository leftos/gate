//! `gate run`: a log inside the state root refused, the parent gate's slot marker read and a critical gate under a heavy
//! or light one refused, the program found and the words written into its command line, a batch word refused, the log's
//! mutex taken so no other gate writes the same log, the log readied, a slot queued for and taken unless a parent's slot
//! covers the gate, the command started in its job with its environment and priority and waited for, the log completed,
//! the failure markers scanned, the tail and the verdict line said, and the slot released.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::windows::ffi::OsStrExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, io};

use gate_args::{Ceiling, ProgramSearch, RunRequest, Slot, batch_command_line, batch_refusal, command_line, is_batch_target, resolve_program};
use gate_router::warm::{RecordRead, RecordState};
use gate_router::{
    BuildSettings, BuildTarget, DEFAULT_OUTPUTS, Place, RemoteRun, Route, build_settings, dotnet_builds, dotnet_target, sdk_build_target,
    warm::warm_hint_line, without_no_build, work_folder_key,
};
use gate_state::{Estimate, FallbackReason, JournalEvent, KernelObject, RebuildOwner, RemoteTag, RunFacts, StateRoot, Verdict, log_hash, v1};
use gate_watchdog::{Cores, Hint, SlotKind, hint};
use gate_win32::{
    AcquireError, Interrupt, Job, Launch, LaunchError, LaunchOutput, NamedMutex, OpenedJob, OwnedMutexGuard, PriorityClass, Running, StoppableProcess,
};
use jiff::Timestamp;
use proc_table::{ProcessTree, Win32ProcessTable};
use serde_json::{Map, Value};

use crate::acquire;
use crate::answer::USAGE_STATUS;
use crate::console::Console;
use crate::covered::Covered;
use crate::durations::{
    Resolved, RunDurations, RunIdentity, Usual, ceiling_hint, expected_field, expected_line, raised_ceiling_line, resolve_ceiling, span_text,
};
use crate::environment::{self, BOX_JOB_VARIABLE, SLOT_HELD_VARIABLE, Variables};
use crate::escaped::{self, Found, Reach, Reached, Recorded};
use crate::journal::{self, RunJournal};
use crate::late::{Late, LateOf, LateSlot};
use crate::log::Log;
use crate::machine::{self, Machine, MachineWait, MachineWaited};
use crate::nesting::{Nesting, Parent, enclosing_of};
use crate::no_build::{self, NothingBack, Unfound, Worktree};
use crate::outputs::{stale, walk::walk};
use crate::remote::{self, KEY_WAIT_SLICE, KeyWait, RemoteCall, RemoteClient, RemoteOutcome, Split, git};
use crate::resolve::{Environment, ResolvedRoot};
use crate::route::{self, Placed};
use crate::slots::{self, ClaimOf, LendMarker, Pool, Survey, Taken, Takes, ThisGate, epoch_ms};
use crate::stopping::{TERMINATE_CHECK, failure};
use crate::wait::{self, Arrival, Host, UnderParent, Wait, Waited};
use crate::warm;
use crate::watch::{Start, Watched, Watcher, Watching};

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
/// here as `busy` ([`Run::take_key`]).
const KEY_WAIT: Duration = Duration::from_secs(90);

/// Runs `request`'s command, logging to `log` (absolute) under `root`, routing a heavy one by `environment`'s routing
/// file and git hook, saying each line as it goes; returns the gate's exit status.
///
/// The status is the command's own whole 32-bit code, 1 for a failure marker in its log or a failure of the gate after
/// it started, 124 for a command killed as stalled, timed out or past the backstop, 127 when it could not be started,
/// 0xC000013A for a Ctrl+C in the queue, before the command started, or with the command still running 5 s after it,
/// or 2 for a log inside the state root, a critical gate under a heavy or light parent's slot or a batch word refused
/// before anything was queued.
pub(crate) fn run(request: &RunRequest, log: &Path, root: &ResolvedRoot, environment: &Environment, console: &mut Console) -> u32 {
    let (nesting, asked) = match admitted(request, log, root, environment, console) {
        Ok(admitted) => admitted,
        Err(status) => return status,
    };
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
        log: &log,
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
    if let Err(problem) = log.prepare() {
        console.err(&format!("gate: {problem}"));
        return exit_status(GATE_FAILED);
    }
    // The request and nesting of a run here that dropped its `--no-build` and asks for a heavy slot.
    let mut heavy = None;
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
        warm_hash: None,
        would: None,
        arrived: epoch_ms(Timestamp::now()),
        queued_at: None,
        lent: false,
        lend: None,
        covered: None,
        late: None,
        durations: None,
    };
    let (route, placed) = run.route(environment, console);
    let local = run.local_words(&route, placed.place(), console);
    take_heavy(&mut run, &mut heavy, local.as_ref(), console);
    run.durations = run.open_durations(&placed);
    say_nesting(run.nesting, run.request, console);
    run.queue(&route, console);
    let mut hold = Hold::nothing();
    let watched = panic::catch_unwind(AssertUnwindSafe(|| run.through(&route, plan, local, &mut hold, console)));
    let status = match watched {
        Ok(Some((ended, here))) => {
            run.release(hold, Some(&ended), console);
            if ended.status == 0
                && let Some(here) = here
            {
                run.clear_nothing_back(here, console);
            }
            run.rebuild_durations(console);
            exit_status(ended.status)
        }
        Ok(None) => CONTROL_C_EXIT,
        Err(payload) => {
            run.after_panic(hold, console);
            panic::resume_unwind(payload);
        }
    };
    if let Err(error) = run.log.append(&console.take_notes()) {
        console.err(&format!("gate: cannot write to {}: {error}", run.log.path().display()));
    }
    status
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

/// Has `run` take the request and nesting of a run here that asks for a heavy slot ([`Run::as_heavy`]), kept in
/// `heavy`, when `local` dropped its `--no-build` ([`Run::local_words`]), and says the line saying so. Its journal
/// lines, ticket and claim then name the heavy kind and the command it runs. A light parent whose slot no longer covers
/// it is asked again whether it lends that slot before the run queues ([`Run::lend_again`]).
fn take_heavy<'a>(run: &mut Run<'a>, heavy: &'a mut Option<(RunRequest, Nesting)>, local: Option<&HereWords>, console: &mut Console) {
    let Some(HereWords {
        words,
        dropped: Some(reason),
        ..
    }) = local
    else {
        return;
    };
    let (request, nesting) = heavy.insert(run.as_heavy(words, console));
    run.request = request;
    run.nesting = nesting;
    run.gate = run.gate.running(request);
    run.journal = RunJournal::open(run.root, &run.gate);
    run.lend_again(console);
    console.err(&run.dropped_line(reason));
}

/// The nesting `request` runs under, or the status of a run refused before anything is taken or written, its lines
/// said: a log inside the state root, or a critical gate under a heavy or light parent's slot, which leaves an earlier
/// log in place. Any other nesting's lines are said once the run knows the slot it asks for ([`say_nesting`]). With the
/// nesting, `request` as the gate runs it: a critical gate inside a box run runs as heavy ([`Nesting::requested`]).
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
    let nesting = Nesting::read(environment, request.slot, root, request.test_seams.parent_job_trusted, console);
    if matches!(nesting, Nesting::Refused(_)) {
        say_nesting(&nesting, request, console);
        return Err(u32::from(USAGE_STATUS));
    }
    let slot = nesting.requested(request.slot);
    let request = if slot == request.slot {
        Cow::Borrowed(request)
    } else {
        Cow::Owned(RunRequest { slot, ..request.clone() })
    };
    Ok((nesting, request))
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

/// Where a planned command logs, under which state root, which gate runs it, and under which parent's slot.
struct Where<'a> {
    root: &'a ResolvedRoot,
    gate: &'a ThisGate,
    log: &'a Log,
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
        let Where { root, gate, log, nesting } = *at;
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
                output: LaunchOutput::Files {
                    log: log.path().to_owned(),
                    err_log: log.err_path().to_owned(),
                },
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
    /// The hash of the warm-up file a routed run's work folder must be warm with, which its call asks the builder for;
    /// `None` when the route asks no warmth. Set once, by [`Run::route`].
    warm_hash: Option<String>,
    /// What the routing decision saw of the run, kept for a passed light run's route line, which asks the decision again
    /// as a heavy gate would ([`route::would_route_on_heavy`]). `None` for a run whose light parent lent it its slot,
    /// which never asked.
    would: Option<route::Decision>,
    /// The run's first arrival in Unix epoch milliseconds, which every ticket it writes keeps.
    arrived: u64,
    /// The moment of the run's `queue` line: the moment the local `take` of a run the full remote pool sent back counts
    /// its wait from, while every other wait counts from its own take. `None` for a run that is not routed, and for one
    /// sent back for any other reason.
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
    /// What the call asks of the work folder's warm-up.
    warmth: remote::Warmth<'r>,
}

/// Whether the warm-up record at `path` is still the one the route judged: `warm` for the warm-up file of `hash`. A
/// record a warm-up wrote since, for another hash or state, or one that cannot be read, is not, and is left alone.
fn still_judged(path: &Path, hash: &str) -> bool {
    matches!(warm::read_warm_record(path), RecordRead::Parsed(record) if record.state == RecordState::Warm && record.hash == hash)
}

/// What a routed run's call asks of the work folder's warm-up: that it be warm with the warm-up file of `warm_hash`
/// when the route asked that, else nothing.
fn warmth_of(warm_hash: Option<&str>) -> remote::Warmth<'_> {
    warm_hash.map_or(remote::Warmth::Unasked, remote::Warmth::Needs)
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
    } = parts;
    Ok(RemoteCall {
        host: &remote.host,
        key: &remote.key,
        root: &remote.root,
        root_id: remote.root_id,
        leaf: &remote.leaf,
        siblings,
        repo: &remote.repo,
        subfolder: &remote.subfolder,
        words: &remote.words,
        outputs: &remote.outputs,
        outputs_withheld: remote.outputs_withheld,
        inputs: &remote.inputs,
        desktop: remote.desktop,
        log: log.path(),
        timeout: ceiling,
        usual,
        stall: request.stall,
        tail: request.tail,
        no_markers: request.no_markers,
        bring_tracked: &request.bring_tracked,
        env: &request.env,
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
    took: Instant,
}

impl Hold {
    /// A hold of nothing yet, its take counted from now until a slot or the machine is taken.
    fn nothing() -> Hold {
        Hold {
            taken: None,
            machine: None,
            took: Instant::now(),
        }
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

impl Run<'_> {
    /// Decides where the command runs, and sets the remote tag of a routed run. A gate a parent's slot covers runs
    /// here, as does one whose light parent lends it its slot before it queues, while a critical request drains, which
    /// is not routed ([`Run::on_parent`]). The route, and what the decision learnt of the gate's place in its worktree.
    /// The hash of the warm-up file the route asks the builder's work folder to be warm with is kept for the call.
    fn route(&mut self, environment: &Environment, console: &mut Console) -> (Route, Placed) {
        if self.lend_before_queue(console) {
            self.lent = true;
            return (Route::Local { reason: None }, Placed::NotAsked);
        }
        // A gate inside a box run stays here as a covered gate does, saying nothing.
        let covering = self.nesting.covering().is_some() || matches!(self.nesting, Nesting::InBox);
        let route::Decided {
            route,
            placed: place,
            warm_hash,
            decision,
        } = route::route(self.request, environment, covering, self.root, console);
        self.warm_hash = warm_hash;
        self.would = Some(decision);
        self.tag = match &route {
            Route::Routed(remote) => Some(RemoteTag {
                host: remote.host.clone(),
                key: remote.key.clone(),
            }),
            Route::Local { .. } => None,
        };
        (route, place)
    }

    /// The durations of a run that takes a slot of its own, read once it knows the command it journals and before it
    /// queues, its folder placed as the routing decision `placed` it when the decision asked git: `None` for a no-slot
    /// run, one a parent's slot covers or lends, and one whose journal lines cannot be written.
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
        let release = JournalEvent::Release {
            slots,
            held_seconds,
            status: ended.map(|ended| ended.status),
            facts,
        };
        let remote = self.tag.as_ref();
        let filing = self
            .durations
            .as_ref()
            .and_then(|durations| durations.filing(remote.map(|tag| tag.host.as_str()), facts));
        let filed = self
            .journal
            .write_then(&release, remote, console, |time| filing.as_ref().map(|filing| filing.file(time)));
        if let (Some(durations), Some(Some(filed))) = (&mut self.durations, filed) {
            durations.filed(filed, console);
        }
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
    /// with; a run already lent its parent's slot keeps it.
    fn lend_again(&mut self, console: &mut Console) {
        if !self.lent {
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
        let covering = self.nesting.covering().map(|parent| parent.pid);
        let event = match covering {
            Some(parent_pid) => JournalEvent::Covered { parent_pid },
            None => JournalEvent::Queue {
                log: self.log.path().to_string_lossy().into_owned(),
                not_routed: route::not_routed(route),
            },
        };
        if matches!(route, Route::Routed(_)) {
            self.queued_at = Some(Instant::now());
        }
        self.journal.write(&event, self.tag.as_ref(), console);
        if let Some(parent_pid) = covering {
            self.covered = Some(Covered::write(&self.root.path, &self.gate, parent_pid, console));
        }
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
                Remotely::Here => self
                    .plan_here(remote, plan, console)
                    .map(|plan| (plan, Some(HereWords::fallback(remote)))),
            },
        };
        let (plan, here) = match planned {
            Ok(planned) => planned,
            Err(ended) => return Some((ended, None)),
        };
        let ended = self.here(plan, hold, console)?;
        Some((ended, here))
    }

    /// Runs the command here. A gate a parent's slot covers takes no slot; nor does one whose light parent lends it its
    /// slot while a critical request drains, before it queued or as it waits ([`Run::on_parent`]). Any other gate takes
    /// a slot into `hold`, one whose parent's slot does not cover it saying so first, before any wait line. How the run
    /// ended; `None` when a Ctrl+C ended the wait for a slot.
    fn here(&mut self, plan: Plan, hold: &mut Hold, console: &mut Console) -> Option<Ended> {
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
        match self.take_own(hold, console) {
            Taking::Took(took, figures) => {
                hold.took = took;
                Some(self.command(plan, figures, console))
            }
            Taking::Lent(figures) => Some(self.lent_as_it_waited(plan, hold, figures, console)),
            Taking::Interrupted => None,
            // Only a remote pool has a wait limit, so a local take never times out.
            Taking::Full => unreachable!("a local pool is never full past a limit"),
        }
    }

    /// Waits for and takes the run's own slot of its pool into `hold`, or the machine for a critical run; a no-slot run
    /// takes nothing and waits for nothing ([`Run::take_nothing`]).
    fn take_own(&mut self, hold: &mut Hold, console: &mut Console) -> Taking {
        match Takes::of(self.nesting.takes(self.request.slot)) {
            Takes::Pool(pool) => self.take(pool, self.queued_at, &mut hold.taken, console),
            Takes::Machine => self.take_machine(&mut hold.machine, console),
            Takes::Nothing => self.take_nothing(console),
        }
    }

    /// Starts a no-slot run at once: no ticket, claim or mutex, and no waiter woken. It journals a `take` of no slot,
    /// waited for nothing, naming the parent gate it runs under when there is one.
    fn take_nothing(&mut self, console: &mut Console) -> Taking {
        let took = Instant::now();
        let parent_pid = self.nesting.uncovering().map(|parent| parent.pid);
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
        hold.took = Instant::now();
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

    /// Runs the routed command on the builder: takes the client's work-folder key lock and a remote slot into `hold`,
    /// calls the client and ends the run as the client ended it, the lock held until then. A key lock another run of the
    /// worktree holds is waited for up to its bound; a full remote pool, a refusal or a client that cannot start falls
    /// back here; with a client not ready, the run falls back before it takes a remote slot. A Ctrl+C ends the run here,
    /// with the remote slot released: one raised before the call, or during it over a refusal. A client that ended on its
    /// own keeps its own status, and the run writes or clears the worktree's nothing-back record
    /// ([`Run::leave_nothing_back`]).
    fn routed(&mut self, remote: &RemoteRun, hold: &mut Hold, console: &mut Console) -> Remotely {
        if let Err(why) = self.client.ready() {
            return self.fell_back(FallbackReason::Client, &RemoteOutcome::not_started(why), None, console);
        }
        let (_key, figures) = match self.hold_remote(remote, hold, console) {
            Ok(held) => held,
            Err(remotely) => return remotely,
        };
        // A Ctrl+C that came once the remote slot was held, before the client was called, ends the run as an
        // interrupt before a command starts does here; the caller releases the slot with the run's tag.
        if self.host.interrupt.is_some_and(Interrupt::is_raised) {
            return Remotely::Ended(self.interrupted_before_start(console));
        }
        let started = Instant::now();
        let parts = CallParts {
            request: self.request,
            figures,
            log: &self.log,
            host: &self.host,
            root: self.root,
            warmth: warmth_of(self.warm_hash.as_deref()),
        };
        let call = match call_of(parts, remote) {
            Ok(call) => call,
            Err(why) => return self.fell_back(FallbackReason::Client, &RemoteOutcome::not_started(why), hold.taken.take(), console),
        };
        let outcome = self.client.run(&call);
        let fallback = outcome.fallback();
        // A Ctrl+C during the call wins over a refusal: the command never runs here.
        if fallback.is_some() && self.host.interrupt.is_some_and(Interrupt::is_raised) {
            return Remotely::Ended(self.interrupted_before_start(console));
        }
        if let Some(reason) = fallback {
            let said = self.drop_warm_record(remote, reason);
            let here = self.fell_back(reason, &outcome, hold.taken.take(), console);
            for line in &said {
                console.err(line);
            }
            return here;
        }
        let wall = started.elapsed();
        let words = unicode_words(&self.request.command);
        self.leave_nothing_back(remote, &outcome, words.as_deref(), console);
        let stale = stale_lines(remote, &outcome, words.as_deref());
        let passing = Passing {
            pass: Pass::On(&remote.host, wall, outcome.split, outcome.adjusted),
            ceiling: figures.ceiling,
            stale: &stale,
        };
        Remotely::Ended(self.remote_ended(&outcome, passing, wall, console))
    }

    /// Drops the work folder's warm-up record after the client's refusal for `reason`, while the run still holds the
    /// work folder's key: a `cold` or a `layout` refusal deletes it while it is still the record the route judged
    /// ([`still_judged`]), one already gone among them, and any other keeps it. A run that asked no warmth, a repo
    /// with no consent among them, neither reads nor deletes anything under `remote\warm`.
    /// The lines to say once the run has fallen back: why the delete failed, then, after `cold`, how the work folder is
    /// warmed.
    fn drop_warm_record(&self, remote: &RemoteRun, reason: FallbackReason) -> Vec<String> {
        if !matches!(reason, FallbackReason::Cold | FallbackReason::Layout) {
            return Vec::new();
        }
        let mut lines = Vec::new();
        let path = v1::warm_record_path(&self.root.path, &remote.key);
        let judged = self.warm_hash.as_deref().is_some_and(|hash| still_judged(&path, hash));
        match judged.then(|| fs::remove_file(&path)) {
            Some(Err(error)) if error.kind() != io::ErrorKind::NotFound => lines.push(format!(
                "gate: cannot remove the warm-up record {}: {}",
                gate_router::escaped(&path.to_string_lossy()),
                gate_router::escaped(&error.to_string())
            )),
            _ => {}
        }
        if reason == FallbackReason::Cold {
            lines.push(warm_hint_line(&remote.host));
        }
        lines
    }

    /// Updates the worktree's nothing-back record after a routed run that ended on the builder, killed, interrupted or
    /// unreported among them. A dotnet command whose `--no-build` the builder dropped built there and brings nothing
    /// back: it writes the record when none stands, and never rewrites or clears one. A dotnet build ([`dotnet_builds`])
    /// that passed with its outputs brought back ([`brought_everything_back`]) clears the record, as a passing build here
    /// of its words would ([`Run::clear_nothing_back`]); any other dotnet build writes it, an unknown counting as not
    /// brought back. The record names the builder and the configuration, framework and runtime the words set
    /// ([`build_settings`]); one that cannot be written, or whose presence cannot be read, is said. The command's
    /// `words`, `None` when one is not valid Unicode ([`unicode_words`]), are the caller's, so one reading of them serves
    /// this record and the stale line.
    fn leave_nothing_back(&self, remote: &RemoteRun, outcome: &RemoteOutcome, words: Option<&[String]>, console: &mut Console) {
        let Some(words) = words else {
            return;
        };
        let path = v1::nothing_back_path(&self.root.path, &remote.key);
        if remote.dropped_no_build {
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
        let record = v1::NothingBackRecord {
            host: remote.host.clone(),
            configuration: settings.configuration,
            framework: settings.framework,
            runtime: settings.runtime,
        };
        if let Err(error) = v1::write_nothing_back(&path, &record) {
            console.err(&format!("gate: cannot write {}: {error}", path.display()));
        }
    }

    /// Takes the work-folder key lock the run's client takes, waiting up to the run's key-wait bound for another run of
    /// the worktree to let it go, then a remote slot into `hold`: the lock, held until the remote attempt ends, with the
    /// figures of the remote take. Else how the run goes on: here, `busy`, when the lock is held elsewhere past the
    /// bound, or by a warm-up of the work folder, or cannot be opened or taken, which says why; here, `full`, when the
    /// pool stayed full; or interrupted during the wait, which journals the abandon.
    fn hold_remote(&mut self, remote: &RemoteRun, hold: &mut Hold, console: &mut Console) -> Result<(Option<OwnedMutexGuard>, Figures), Remotely> {
        // The key lock reads neither figure of the call; no take has resolved them yet. A sibling that fails its check
        // again sends nothing: the run falls back here before it takes the lock or a slot.
        let key = self.take_key(remote, console)?;
        let figures = match self.take(Pool::Remote(remote.slots), None, &mut hold.taken, console) {
            Taking::Took(took, figures) => {
                hold.took = took;
                figures
            }
            Taking::Lent(_) => unreachable!("only a local wait is lent a parent's slot"),
            Taking::Interrupted => return Err(Remotely::Interrupted),
            Taking::Full => return Err(self.fell_back(FallbackReason::Full, &RemoteOutcome::default(), None, console)),
        };
        Ok((key, figures))
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
        let parts = CallParts {
            request: self.request,
            figures: Figures::floor(self.request.timeout),
            log: &self.log,
            host: &self.host,
            root: self.root,
            warmth: warmth_of(self.warm_hash.as_deref()),
        };
        let call = match call_of(parts, remote) {
            Ok(call) => call,
            Err(why) => return Err(self.fell_back(FallbackReason::Client, &RemoteOutcome::not_started(why), None, console)),
        };
        let busy = |run: &mut Run<'_>, console: &mut Console| run.fell_back(FallbackReason::Busy, &RemoteOutcome::default(), None, console);
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
        let mut probe_said = false;
        if warm_up_holds(&self.root.path, &remote.key, console, &mut probe_said) {
            return Err(busy(self, console));
        }
        let bound = self.request.test_seams.key_wait_ms.map_or(KEY_WAIT, Duration::from_millis);
        let heartbeat = self.request.test_seams.heartbeat_ms.map_or(wait::HEARTBEAT, Duration::from_millis);
        let mut next = heartbeat;
        let left = holder_time_left(&mut self.survey, &self.root.path, call.key, console);
        console.err(&key_wait_line(&remote.host, left));
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
            if waited >= bound {
                return Err(busy(self, console));
            }
            if waited >= next {
                // A warm-up that took the key during the wait is not waited for either.
                if warm_up_holds(&self.root.path, &remote.key, console, &mut probe_said) {
                    return Err(busy(self, console));
                }
                console.err(&format!(
                    "gate: still waiting for this worktree's work folder on {} ({} s)",
                    remote.host,
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
            remote.host
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

    /// Gives up the remote attempt for `reason`: journals the fallback, the run's last line with the remote tag, while
    /// the remote slot `held` is still held, then deletes its claim, lets it go and wakes the waiters, removes the log
    /// so the client's own bytes do not stand beside the notes that say them, gathers the attempt's lines into the
    /// log's notes unsaid, and says the run goes on here.
    fn fell_back(&mut self, reason: FallbackReason, outcome: &RemoteOutcome, held: Option<Taken>, console: &mut Console) -> Remotely {
        let tag = self.tag.take();
        // Only the run the full remote pool sent back counts its local take from its own queue line; the local take of
        // every other fallback counts from the take itself, as a run that never fell back does.
        if reason != FallbackReason::Full {
            self.queued_at = None;
        }
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
        let host = tag.map_or_else(String::new, |tag| tag.host);
        console.err(&fallback_line(&host, reason, outcome.detail.as_deref()));
        Remotely::Here
    }

    /// The plan of the run here after a fallback of any reason: the first plan unchanged when the fallback drops no
    /// word, which is the command as given, else a plan of the fallback's words, the command with a dotnet `--no-build`
    /// dropped. Else how the run ended when that command cannot be planned.
    fn plan_here(&self, remote: &RemoteRun, plan: Plan, console: &mut Console) -> Result<Plan, Ended> {
        let words: Vec<OsString> = remote.fallback.iter().map(OsString::from).collect();
        if words == self.request.command {
            return Ok(plan);
        }
        self.replan(&words, console)
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

    /// The words of a run here from the start, its worktree when it is known, and why its `--no-build` was dropped: a
    /// dotnet command with an exact `--no-build` word of its own that would find no build here
    /// ([`Run::no_build_reason`]) loses each such word. Its worktree comes from the route's `place`, else from one git
    /// call; a command outside a worktree keeps its words. `None` for a routed run, and for a command with a word that
    /// is not valid Unicode, which runs as it is.
    fn local_words(&self, route: &Route, place: Option<&Place>, console: &mut Console) -> Option<HereWords> {
        if matches!(route, Route::Routed(_)) {
            return None;
        }
        let words = unicode_words(&self.request.command)?;
        let known = place.map(|place| Worktree {
            root: place.root.clone(),
            prefix: place.prefix.clone(),
        });
        let dropped = without_no_build(words.clone());
        let kept = |words, worktree| HereWords {
            words,
            worktree,
            dropped: None,
        };
        if dropped.len() == words.len() {
            return Some(kept(words, known));
        }
        let Some(worktree) = known.or_else(|| self.worktree(false, console)) else {
            return Some(kept(words, None));
        };
        let Some(reason) = self.no_build_reason(&worktree.root, &words, console) else {
            return Some(kept(words, Some(worktree)));
        };
        Some(HereWords {
            words: dropped,
            worktree: Some(worktree),
            dropped: Some(reason),
        })
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
    /// gate that covers or lent it, on no slot inside a box run, else on a heavy slot of its own ([`dropped_line`]).
    fn dropped_line(&self, reason: &str) -> String {
        let lent_by = self.lent.then(|| self.nesting.uncovering()).flatten();
        dropped_line(reason, self.nesting.covering().or(lent_by), matches!(self.nesting, Nesting::InBox))
    }

    /// Why a dotnet `--no-build` run of `words` in the worktree at `root` drops the word, `None` when it keeps it: the
    /// worktree has a nothing-back record, or a project it builds has no `obj\project.assets.json` ([`Run::without_assets`]),
    /// asked only without a record ([`no_build::drop_reason`]). A record that cannot be read counts as there, said.
    fn no_build_reason(&self, root: &str, words: &[String], console: &mut Console) -> Option<String> {
        let path = v1::nothing_back_path(&self.root.path, &work_folder_key(root));
        let record = no_build::read_record(&path).unwrap_or_else(|error| {
            console.err(&format!("gate: cannot read {}: {error}", path.display()));
            Some(NothingBack {
                host: None,
                settings: BuildSettings::default(),
            })
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

    /// Removes the worktree's nothing-back record after a run here that passed, once its slot is released, or after a
    /// routed run that brought its outputs back ([`Run::leave_nothing_back`]), when the words it ran are a dotnet build
    /// of the whole worktree that rebuilt what the record stands for ([`Run::cleared_record`]). A record that cannot be
    /// removed is said.
    fn clear_nothing_back(&self, here: HereWords, console: &mut Console) {
        let Some(path) = self.cleared_record(here, console) else {
            return;
        };
        match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                console.err(&format!("gate: cannot remove {}: {error}", path.display()));
            }
            _ => {}
        }
    }

    /// The nothing-back record a passing run here of `here`'s words clears: that of its worktree (`here`'s, else the
    /// one git gives, git that cannot be found or started counting as none, unsaid), when the words are an SDK dotnet
    /// build other than `msbuild` ([`sdk_build_target`]) of a solution, or of nothing run at the worktree's root, and
    /// set the configuration, framework and runtime the record's build set ([`BuildSettings::same_as`]). `None` when
    /// there is no such record; a record that cannot be read is said and left.
    fn cleared_record(&self, here: HereWords, console: &mut Console) -> Option<PathBuf> {
        let target = sdk_build_target(&here.words).filter(|target| *target != BuildTarget::Other)?;
        let worktree = here.worktree.or_else(|| self.worktree(true, console))?;
        let whole = match target {
            BuildTarget::Solution => true,
            BuildTarget::Unnamed => worktree.prefix.is_empty(),
            BuildTarget::Other => false,
        };
        if !whole {
            return None;
        }
        let path = v1::nothing_back_path(&self.root.path, &work_folder_key(&worktree.root));
        let record = match no_build::read_record(&path) {
            Ok(record) => record?,
            Err(error) => {
                console.err(&format!("gate: cannot read {}: {error}", path.display()));
                return None;
            }
        };
        record.settings.same_as(&build_settings(&here.words)).then_some(path)
    }

    /// A plan of `words`, which a run here runs in place of the command as given; else how the run ended when they
    /// cannot be planned.
    fn replan(&self, words: &[OsString], console: &mut Console) -> Result<Plan, Ended> {
        let at = Where {
            root: self.root,
            gate: &self.gate,
            log: &self.log,
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
        let interrupted = self.host.interrupt.is_some_and(Interrupt::is_raised);
        // A client that wrote nothing left no log; the verdict reads one.
        if let Err(error) = OpenOptions::new().append(true).create(true).open(self.log.path()) {
            let problem = format!("cannot create the log {}: {error}", self.log.path().display());
            return self.gate_failed(&problem, None, console);
        }
        match self.verdict(outcome.status, Some(passing), outcome.killed || interrupted, &[], console) {
            Ok(status) => {
                let verdict = if let Some(kill) = outcome.box_killed {
                    kill.verdict()
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
    /// own `queue` line for a run the full remote pool sent back and `None` for every other wait, which counts from now;
    /// the wait, its still-waiting lines and its journal all count from that one instant.
    /// When the slot was taken; else a Ctrl+C ended the wait, which deletes the ticket at once, wakes the waiters and
    /// journals the abandon, or a remote pool stayed full past the wait's limit, which deletes the ticket, or a light
    /// parent lent the run its slot, which deletes the ticket and wakes the waiters.
    fn take(&mut self, pool: Pool, since: Option<Instant>, held: &mut Option<Taken>, console: &mut Console) -> Taking {
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
            }
        };
        let waited = queued.elapsed().as_secs_f64();
        wait.end(console);
        let took = Instant::now();
        let parent_pid = self.nesting.uncovering().map(|parent| parent.pid);
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
        let line = machine::holds_line(Timestamp::now(), waited, &snapshot);
        let take = JournalEvent::Take {
            slots: machine.names(),
            waited_seconds: waited,
            abandoned: machine.abandoned().to_vec(),
            machine: Some(snapshot),
            // A critical gate under a parent whose slot does not cover it is refused, so only a parent that holds no
            // slot is named here.
            parent_pid: self.nesting.uncovering().map(|parent| parent.pid),
        };
        console.err(&line);
        let took = Instant::now();
        let (figures, resolved) = Figures::resolved(self.request.timeout, estimate.as_ref());
        let of = ClaimOf {
            gate: &self.gate,
            ceiling: figures.ceiling,
            log: self.log.path(),
            remote: None,
            expected: expected.as_ref(),
        };
        machine.write_claim(&of, console);
        self.journal.write(&take, None, console);
        slots::wake_waiters(self.root, &mut self.survey, console);
        say_expected(estimate.as_ref(), resolved, console);
        Taking::Took(took, figures)
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
        if let Err(error) = self.log.finish(console) {
            console.err(&format!(
                "gate: could not write to {} after the failure: {error}",
                self.log.path().display()
            ));
        }
        self.release(hold, None, console);
    }

    /// Deletes the claim, journals the release with how the run `ended` (status and verdict unknown when it did not
    /// end on its own path), filing a run that reached a verdict in the durations store in the same journal hold
    /// ([`Run::write_release`]), and lets the slot go, then wakes the waiters. A critical gate journals the release of every
    /// slot it held first, then lets the machine go ([`Machine::release`]). A gate a parent's slot covers deletes its
    /// covered marker, then lets go the slot it took when that parent ended as its command ran ([`Late`]), as any slot
    /// is let go; it took no other slot, and journals no release when it took none. A gate a light parent lent its slot
    /// took no slot and journals no release; it deletes its lend marker and wakes the waiters.
    fn release(&mut self, hold: Hold, ended: Option<&Ended>, console: &mut Console) {
        if let Some(lend) = self.lend.take() {
            lend.end(self.root, &mut self.survey, console);
        }
        if let Some(mut covered) = self.covered.take() {
            covered.end(console);
        }
        if let Some(LateSlot { taken, took }) = self.late.take() {
            self.let_go(Some(taken), took, ended, console);
        }
        if self.nesting.covering().is_some() || self.lent {
            return;
        }
        let Hold { taken, machine, took } = hold;
        if let Some(machine) = machine {
            self.write_release(machine.names(), took.elapsed().as_secs_f64(), ended, console);
            machine.release(self.root, &mut self.survey, console);
            return;
        }
        self.let_go(taken, took, ended, console);
    }

    /// Deletes the claim of `taken`, the slot of a pool the run took at `took`, journals the release with how the run
    /// `ended` and lets the slot go, then wakes the waiters; a run that took no slot journals a release of none.
    fn let_go(&mut self, taken: Option<Taken>, took: Instant, ended: Option<&Ended>, console: &mut Console) {
        let held_seconds = took.elapsed().as_secs_f64();
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
        // The job remembers every process put in it from the command on, so a kill still reaches what one that has
        // ended started outside it.
        let created = created.and_then(|mut job| job.remember().map(|()| job));
        let job = match created {
            Ok(job) => job,
            Err(error) => return self.gate_failed(&format!("cannot create the job {name}: {error}"), None, console),
        };
        let start = Start::now();
        match launch.start_suspended().and_then(|suspended| suspended.assign_and_resume(&job)) {
            Ok(running) => self.watched(&job, &running, &start, figures, console),
            Err(LaunchError::CreateProcess { message, .. }) => self.not_started(&plan.word, plan.resolved.as_deref(), &message, console),
            Err(error) => self.gate_failed(&error.to_string(), None, console),
        }
    }

    /// Says that a Ctrl+C came before the command started, which then never starts, and completes the log; the status
    /// is `STATUS_CONTROL_C_EXIT`, with no verdict.
    fn interrupted_before_start(&mut self, console: &mut Console) -> Ended {
        console.err("gate: interrupted before the command started");
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
    fn watched(&mut self, job: &Job, running: &Running, start: &Start, figures: Figures, console: &mut Console) -> Ended {
        // A copy of the run's own reference, so the stopper holds no borrow of the run while it is watched.
        let root = self.root;
        let recorded = RefCell::new(Recorded::default());
        let started = Started {
            job,
            running,
            recorded: Some(&recorded),
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
        let (root, gate, request, log) = (taker.root, taker.gate, taker.request, taker.log);
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
            log,
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
                processes,
                diagnostics,
            } => {
                for note in &diagnostics {
                    console.note(note);
                }
                console.err(&line);
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
        match self.verdict(NOT_STARTED, None, false, &[], console) {
            Ok(status) => Ended { status, facts: None },
            Err(problem) => self.gate_failed(&problem, None, console),
        }
    }

    /// Completes the log with the notes so far, and says the marker lines of a failure, the log's tail and the verdict
    /// line; the gate's status. `exited` is the command's status, and `passing` how long it ran and under what ceiling
    /// for the passed line, `None` when it never started; a run the gate ended itself is `unscanned`, its markers not
    /// read. `diagnostics` is the block a kill wrote to the log: the log keeps it, and the tail leaves exactly its lines
    /// out.
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
        for line in &tail {
            console.out(line);
        }
        match passing {
            Some(passed) if status == 0 => self.say_passed(passed, &shown, console),
            _ => console.err(&format!("gate: FAILED (status {status}). Full output: {shown}")),
        }
        Ok(status)
    }

    /// Says what a passed run says: the lines naming the folders it left stale when it has any, the cores hint, the
    /// route line, and the passed line.
    fn say_passed(&self, passing: Passing<'_>, shown: &str, console: &mut Console) {
        let Passing { pass, ceiling, stale } = passing;
        for line in stale {
            console.err(line);
        }
        if let Some(line) = self.cores_hint(pass) {
            console.note_out(&line);
        }
        if let Some(line) = self.route_hint(pass) {
            console.note_out(&line);
        }
        console.note_out(&passed_line(pass, ceiling, shown));
    }

    /// The cores line of a passed run that ran as `pass` says, when its slot did not fit ([`cores_hint_for`]).
    fn cores_hint(&self, pass: Pass<'_>) -> Option<String> {
        cores_hint_for(Judged {
            passed: true,
            own_slot: takes_own_slot(self.nesting, self.lent),
            slot: self.request.slot,
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
        let wanted = self.request.tail.saturating_add(u64::try_from(diagnostics.len()).unwrap_or(u64::MAX));
        Ok((found, tail_without_block(self.log.tail(wanted)?, diagnostics, self.request.tail)))
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
        if let Some(Started { job, running, recorded, .. }) = started {
            stop(&self.ender(job, running), recorded, console);
        }
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

/// Terminates every one of `doomed`, then gives them [`TERMINATE_CHECK`] together, from the first wait, to leave the
/// job before the next pass lists it, each wait taking what is left of that time ([`escaped::wait_left`]). A process
/// that refused the terminate is added to `problems` only when its wait has not seen it end, as one that was ending
/// already refuses it and is not a failure once it has ended.
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
        StoppableProcess::terminate(self, status)
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

/// What a routed run's passed line says of its parts, `split`: the command's own time, then each transfer the run made,
/// the push always, in the order they ran, each in whole seconds.
fn split_said(split: Split) -> String {
    let seconds = |part: Duration| whole_seconds(part.as_secs_f64());
    let mut parts = vec![format!("command {} s", seconds(split.command)), format!("push {} s", seconds(split.push))];
    parts.extend(split.inputs.map(|part| format!("inputs {} s", seconds(part))));
    parts.extend(split.sync_back.map(|part| format!("sync-back {} s", seconds(part))));
    parts.extend(split.outputs.map(|part| format!("outputs {} s", seconds(part))));
    parts.join(", ")
}

/// `seconds` in whole seconds, rounded half to even as the PowerShell gate's `[math]::Round` rounds.
fn whole_seconds(seconds: f64) -> f64 {
    seconds.round_ties_even()
}

/// What the cores line of a run is decided from: whether it passed, whether it took its own slot (not covered by a
/// parent's, nor lent one), the slot kind it asked for, and how long it ran here, `None` for a run on a builder.
#[derive(Clone, Copy)]
struct Judged {
    passed: bool,
    own_slot: bool,
    slot: Slot,
    timing: Option<Timing>,
}

/// The line a run says as it drops its `--no-build` for `reason`: building on the slot of `parent`, the parent gate that
/// covers or lent it, on no slot when it runs inside a box run (`in_box`), else on a heavy slot of its own.
fn dropped_line(reason: &str, parent: Option<&Parent>, in_box: bool) -> String {
    let building = match parent {
        Some(parent) => format!("building on the parent gate's {} slot", parent.held.as_str()),
        None if in_box => "building on no slot".to_owned(),
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
    hint(cores, slot).map(|misfit| cores_line(cores, misfit))
}

/// The line that tells a run its slot did not fit, `misfit` as `cores` show it: the average and free cores to one
/// decimal, the peak to a whole number, halves to even. A run on no slot is told without the free cores.
fn cores_line(cores: Cores, misfit: Hint) -> String {
    let used = format!("gate: used {:.1} cores on average (peak {}", cores.avg, cores.peak.round_ties_even());
    let free = cores.free;
    match misfit {
        Hint::Hot => format!("{used}, {free:.1} free) on a light slot; work that keeps more than 2 busy belongs on heavy"),
        Hint::Cold => format!("{used}, {free:.1} free) on a heavy slot; work that keeps under 1.5 busy fits on light"),
        Hint::Busy => format!("{used}) on no slot; work that keeps 1.5 or more busy belongs on light or heavy"),
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

/// The line a routed run says as it falls back here for `reason` from the builder `host`; a client that could not
/// start says why, `detail`.
fn fallback_line(host: &str, reason: FallbackReason, detail: Option<&str>) -> String {
    match reason {
        FallbackReason::Client => format!(
            "gate: remote {host} client could not start: {}; running here",
            detail.unwrap_or("no reason given")
        ),
        _ => format!("gate: remote {host} {}; running here", reason.as_str()),
    }
}

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

    /// The words of the run here after `remote`'s fallback, in the worktree and folder the route placed it in.
    fn fallback(remote: &RemoteRun) -> HereWords {
        HereWords::routed(remote, remote.fallback.clone())
    }
}

/// Whether a routed run's `outcome` lets it clear the worktree's nothing-back record: it passed, as a run here must to
/// clear it, and its outputs are known to be brought back.
fn brought_everything_back(outcome: &RemoteOutcome) -> bool {
    outcome.status == 0 && outcome.brought_back == Some(true)
}

/// The lines a passing routed build says of the folders it left stale, none when it says none: the folders in the
/// worktree the run's judging globs stop at that the run did not bring back. Only a dotnet build the gate treats as one
/// ([`dotnet_builds`]) that ended on the builder has any, and the run's verdict decides whether it is said
/// ([`Run::verdict`]), so a failed build, one whose log reports a failure among them, and a fallback (whose build ran
/// here) say nothing. The judging globs are the run's own, the defaults beside them, even in a repo whose `outputs` list
/// is empty ([`DEFAULT_OUTPUTS`]); a file the run brought back is no stale folder's. A pair's siblings are judged by
/// the same globs, and named after the main's on the same line, relative to the main worktree; a sibling's output is
/// never brought back. A tree that cannot be listed says why it could not, on a line of its own ([`stale::stale_lines`]).
fn stale_lines(remote: &RemoteRun, outcome: &RemoteOutcome, words: Option<&[String]>) -> Vec<String> {
    let Some(words) = words else {
        return Vec::new();
    };
    if outcome.status != 0 || !dotnet_builds(words) {
        return Vec::new();
    }
    let mut judging = remote.outputs.clone();
    judging.extend(DEFAULT_OUTPUTS.iter().map(|glob| (*glob).to_owned()));
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
        Ends, Judged, Pass, ROUTE_HINT_MIN_WALL, RouteJudged, Stopper, Timing, brought_everything_back, cores_hint_for, cores_line, dropped_line,
        earlier, holder_claim, interrupted, key_wait_left, key_wait_line, kill, log_in_state_root, passed_line, records_after, route_hint_for,
        route_hint_line, stop, takes_own_slot,
    };
    use crate::console::Console;
    use crate::escaped::{Found, Look, Reached, Recorded};
    use crate::nesting::{Nesting, Parent};
    use crate::remote::{RemoteOutcome, Split};
    use crate::watch::Watched;
    use gate_state::v1::{Claim, ClaimKind};
    use jiff::Timestamp;
    use serde_json::{Map, Value};

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
        };
        assert_eq!(
            dropped_line(reason, None, true),
            format!("gate: dropped --no-build: {reason}; building on no slot")
        );
        assert_eq!(
            dropped_line(reason, None, false),
            format!("gate: dropped --no-build: {reason}; building on a heavy slot")
        );
        assert_eq!(
            dropped_line(reason, Some(&parent), false),
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
        let rows: [(&str, Judged, Option<&str>); 11] = [
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
            cores_line(hot, Hint::Hot),
            "gate: used 7.4 cores on average (peak 15, 9.3 free) on a light slot; work that keeps more than 2 busy belongs on heavy"
        );
        let cold = Cores {
            avg: 0.04,
            peak: 0.5,
            free: 12.0,
        };
        assert_eq!(
            cores_line(cold, Hint::Cold),
            "gate: used 0.0 cores on average (peak 0, 12.0 free) on a heavy slot; work that keeps under 1.5 busy fits on light"
        );
        let busy = Cores {
            avg: 1.54,
            peak: 2.5,
            free: 3.0,
        };
        assert_eq!(
            cores_line(busy, Hint::Busy),
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
