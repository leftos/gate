//! `gate run`: a log inside the state root refused, the program found and the words written into its command line, a
//! batch word refused, the log's mutex taken so no other gate writes the same log, the log readied,
//! a slot queued for and taken, the command started in its job with its environment and priority and waited for, the
//! log completed, the failure markers scanned, the tail and the verdict line said, and the slot released.

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, io};

use gate_args::{ProgramSearch, RunRequest, Slot, batch_command_line, batch_refusal, command_line, is_batch_target, resolve_program};
use gate_state::{JournalEvent, KernelObject, RunFacts, StateRoot, Verdict, log_hash};
use gate_win32::{AcquireError, Job, Launch, LaunchError, NamedMutex, OwnedMutexGuard, PriorityClass, Running};

use crate::acquire;
use crate::answer::USAGE_STATUS;
use crate::console::Console;
use crate::environment::{self, Variables};
use crate::journal::RunJournal;
use crate::log::Log;
use crate::resolve::ResolvedRoot;
use crate::slots::{self, Pool, Survey, Taken, ThisGate};
use crate::wait::Wait;

/// The status of a run the gate itself failed.
const GATE_FAILED: i64 = 1;
/// The status of a command that exited 0 with a failure marker in its log.
const LOG_FAILURE: i64 = 1;
/// The status of a command that could not be started.
const NOT_STARTED: i64 = 127;
/// The exit code the job's processes are ended with when the gate stops the command.
const STOPPED: u32 = 124;
/// The Win32 error of a file not found, which a program found nowhere is reported as.
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
/// How long one wait for the command's exit lasts before the gate waits again.
const WAIT_STEP: Duration = Duration::from_secs(60);
/// How long the gate waits for the command once it has terminated its job after a failure of its own.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// Runs `request`'s command, logging to `log` (absolute) under `root`, saying each line as it goes; returns the gate's
/// exit status: the command's own whole 32-bit code, 1 for a failure marker in its log or a failure of the gate after
/// it started, 127 when it could not be started, or 2 for a log inside the state root or a batch word refused before
/// anything was queued.
pub(crate) fn run(request: &RunRequest, log: &Path, root: &ResolvedRoot, console: &mut Console) -> u32 {
    if log_in_state_root(log, &root.path) {
        console.err(&format!(
            "gate: the log {} is inside the gate's state folder {}",
            log.display(),
            root.path.display()
        ));
        return u32::from(USAGE_STATUS);
    }
    let gate = ThisGate::read(request);
    let log = Log::new(log);
    let plan = match Plan::prepare(request, root, &gate, &log, console) {
        Ok(plan) => plan,
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
    let mut run = Run {
        request,
        root,
        survey: Survey::new(&gate),
        journal: RunJournal::open(root, &gate),
        gate,
        log,
    };
    let queue = JournalEvent::Queue {
        log: run.log.path().to_string_lossy().into_owned(),
    };
    run.journal.write(&queue, console);
    let mut taken = None;
    let mut took = Instant::now();
    let watched = panic::catch_unwind(AssertUnwindSafe(|| {
        took = run.take(&mut taken, console);
        run.command(plan, console)
    }));
    let ended = match watched {
        Ok(ended) => ended,
        Err(payload) => {
            run.after_panic(taken, took, console);
            panic::resume_unwind(payload);
        }
    };
    run.release(taken, took, Some(&ended), console);
    if let Err(error) = run.log.append(&console.take_notes()) {
        console.err(&format!("gate: cannot write to {}: {error}", run.log.path().display()));
    }
    exit_status(ended.status)
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
fn resolved(path: &Path) -> PathBuf {
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
fn is_within(path: &Path, folder: &Path) -> bool {
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
    /// Finds the program, writes the command line and builds the environment, in the caller's working folder.
    ///
    /// # Errors
    ///
    /// A refused batch word (status 2), or a working folder or environment the gate cannot read or pass on (status 1).
    fn prepare(request: &RunRequest, root: &ResolvedRoot, gate: &ThisGate, log: &Log, console: &mut Console) -> Result<Plan, NotRun> {
        let folder = std::env::current_dir().map_err(|error| NotRun::failed(format!("gate: cannot read the working directory: {error}")))?;
        let variables = Variables::for_command(root, &environment::slot_marker(request.slot.as_str(), gate, &root.root));
        let environment = variables
            .block()
            .map_err(|error| NotRun::failed(format!("gate: cannot build the command's environment: {error}")))?;
        let Some((first, rest)) = request.command.split_first() else {
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
                log: log.path().to_owned(),
                err_log: log.err_path().to_owned(),
                priority: priority(request.slot, console),
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

/// The command's priority class: high for a critical gate; otherwise below normal, unless the gate's own class is
/// already below normal or idle, which the command then inherits.
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
    gate: ThisGate,
    log: Log,
    journal: RunJournal,
    survey: Survey,
}

/// How the run ended: the gate's status, and the facts its release line carries when its verdict was reached.
struct Ended {
    status: i64,
    facts: Option<RunFacts>,
}

impl Run<'_> {
    /// Waits its turn for a slot of the run's pool and takes it into `held` the moment it has it, then journals the
    /// take and writes the claim; a critical run takes no pool slot. When the slot was taken.
    fn take(&mut self, held: &mut Option<Taken>, console: &mut Console) -> Instant {
        let queued = Instant::now();
        let Some(pool) = Pool::of(self.request.slot) else {
            self.journal.write(&take_event(Vec::new(), 0.0, Vec::new()), console);
            return Instant::now();
        };
        let mut wait = Wait::queue(self.root, pool, &self.gate, self.request, queued, console);
        let taken = held.insert(wait.until_taken(&mut self.survey, console));
        let waited = queued.elapsed().as_secs_f64();
        let name = taken.held.name().to_owned();
        let mut abandoned = Vec::new();
        if taken.abandoned {
            acquire::say_taken_abandoned(console, &name);
            abandoned.push(name.clone());
        }
        wait.end(console);
        let took = Instant::now();
        self.journal.write(&take_event(vec![name], waited, abandoned), console);
        slots::write_claim(&taken.held, pool, &self.gate, self.request, self.log.path(), console);
        slots::wake_waiters(self.root, &mut self.survey, console);
        took
    }

    /// After a panic from the moment the slot was taken: completes the log as far as it can, the `.err` file and the
    /// notes so far, then releases the slot with the run's end unknown. The job of a started command was terminated
    /// as the panic left its watch.
    fn after_panic(&mut self, taken: Option<Taken>, took: Instant, console: &mut Console) {
        if let Err(error) = self.log.finish(console) {
            console.err(&format!(
                "gate: could not write to {} after the failure: {error}",
                self.log.path().display()
            ));
        }
        self.release(taken, took, None, console);
    }

    /// Deletes the claim, journals the release with how the run `ended` (status and verdict unknown when it did not
    /// end on its own path) and lets the slot go, then wakes the waiters.
    fn release(&mut self, taken: Option<Taken>, took: Instant, ended: Option<&Ended>, console: &mut Console) {
        let held_seconds = took.elapsed().as_secs_f64();
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
            status: ended.map(|ended| ended.status),
            facts: ended.and_then(|ended| ended.facts),
        };
        self.journal.write(&release, console);
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
        let name = self.root.root.object_name(KernelObject::Job { gate_pid: self.gate.pid });
        let job = match Job::create(&name) {
            Ok(job) => job,
            Err(error) => return self.gate_failed(&format!("cannot create the job {name}: {error}"), None, console),
        };
        let started = Instant::now();
        match launch.start_suspended().and_then(|suspended| suspended.assign_and_resume(&job)) {
            Ok(running) => self.watched(&job, &running, started, console),
            Err(LaunchError::CreateProcess { message, .. }) => self.not_started(&plan.word, plan.resolved.as_deref(), &message, console),
            Err(error) => self.gate_failed(&error.to_string(), None, console),
        }
    }

    /// Waits for the started command to exit, then reaches the verdict. Should the gate fail or panic first, the job is
    /// terminated, so the command does not outlive the run.
    fn watched(&mut self, job: &Job, running: &Running, started: Instant, console: &mut Console) -> Ended {
        let mut stopper = Stopper { job, armed: true };
        let ended = match wait_for_exit(running) {
            Ok(code) => {
                let wall = started.elapsed();
                let exited = i64::from(code.cast_signed());
                match self.verdict(exited, Some(wall), console) {
                    Ok(status) => Ended {
                        status,
                        facts: Some(RunFacts {
                            wall_seconds: wall.as_secs_f64(),
                            adjusted_seconds: None,
                            verdict: Verdict::of(None, exited, status),
                        }),
                    },
                    Err(problem) => self.gate_failed(&problem, Some((job, running)), console),
                }
            }
            Err(error) => self.gate_failed(&format!("cannot wait for the command: {error}"), Some((job, running)), console),
        };
        stopper.armed = false;
        ended
    }

    /// Says the start failure of `word`, found as `resolved`, with the system's `message`, and ends with status 127.
    fn not_started(&mut self, word: &str, resolved: Option<&str>, message: &str, console: &mut Console) -> Ended {
        let line = match resolved.filter(|resolved| *resolved != word) {
            Some(resolved) => format!("gate: could not start {word} ({resolved}): {message}"),
            None => format!("gate: could not start {word}: {message}"),
        };
        console.err(&line);
        match self.verdict(NOT_STARTED, None, console) {
            Ok(status) => Ended { status, facts: None },
            Err(problem) => self.gate_failed(&problem, None, console),
        }
    }

    /// Completes the log with the notes so far, and says the marker lines of a failure, the log's tail and the verdict
    /// line; the gate's status. `exited` is the command's status, and `wall` how long it ran, `None` when it never
    /// started.
    fn verdict(&mut self, exited: i64, wall: Option<Duration>, console: &mut Console) -> Result<i64, String> {
        let shown = self.log.path().display().to_string();
        self.log
            .finish(console)
            .map_err(|error| format!("cannot complete the log {shown}: {error}"))?;
        let (found, tail) = self.read_back().map_err(|error| format!("cannot read the log {shown}: {error}"))?;
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
        match wall {
            Some(wall) if status == 0 => console.note_out(&format!(
                "gate: passed in {} s (ceiling {} s). Full output: {shown}",
                whole_seconds(wall),
                self.request.timeout
            )),
            _ => console.err(&format!("gate: FAILED (status {status}). Full output: {shown}")),
        }
        Ok(status)
    }

    /// The log's first marker lines, none when the run turned the scan off, and its tail.
    fn read_back(&self) -> io::Result<(Vec<Marker>, Vec<String>)> {
        let found = if self.request.no_markers { Vec::new() } else { self.log.markers()? };
        Ok((found, self.log.tail(self.request.tail)?))
    }

    /// Says the gate's own failure, terminates the job of a command that started, and completes the log; status 1.
    fn gate_failed(&mut self, problem: &str, started: Option<(&Job, &Running)>, console: &mut Console) -> Ended {
        console.err(&format!("gate: the gate failed while running the command: {problem}"));
        if let Some((job, running)) = started {
            stop(job, running, console);
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

/// Terminates the job when dropped armed: on a return or a panic the run did not plan for after the command started.
struct Stopper<'a> {
    job: &'a Job,
    armed: bool,
}

impl Drop for Stopper<'_> {
    fn drop(&mut self) {
        if self.armed {
            // The run is already leaving on a failure, with nowhere left to report a failed terminate to.
            let _ = self.job.terminate(STOPPED);
        }
    }
}

/// Terminates the job after a failure of the gate's own, and gives the command a moment to end.
fn stop(job: &Job, running: &Running, console: &mut Console) {
    if let Err(error) = job.terminate(STOPPED) {
        console.err(&format!("gate: and could not terminate the command's job: {error}"));
        return;
    }
    match running.wait(STOP_WAIT) {
        Ok(Some(_)) => {}
        Ok(None) => console.err("gate: the command was still running 5 s after its job was terminated"),
        Err(error) => console.err(&format!("gate: cannot wait for the command after terminating its job: {error}")),
    }
}

/// Waits for the command to exit; its exit code.
fn wait_for_exit(running: &Running) -> io::Result<u32> {
    loop {
        if let Some(code) = running.wait(WAIT_STEP)? {
            return Ok(code);
        }
    }
}

/// `wall` in whole seconds, rounded half to even as the PowerShell gate's `[math]::Round` rounds.
fn whole_seconds(wall: Duration) -> f64 {
    wall.as_secs_f64().round_ties_even()
}

/// A line holding a failure marker: its number, counted from 1, and its text as printed.
type Marker = (usize, String);

fn take_event(slots: Vec<String>, waited_seconds: f64, abandoned: Vec<String>) -> JournalEvent {
    JournalEvent::Take {
        slots,
        waited_seconds,
        abandoned,
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders must fail loudly")]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::log_in_state_root;

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
