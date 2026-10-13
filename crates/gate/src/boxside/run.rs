//! `gate box run`: the work folder's lock taken, the subfolder checked, the snapshot checked out, the repo's dotnet tools
//! restored, the command started in the payload's job with its output forwarded, the tree it left committed back, the
//! outputs of a passed command packed ([`crate::boxside::outputs`]), and the command's status passed whole, as the
//! PowerShell wrapper's `Invoke-Run` does. git starts through [`gate_win32::spawn`], under the spawn lock the command's
//! own launch holds while its inheritable handle copies are open, so no git inherits a handle meant for the command; what
//! git writes is forwarded line by line under the box's output lock, as the command's output is, so no heartbeat lands
//! inside a line of git's ([`Git::forwarded`]). The command runs on a console whose code pages are UTF-8
//! ([`Utf8Console`]), or, when it cannot have one, on the box's default code page, which the box says and never fails
//! the run for.
//!
//! The payload is data: it is refused unless well formed, and runs nothing but its command and writes nothing but the
//! work folder's own files. The command, and whatever it writes into the work folder, `.git` included, is the user's
//! own code running as the user, and is trusted. The box's git runs with no hooks and no file-system monitor only so
//! that a repository's own hooks or monitor, such as one a build installed, do not run in it.

use std::cell::Cell;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use gate_args::BoxSeams;
use gate_watchdog::Pump;
use gate_win32::{Donor, Job, LaunchError, NamedEvent, PipedConsole, PipedLaunch, PipedSuspended, Running, Utf8Console};

use crate::answer::USAGE_STATUS;
use crate::boxside::forward::{DRAIN, Forwarders, OutputLock};
use crate::boxside::heartbeat;
use crate::boxside::outputs::{self, Asked};
use crate::boxside::payload::{Payload, escaped, ordinal_upper};
use crate::boxside::preflight::{Failed, GIT_REDIRECTS, Git, GitFailure, exit_code, git_failed};
use crate::boxside::stop::{Ending, STOPPED, stop_event_name, terminate_and_wait};
use crate::boxside::tools;
use crate::console::Console;
use crate::environment::Variables;
use crate::outputs::manifest::{Listing, Manifest};
use crate::remote::{Refusal, refused_line, started_line};
use crate::run::{Written, find_program, written};

/// The status of a run refused: another run holds the work folder, or the work folder's dotnet tools could not be
/// restored.
pub(super) const REFUSED_STATUS: u32 = 75;
/// The status of a run the box failed on its own, and of a passed command whose tree could not be committed back.
pub(super) const FAILED_STATUS: u32 = 1;
/// The status of a command that could not be started.
const NOT_STARTED: u32 = 127;
/// The status of a run whose client closed the connection: the box stopped the job and exits as a lost connection does.
const CLOSED_STATUS: u32 = 255;
/// How long one wait for a watched process's exit lasts before the box looks again whether the client is still there
/// and, for the restore, whether a stop came.
const WAIT_STEP: Duration = Duration::from_millis(100);
/// What the box does instead when it cannot read the stop signal just before it starts the command.
const STARTING_COMMAND: &str = "starting the command";
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
pub(super) const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
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
pub(super) struct Request<'a> {
    pub(super) key: &'a str,
    snapshot: &'a str,
    bare: &'a str,
    pub(super) work: &'a str,
    lock: &'a str,
    index: &'a str,
    pub(super) job: &'a str,
    command: Vec<String>,
    subfolder: &'a str,
    identity: Vec<(String, String)>,
    /// The outputs the run brings back, `None` when it has no output globs.
    outputs: Option<Asked<'a>>,
    /// The run's `Nonce`, which its start marker and its refusal tags name.
    pub(super) nonce: &'a str,
}

impl Request<'_> {
    fn read(payload: &Payload) -> Result<Request<'_>, String> {
        // The snapshot first, as the wrapper refuses a payload without one before the lock or the checkout.
        let snapshot = payload.snapshot()?;
        let (key, work) = payload.work_folder()?;
        let (globs, full) = (payload.output_globs()?, payload.outputs_full()?);
        let nonce = payload.nonce()?;
        let outputs = Asked::new(&globs, full, key, nonce, payload.output_folder(work)?)?;
        Ok(Request {
            outputs,
            nonce,
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
/// when it could not be started, 75 when another run holds the work folder or its dotnet tools could not be restored,
/// 2 for a payload or a word refused, a failing git step's status, 1 when the box failed, the tree of a passed command
/// could not be committed back or a passed command's outputs are not brought back, or 255 when the client closed the
/// connection and the box stopped the job.
/// Once the subfolder is checked, a heartbeat goes to stderr every period, 5 s unless `seams` set it, until the run
/// ends; `seams` may also make the command's console fail, or park its launch.
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
            console.err(&refused_line(request.nonce, Refusal::Busy));
            console.err(&crate::remote::busy_line(request.key));
            return REFUSED_STATUS;
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
pub(super) struct Beat<'a> {
    pub(super) lock: &'a OutputLock,
    pump: Option<&'a Pump>,
    seams: &'a BoxSeams,
    pub(super) stop: StopSignal<'a>,
}

/// The run's stop signal as the box looks at it ([`look`]): set is a stop, unset none, and one that cannot be read is
/// no stop, its line given only the first time in the run, however often the box looks.
pub(super) struct StopSignal<'a> {
    event: &'a NamedEvent,
    /// The run's job, which the signal belongs to and its line names.
    job: &'a str,
    unread_said: Cell<bool>,
}

impl<'a> StopSignal<'a> {
    fn new(event: &'a NamedEvent, job: &'a str) -> StopSignal<'a> {
        StopSignal {
            event,
            job,
            unread_said: Cell::new(false),
        }
    }

    /// What a look at the signal finds, the line saying it cannot be read, ending with `then`, returned unsaid for the
    /// caller to say, and only the first time.
    pub(super) fn look(&self, then: &str) -> Look {
        match look(|| self.event.is_set(), self.job, then) {
            Look::Go(Some(_)) if self.unread_said.replace(true) => Look::Go(None),
            seen => seen,
        }
    }

    /// Whether a stop came, the line saying the signal cannot be read, ending with `then`, said the first time.
    fn seen(&self, then: &str, console: &mut Console) -> bool {
        match self.look(then) {
            Look::Stopped => true,
            Look::Go(unread) => {
                if let Some(line) = unread {
                    console.err(&line);
                }
                false
            }
        }
    }
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

    /// Says `line` on stderr holding the box's output lock, so it never lands inside a heartbeat or a forwarded line.
    fn say(&self, console: &mut Console, line: &str) {
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        console.err(line);
    }
}

/// The work folder and the folder below it the command runs in, both full paths.
struct Folders<'a> {
    work: &'a Path,
    place: &'a Path,
}

/// A run's outputs step as it goes: what the run asks for, the PC's manifest or why it could not be read, and the
/// listing taken before the command, `None` until then and under `OutputsFull`.
struct RunOutputs<'a> {
    asked: &'a Asked<'a>,
    manifest: Result<Manifest, String>,
    listing: Option<Listing>,
}

impl<'a> RunOutputs<'a> {
    /// Reads the PC's manifest from stdin, up to its first newline, then removes the output folder's old archives,
    /// saying each that could not be removed.
    fn read(asked: &'a Asked<'a>, beat: &Beat<'_>, console: &mut Console) -> RunOutputs<'a> {
        let manifest = outputs::read_manifest(io::stdin().lock());
        for line in outputs::prune(asked, SystemTime::now()) {
            beat.say(console, &line);
        }
        RunOutputs {
            asked,
            manifest,
            listing: None,
        }
    }

    /// Takes the listing of the files under the globs in `work` before the command, saying it could not be taken.
    fn list(&mut self, work: &Path, beat: &Beat<'_>, console: &mut Console) {
        let (listing, line) = outputs::listing(self.asked, work);
        if let Some(line) = line {
            beat.say(console, &line);
        }
        self.listing = listing;
    }

    /// Packs the outputs of a passed command run in `work`, leaving out `skip`, the back commit's paths or why they
    /// could not be read, and says how it went: 0, or 1 when the outputs are not brought back, the run's own archive
    /// among them when it alone is over the box's outputs budget.
    fn bring_back(&self, work: &Path, skip: Result<Vec<String>, String>, beat: &Beat<'_>, console: &mut Console) -> u32 {
        let brought = skip.and_then(|skip| outputs::bring_back(self.asked, &self.manifest, self.listing.as_ref(), work, &skip));
        match brought {
            Ok((skipped, line)) => {
                if let Some(skipped) = skipped {
                    beat.say(console, &skipped);
                }
                beat.say(console, &line);
                self.trim_outputs(line != outputs::NOTHING_PACKED, beat, console)
            }
            Err(reason) => {
                beat.say(console, &outputs::not_brought_back(&reason));
                FAILED_STATUS
            }
        }
    }

    /// Brings the output folder back within the box's budget once a pack wrote an archive, saying each archive that
    /// could not be removed; the run's own archive alone over the budget, or gone before the trim, leaves its outputs
    /// not brought back. 0, or 1 when the run's own archive is not brought back.
    fn trim_outputs(&self, wrote_archive: bool, beat: &Beat<'_>, console: &mut Console) -> u32 {
        if !wrote_archive {
            return 0;
        }
        let trimmed = outputs::trim(Path::new(self.asked.folder), &self.asked.archive, outputs::budget(beat.seams));
        for line in &trimmed.lines {
            beat.say(console, line);
        }
        match &trimmed.reason {
            Some(reason) => {
                beat.say(console, &outputs::not_brought_back(reason));
                FAILED_STATUS
            }
            None => 0,
        }
    }
}

/// The run once the lock is held: `Ok` with the status after the back commit, or `Err` with the status of a run that
/// ended before it, its lines said. The heartbeat starts once the subfolder is checked and stops on every way out.
fn in_work_folder(request: &Request<'_>, stop: &NamedEvent, seams: &BoxSeams, console: &mut Console) -> Result<u32, u32> {
    let lock = OutputLock::default();
    let git = WorkGit::ready(request, &lock);
    // Refused before the checkout, so a payload naming a folder outside the work folder changes nothing there; the
    // earlier run's back ref goes, as the checkout's would, so the client finds none to fetch.
    let Some((work, place)) = work_and_place(request.work, request.subfolder) else {
        console.err(&format!(
            "gate: remote subfolder {} is not a folder inside the work folder",
            escaped(request.subfolder)
        ));
        git.clear_back_ref(request, console).map_err(|failed| said(console, &failed))?;
        return Err(u32::from(USAGE_STATUS));
    };
    // A heartbeat that cannot start ends the heartbeats, never the run.
    let pump = heartbeat::start(&lock, heartbeat::period(seams))
        .inspect_err(|error| console.err(&format!("gate: remote heartbeat stopped: cannot start the heartbeat: {error}")))
        .ok();
    let beat = Beat {
        lock: &lock,
        pump: pump.as_ref(),
        seams,
        stop: StopSignal::new(stop, request.job),
    };
    // Read while the heartbeat beats and before the checkout, so the client can take its time hashing the PC's files.
    let mut outputs = request.outputs.as_ref().map(|asked| RunOutputs::read(asked, &beat, console));
    let folders = Folders { work: &work, place: &place };
    let ran = checked_out(request, &git, &folders, &beat, outputs.as_mut(), console);
    if let Some(pump) = pump {
        // Bounded, so a client that stopped reading cannot keep the work folder's lock held. Nothing is said for a
        // pump left stuck or one that panicked: the stuck stream is the one it would be said on, and a panicking pump
        // has already said so itself whenever it still could. The run's status stands either way.
        let _ = pump.stop_within(DRAIN);
    }
    ran
}

/// The run once the heartbeat beats: the snapshot checked out, the repo's dotnet tools restored, the outputs listed,
/// the command seen to its end in the place, the tree it left committed back, and, after a passed command, its outputs
/// packed; the status as [`in_work_folder`] gives it, 1 for a passed command whose outputs are not brought back.
fn checked_out(
    request: &Request<'_>,
    git: &WorkGit,
    folders: &Folders<'_>,
    beat: &Beat<'_>,
    mut outputs: Option<&mut RunOutputs<'_>>,
    console: &mut Console,
) -> Result<u32, u32> {
    git.checkout(request, console).map_err(|failed| said(console, &failed))?;
    if !folders.place.is_dir() {
        console.err(&format!(
            "gate: remote subfolder {} does not exist in the snapshot",
            escaped(request.subfolder)
        ));
        return Err(u32::from(USAGE_STATUS));
    }
    tools::update(request, folders.work, folders.place, beat, console)?;
    if let Some(outputs) = outputs.as_deref_mut() {
        outputs.list(folders.work, beat, console);
    }
    let status = command(request, folders.place, beat, console)?;
    let commit = match git.back_commit(request, console) {
        Ok(commit) => commit,
        Err(failed) => {
            let message = failed.or_end(console)?;
            console.err(&format!("gate: remote sync-back failed: {message}"));
            return Ok(if status == 0 { FAILED_STATUS } else { status });
        }
    };
    match outputs {
        Some(outputs) if status == 0 => {
            // Read after the push and only for outputs, so failing to read them fails the outputs, never the sync-back.
            let skip = back_skip(request, git, commit.as_deref(), console)?;
            Ok(outputs.bring_back(folders.work, skip, beat, console))
        }
        _ => Ok(status),
    }
}

/// The paths the back commit `commit` changes, which the outputs leave out, none without a commit, or why they could
/// not be read; `Err` with the status the run ends on when the client closed the connection meanwhile, its line said.
fn back_skip(request: &Request<'_>, git: &WorkGit, commit: Option<&str>, console: &mut Console) -> Result<Result<Vec<String>, String>, u32> {
    let Some(commit) = commit else {
        return Ok(Ok(Vec::new()));
    };
    match git.back_paths(request, commit, console) {
        Ok(paths) => Ok(Ok(paths)),
        Err(failed) => failed.or_end(console).map(Err),
    }
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

/// The work folder and the folder the command runs in, both as full paths: `work` folded, then `sub` below it (the
/// work folder itself for an empty `sub`); `None` when `sub` is rooted (a drive, a drive-relative path or a leading
/// separator), climbs out of `work`, or the work folder cannot be made a full path. Names are compared as .NET's
/// `OrdinalIgnoreCase` compares them, each character's simple uppercase.
fn work_and_place(work: &str, sub: &str) -> Option<(PathBuf, PathBuf)> {
    let work = full_path(Path::new(work))?;
    if sub.is_empty() {
        return Some((work.clone(), work));
    }
    let rooted = sub.starts_with(['\\', '/']) || sub.as_bytes().get(1) == Some(&b':');
    if rooted {
        return None;
    }
    let full = full_path(&work.join(sub))?;
    let (work_text, full_text) = (ordinal_upper(&work.to_string_lossy()), ordinal_upper(&full.to_string_lossy()));
    if full_text == work_text {
        Some((work.clone(), work))
    } else if full_text.starts_with(&format!("{work_text}\\")) {
        Some((work, full))
    } else {
        None
    }
}

/// `path` made a full path as Windows reads it, `.` and `..` folded and separators written `\`, without a trailing
/// separator; `None` for a path Windows cannot make full, which no folder inside the work folder is.
pub(super) fn full_path(path: &Path) -> Option<PathBuf> {
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
/// nothing; the restore looks at it the same way before it runs, so such a stop runs no restore either
/// ([`crate::boxside::tools`]). The command starts on a UTF-8 console when it can have one ([`start_command`]), whose
/// donor lives until the wait for the command is over, however it ends. The heartbeat watches the job while [`watch`]
/// waits for it. The command's environment is the box's own with `MSBUILDNODEHANDSHAKESALT` set to the work folder's
/// key, replacing any value the box inherited, so `MSBuild`'s worker nodes stay warm within one work folder and are
/// never reused across work folders ([`Variables::with`]).
fn command(request: &Request<'_>, place: &Path, beat: &Beat<'_>, console: &mut Console) -> Result<u32, u32> {
    let variables = Variables::from_process().with("MSBUILDNODEHANDSHAKESALT", request.key);
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
    before_the_job(beat.stop.look(STARTING_COMMAND), request.job, console)?;
    let job = create_job(request.job, console)?;
    // Started right after the job and kept until the command's wait is over, so its console outlives the command.
    let donor = console_donor(&job, beat.seams);
    if let Some(park) = beat.seams.park_launch.as_deref() {
        wait_for_path(park, console);
    }
    let mut start = Start {
        launch,
        job: &job,
        stop: &beat.stop,
        word,
        program: &program,
        nonce: request.nonce,
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

/// The command to start: its launch, its job and the job's stop signal, the word and program a start failure names, and
/// the run's nonce its start marker names.
struct Start<'a> {
    launch: PipedLaunch,
    job: &'a Job,
    stop: &'a StopSignal<'a>,
    word: &'a str,
    program: &'a Path,
    nonce: &'a str,
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
/// has to say is said once it is over and the box is home ([`Ended::say`]). The start marker is said first, before the
/// launch and while the box is home, so the client hears it before any output of the command; a launch then refused or
/// stopped has said it too, which only keeps the client from making the call again. The command started, or the run's
/// answer: `Ok(127)` for a command that could not be started, `Err` with the status of the box's own failure, its line
/// said.
fn start_command(start: &mut Start<'_>, donor: Result<&Donor, &str>, beat: &Beat<'_>, console: &mut Console) -> Result<Launched, Result<u32, u32>> {
    beat.say(console, &started_line(start.nonce));
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
    let unread = match start.stop.look(STARTING_COMMAND) {
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
pub(super) enum Look {
    /// It is set: a stop came.
    Stopped,
    /// It is unset, or it could not be read, with the line saying so: no stop came.
    Go(Option<String>),
}

/// What `check` reads of the stop signal of the job `job`: set is a stop, unset none. A signal that cannot be read is
/// no stop, and the box goes on, doing what `then` says; the line saying so is returned, not said, for the caller to
/// say where the client hears it.
fn look(check: impl FnOnce() -> io::Result<bool>, job: &str, then: &str) -> Look {
    match check() {
        Ok(true) => Look::Stopped,
        Ok(false) => Look::Go(None),
        Err(error) => Look::Go(Some(format!("gate: remote cannot read the stop signal of job {job}: {error}; {then}"))),
    }
}

/// Ends the run before a job is created when a stop came already, as `seen` found the stop signal of the run's job
/// `job`: the stop line said, status 1, and nothing started. A signal that could not be read is said, and the run goes
/// on.
pub(super) fn before_the_job(seen: Look, job: &str, console: &mut Console) -> Result<(), u32> {
    match seen {
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

/// Stops the job of a command a stop came before, then says the run failed for it ([`said_stopped`]); status 1.
fn stopped_before_start(job: &Job, console: &mut Console) -> u32 {
    let ending = terminate_and_wait(job);
    said_stopped(job, ending, console)
}

/// Says the run failed for a stop that came before the command started, naming `job`, whose `ending` is already
/// decided; status 1. What the job still holds after the stop and a later terminate that failed ([`Ending::say`]), or
/// why it could not be stopped, is said after.
pub(super) fn said_stopped(job: &Job, ending: io::Result<Ending>, console: &mut Console) -> u32 {
    let status = stopped_first(console, job.name());
    match ending {
        Ok(ending) => ending.say(job.name(), console),
        Err(error) => console.err(&format!("gate: and could not terminate the job {}: {error}", job.name())),
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

/// Terminates the job when dropped armed: the command's on a panic after it was resumed, so the command does not
/// outlive the run; the restore's on every way out of it, a panic included, as a restore has nothing to keep warm.
pub(super) struct Stopper<'a> {
    pub(super) job: &'a Job,
    pub(super) armed: bool,
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
    let watched = Watched {
        job,
        running,
        forwarders,
        watching: Watching::Command,
    };
    beat.watch(job);
    // No stop is looked for: a stop ends the command through its job, and the status it ends with passes.
    let waited = watched.wait(None, console);
    beat.unwatch();
    match waited {
        WaitEnd::Exited { code, .. } => watched.drained(console).map(|()| code),
        WaitEnd::ClientGone => Err(watched.client_gone(console)),
        WaitEnd::Failed(status) => Err(status),
    }
}

/// Which process the box watches, as its lines name it.
#[derive(Clone, Copy)]
pub(super) enum Watching {
    Command,
    Restore,
}

impl Watching {
    /// Its name in the line of a wait that failed.
    fn what(self) -> &'static str {
        match self {
            Watching::Command => "the command",
            Watching::Restore => "the restore",
        }
    }

    /// What the box does instead when it cannot read the stop signal while it waits.
    fn while_waiting(self) -> &'static str {
        match self {
            Watching::Command => "watching the command",
            Watching::Restore => "watching the restore",
        }
    }

    /// The line said when a leftover process still holds its output open once its main process exited.
    fn leftover(self) -> &'static str {
        match self {
            Watching::Command => "gate: remote command's output is still held open by a leftover process; stopped reading",
            Watching::Restore => "gate: remote the restore's output is still held open by a leftover process; stopped reading",
        }
    }
}

/// A started process the box watches to its end: its job, its main process, the readers forwarding its output, and
/// which process it is.
pub(super) struct Watched<'a> {
    pub(super) job: &'a Job,
    pub(super) running: &'a Running,
    pub(super) forwarders: Forwarders,
    pub(super) watching: Watching,
}

/// How the wait for a watched process's main process ended.
pub(super) enum WaitEnd {
    /// It exited with `code`; `stopped` when a stop set the run's signal, looked at only when a signal was given.
    Exited { code: u32, stopped: bool },
    /// A write to the box's own output failed first: the client closed the connection.
    ClientGone,
    /// The box could not wait for it: the status 1, its line said and its job terminated.
    Failed(u32),
}

impl Watched<'_> {
    /// Waits for the main process to exit, looking every [`WAIT_STEP`] whether the client is still there and, when
    /// `stop` is given, whether a stop came: a stop terminates the job, so the main process exits and the wait sees it,
    /// and is looked at once more as it exits, so a stop that ended the job before the box looked counts too.
    pub(super) fn wait(&self, stop: Option<&StopSignal<'_>>, console: &mut Console) -> WaitEnd {
        let mut stopped = false;
        loop {
            match self.running.wait(WAIT_STEP) {
                Ok(Some(code)) => {
                    let stopped = stopped || stop.is_some_and(|stop| stop.seen(self.watching.while_waiting(), console));
                    return WaitEnd::Exited { code, stopped };
                }
                Ok(None) if self.forwarders.pipe_closed() => return WaitEnd::ClientGone,
                Ok(None) => {}
                Err(error) => {
                    let status = failed(console, &format!("cannot wait for {}: {error}", self.watching.what()));
                    end_job(self.job, console);
                    return WaitEnd::Failed(status);
                }
            }
            if !stopped && stop.is_some_and(|stop| stop.seen(self.watching.while_waiting(), console)) {
                stopped = true;
                end_job(self.job, console);
            }
        }
    }

    /// Once the main process exited: lets the readers drain for at most [`DRAIN`]. A write to the box's own output
    /// that failed meanwhile stops the job and answers [`CLOSED_STATUS`]; a leftover process still holding the output
    /// open is said, and the run goes on.
    pub(super) fn drained(self, console: &mut Console) -> Result<(), u32> {
        let drained = self.forwarders.drain(DRAIN);
        if drained.pipe_closed {
            return Err(closed_connection(self.job, terminate_and_wait(self.job), console));
        }
        if !drained.all_ended {
            console.err(self.watching.leftover());
        }
        Ok(())
    }

    /// The client closed the connection: the job is stopped first, then the readers drain for at most [`DRAIN`];
    /// [`CLOSED_STATUS`].
    pub(super) fn client_gone(self, console: &mut Console) -> u32 {
        let ending = terminate_and_wait(self.job);
        self.forwarders.drain(DRAIN);
        closed_connection(self.job, ending, console)
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

/// Terminates the job of a process the box no longer watches, saying a terminate that failed.
pub(super) fn end_job(job: &Job, console: &mut Console) {
    if let Err(error) = job.terminate(STOPPED) {
        console.err(&format!("gate: and could not terminate the job {}: {error}", job.name()));
    }
}

/// Says the box's own failure; status 1.
pub(super) fn failed(console: &mut Console, problem: &str) -> u32 {
    console.err(&format!("gate: the gate failed while running the command: {problem}"));
    FAILED_STATUS
}

/// Says the start failure of `word`, found as `resolved`, with the system's `message`, as `gate run` says it; 127.
fn not_started(console: &mut Console, word: &str, resolved: Option<&Path>, message: &str) -> u32 {
    let resolved = resolved.map(|path| path.to_string_lossy().into_owned());
    match resolved.filter(|resolved| resolved != word) {
        Some(resolved) => console.err(&format!("gate: could not start {} ({}): {message}", escaped(word), escaped(&resolved))),
        None => console.err(&format!("gate: could not start {}: {message}", escaped(word))),
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

/// The failure of git `step` whose forwarded output found the client gone: the run ends as it ends when the command's
/// forwarded output finds it gone, with [`CLOSED_STATUS`], git having already ended.
fn client_gone(step: &str) -> Failed {
    Failed {
        line: format!("gate: remote the client closed the connection during git {step}"),
        status: CLOSED_STATUS,
    }
}

/// Why the back commit, or the reading of its paths, failed.
enum BackFailed {
    /// A step failed, for this reason, which the run says as its sync-back failure.
    Step(String),
    /// The client closed the connection while a step's output was forwarded: the run ends here ([`client_gone`]).
    ClientGone(Failed),
}

impl BackFailed {
    /// Why a step failed, or, for a client gone, `Err` with the status the run ends on, its line said.
    fn or_end(self, console: &mut Console) -> Result<String, u32> {
        match self {
            BackFailed::Step(message) => Ok(message),
            BackFailed::ClientGone(failed) => Err(said(console, &failed)),
        }
    }
}

/// git as the box runs it in the work folder: none of the variables that would point it elsewhere, no standard input,
/// its hooks read from a folder that cannot exist, no file-system monitor, and its output forwarded under the box's
/// output lock.
struct WorkGit {
    git: Git,
    /// `core.hooksPath=<lock>\hooks`: a folder inside the lock file, which cannot exist while the run holds the lock
    /// open sharing nothing, since a file has no children and cannot be deleted, renamed or replaced meanwhile.
    hooks: String,
    /// The box's output lock, which every line git writes is forwarded under.
    output: OutputLock,
}

impl WorkGit {
    /// Finds git, its output to be forwarded under `output`; called only once the run holds the lock.
    fn ready(request: &Request<'_>, output: &OutputLock) -> WorkGit {
        WorkGit {
            git: Git::find(),
            hooks: format!(r"core.hooksPath={}\hooks", request.lock),
            output: Arc::clone(output),
        }
    }

    /// Runs git with the safety options and `args`, both of its streams forwarded ([`Git::forwarded`]); a non-zero
    /// status, or a git that cannot be started, fails `step`, said as [`Git::step`] says it, and a client gone ends the
    /// run ([`client_gone`]).
    fn git_step(&self, step: &str, args: &[&str], console: &mut Console) -> Result<(), Failed> {
        let ran = self
            .git
            .command(&self.args(args))
            .map_err(GitFailure::Failed)
            .and_then(|command| Git::forwarded(command, false, &self.output, console));
        match ran {
            Ok(output) => match exit_code(output.status) {
                0 => Ok(()),
                code => Err(git_failed(step, code)),
            },
            Err(GitFailure::Failed(message)) => Err(Failed::without_status(format!("gate: remote {step} failed: {message}"))),
            Err(GitFailure::ClientGone) => Err(client_gone(step)),
        }
    }

    /// The safety options, then `args`.
    fn args<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut all = vec!["-c", self.hooks.as_str(), "-c", "core.fsmonitor=false"];
        all.extend_from_slice(args);
        all
    }

    /// Runs `step` in the work folder with `args`, its output forwarded.
    fn step(&self, step: &str, request: &Request<'_>, args: &[&str], console: &mut Console) -> Result<(), Failed> {
        let mut all = vec!["-C", request.work];
        all.extend_from_slice(args);
        self.git_step(step, &all, console)
    }

    /// The snapshot the payload names, fetched from the bare repository at `refs/gate/<key>` and checked out by that
    /// commit over whatever the last run left, untracked files removed and ignored ones (build output) kept, and no
    /// back commit of an earlier run left behind. The commit, never the ref's tip, is what is checked out, so a later
    /// push of the key changes nothing this run runs. A commit the work folder's repository lacks after the fetch ends
    /// the run with the box's own line.
    fn checkout(&self, request: &Request<'_>, console: &mut Console) -> Result<(), Failed> {
        let refname = format!("refs/gate/{}", request.key);
        self.step("fetch", request, &["fetch", "-q", request.bare, &refname], console)?;
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
        self.step("checkout", request, &["checkout", "-q", "--force", "--detach", request.snapshot], console)?;
        self.step("clean", request, &["clean", "-fdq"], console)?;
        self.clear_back_ref(request, console)
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
    fn clear_back_ref(&self, request: &Request<'_>, console: &mut Console) -> Result<(), Failed> {
        let bare = format!("--git-dir={}", request.bare);
        let back = format!("refs/gate-back/{}", request.key);
        self.git_step("back ref reset", &[&bare, "update-ref", "-d", &back], console)
    }

    /// The tree the command left, committed on `HEAD` and pushed to `refs/gate-back/<key>` in the bare repository;
    /// nothing when the tree is unchanged. It starts from a copy of the work folder's index, which the checkout just
    /// refreshed, so `add -A` skips every unchanged file; a copy that cannot be removed afterwards is said and fails
    /// nothing. The commit pushed, `None` when the tree is unchanged; else why it failed.
    fn back_commit(&self, request: &Request<'_>, console: &mut Console) -> Result<Option<String>, BackFailed> {
        let program = self
            .git
            .program
            .as_ref()
            .map_err(|message| BackFailed::Step(format!("git cannot be started: {message}")))?;
        let back = BackGit {
            program,
            request,
            work: self,
        };
        let index = Path::new(request.work).join(".git").join("index");
        fs::copy(&index, request.index)
            .map_err(|error| BackFailed::Step(format!("cannot copy {} to {}: {error}", index.display(), request.index)))?;
        let tree = back
            .status("add", &["add", "-A"], true, console)
            .and_then(|()| back.line("write-tree", &["write-tree"], true, console));
        match fs::remove_file(request.index) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                console.err(&format!("gate: remote could not remove {}: {error}", request.index));
            }
            _ => {}
        }
        let tree = tree?;
        if tree == back.line("rev-parse", &["rev-parse", "HEAD^{tree}"], false, console)? {
            return Ok(None);
        }
        let commit = back.line(
            "commit-tree",
            &["commit-tree", &tree, "-p", "HEAD", "-m", "gate-remote back"],
            false,
            console,
        )?;
        let target = format!("{commit}:refs/gate-back/{}", request.key);
        back.status("push", &["push", "-q", request.bare, &target], false, console)?;
        Ok(Some(commit))
    }

    /// The `/`-separated paths the back commit `commit` changes against `HEAD`; else why they could not be read.
    fn back_paths(&self, request: &Request<'_>, commit: &str, console: &mut Console) -> Result<Vec<String>, BackFailed> {
        let program = self
            .git
            .program
            .as_ref()
            .map_err(|message| BackFailed::Step(format!("git cannot be started: {message}")))?;
        BackGit {
            program,
            request,
            work: self,
        }
        .changed(commit, console)
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

    /// Runs `step` to its end, its stderr, and its stdout unless `read_stdout`, forwarded ([`Git::forwarded`]); its
    /// output, else why it could not run or that the client is gone.
    fn run(&self, step: &str, args: &[&str], copied_index: bool, read_stdout: bool, console: &mut Console) -> Result<Output, BackFailed> {
        Git::forwarded(self.command(args, copied_index), read_stdout, &self.work.output, console).map_err(|failure| match failure {
            GitFailure::Failed(error) => BackFailed::Step(format!("{step} failed: {error}")),
            GitFailure::ClientGone => BackFailed::ClientGone(client_gone(step)),
        })
    }

    /// Runs `step`, its output forwarded; else why it failed.
    fn status(&self, step: &str, args: &[&str], copied_index: bool, console: &mut Console) -> Result<(), BackFailed> {
        let output = self.run(step, args, copied_index, false, console)?;
        match exit_code(output.status) {
            0 => Ok(()),
            code => Err(BackFailed::Step(format!("{step} failed (status {code})"))),
        }
    }

    /// Runs `step` and returns its stdout, its stderr forwarded; else why it failed.
    fn stdout(&self, step: &str, args: &[&str], copied_index: bool, console: &mut Console) -> Result<Vec<u8>, BackFailed> {
        let output = self.run(step, args, copied_index, true, console)?;
        match exit_code(output.status) {
            0 => Ok(output.stdout),
            code => Err(BackFailed::Step(format!("{step} failed (status {code})"))),
        }
    }

    /// Runs `step` and returns the first line of its stdout, its stderr forwarded; else why it failed.
    fn line(&self, step: &str, args: &[&str], copied_index: bool, console: &mut Console) -> Result<String, BackFailed> {
        let stdout = self.stdout(step, args, copied_index, console)?;
        String::from_utf8_lossy(&stdout)
            .lines()
            .next()
            .map(str::to_owned)
            .ok_or_else(|| BackFailed::Step(format!("{step} printed nothing")))
    }

    /// The `/`-separated paths `commit` changes against `HEAD`; else why they could not be read.
    fn changed(&self, commit: &str, console: &mut Console) -> Result<Vec<String>, BackFailed> {
        let stdout = self.stdout("diff-tree", &["diff-tree", "--name-only", "-r", "-z", "HEAD", commit], false, console)?;
        stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8(path.to_vec()).map_err(|_| BackFailed::Step("diff-tree printed a path that is not UTF-8".to_owned())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{Look, STARTING_COMMAND, before_the_job, look};
    use crate::console::Console;

    const JOB: &str = r"Local\gate-job-k-0001";

    /// The line for a stop signal of [`JOB`] whose wait failed with `the wait failed`.
    fn unread() -> String {
        format!("gate: remote cannot read the stop signal of job {JOB}: the wait failed; starting the command")
    }

    #[test]
    fn launch_sees_a_set_stop_signal_as_a_stop() {
        assert_eq!(look(|| Ok(true), JOB, STARTING_COMMAND), Look::Stopped);
    }

    #[test]
    fn launch_sees_an_unset_stop_signal_as_no_stop() {
        assert_eq!(look(|| Ok(false), JOB, STARTING_COMMAND), Look::Go(None));
    }

    #[test]
    fn launch_that_cannot_read_the_stop_signal_starts_the_command_and_returns_the_line_unsaid() {
        assert_eq!(
            look(|| Err(io::Error::other("the wait failed")), JOB, STARTING_COMMAND),
            Look::Go(Some(unread()))
        );
    }

    #[test]
    fn run_stopped_before_its_job_exists_says_so_and_fails() {
        let mut console = Console::new();
        assert_eq!(before_the_job(look(|| Ok(true), JOB, STARTING_COMMAND), JOB, &mut console), Err(1));
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
        assert_eq!(before_the_job(look(|| Ok(false), JOB, STARTING_COMMAND), JOB, &mut console), Ok(()));
        assert!(console.take_notes().is_empty(), "nothing is said");
    }

    #[test]
    fn run_that_cannot_read_the_stop_signal_before_its_job_exists_says_so_and_goes_on() {
        let mut console = Console::new();
        let seen = look(|| Err(io::Error::other("the wait failed")), JOB, STARTING_COMMAND);
        assert_eq!(before_the_job(seen, JOB, &mut console), Ok(()));
        assert_eq!(console.take_notes(), [unread()]);
    }
}
