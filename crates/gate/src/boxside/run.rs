//! `gate box run`: the work folder's lock taken, the subfolder checked, the snapshot checked out, the command started
//! in the payload's job with its output forwarded, the tree it left committed back, and the command's status passed
//! whole, as the PowerShell wrapper's `Invoke-Run` does. git starts through [`gate_win32::spawn`], under the spawn lock
//! the command's own launch holds while its inheritable handle copies are open, so no git inherits a handle meant for
//! the command. The command runs on a console whose code pages are UTF-8 ([`Utf8Console`]), or, when it cannot have
//! one, on the box's default code page, which the box says and never fails the run for.
//!
//! The payload is data: it is refused unless well formed, and runs nothing but its command and writes nothing but the
//! work folder's own files. The command, and whatever it writes into the work folder, `.git` included, is the user's
//! own code running as the user, and is trusted. The box's git runs with no hooks and no file-system monitor only so
//! that a repository's own hooks or monitor, such as one a build installed, do not run in it.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::BoxSeams;
use gate_watchdog::Pump;
use gate_win32::{Donor, Job, LaunchError, NamedEvent, PipedConsole, PipedLaunch, PipedSuspended, Running, Utf8Console};

use crate::answer::USAGE_STATUS;
use crate::boxside::forward::{DRAIN, Forwarders, OutputLock};
use crate::boxside::heartbeat;
use crate::boxside::payload::{Payload, ordinal_upper};
use crate::boxside::preflight::{Failed, GIT_REDIRECTS, Git, exit_code};
use crate::boxside::stop::{Ending, STOPPED, stop_event_name, terminate_and_wait};
use crate::console::Console;
use crate::environment::Variables;
use crate::run::{Written, find_program, written};

/// The status of a run refused because another run holds the work folder.
const BUSY_STATUS: u32 = 75;
/// The status of a run the box failed on its own, and of a passed command whose tree could not be committed back.
const FAILED_STATUS: u32 = 1;
/// The status of a command that could not be started.
const NOT_STARTED: u32 = 127;
/// The status of a run whose client closed the connection: the box stopped the job and exits as a lost connection does.
const CLOSED_STATUS: u32 = 255;
/// How long one wait for the command's exit lasts before the box looks again whether the client is still there.
const WAIT_STEP: Duration = Duration::from_millis(100);
/// How often the park seam looks for its path, and how long it looks before the launch goes on.
const PARK_STEP: Duration = Duration::from_millis(20);
const PARK_LIMIT: Duration = Duration::from_secs(600);
/// The reason the console seam gives for the failure it forces.
const FORCED: &str = "the test seam --test-box-console forced the failure";
/// The Win32 errors of an open refused because another process has the file open.
const SHARING_VIOLATION: i32 = 32;
const LOCK_VIOLATION: i32 = 33;
/// The Win32 error `CreateProcessW` gives when the box's own job refuses breakaway, among other refusals.
const ERROR_ACCESS_DENIED: u32 = 5;
/// `FILE_FLAG_OPEN_REPARSE_POINT`: opens a link or junction itself, never its target.
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// The identity variables the payload may set for the back commit.
const IDENTITY_NAMES: [&str; 6] = [
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
];

/// What a run reads of its payload.
struct Request<'a> {
    key: &'a str,
    snapshot: &'a str,
    bare: &'a str,
    work: &'a str,
    lock: &'a str,
    index: &'a str,
    job: &'a str,
    command: Vec<String>,
    subfolder: &'a str,
    identity: Vec<(String, String)>,
}

impl Request<'_> {
    fn read(payload: &Payload) -> Result<Request<'_>, String> {
        // The snapshot first, as the wrapper refuses a payload without one before the lock or the checkout.
        let snapshot = payload.snapshot()?;
        let (key, work) = payload.work_folder()?;
        Ok(Request {
            key,
            snapshot,
            bare: payload.path("Bare")?,
            work,
            lock: payload.beside_work("Lock", work, "lock")?,
            index: payload.beside_work("Index", work, "index")?,
            job: payload.job(key)?,
            command: payload.words("Command")?,
            subfolder: payload.optional_text("Subfolder")?,
            identity: identity(payload)?,
        })
    }
}

/// The payload's `Identity`: only the git author and committer variables, none holding a NUL; else the usage line.
fn identity(payload: &Payload) -> Result<Vec<(String, String)>, String> {
    let pairs = payload.pairs("Identity")?;
    for (name, value) in &pairs {
        if !IDENTITY_NAMES.contains(&name.as_str()) {
            return Err(format!(
                "gate: box payload Identity may set only GIT_AUTHOR_* and GIT_COMMITTER_* names: {}",
                name.escape_debug()
            ));
        }
        if value.contains('\0') {
            return Err(format!("gate: box payload Identity {name} holds a NUL"));
        }
    }
    Ok(pairs)
}

/// Runs the payload's command in its work folder and returns the status: the command's own whole 32-bit code, 127
/// when it could not be started, 75 when another run holds the work folder, 2 for a payload or a word refused, a
/// failing git step's status, 1 when the box failed or the tree of a passed command could not be committed back, or
/// 255 when the client closed the connection and the box stopped the job. Once the subfolder is checked, a heartbeat
/// goes to stderr every period, 5 s unless `seams` set it, until the run ends; `seams` may also make the command's
/// console fail, or park its launch.
pub(crate) fn run(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u32 {
    let request = match Request::read(payload) {
        Ok(request) => request,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    // Held until the run returns; the command does not inherit it, so no process it leaves running keeps the work
    // folder busy.
    let _lock = match take_lock(request.lock) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            console.err(&format!("gate: remote work folder {} is busy", request.key));
            return BUSY_STATUS;
        }
        Err(line) => {
            console.err(&line);
            return FAILED_STATUS;
        }
    };
    // Made as soon as the lock is held and kept for the run, so a stop that comes before the job exists, during the
    // checkout, is kept for the look just before the job is created.
    let stop = match create_stop_event(request.job, console) {
        Ok(stop) => stop,
        Err(status) => return status,
    };
    match in_work_folder(&request, &stop, seams, console) {
        Ok(status) | Err(status) => status,
    }
}

/// What the run writes besides its console lines: the lock every line on the box's own output takes, and the
/// heartbeat pump writing under it, `None` when it could not be started; the seams that may make the command's
/// console fail; and the job's stop signal, made once the work folder's lock is held.
struct Beat<'a> {
    lock: &'a OutputLock,
    pump: Option<&'a Pump>,
    seams: &'a BoxSeams,
    stop: &'a NamedEvent,
}

impl Beat<'_> {
    /// Has the heartbeat count the CPU of `job` and the box's build servers from now on.
    fn watch(&self, job: &Arc<Job>) {
        if let Some(pump) = self.pump {
            pump.watch(heartbeat::job_cpu(Arc::clone(job)));
        }
    }

    /// Has the heartbeat write every period again.
    fn unwatch(&self) {
        if let Some(pump) = self.pump {
            pump.unwatch();
        }
    }
}

/// The run once the lock is held: `Ok` with the status after the back commit, or `Err` with the status of a run that
/// ended before it, its lines said. The heartbeat starts once the subfolder is checked and stops on every way out.
fn in_work_folder(request: &Request<'_>, stop: &NamedEvent, seams: &BoxSeams, console: &mut Console) -> Result<u32, u32> {
    let git = WorkGit::ready(request);
    // Refused before the checkout, so a payload naming a folder outside the work folder changes nothing there; the
    // earlier run's back ref goes, as the checkout's would, so the client finds none to fetch.
    let Some(place) = subfolder_path(request.work, request.subfolder) else {
        console.err(&format!(
            "gate: remote subfolder {} is not a folder inside the work folder",
            request.subfolder
        ));
        git.clear_back_ref(request).map_err(|failed| said(console, &failed))?;
        return Err(u32::from(USAGE_STATUS));
    };
    let lock = OutputLock::default();
    // A heartbeat that cannot start ends the heartbeats, never the run.
    let pump = heartbeat::start(&lock, heartbeat::period(seams))
        .inspect_err(|error| console.err(&format!("gate: remote heartbeat stopped: cannot start the heartbeat: {error}")))
        .ok();
    let beat = Beat {
        lock: &lock,
        pump: pump.as_ref(),
        seams,
        stop,
    };
    let ran = checked_out(request, &git, &place, &beat, console);
    if let Some(pump) = pump {
        // Bounded, so a client that stopped reading cannot keep the work folder's lock held. Nothing is said for a
        // pump left stuck or one that panicked: the stuck stream is the one it would be said on, and a panicking pump
        // has already said so itself whenever it still could. The run's status stands either way.
        let _ = pump.stop_within(DRAIN);
    }
    ran
}

/// The run once the heartbeat beats: the snapshot checked out, the command seen to its end in `place`, and the tree it
/// left committed back; the status as [`in_work_folder`] gives it.
fn checked_out(request: &Request<'_>, git: &WorkGit, place: &Path, beat: &Beat<'_>, console: &mut Console) -> Result<u32, u32> {
    git.checkout(request).map_err(|failed| said(console, &failed))?;
    if !place.is_dir() {
        console.err(&format!("gate: remote subfolder {} does not exist in the snapshot", request.subfolder));
        return Err(u32::from(USAGE_STATUS));
    }
    let mut status = command(request, place, beat, console)?;
    if let Err(message) = git.back_commit(request, console) {
        console.err(&format!("gate: remote sync-back failed: {message}"));
        if status == 0 {
            status = FAILED_STATUS;
        }
    }
    Ok(status)
}

/// Says the failed step's line; its status.
fn said(console: &mut Console, failed: &Failed) -> u32 {
    console.err(&failed.line);
    failed.status
}

/// The work folder's lock: `path` opened for reading and writing, created when missing, shared with no one, never
/// through a link, and only as a plain disk file. `None` when another process has it open; else the line saying why it
/// could not be opened.
fn take_lock(path: &str) -> Result<Option<File>, String> {
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    let cannot = |error: io::Error| format!("gate: remote could not open the work folder lock {path}: {error}");
    match opened {
        Ok(lock) => gate_win32::check_plain_file(&lock).map(|()| Some(lock)).map_err(cannot),
        Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => Ok(None),
        Err(error) => Err(cannot(error)),
    }
}

/// The folder the command runs in, as a full path: `sub` below `work`, the work folder itself for an empty `sub`;
/// `None` when `sub` is rooted (a drive, a drive-relative path or a leading separator) or climbs out of `work`. Names
/// are compared as .NET's `OrdinalIgnoreCase` compares them, each character's simple uppercase.
fn subfolder_path(work: &str, sub: &str) -> Option<PathBuf> {
    let work = full_path(Path::new(work))?;
    if sub.is_empty() {
        return Some(work);
    }
    let rooted = sub.starts_with(['\\', '/']) || sub.as_bytes().get(1) == Some(&b':');
    if rooted {
        return None;
    }
    let full = full_path(&work.join(sub))?;
    let (work_text, full_text) = (ordinal_upper(&work.to_string_lossy()), ordinal_upper(&full.to_string_lossy()));
    if full_text == work_text {
        Some(work)
    } else if full_text.starts_with(&format!("{work_text}\\")) {
        Some(full)
    } else {
        None
    }
}

/// `path` made a full path as Windows reads it, `.` and `..` folded and separators written `\`, without a trailing
/// separator; `None` for a path Windows cannot make full, which no folder inside the work folder is.
fn full_path(path: &Path) -> Option<PathBuf> {
    let full = std::path::absolute(path).ok()?;
    let text = full.to_string_lossy();
    let trimmed = text.trim_end_matches('\\');
    // A drive's root keeps its separator.
    Some(if trimmed.ends_with(':') { full } else { PathBuf::from(trimmed) })
}

/// Starts the command in `place` and sees it to its end: `Ok` with its status (127 when it could not be started), or
/// `Err` with the status of a run that ends here, its line said: 2 for a batch word refused, 1 for the box's own
/// failure or a stop that came first, 255 for a client that closed the connection. The job's stop signal is looked at
/// just before the job is created ([`before_the_job`]), so a stop that came while the snapshot was checked out starts
/// nothing. The command starts on a UTF-8 console when it can have one
/// ([`start_command`]), whose donor lives until the wait for the command is over, however it ends. The heartbeat
/// watches the job while [`watch`] waits for it.
fn command(request: &Request<'_>, place: &Path, beat: &Beat<'_>, console: &mut Console) -> Result<u32, u32> {
    let variables = Variables::from_process();
    let environment = variables
        .block()
        .map_err(|error| failed(console, &format!("cannot build the command's environment: {error}")))?;
    let Some((word, rest)) = request.command.split_first() else {
        return Err(failed(console, "the run has no command"));
    };
    let rest: Vec<OsString> = rest.iter().map(OsString::from).collect();
    let program = match find_program(word, &variables, place) {
        Ok(program) => program,
        Err(message) => return Ok(not_started(console, word, None, &message)),
    };
    let launch = match written(&program, &rest, &variables) {
        Written::Refused(line) => {
            console.err(&line);
            return Err(u32::from(USAGE_STATUS));
        }
        Written::NoShell(message) => return Ok(not_started(console, word, Some(&program), &message)),
        Written::Line(application, command_line) => PipedLaunch {
            application,
            command_line,
            directory: place.to_owned(),
            environment,
            console: PipedConsole::Hidden,
        },
    };
    before_the_job(|| beat.stop.is_set(), request.job, console)?;
    let job = create_job(request.job, console)?;
    // Started right after the job and kept until the command's wait is over, so its console outlives the command.
    let donor = console_donor(&job, beat.seams);
    if let Some(park) = beat.seams.park_launch.as_deref() {
        wait_for_path(park, console);
    }
    let mut start = Start {
        launch,
        job: &job,
        stop: beat.stop,
        word,
        program: &program,
    };
    let launched = match start_command(&mut start, donor.as_ref().map_err(String::as_str), beat, console) {
        Ok(launched) => launched,
        Err(answer) => return answer,
    };
    let mut stopper = Stopper { job: &job, armed: true };
    let watched = watch(&job, &launched.running, launched.pipes, beat, console);
    stopper.armed = false;
    // Ended only now, so the command's console stays up for as long as the command may use it.
    drop(donor);
    watched
}

/// The command to start: its launch, its job and the job's stop signal, and the word and program a start failure names.
struct Start<'a> {
    launch: PipedLaunch,
    job: &'a Job,
    stop: &'a NamedEvent,
    word: &'a str,
    program: &'a Path,
}

/// A command started in its job, and the read ends of its pipes.
struct Launched {
    running: Running,
    pipes: Pipes,
}

/// The console's donor, put in `job`, or why there is none: the console seam forces that before any donor starts.
fn console_donor(job: &Job, seams: &BoxSeams) -> Result<Donor, String> {
    if seams.fail_console {
        return Err(FORCED.to_owned());
    }
    Donor::start(job)
}

/// Starts the command on `donor`'s UTF-8 console, or, when the console cannot be had (`donor` may say why none
/// started), on the plain hidden console, which is said and never fails the run. A stop signalled before the command
/// is resumed keeps it from starting ([`launch`]), which stops the job and fails the run with status 1. What the launch
/// has to say is said once it is over and the box is home ([`Ended::say`]). The command started, or the run's answer:
/// `Ok(127)` for a command that could not be started, `Err` with the status of the box's own failure, its line said.
fn start_command(start: &mut Start<'_>, donor: Result<&Donor, &str>, beat: &Beat<'_>, console: &mut Console) -> Result<Launched, Result<u32, u32>> {
    let ended = match donor {
        Ok(donor) => on_utf8_console(start, donor, beat, console),
        Err(reason) => Ended {
            fallback: Some(reason.to_owned()),
            ..launch(start, PipedConsole::Hidden)
        },
    };
    ended.say(console);
    match ended.how {
        LaunchEnd::Started(launched) => Ok(launched),
        LaunchEnd::Refused(error) => Err(start_refused(console, start, error)),
        LaunchEnd::Stopped => Err(Err(stopped_before_start(start.job, console))),
    }
}

/// The launch on `donor`'s UTF-8 console, the box's own output held from just before the box leaves its console until
/// it is home again, since its standard handles may point at the donor's console meanwhile; on the plain hidden
/// console when the UTF-8 one cannot be had, as when the output's lock is not free within the console's wait. How the
/// launch ended, with why the UTF-8 console could not be had, for the caller to say.
fn on_utf8_console(start: &mut Start<'_>, donor: &Donor, beat: &Beat<'_>, console: &mut Console) -> Ended {
    let lock = beat.lock;
    let hold = || match lock.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    };
    match Utf8Console::enter(donor, hold) {
        Ok(utf8) => {
            let ended = launch(start, PipedConsole::Attached);
            if let Err(problem) = utf8.leave() {
                console.err(&format!("gate: remote {problem}"));
            }
            ended
        }
        Err(reason) => Ended {
            fallback: Some(reason),
            ..launch(start, PipedConsole::Hidden)
        },
    }
}

/// How a launch ended, and what the box says of it once it is home: never while it is away on the donor's console,
/// where the client does not hear it.
struct Ended {
    how: LaunchEnd,
    /// Why the command could not have a UTF-8 console, when it was launched on the plain hidden one.
    fallback: Option<String>,
    /// The line saying the job's stop signal could not be read, when it could not.
    unread: Option<String>,
}

impl Ended {
    /// Says the fallback, unless the launch was stopped, so no fallback line comes before a stop line; then that the
    /// stop signal could not be read.
    fn say(&self, console: &mut Console) {
        if let Some(reason) = self.fallback.as_deref().filter(|_| !matches!(self.how, LaunchEnd::Stopped)) {
            say_fallback(console, reason);
        }
        if let Some(line) = &self.unread {
            console.err(line);
        }
    }
}

/// How a launch ended.
enum LaunchEnd {
    /// The command runs in its job.
    Started(Launched),
    /// The command could not be created, put in its job or resumed.
    Refused(LaunchError),
    /// A stop was signalled before the command was resumed: the command never ran.
    Stopped,
}

/// Creates the command suspended on the console `on`, puts it in its job, then, unless the job's stop signal says a
/// stop came first ([`look`]), resumes it. A stop that comes after the look terminates the job, the command in it.
/// Nothing is said here, as the box may be away on the donor's console: a signal that could not be read is in the
/// [`Ended`], for the caller to say once the box is home.
fn launch(start: &mut Start<'_>, on: PipedConsole) -> Ended {
    start.launch.console = on;
    let ended = |how, unread| Ended { how, fallback: None, unread };
    let PipedSuspended { suspended, stdout, stderr } = match start.launch.start_suspended(start.job) {
        Ok(created) => created,
        Err(error) => return ended(LaunchEnd::Refused(error), None),
    };
    let assigned = suspended.assign(start.job);
    // Whether the command went in the job or the stopped job refused it, it never runs: it is ended as it is dropped.
    let unread = match look(|| start.stop.is_set(), start.job.name()) {
        Look::Stopped => return ended(LaunchEnd::Stopped, None),
        Look::Go(unread) => unread,
    };
    let how = match assigned.and_then(|()| suspended.resume()) {
        Ok(running) => LaunchEnd::Started(Launched {
            running,
            pipes: Pipes { stdout, stderr },
        }),
        Err(error) => LaunchEnd::Refused(error),
    };
    ended(how, unread)
}

/// What a look at a job's stop signal found.
#[derive(Debug, PartialEq, Eq)]
enum Look {
    /// It is set: a stop came.
    Stopped,
    /// It is unset, or it could not be read, with the line saying so: no stop came.
    Go(Option<String>),
}

/// What `check` reads of the stop signal of the job `job`: set is a stop, unset none. A signal that cannot be read is
/// no stop, and the command starts; the line saying so is returned, not said, for the caller to say where the client
/// hears it.
fn look(check: impl FnOnce() -> io::Result<bool>, job: &str) -> Look {
    match check() {
        Ok(true) => Look::Stopped,
        Ok(false) => Look::Go(None),
        Err(error) => Look::Go(Some(format!(
            "gate: remote cannot read the stop signal of job {job}: {error}; starting the command"
        ))),
    }
}

/// Ends the run before its job `job` is created when a stop came already, as `check` reads the job's stop signal: the
/// stop line said, status 1, and nothing started. A signal that cannot be read is said, and the run goes on.
fn before_the_job(check: impl FnOnce() -> io::Result<bool>, job: &str, console: &mut Console) -> Result<(), u32> {
    match look(check, job) {
        Look::Stopped => Err(stopped_first(console, job)),
        Look::Go(unread) => {
            if let Some(line) = unread {
                console.err(&line);
            }
            Ok(())
        }
    }
}

/// Says the run failed for a stop that came before the command of the job `job` started; status 1.
fn stopped_first(console: &mut Console, job: &str) -> u32 {
    failed(console, &format!("the job {job} was stopped before the command started"))
}

/// Stops the job of a command a stop came before, then says the run failed for it; status 1. What the job still holds
/// after the stop and a later terminate that failed ([`Ending::say`]), or why it could not be stopped, is said after.
fn stopped_before_start(job: &Job, console: &mut Console) -> u32 {
    let ending = terminate_and_wait(job);
    let status = stopped_first(console, job.name());
    match ending {
        Ok(ending) => ending.say(job.name(), console),
        Err(error) => console.err(&format!("gate: and could not terminate the command's job {}: {error}", job.name())),
    }
    status
}

/// Says why the command did not start: its start failure, or the box's own failure; the run's answer.
fn start_refused(console: &mut Console, start: &Start<'_>, error: LaunchError) -> Result<u32, u32> {
    match error {
        LaunchError::CreateProcess { code, message } => start_failed(console, start.word, start.program, code, &message),
        error @ LaunchError::Setup { .. } => Err(failed(console, &error.to_string())),
    }
}

/// Says that the command runs on the box's default code page, for `reason`.
fn say_fallback(console: &mut Console, reason: &str) {
    console.err(&format!(
        "gate: remote could not give the command a UTF-8 console ({reason}); running it on the box's default code page"
    ));
}

/// The park seam's wait, before the box's output is locked: it creates the empty file `<path>.parked` as it starts, so
/// a case sees the box parked, then waits until `path` exists, for at most [`PARK_LIMIT`], so a box no case lets go
/// still ends.
fn wait_for_path(path: &Path, console: &mut Console) {
    let mut parked = path.as_os_str().to_owned();
    parked.push(".parked");
    if let Err(error) = File::create(&parked) {
        console.err(&format!("gate: remote park seam cannot create {}: {error}", Path::new(&parked).display()));
    }
    let started = Instant::now();
    while !path.exists() && started.elapsed() < PARK_LIMIT {
        thread::sleep(PARK_STEP);
    }
}

/// A `CreateProcessW` failure: the box's own when its job refuses breakaway (status 1), else the command's start
/// failure (127).
fn start_failed(console: &mut Console, word: &str, program: &Path, code: u32, message: &str) -> Result<u32, u32> {
    if code != ERROR_ACCESS_DENIED {
        return Ok(not_started(console, word, Some(program), message));
    }
    match gate_win32::breakaway_refused() {
        Ok(false) => Ok(not_started(console, word, Some(program), message)),
        Ok(true) => {
            console.err(&format!("gate: the box cannot start a command outside its own job: {message}"));
            Err(FAILED_STATUS)
        }
        Err(error) => Err(failed(
            console,
            &format!("cannot tell whether the box's own job refused the start ({message}): {error}"),
        )),
    }
}

/// Creates the stop signal of the run's job `name` ([`stop_event_name`]), unset; a name some event already has is
/// refused, so no earlier set reaches this run. Else the status 1, its line said, as for a job that cannot be created.
fn create_stop_event(name: &str, console: &mut Console) -> Result<NamedEvent, u32> {
    let event = stop_event_name(name);
    NamedEvent::create_new_manual(&event).map_err(|error| failed(console, &format!("cannot create the event {event}: {error}")))
}

/// Creates the run's job `name`; a name some job already has is refused, so a run never shares a job and a stop meant
/// for an earlier run never reaches it; shared, so the heartbeat's thread can read its CPU. Else the status 1, its line
/// said.
fn create_job(name: &str, console: &mut Console) -> Result<Arc<Job>, u32> {
    Job::create(name).map(Arc::new).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            console.err(&format!("gate: the job {name} already exists; a run's job name must be new"));
            FAILED_STATUS
        } else {
            failed(console, &format!("cannot create the job {name}: {error}"))
        }
    })
}

/// Terminates the job when dropped armed: on a panic after the command was resumed, so the command does not outlive
/// the run.
struct Stopper<'a> {
    job: &'a Job,
    armed: bool,
}

impl Drop for Stopper<'_> {
    fn drop(&mut self) {
        if self.armed {
            // The run is already leaving on a panic, with nowhere left to report a failed terminate to.
            let _ = self.job.terminate(STOPPED);
        }
    }
}

/// The read ends of the command's two pipes.
struct Pipes {
    stdout: File,
    stderr: File,
}

/// Forwards the running command's output, waits for its main process to exit, then lets the readers drain for at most
/// [`DRAIN`]; the command's exit code. A failed write to the box's own output means the client closed the connection,
/// whenever it is noticed: the command's job is stopped then, before the readers drain or after them, and
/// [`CLOSED_STATUS`] answered for it instead. Should the box fail first, the job is terminated, so the command does
/// not outlive a run that no longer watches it. The heartbeat watches the job once the readers drain its pipes, so no
/// CPU read runs while the command's output is left unread, and stops watching it once the wait ends, however it ends.
fn watch(job: &Arc<Job>, running: &Running, pipes: Pipes, beat: &Beat<'_>, console: &mut Console) -> Result<u32, u32> {
    let forwarders = match Forwarders::start(pipes.stdout, pipes.stderr, beat.lock) {
        Ok(forwarders) => forwarders,
        Err(error) => {
            let status = failed(console, &format!("cannot start reading the command's output: {error}"));
            end_job(job, console);
            return Err(status);
        }
    };
    beat.watch(job);
    let waited = wait_for_exit(job, running, &forwarders, console);
    beat.unwatch();
    let code = match waited {
        WaitEnd::Exited(code) => code,
        WaitEnd::ClientGone => {
            let ending = terminate_and_wait(job.as_ref());
            forwarders.drain(DRAIN);
            return Err(closed_connection(job, ending, console));
        }
        WaitEnd::Failed(status) => return Err(status),
    };
    let drained = forwarders.drain(DRAIN);
    if drained.pipe_closed {
        return Err(closed_connection(job, terminate_and_wait(job.as_ref()), console));
    }
    if !drained.all_ended {
        console.err("gate: remote command's output is still held open by a leftover process; stopped reading");
    }
    Ok(code)
}

/// How the wait for the command's main process ended.
enum WaitEnd {
    /// It exited with this code.
    Exited(u32),
    /// A write to the box's own output failed first: the client closed the connection.
    ClientGone,
    /// The box could not wait for it: the status 1, its line said and its job terminated.
    Failed(u32),
}

/// Waits for the command's main process to exit, looking every [`WAIT_STEP`] whether the client is still there.
fn wait_for_exit(job: &Job, running: &Running, forwarders: &Forwarders, console: &mut Console) -> WaitEnd {
    loop {
        match running.wait(WAIT_STEP) {
            Ok(Some(code)) => return WaitEnd::Exited(code),
            Ok(None) if forwarders.pipe_closed() => return WaitEnd::ClientGone,
            Ok(None) => {}
            Err(error) => {
                let status = failed(console, &format!("cannot wait for the command: {error}"));
                end_job(job, console);
                return WaitEnd::Failed(status);
            }
        }
    }
}

/// The answer to a run whose client closed the connection, its job's `ending` already decided and its readers already
/// drained: says on stderr what became of the job — `stopped` with no process left, `stopping` before why it did not
/// stop — best effort, since the stream that failed may be the one it is said on. [`CLOSED_STATUS`], which the box
/// exits with.
fn closed_connection(job: &Job, ending: io::Result<Ending>, console: &mut Console) -> u32 {
    let said = if matches!(&ending, Ok(Ending { left: 0, .. })) {
        "stopped"
    } else {
        "stopping"
    };
    console.err(&format!("gate: remote the client closed the connection; {said} job {}", job.name()));
    match ending {
        Ok(ending) => ending.say(job.name(), console),
        Err(error) => console.err(&format!("gate: remote could not stop job {}: {error}", job.name())),
    }
    CLOSED_STATUS
}

/// Terminates the job of a command the box no longer watches.
fn end_job(job: &Job, console: &mut Console) {
    if let Err(error) = job.terminate(STOPPED) {
        console.err(&format!("gate: and could not terminate the command's job {}: {error}", job.name()));
    }
}

/// Says the box's own failure; status 1.
fn failed(console: &mut Console, problem: &str) -> u32 {
    console.err(&format!("gate: the gate failed while running the command: {problem}"));
    FAILED_STATUS
}

/// Says the start failure of `word`, found as `resolved`, with the system's `message`, as `gate run` says it; 127.
fn not_started(console: &mut Console, word: &str, resolved: Option<&Path>, message: &str) -> u32 {
    let resolved = resolved.map(|path| path.to_string_lossy().into_owned());
    match resolved.filter(|resolved| resolved != word) {
        Some(resolved) => console.err(&format!("gate: could not start {word} ({resolved}): {message}")),
        None => console.err(&format!("gate: could not start {word}: {message}")),
    }
    NOT_STARTED
}

/// The failure of a fetch that left the payload's snapshot out of the work folder's repository: the wrapper's line,
/// its status 1.
fn missing_snapshot(sha: &str) -> Failed {
    Failed {
        line: format!("gate: remote snapshot {sha} is not in the work folder's repository after the fetch"),
        status: FAILED_STATUS,
    }
}

/// git as the box runs it in the work folder: none of the variables that would point it elsewhere, no standard input,
/// its hooks read from a folder that cannot exist, and no file-system monitor.
struct WorkGit {
    git: Git,
    /// `core.hooksPath=<lock>\hooks`: a folder inside the lock file, which cannot exist while the run holds the lock
    /// open sharing nothing, since a file has no children and cannot be deleted, renamed or replaced meanwhile.
    hooks: String,
}

impl WorkGit {
    /// Finds git; called only once the run holds the lock.
    fn ready(request: &Request<'_>) -> WorkGit {
        WorkGit {
            git: Git::find(),
            hooks: format!(r"core.hooksPath={}\hooks", request.lock),
        }
    }

    /// The safety options, then `args`.
    fn args<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut all = vec!["-c", self.hooks.as_str(), "-c", "core.fsmonitor=false"];
        all.extend_from_slice(args);
        all
    }

    /// Runs `step` in the work folder with `args`, its output passed through.
    fn step(&self, step: &str, request: &Request<'_>, args: &[&str]) -> Result<(), Failed> {
        let mut all = vec!["-C", request.work];
        all.extend_from_slice(args);
        self.git.step(step, &self.args(&all))
    }

    /// The snapshot the payload names, fetched from the bare repository at `refs/gate/<key>` and checked out by that
    /// commit over whatever the last run left, untracked files removed and ignored ones (build output) kept, and no
    /// back commit of an earlier run left behind. The commit, never the ref's tip, is what is checked out, so a later
    /// push of the key changes nothing this run runs. A commit the work folder's repository lacks after the fetch ends
    /// the run with the box's own line.
    fn checkout(&self, request: &Request<'_>) -> Result<(), Failed> {
        let refname = format!("refs/gate/{}", request.key);
        self.step("fetch", request, &["fetch", "-q", request.bare, &refname])?;
        let commit = format!("{}^{{commit}}", request.snapshot);
        match self.quiet(request, &["cat-file", "-e", &commit]) {
            Ok(0) => {}
            Ok(_) => return Err(missing_snapshot(request.snapshot)),
            Err(message) => {
                return Err(Failed {
                    line: format!("gate: remote cat-file failed: {message}"),
                    status: FAILED_STATUS,
                });
            }
        }
        self.step("checkout", request, &["checkout", "-q", "--force", "--detach", request.snapshot])?;
        self.step("clean", request, &["clean", "-fdq"])?;
        self.clear_back_ref(request)
    }

    /// git's exit code for `args` in the work folder, both of its streams discarded, as the wrapper discards
    /// `cat-file`'s own; else why git could not be started.
    fn quiet(&self, request: &Request<'_>, args: &[&str]) -> Result<i32, String> {
        let mut all = vec!["-C", request.work];
        all.extend_from_slice(args);
        self.git.code(&self.args(&all), Stdio::null(), Stdio::null())
    }

    /// Deletes `refs/gate-back/<key>` in the bare repository, an earlier run's back commit, so the client finds none
    /// to fetch after a run that makes none.
    fn clear_back_ref(&self, request: &Request<'_>) -> Result<(), Failed> {
        let bare = format!("--git-dir={}", request.bare);
        let back = format!("refs/gate-back/{}", request.key);
        self.git.step("back ref reset", &self.args(&[&bare, "update-ref", "-d", &back]))
    }

    /// The tree the command left, committed on `HEAD` and pushed to `refs/gate-back/<key>` in the bare repository;
    /// nothing when the tree is unchanged. It starts from a copy of the work folder's index, which the checkout just
    /// refreshed, so `add -A` skips every unchanged file; a copy that cannot be removed afterwards is said and fails
    /// nothing. Else why it failed.
    fn back_commit(&self, request: &Request<'_>, console: &mut Console) -> Result<(), String> {
        let program = self.git.program.as_ref().map_err(|message| format!("git cannot be started: {message}"))?;
        let back = BackGit {
            program,
            request,
            work: self,
        };
        let index = Path::new(request.work).join(".git").join("index");
        fs::copy(&index, request.index).map_err(|error| format!("cannot copy {} to {}: {error}", index.display(), request.index))?;
        let tree = back
            .status("add", &["add", "-A"], true)
            .and_then(|()| back.line("write-tree", &["write-tree"], true));
        match fs::remove_file(request.index) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                console.err(&format!("gate: remote could not remove {}: {error}", request.index));
            }
            _ => {}
        }
        let tree = tree?;
        if tree == back.line("rev-parse", &["rev-parse", "HEAD^{tree}"], false)? {
            return Ok(());
        }
        let commit = back.line("commit-tree", &["commit-tree", &tree, "-p", "HEAD", "-m", "gate-remote back"], false)?;
        let target = format!("{commit}:refs/gate-back/{}", request.key);
        back.status("push", &["push", "-q", request.bare, &target], false)
    }
}

/// git as the back commit runs it: in the work folder as [`WorkGit`] runs it, with the payload's identity.
struct BackGit<'a> {
    program: &'a Path,
    request: &'a Request<'a>,
    work: &'a WorkGit,
}

impl BackGit<'_> {
    /// git with `args`, using the copied index when `copied_index` is set.
    fn command(&self, args: &[&str], copied_index: bool) -> Command {
        let mut command = Command::new(self.program);
        command.envs(self.request.identity.iter().map(|(name, value)| (name, value)));
        // After the identity, so no identity name can point git elsewhere even if the payload check missed one.
        for name in GIT_REDIRECTS {
            command.env_remove(name);
        }
        if copied_index {
            command.env("GIT_INDEX_FILE", self.request.index);
        }
        command.args(self.work.args(args)).current_dir(self.request.work).stdin(Stdio::null());
        command
    }

    /// Runs `step`, its output passed through; else why it failed.
    fn status(&self, step: &str, args: &[&str], copied_index: bool) -> Result<(), String> {
        // Started under the spawn lock, so it inherits no launch's handles.
        let status = gate_win32::spawn(&mut self.command(args, copied_index))
            .and_then(|mut child| child.wait())
            .map_err(|error| format!("{step} failed: {error}"))?;
        match exit_code(status) {
            0 => Ok(()),
            code => Err(format!("{step} failed (status {code})")),
        }
    }

    /// Runs `step` and returns the first line of its stdout, its stderr passed through; else why it failed.
    fn line(&self, step: &str, args: &[&str], copied_index: bool) -> Result<String, String> {
        // Started under the spawn lock, so it inherits no launch's handles.
        let output = gate_win32::spawn(self.command(args, copied_index).stdout(Stdio::piped()).stderr(Stdio::inherit()))
            .and_then(std::process::Child::wait_with_output)
            .map_err(|error| format!("{step} failed: {error}"))?;
        match exit_code(output.status) {
            0 => String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::to_owned)
                .ok_or_else(|| format!("{step} printed nothing")),
            code => Err(format!("{step} failed (status {code})")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{Look, before_the_job, look};
    use crate::console::Console;

    const JOB: &str = r"Local\gate-job-k-0001";

    /// The line for a stop signal of [`JOB`] whose wait failed with `the wait failed`.
    fn unread() -> String {
        format!("gate: remote cannot read the stop signal of job {JOB}: the wait failed; starting the command")
    }

    #[test]
    fn launch_sees_a_set_stop_signal_as_a_stop() {
        assert_eq!(look(|| Ok(true), JOB), Look::Stopped);
    }

    #[test]
    fn launch_sees_an_unset_stop_signal_as_no_stop() {
        assert_eq!(look(|| Ok(false), JOB), Look::Go(None));
    }

    #[test]
    fn launch_that_cannot_read_the_stop_signal_starts_the_command_and_returns_the_line_unsaid() {
        assert_eq!(look(|| Err(io::Error::other("the wait failed")), JOB), Look::Go(Some(unread())));
    }

    #[test]
    fn run_stopped_before_its_job_exists_says_so_and_fails() {
        let mut console = Console::new();
        assert_eq!(before_the_job(|| Ok(true), JOB, &mut console), Err(1));
        assert_eq!(
            console.take_notes(),
            [format!(
                "gate: the gate failed while running the command: the job {JOB} was stopped before the command started"
            )]
        );
    }

    #[test]
    fn run_not_stopped_before_its_job_exists_goes_on_and_says_nothing() {
        let mut console = Console::new();
        assert_eq!(before_the_job(|| Ok(false), JOB, &mut console), Ok(()));
        assert!(console.take_notes().is_empty(), "nothing is said");
    }

    #[test]
    fn run_that_cannot_read_the_stop_signal_before_its_job_exists_says_so_and_goes_on() {
        let mut console = Console::new();
        assert_eq!(before_the_job(|| Err(io::Error::other("the wait failed")), JOB, &mut console), Ok(()));
        assert_eq!(console.take_notes(), [unread()]);
    }
}
