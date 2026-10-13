//! `gate run`: a log inside the state root refused, the parent gate's slot marker read and a critical gate under a heavy
//! or light one refused, the program found and the words written into its command line, a batch word refused, the log's
//! mutex taken so no other gate writes the same log, the log readied, a slot queued for and taken unless a parent's slot
//! covers the gate, the command started in its job with its environment and priority and waited for, the log completed,
//! the failure markers scanned, the tail and the verdict line said, and the slot released.

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::windows::ffi::OsStrExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, io};

use gate_args::{ProgramSearch, RunRequest, Slot, batch_command_line, batch_refusal, command_line, is_batch_target, resolve_program};
use gate_router::{
    BuildSettings, BuildTarget, Place, RemoteRun, Route, build_settings, dotnet_builds, dotnet_target, sdk_build_target, without_no_build,
    work_folder_key,
};
use gate_state::{FallbackReason, JournalEvent, KernelObject, RemoteTag, RunFacts, StateRoot, Verdict, log_hash, v1};
use gate_watchdog::{Cores, Hint, SlotKind, hint};
use gate_win32::{AcquireError, Interrupt, Job, Launch, LaunchError, LaunchOutput, NamedMutex, OwnedMutexGuard, PriorityClass, Running};
use jiff::Timestamp;

use crate::acquire;
use crate::answer::USAGE_STATUS;
use crate::console::Console;
use crate::covered::Covered;
use crate::environment::{self, Variables};
use crate::escaped::{self, Found, Reach, Reached, Recorded};
use crate::journal::{self, RunJournal};
use crate::log::Log;
use crate::machine::{self, Machine, MachineWait, MachineWaited};
use crate::nesting::{Nesting, Parent};
use crate::no_build::{self, NothingBack, Unfound, Worktree};
use crate::remote::{self, RemoteCall, RemoteClient, RemoteOutcome, Unready, git};
use crate::resolve::{Environment, ResolvedRoot};
use crate::route;
use crate::slots::{self, ClaimOf, Pool, Survey, Taken, Takes, ThisGate, epoch_ms};
use crate::wait::{self, Arrival, Host, UnderParent, Wait, Waited};
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
/// The wall time under which a passed run is too short to judge its slot by, so it gets no cores line.
const HINT_MIN_WALL: Duration = Duration::from_secs(10);

/// Runs `request`'s command, logging to `log` (absolute) under `root`, routing a heavy one by `environment`'s routing
/// file and git hook, saying each line as it goes; returns the gate's exit status.
///
/// The status is the command's own whole 32-bit code, 1 for a failure marker in its log or a failure of the gate after
/// it started, 124 for a command killed as stalled, timed out or past the backstop, 127 when it could not be started,
/// 0xC000013A for a Ctrl+C in the queue, before the command started, or with the command still running 5 s after it,
/// or 2 for a log inside the state root, a critical gate under a heavy or light parent's slot or a batch word refused
/// before anything was queued.
pub(crate) fn run(request: &RunRequest, log: &Path, root: &ResolvedRoot, environment: &Environment, console: &mut Console) -> u32 {
    let nesting = match admitted(request, log, root, environment, console) {
        Ok(nesting) => nesting,
        Err(status) => return status,
    };
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
    let interrupt = Interrupt::install()
        .inspect_err(|error| {
            console.err(&format!(
                "gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: {error}"
            ));
        })
        .ok();
    let host = Host {
        processors: wait::processors(console),
        interrupt,
    };
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
        host,
        client,
        tag: None,
        arrived: epoch_ms(Timestamp::now()),
        queued_at: None,
        lent: false,
        covered: None,
    };
    let (route, place) = run.route(environment, console);
    let local = run.local_words(&route, place.as_ref(), console);
    take_heavy(&mut run, &mut heavy, local.as_ref(), console);
    say_nesting(run.nesting, run.request, console);
    run.queue(&route, console);
    let mut hold = Hold {
        taken: None,
        machine: None,
        took: Instant::now(),
    };
    let watched = panic::catch_unwind(AssertUnwindSafe(|| run.through(&route, plan, local, &mut hold, console)));
    let status = match watched {
        Ok(Some((ended, here))) => {
            run.release(hold, Some(&ended), console);
            if ended.status == 0
                && let Some(here) = here
            {
                run.clear_nothing_back(here, console);
            }
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
    let (request, nesting) = heavy.insert(run.as_heavy(words));
    run.request = request;
    run.nesting = nesting;
    run.gate = run.gate.running(request);
    run.journal = RunJournal::open(run.root, &run.gate);
    run.lend_again(console);
    console.err(&run.dropped_line(reason));
}

/// The nesting `request` runs under, or the status of a run refused before anything is taken or written, its lines
/// said: a log inside the state root, or a critical gate under a heavy or light parent's slot, which leaves an earlier
/// log in place. Any other nesting's lines are said once the run knows the slot it asks for ([`say_nesting`]).
fn admitted(request: &RunRequest, log: &Path, root: &ResolvedRoot, environment: &Environment, console: &mut Console) -> Result<Nesting, u32> {
    if log_in_state_root(log, &root.path) {
        console.err(&format!(
            "gate: the log {} is inside the gate's state folder {}",
            log.display(),
            root.path.display()
        ));
        return Err(u32::from(USAGE_STATUS));
    }
    let nesting = Nesting::read(environment, request.slot, &root.root, request.test_seams.parent_job_trusted);
    if matches!(nesting, Nesting::Refused(_)) {
        say_nesting(&nesting, request, console);
        return Err(u32::from(USAGE_STATUS));
    }
    Ok(nesting)
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
        Nesting::Malformed(_) | Nesting::Ended { .. } | Nesting::Unbound { .. } | Nesting::Covered(_) | Nesting::Refused(_) => {
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
    let client = remote::chosen(request, &at.root.path).map_err(NotRun::failed)?;
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
        // A covered gate hands on the marker of the parent whose slot its command runs under; one its parent lends its
        // slot later is handed that marker then ([`Plan::handing_on`]).
        let marker = match nesting.handed_on(false) {
            Some(parent) => parent.marker.clone(),
            None => environment::slot_marker(request.slot.as_str(), gate, &root.root),
        };
        let variables = Variables::for_command(root, &marker);
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

    /// The plan with its command's environment handing on `marker`, the marker of the parent gate whose slot the
    /// command runs on.
    ///
    /// # Errors
    ///
    /// An environment the gate cannot build (status 1).
    fn handing_on(mut self, root: &ResolvedRoot, marker: &str) -> Result<Plan, NotRun> {
        if let Ok(launch) = &mut self.launch {
            launch.environment = Variables::for_command(root, marker)
                .block()
                .map_err(|error| NotRun::failed(format!("gate: cannot build the command's environment: {error}")))?;
        }
        Ok(self)
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
    /// The run's first arrival in Unix epoch milliseconds, which every ticket it writes keeps.
    arrived: u64,
    /// The moment of the run's `queue` line: the moment the local `take` of a run the full remote pool sent back counts
    /// its wait from, while every other wait counts from its own take. `None` for a run that is not routed, and for one
    /// sent back for any other reason.
    queued_at: Option<Instant>,
    /// Whether the run's light parent lent it its slot while a critical request drained, before it queued or once it
    /// journalled `parent` as it waited, so it takes, journals and releases no slot.
    lent: bool,
    /// The covered marker of a run a parent's slot covers, from its `covered` line to its release.
    covered: Option<Covered>,
}

/// The client's call for `remote`, with the run's log, limits and interrupt, made of the run's parts so the run's
/// client can be borrowed apart from them.
fn call_of<'r>(request: &'r RunRequest, log: &'r Log, host: &Host, root: &'r ResolvedRoot, remote: &'r RemoteRun) -> RemoteCall<'r> {
    RemoteCall {
        host: &remote.host,
        key: &remote.key,
        root: &remote.root,
        repo: &remote.repo,
        subfolder: &remote.subfolder,
        words: &remote.words,
        outputs: &remote.outputs,
        outputs_withheld: remote.outputs_withheld,
        log: log.path(),
        timeout: request.timeout,
        stall: request.stall,
        tail: request.tail,
        no_markers: request.no_markers,
        bring_tracked: &request.bring_tracked,
        interrupt: host.interrupt,
        state: &root.root,
        state_dir: &root.path,
    }
}

/// The slot a run holds, or the machine a critical run holds, once it has taken it, and when it took it.
struct Hold {
    taken: Option<Taken>,
    machine: Option<Machine>,
    took: Instant,
}

/// How a wait for a slot ended.
enum Taking {
    /// The slot was taken at this moment.
    Took(Instant),
    /// The run's light parent lent it its slot while a critical request drains; the run left the queue.
    Lent,
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
/// by the wall clock.
#[derive(Clone, Copy)]
enum Pass<'h> {
    Here(Timing),
    On(&'h str, Duration),
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
    /// is not routed ([`Run::on_parent`]). The route, and the gate's place in its worktree when the decision asked git
    /// for it.
    fn route(&mut self, environment: &Environment, console: &mut Console) -> (Route, Option<Place>) {
        let nesting = self.nesting;
        if let Nesting::Under(parent) = nesting
            && wait::lent_before_queue(self.root, parent.pid, self.host.processors, &mut self.survey, console)
        {
            self.lent = true;
            return (Route::Local { reason: None }, None);
        }
        let covering = nesting.covering().is_some();
        let (route, place) = route::route(self.request, environment, covering, console);
        self.tag = match &route {
            Route::Routed(remote) => Some(RemoteTag {
                host: remote.host.clone(),
                key: remote.key.clone(),
            }),
            Route::Local { .. } => None,
        };
        (route, place)
    }

    /// Asks a light parent whose slot does not cover this run, once it dropped its `--no-build` and asks for a heavy
    /// slot, whether it lends that slot before the run queues, as [`Run::route`] asked of the nesting the run was read
    /// with; a run already lent its parent's slot keeps it.
    fn lend_again(&mut self, console: &mut Console) {
        if let Nesting::Under(parent) = self.nesting
            && !self.lent
        {
            self.lent = wait::lent_before_queue(self.root, parent.pid, self.host.processors, &mut self.survey, console);
        }
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
        if nesting.covering().is_some() {
            return Some(self.command(plan, console));
        }
        if let Nesting::Under(parent) = nesting {
            if self.lent {
                return Some(self.on_parent(parent, plan, console));
            }
            console.err(&parent.uncovered_line(self.request.slot));
        }
        match self.take_own(hold, console) {
            Taking::Took(took) => {
                hold.took = took;
                Some(self.command(plan, console))
            }
            Taking::Lent => Some(self.lent_as_it_waited(plan, hold, console)),
            Taking::Interrupted => None,
            // Only a remote pool has a wait limit, so a local take never times out.
            Taking::Full => unreachable!("a local pool is never full past a limit"),
        }
    }

    /// Waits for and takes the run's own slot of its pool into `hold`, or the machine for a critical run; a no-slot run
    /// takes nothing and waits for nothing ([`Run::take_nothing`]).
    fn take_own(&mut self, hold: &mut Hold, console: &mut Console) -> Taking {
        match Takes::of(self.request.slot) {
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
        Taking::Took(took)
    }

    /// Runs the command on the slot of the light parent that lent it as the gate waited ([`Run::on_parent`]); `hold`'s
    /// time starts at the end of the wait, which a release journalled for a command that never ran counts from.
    fn lent_as_it_waited(&mut self, plan: Plan, hold: &mut Hold, console: &mut Console) -> Ended {
        let nesting = self.nesting;
        let Nesting::Under(parent) = nesting else {
            unreachable!("only a gate under a light parent is lent its parent's slot")
        };
        hold.took = Instant::now();
        self.on_parent(parent, plan, console)
    }

    /// Runs the command on the slot of the light parent `parent`, which lends it while a critical request drains: hands
    /// the command the parent's marker, then says so, journals `parent` with the parent's pid and runs it. The run
    /// takes, claims and releases no slot. A command environment that cannot be built fails the gate (status 1) before
    /// it says or journals the lend: a gate lent the slot before it queued then journals nothing, and one that queued
    /// journals its release as any gate that fails before its command does.
    fn on_parent(&mut self, parent: &Parent, plan: Plan, console: &mut Console) -> Ended {
        let plan = match plan.handing_on(self.root, &parent.marker) {
            Ok(plan) => plan,
            Err(NotRun { status, line }) => {
                console.err(&line);
                return Ended {
                    status: i64::from(status),
                    facts: None,
                };
            }
        };
        self.lent = true;
        console.err(&format!(
            "gate: running under the {} slot of gate {} while a critical request drains; taking no slot",
            parent.held.as_str(),
            parent.pid
        ));
        self.journal.write(&JournalEvent::Parent { parent_pid: parent.pid }, None, console);
        self.command(plan, console)
    }

    /// Runs the routed command on the builder: takes the client's work-folder key lock and a remote slot into `hold`,
    /// calls the client and ends the run as the client ended it, the lock held until then. A key lock held elsewhere, a
    /// full remote pool, a refusal or a client that cannot start falls back here; with a client not ready, the run
    /// falls back before it takes a remote slot, or ends at a Ctrl+C that came while the client waited to start
    /// ([`Run::interrupted_waiting_to_start`]). A
    /// Ctrl+C ends the run here, with the remote slot released: one raised before the call, or during it over a refusal.
    /// A client that ended on its own keeps its own status, and the run writes or clears the worktree's nothing-back
    /// record ([`Run::leave_nothing_back`]).
    fn routed(&mut self, remote: &RemoteRun, hold: &mut Hold, console: &mut Console) -> Remotely {
        let waiting = Instant::now();
        match self.client.ready(console, self.host.interrupt) {
            Ok(()) => {}
            Err(Unready::Missing(why)) => {
                return self.fell_back(FallbackReason::Client, &RemoteOutcome::not_started(why), None, console);
            }
            Err(Unready::RunsHere(line)) => {
                self.give_up(FallbackReason::Client, &RemoteOutcome::default(), None, console);
                console.err(&line);
                return Remotely::Here;
            }
            Err(Unready::Interrupted) => {
                self.interrupted_waiting_to_start(waiting, console);
                return Remotely::Interrupted;
            }
        }
        let _key = match self.hold_remote(remote, hold, console) {
            Ok(key) => key,
            Err(remotely) => return remotely,
        };
        // A Ctrl+C that came once the remote slot was held, before the client was called, ends the run as an
        // interrupt before a command starts does here; the caller releases the slot with the run's tag.
        if self.host.interrupt.is_some_and(Interrupt::is_raised) {
            return Remotely::Ended(self.interrupted_before_start(console));
        }
        let started = Instant::now();
        let call = call_of(self.request, &self.log, &self.host, self.root, remote);
        let outcome = self.client.run(&call);
        let fallback = outcome.fallback();
        // A Ctrl+C during the call wins over a refusal: the command never runs here.
        if fallback.is_some() && self.host.interrupt.is_some_and(Interrupt::is_raised) {
            return Remotely::Ended(self.interrupted_before_start(console));
        }
        if let Some(reason) = fallback {
            return self.fell_back(reason, &outcome, hold.taken.take(), console);
        }
        self.leave_nothing_back(remote, &outcome, console);
        Remotely::Ended(self.remote_ended(&remote.host, &outcome, started, console))
    }

    /// Updates the worktree's nothing-back record after a routed run that ended on the builder, killed, interrupted or
    /// unreported among them. A dotnet command whose `--no-build` the builder dropped built there and brings nothing
    /// back: it writes the record when none stands, and never rewrites or clears one. A dotnet build ([`dotnet_builds`])
    /// that passed with its outputs brought back ([`brought_everything_back`]) clears the record, as a passing build here
    /// of its words would ([`Run::clear_nothing_back`]); any other dotnet build writes it, an unknown counting as not
    /// brought back. The record names the builder and the configuration, framework and runtime the words set
    /// ([`build_settings`]); one that cannot be written, or whose presence cannot be read, is said.
    fn leave_nothing_back(&self, remote: &RemoteRun, outcome: &RemoteOutcome, console: &mut Console) {
        let Some(words) = unicode_words(&self.request.command) else {
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
        } else if !dotnet_builds(&words) {
            return;
        } else if brought_everything_back(outcome) {
            self.clear_nothing_back(HereWords::routed(remote, words), console);
            return;
        }
        let settings = build_settings(&words);
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

    /// Takes the work-folder key lock the run's client takes, without waiting, then a remote slot into `hold`: the
    /// lock, held until the remote attempt ends. Else how the run goes on: here, `busy`, when the lock is held
    /// elsewhere or cannot be opened or taken, which says why; here, `full`, when the pool stayed full; or interrupted
    /// during the wait.
    fn hold_remote(&mut self, remote: &RemoteRun, hold: &mut Hold, console: &mut Console) -> Result<Option<OwnedMutexGuard>, Remotely> {
        let held = self.client.hold_key(&self.call(remote));
        let key = match held {
            Ok(key) => key,
            Err(why) => {
                if let Some(line) = why {
                    console.err(&line);
                }
                return Err(self.fell_back(FallbackReason::Busy, &RemoteOutcome::default(), None, console));
            }
        };
        match self.take(Pool::Remote(remote.slots), None, &mut hold.taken, console) {
            Taking::Took(took) => hold.took = took,
            Taking::Lent => unreachable!("only a local wait is lent a parent's slot"),
            Taking::Interrupted => return Err(Remotely::Interrupted),
            Taking::Full => return Err(self.fell_back(FallbackReason::Full, &RemoteOutcome::default(), None, console)),
        }
        Ok(key)
    }

    /// Ends a routed run at a Ctrl+C that came while its client waited to start, since `waiting`: says the command never
    /// started and completes the log ([`Run::interrupted_before_start`]), then journals the abandon of the wait with the
    /// remote tag, as a Ctrl+C during a slot wait does. The run took no slot, so it journals no release.
    fn interrupted_waiting_to_start(&mut self, waiting: Instant, console: &mut Console) {
        self.interrupted_before_start(console);
        let waited_seconds = waiting.elapsed().as_secs_f64();
        self.journal.write(&JournalEvent::Abandon { waited_seconds }, self.tag.as_ref(), console);
    }

    /// The client's call for `remote`, with the run's log, limits and interrupt.
    fn call<'r>(&'r self, remote: &'r RemoteRun) -> RemoteCall<'r> {
        call_of(self.request, &self.log, &self.host, self.root, remote)
    }

    /// Gives up the remote attempt for `reason`: journals the fallback, the run's last line with the remote tag, while
    /// the remote slot `held` is still held, then deletes its claim, lets it go and wakes the waiters, removes the log
    /// so the client's own bytes do not stand beside the notes that say them, gathers the attempt's lines into the
    /// log's notes unsaid ([`Run::give_up`]), and says the run goes on here.
    fn fell_back(&mut self, reason: FallbackReason, outcome: &RemoteOutcome, held: Option<Taken>, console: &mut Console) -> Remotely {
        let host = self.give_up(reason, outcome, held, console);
        console.err(&fallback_line(&host, reason, outcome.detail.as_deref()));
        Remotely::Here
    }

    /// Gives up the remote attempt for `reason` as [`Run::fell_back`] does, saying no line that the run goes on here:
    /// the host the run was routed to.
    fn give_up(&mut self, reason: FallbackReason, outcome: &RemoteOutcome, held: Option<Taken>, console: &mut Console) -> String {
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
        tag.map_or_else(String::new, |tag| tag.host)
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
    /// heavy slot whatever slot the caller named, runs `words`, and is under its parent as a heavy gate is.
    fn as_heavy(&self, words: &[String]) -> (RunRequest, Nesting) {
        let request = RunRequest {
            slot: Slot::Heavy,
            command: words.iter().map(OsString::from).collect(),
            ..self.request.clone()
        };
        (request, self.nesting.as_heavy())
    }

    /// The line a run says as it drops its `--no-build` for `reason`, naming where it builds: on the slot of the parent
    /// gate that covers or lent it, else on a heavy slot of its own.
    fn dropped_line(&self, reason: &str) -> String {
        let building = match self.nesting.handed_on(self.lent) {
            Some(parent) => format!("building on the parent gate's {} slot", parent.held.as_str()),
            None => "building on a heavy slot".to_owned(),
        };
        format!("gate: dropped --no-build: {reason}; {building}")
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

    /// Ends a run the client ran to its end: the verdict with the passed line on `host`, its markers not scanned when
    /// the client killed the run or the console was interrupted, and the facts its release carries, with no adjusted
    /// seconds. It is a kill only when the client killed it, by the kill line in the log.
    fn remote_ended(&mut self, host: &str, outcome: &RemoteOutcome, started: Instant, console: &mut Console) -> Ended {
        let interrupted = self.host.interrupt.is_some_and(Interrupt::is_raised);
        let wall = started.elapsed();
        // A client that wrote nothing left no log; the verdict reads one.
        if let Err(error) = OpenOptions::new().append(true).create(true).open(self.log.path()) {
            let problem = format!("cannot create the log {}: {error}", self.log.path().display());
            return self.gate_failed(&problem, None, console);
        }
        match self.verdict(outcome.status, Some(Pass::On(host, wall)), outcome.killed || interrupted, &[], console) {
            Ok(status) => {
                let kill = if outcome.killed { self.kill_line(console) } else { None };
                Ended {
                    status,
                    facts: Some(RunFacts {
                        wall_seconds: wall.as_secs_f64(),
                        adjusted_seconds: None,
                        avg_cores: None,
                        peak_cores: None,
                        free_cores: None,
                        verdict: Verdict::of(kill.as_deref(), outcome.status, status),
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

    /// Waits its turn for a slot of `pool` and takes it into `held` the moment it has it, then journals the take, with
    /// the remote tag while the run has one, and writes the claim. `since` is the moment the wait counts from, the run's
    /// own `queue` line for a run the full remote pool sent back and `None` for every other wait, which counts from now.
    /// When the slot was taken; else a Ctrl+C ended the wait, which deletes the ticket at once, wakes the waiters and
    /// journals the abandon, or a remote pool stayed full past the wait's limit, which deletes the ticket, or a light
    /// parent lent the run its slot, which deletes the ticket and wakes the waiters.
    fn take(&mut self, pool: Pool, since: Option<Instant>, held: &mut Option<Taken>, console: &mut Console) -> Taking {
        let queued = Instant::now();
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
        };
        let mut wait = Wait::queue(self.root, pool, arrival, self.host, console);
        let taken = match wait.until_taken(&mut self.survey, console) {
            Waited::Taken(taken) => held.insert(taken),
            Waited::Lent => {
                wait.end(console);
                slots::wake_waiters(self.root, &mut self.survey, console);
                return Taking::Lent;
            }
            Waited::Interrupted => {
                self.abandon(wait, pool, since.unwrap_or(queued), console);
                return Taking::Interrupted;
            }
            Waited::TimedOut => {
                wait.end(console);
                slots::wake_waiters(self.root, &mut self.survey, console);
                return Taking::Full;
            }
        };
        let waited = since.unwrap_or(queued).elapsed().as_secs_f64();
        let name = taken.held.name().to_owned();
        let mut abandoned = Vec::new();
        if taken.abandoned {
            acquire::say_taken_abandoned(console, &name);
            abandoned.push(name.clone());
        }
        wait.end(console);
        let took = Instant::now();
        let parent_pid = self.nesting.uncovering().map(|parent| parent.pid);
        self.journal
            .write(&take_event(vec![name], waited, abandoned, parent_pid), self.tag.as_ref(), console);
        let of = ClaimOf {
            gate: &self.gate,
            request: self.request,
            log: self.log.path(),
            remote: self.tag.as_ref(),
        };
        slots::write_claim(&taken.held, pool.claim_kind(), &of, console);
        slots::wake_waiters(self.root, &mut self.survey, console);
        Taking::Took(took)
    }

    /// Waits for the machine as a critical gate and takes it into `held` once it holds every heavy and unreserved light
    /// slot, then journals the take with what it found, says the holds line, writes the critical claim and wakes the
    /// waiters. When the machine was taken; else a Ctrl+C ended the wait, which says so, journals the abandon, then
    /// releases what it took so far, deletes the ticket and wakes the waiters.
    fn take_machine(&mut self, held: &mut Option<Machine>, console: &mut Console) -> Taking {
        let queued = Instant::now();
        let arrival = Arrival {
            gate: &self.gate,
            request: self.request,
            queued,
            arrived: self.arrived,
            builder: None,
            after_fallback: false,
            under: None,
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
        self.journal.write(&take, None, console);
        console.err(&line);
        let took = Instant::now();
        let of = ClaimOf {
            gate: &self.gate,
            request: self.request,
            log: self.log.path(),
            remote: None,
        };
        machine.write_claim(&of, console);
        slots::wake_waiters(self.root, &mut self.survey, console);
        Taking::Took(took)
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
    /// end on its own path) and lets the slot go, then wakes the waiters. A critical gate journals the release of every
    /// slot it held first, then lets the machine go ([`Machine::release`]). A gate a parent's slot covers deletes its
    /// covered marker; it, and a gate a light parent lent its slot, took no slot and journals no release.
    fn release(&mut self, hold: Hold, ended: Option<&Ended>, console: &mut Console) {
        if let Some(mut covered) = self.covered.take() {
            covered.end(console);
        }
        if self.nesting.covering().is_some() || self.lent {
            return;
        }
        let Hold { taken, machine, took } = hold;
        let held_seconds = took.elapsed().as_secs_f64();
        let status = ended.map(|ended| ended.status);
        let facts = ended.and_then(|ended| ended.facts);
        if let Some(machine) = machine {
            let release = JournalEvent::Release {
                slots: machine.names(),
                held_seconds,
                status,
                facts,
            };
            self.journal.write(&release, None, console);
            machine.release(self.root, &mut self.survey, console);
            return;
        }
        let slots = match &taken {
            Some(taken) => {
                slots::delete_claim(&taken.held, console);
                vec![taken.held.name().to_owned()]
            }
            None => Vec::new(),
        };
        let release = JournalEvent::Release {
            slots,
            held_seconds,
            status,
            facts,
        };
        self.journal.write(&release, self.tag.as_ref(), console);
        if let Some(taken) = taken {
            drop(taken);
            slots::wake_waiters(self.root, &mut self.survey, console);
        }
    }

    /// Starts the command in the gate's job and sees it to its end.
    fn command(&mut self, plan: Plan, console: &mut Console) -> Ended {
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
            Ok(running) => self.watched(&job, &running, &start, console),
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
    /// or panic first, the job is terminated, so the command does not outlive the run.
    fn watched(&mut self, job: &Job, running: &Running, start: &Start, console: &mut Console) -> Ended {
        // Copies of the run's own references, so the stopper holds no borrow of the run while it is watched.
        let (request, root) = (self.request, self.root);
        let recorded = RefCell::new(Recorded::default());
        let started = Started {
            job,
            running,
            recorded: Some(&recorded),
        };
        let mut stopper = Stopper {
            ends: Ender {
                reach: Reach {
                    job,
                    server_image: request.test_seams.server_image.as_deref(),
                    root: &root.root,
                },
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
    /// `recorded`.
    fn watch(
        &mut self,
        started: Started<'_>,
        recorded: &RefCell<Recorded>,
        start: &Start,
        console: &mut Console,
    ) -> Result<(Watched, Timing), String> {
        let watching = Watching {
            job: started.job,
            running: started.running,
            log: &self.log,
            interrupt: self.host.interrupt,
            runs_as: self.nesting.runs_as(self.request.slot),
            root: self.root,
            gate: &self.gate,
            hands_on: self.nesting.handed_on(self.lent),
            survey: &mut self.survey,
            recorded,
        };
        let mut watcher = Watcher::begin(self.request, watching, start, self.host.processors)
            .map_err(|error| format!("cannot begin to watch the command: {error}"))?;
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

    /// Ends the run as the watch ended it: a command that exited has its verdict; one killed as stalled, timed out or
    /// past the backstop has its diagnostics written to the log, its kill line and the pointer to them said, its job
    /// terminated and the verdict of status 124; an interrupted one is given 5 s to end before its job is stopped, and
    /// has the verdict of its own status, neither scanned for markers. A gate failure after a command's own exit ends its
    /// job, not what it left running outside it.
    fn after_watch(&mut self, watched: Watched, timing: Timing, started: Started<'_>, console: &mut Console) -> Ended {
        let started = Started {
            recorded: records_after(&watched, started.recorded),
            ..started
        };
        let Started { job, running, recorded } = started;
        match watched {
            Watched::Exited(code) => {
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
        match self.verdict(outcome.exited, Some(Pass::Here(timing)), unscanned, diagnostics, console) {
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
    /// line; the gate's status. `exited` is the command's status, and `pass` how long it ran for the passed line, `None`
    /// when it never started; a run the gate ended itself is `unscanned`, its markers not read. `diagnostics` is the
    /// block a kill wrote to the log: the log keeps it, and the tail leaves exactly its lines out.
    fn verdict(
        &mut self,
        exited: i64,
        pass: Option<Pass<'_>>,
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
        match pass {
            Some(pass) if status == 0 => {
                if let Some(line) = self.cores_hint(pass) {
                    console.note_out(&line);
                }
                console.note_out(&self.passed_line(pass, &shown));
            }
            _ => console.err(&format!("gate: FAILED (status {status}). Full output: {shown}")),
        }
        Ok(status)
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

    /// The passed line of a run that ran as `pass` says, its log shown as `shown`.
    fn passed_line(&self, pass: Pass<'_>, shown: &str) -> String {
        passed_line(pass, self.request.timeout, shown)
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

    /// What a kill of the command in `job` reaches: the job, the one more compiler server image it leaves running
    /// (`--test-server-image`), as the watch reads it, and the state root that names a gate's job.
    fn reach<'s>(&'s self, job: &'s Job) -> Reach<'s> {
        Reach {
            job,
            server_image: self.request.test_seams.server_image.as_deref(),
            root: &self.root.root,
        }
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
        if let Some(Started { job, running, recorded }) = started {
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

/// The command's started job and process, and the processes outside the job its watch records, which every end of the
/// run but the command's own exit ends: `None` once the command has exited.
#[derive(Clone, Copy)]
struct Started<'a> {
    job: &'a Job,
    running: &'a Running,
    recorded: Option<&'a RefCell<Recorded>>,
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
/// `recorded` with them, but the build servers, and gives the command a moment to end.
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
/// it, those the watch `recorded` with them, but the build servers, and gives the command a moment to end.
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
/// outside the job are ended with those the first look found. The build servers outside the job are left running.
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
/// the kill's own, so the line says it ran past exactly when the kill would have fired.
#[expect(
    clippy::cast_precision_loss,
    reason = "the ceiling is read as the double the clock counts in, as the ceiling kill reads it"
)]
fn passed_line(pass: Pass<'_>, ceiling: u64, shown: &str) -> String {
    match pass {
        Pass::Here(timing) => {
            let (adjusted, over) = match timing.adjusted {
                Some(adjusted) => (format!("{} s", whole_seconds(adjusted)), adjusted >= ceiling as f64),
                None => ("unknown: the sampler failed".to_owned(), timing.wall.as_secs_f64() >= ceiling as f64),
            };
            let ceiling = if over {
                format!("past its ceiling of {ceiling} s")
            } else {
                format!("ceiling {ceiling} s")
            };
            format!(
                "gate: passed in {} s (load-adjusted {adjusted}, {ceiling}). Full output: {shown}",
                whole_seconds(timing.wall.as_secs_f64()),
            )
        }
        Pass::On(host, wall) => format!(
            "gate: passed in {} s on {host} (wall clock, ceiling {ceiling} s). Full output: {shown}",
            whole_seconds(wall.as_secs_f64())
        ),
    }
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

/// Whether a run under `nesting` took a slot of its own: not when a parent's slot covers it, nor when a light parent
/// lent it its slot (`lent`). A run here after a fallback from a builder takes its own local slot like any other.
fn takes_own_slot(nesting: &Nesting, lent: bool) -> bool {
    nesting.covering().is_none() && !lent
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
    use std::cell::RefCell;
    use std::fs;
    use std::io;
    use std::process::Command;

    use std::time::Duration;

    use gate_args::Slot;
    use gate_watchdog::{Cores, Hint};

    use super::{
        Ends, Judged, Pass, Stopper, Timing, brought_everything_back, cores_hint_for, cores_line, earlier, interrupted, kill, log_in_state_root,
        passed_line, records_after, stop, takes_own_slot,
    };
    use crate::console::Console;
    use crate::escaped::{Found, Look, Reached, Recorded};
    use crate::nesting::{Nesting, Parent};
    use crate::remote::RemoteOutcome;
    use crate::watch::Watched;

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
        recorded.borrow_mut().record(look, |_| false);
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
        };
        let rows: [(&str, Nesting, bool, bool); 6] = [
            ("covered by its parent's slot", Nesting::Covered(parent.clone()), false, false),
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
    fn a_passed_line_on_a_builder_keeps_the_wall_clock_and_the_ceiling_alone() {
        assert_eq!(
            passed_line(Pass::On("builder", Duration::from_secs(41)), 1, "out.log"),
            "gate: passed in 41 s on builder (wall clock, ceiling 1 s). Full output: out.log"
        );
    }

    #[test]
    fn a_log_reached_through_a_junction_into_the_state_root_is_inside_it() {
        let folder = std::env::temp_dir().join(format!("gate-run-junction-{}", std::process::id()));
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
