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
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use gate_args::BoxSeams;
use gate_watchdog::{Kill, Pump};
use gate_win32::{Donor, Job, LaunchError, NamedEvent, PipedConsole, PipedLaunch, PipedSuspended, RestrictedToken, Running, Utf8Console};

use crate::answer::USAGE_STATUS;
use crate::boxside::ceiling::{Ceiling, KILLED, Spec};
use crate::boxside::forward::{DRAIN, Forwarders, OutputLock};
use crate::boxside::heartbeat;
use crate::boxside::inputs;
use crate::boxside::outputs::{self, Asked};
use crate::boxside::packages;
use crate::boxside::payload::{PairSibling, Payload, bare_stem, escaped, ordinal_upper, trees};
use crate::boxside::preflight::{self, Failed, GIT_REDIRECTS, Git, GitFailure, Unready, exit_code, git_failed};
use crate::boxside::stop::{Ending, STOPPED, stop_event_name, terminate_and_wait};
use crate::boxside::tools;
use crate::console::Console;
use crate::environment::Variables;
use crate::outputs::manifest::{Listing, Manifest};
use crate::remote::{Refusal, refused_line, started_line};
use crate::resolve::{self, Environment};
use crate::run::{Written, end_leftovers, find_program, written};

/// The status of a run refused: another run holds the work folder, or the work folder's dotnet tools could not be
/// restored.
pub(super) const REFUSED_STATUS: u32 = 75;
/// The status of a run the box failed on its own, and of a passed command whose tree could not be committed back.
pub(super) const FAILED_STATUS: u32 = 1;
/// The status of a command that could not be started.
const NOT_STARTED: u32 = 127;
/// The status of a run whose client closed the connection: the box stopped the job and exits as a lost connection does.
const CLOSED_STATUS: u32 = 255;

/// The git step that deletes a repo's back ref ([`WorkGit::clear_back_ref`]).
const BACK_REF_RESET: &str = "back ref reset";
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
pub(super) const SHARING_VIOLATION: i32 = 32;
pub(super) const LOCK_VIOLATION: i32 = 33;
/// The Win32 error `CreateProcessW` gives when the box's own job refuses breakaway, among other refusals.
const ERROR_ACCESS_DENIED: u32 = 5;
/// `FILE_FLAG_OPEN_REPARSE_POINT`: opens a link or junction itself, never its target.
pub(super) const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// `FILE_FLAG_BACKUP_SEMANTICS`: lets a folder be opened.
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
/// `FILE_SHARE_READ | FILE_SHARE_WRITE`: the work folder is held sharing everything but deletion, so it cannot be
/// removed, renamed or replaced while held.
const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
/// What the work folder's layout record, `{Work}.layout`, says while its children are being cleared: a record that
/// still says it when a run reads it is of a clear that did not finish, which clears again.
const LAYOUT_CHANGING: &str = "changing";
/// What the layout record says of a run of one repo's layout, checked out in the work folder itself; no record says the
/// same.
pub(super) const LAYOUT_SINGLE: &str = "single";
/// The identity variables the payload may set for the back commit.
const IDENTITY_NAMES: [&str; 6] = [
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
];

/// What a run reads of its payload. `work` is the work folder, which the lock, the index and the tools record sit beside;
/// it is the main repo's tree on a run of one repo, and holds the tree of each repo of a sibling pair.
pub(super) struct Request<'a> {
    pub(super) key: &'a str,
    /// The main repo, checked out in the work folder, or in `{work}\{leaf}` on a pair run.
    main: Repo<'a>,
    /// Each sibling of a pair run, in the payload's order, checked out in `{work}\{name}`; none on a run of one repo.
    siblings: Vec<Repo<'a>>,
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
    /// The run's ceiling in whole seconds of load-adjusted time, `None` for a run the box gives none.
    timeout: Option<u64>,
    /// The PC's name for the builder, which the box's kill line names, empty when the payload gives none.
    host: &'a str,
    /// The digest of the inputs manifest the client installed the run's inputs from, `None` when it sent none.
    inputs_digest: Option<&'a str>,
}

impl Request<'_> {
    fn read(payload: &Payload) -> Result<Request<'_>, String> {
        // The snapshot first, as the wrapper refuses a payload without one before the lock or the checkout.
        let snapshot = payload.snapshot()?;
        let (key, work) = payload.work_folder()?;
        let (globs, full) = (payload.output_globs()?, payload.outputs_full()?);
        let nonce = payload.nonce()?;
        let outputs = Asked::new(&globs, full, key, nonce, payload.output_folder(work)?)?;
        let (timeout, host) = (payload.timeout()?, payload.optional_text("Host")?);
        let bare = payload.path("Bare")?;
        let lock = payload.beside_work("Lock", work, "lock")?;
        let index = payload.beside_work("Index", work, "index")?;
        let job = payload.job(key)?;
        let command = payload.words("Command")?;
        let subfolder = payload.optional_text("Subfolder")?;
        let identity = identity(payload)?;
        let inputs_digest = payload.inputs_digest()?;
        // Last, so a payload a run of one repo refuses is refused with the same line whatever its Pair says.
        let (main, siblings) = repos(payload, work, bare, snapshot)?;
        Ok(Request {
            key,
            main,
            siblings,
            work,
            lock,
            index,
            job,
            command,
            subfolder,
            identity,
            outputs,
            nonce,
            timeout,
            host,
            inputs_digest,
        })
    }

    /// The main repo's tree, which the command's subfolder, the tool manifests and the outputs are read in.
    pub(super) fn tree(&self) -> &str {
        &self.main.tree
    }

    /// The command's words, its program first, as the payload gives them.
    pub(super) fn command(&self) -> &[String] {
        &self.command
    }

    /// Every repo of the run, the main one first.
    fn repos(&self) -> impl Iterator<Item = &Repo<'_>> {
        std::iter::once(&self.main).chain(&self.siblings)
    }

    /// The work folder and the repos the run lays out in it.
    fn laid(&self) -> Laid<'_> {
        Laid {
            work: self.work,
            main: &self.main,
            siblings: &self.siblings,
        }
    }

    /// The main repo's name as routing names it ([`repo_name_of`]).
    fn repo_name(&self) -> &str {
        repo_name_of(&self.main.bare)
    }
}

/// The name routing gives the repo of the bare repository `bare`: the stem of `{name}.git` ([`bare_stem`]), or the
/// bare repository's whole file name when it is not of that form.
fn repo_name_of(bare: &str) -> &str {
    let (folder, file) = bare.rsplit_once(['\\', '/']).unwrap_or(("", bare));
    bare_stem(bare, folder).unwrap_or(file)
}

/// What the layout step lays out ([`WorkGit::lay_out`]): the work folder, the main repo and the siblings of a pair,
/// none for one repo. A run and the inputs verb each build one, so both lay the work folder out alike.
pub(super) struct Laid<'r> {
    work: &'r str,
    main: &'r Repo<'r>,
    siblings: &'r [Repo<'r>],
}

impl Laid<'_> {
    /// Every repo laid out, the main one first.
    fn repos(&self) -> impl Iterator<Item = &Repo<'_>> {
        std::iter::once(self.main).chain(self.siblings)
    }

    /// The layout the work folder is laid out in, as its record says it: [`LAYOUT_SINGLE`] for one repo, else `pair`
    /// followed by the leaf, then each sibling's name sorted without regard to ASCII case, one space before each, so
    /// the same siblings named in another order are the same layout.
    fn layout(&self) -> String {
        let Some(leaf) = self.main.name.as_deref() else {
            return LAYOUT_SINGLE.to_owned();
        };
        let mut siblings: Vec<&str> = self.siblings.iter().filter_map(|repo| repo.name.as_deref()).collect();
        siblings.sort_by_key(|name| name.to_ascii_lowercase());
        format!("pair {leaf} {}", siblings.join(" "))
    }
}

/// Lays out the work folder `work` for one repo, its bare repository `bare`, as a run does before its checkout
/// ([`WorkGit::lay_out`]), under the work folder's lock file `lock`, which the caller holds; git's output is forwarded
/// as a run's is. The held work folder; else `Err` with the status, its lines said: a failing git step's, or 75 for a
/// work folder that could not be laid out, its `layout` tag and line said as a run says them.
pub(super) fn lay_out_alone(work: &str, bare: &str, lock: &str, nonce: &str, console: &mut Console) -> Result<File, u32> {
    let main = Repo::new(None, work.to_owned(), bare, "");
    let laid = Laid {
        work,
        main: &main,
        siblings: &[],
    };
    match WorkGit::ready(lock, &OutputLock::default()).lay_out(&laid, console) {
        Ok(held) => Ok(held),
        Err(NotLaidOut::Failed(failed)) => Err(said(console, &failed)),
        Err(NotLaidOut::Refused(why)) => {
            console.err(&refused_line(nonce, Refusal::Layout));
            console.err(&crate::remote::layout_line(repo_name_of(bare), &escaped(&why)));
            Err(REFUSED_STATUS)
        }
    }
}

/// Checks the run's `InputsDigest`, when it has one, against the work folder `held`'s inputs record and the copies it
/// holds, and rewrites the tree's exclude file from it ([`inputs::unmatched`]), once the work folder is laid out and
/// before the checkout: a record missing, unreadable or of another digest, a copy changed or gone, or an exclude file
/// that cannot be written refuses the run `inputs`, its tag and line said under the box's output lock, so the PC runs
/// the command itself; 75. A folder moved aside from the exclude file's name is said, under the lock too.
fn inputs_matched(request: &Request<'_>, held: &File, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
    let Some(sent) = request.inputs_digest else {
        return Ok(());
    };
    let why = match inputs::unmatched(request.work, held, sent) {
        Ok(moved_aside) => {
            if let Some(line) = moved_aside {
                let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
                console.err(&line);
            }
            return Ok(());
        }
        Err(why) => why,
    };
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    console.err(&inputs::refused_line(request.nonce));
    console.err(&format!(
        "gate: remote inputs of work folder {} are not the ones sent: {}",
        request.key,
        escaped(&why)
    ));
    Err(REFUSED_STATUS)
}

/// The main repo and the siblings of a run in the work folder `work`: with no `Pair`, the one repo of `bare` checked out
/// at `snapshot` in `work` itself; with one, the main repo in `{work}\{leaf}` and each sibling in `{work}\{name}` with
/// its own bare repository and its commit from `SiblingSnapshots`. Else the usage line.
fn repos<'a>(payload: &'a Payload, work: &str, bare: &str, snapshot: &'a str) -> Result<(Repo<'a>, Vec<Repo<'a>>), String> {
    let Some(pair) = payload.pair()? else {
        let (tree, _) = trees(work, None);
        return Ok((Repo::new(None, tree, bare, snapshot), Vec::new()));
    };
    let commits = payload.sibling_snapshots(&pair)?;
    let names: Vec<&str> = pair.siblings().iter().map(PairSibling::name).collect();
    let (main_tree, sibling_trees) = trees(work, Some((pair.leaf(), &names)));
    let main = Repo::new(Some(pair.leaf()), main_tree, bare, snapshot);
    let siblings = pair
        .siblings()
        .iter()
        .zip(commits)
        .zip(sibling_trees)
        .map(|((sibling, commit), tree)| Repo::new(Some(sibling.name()), tree, sibling.bare(), commit))
        .collect();
    Ok((main, siblings))
}

/// One repo of a run, made once as the payload is read: its name on a pair run, its tree, the bare repository its
/// snapshot is fetched from and its back commit pushed to, and the commit checked out. Every git call in its tree names
/// the tree's own `.git` and the tree ([`Repo::at`]), so no `core.worktree` and no repository above it is followed.
struct Repo<'a> {
    name: Option<String>,
    tree: String,
    bare: String,
    snapshot: &'a str,
    git_dir: String,
    work_tree: String,
}

impl<'a> Repo<'a> {
    fn new(name: Option<&str>, tree: String, bare: &str, snapshot: &'a str) -> Repo<'a> {
        Repo {
            name: name.map(str::to_owned),
            git_dir: format!(r"--git-dir={tree}\.git"),
            work_tree: format!("--work-tree={tree}"),
            tree,
            bare: bare.to_owned(),
            snapshot,
        }
    }

    /// The options that point git at this repo's tree.
    fn at(&self) -> [&str; 2] {
        [&self.git_dir, &self.work_tree]
    }

    /// The step `step` as its lines name it ([`preflight::named_step`]).
    fn step(&self, step: &str) -> String {
        preflight::named_step(step, self.name.as_deref())
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
/// when it could not be started, 75 when another run holds the work folder, it could not be laid out for the run's
/// repos or its dotnet tools could not be restored, 2 for a payload or a word refused, a failing git step's status, 1
/// when the box failed, the tree of a passed command could not be committed back or a passed command's outputs are not
/// brought back, or 255 when the client closed the connection and the box stopped the job.
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
            no_back_commits(&request, console);
            return FAILED_STATUS;
        }
    };
    // Made as soon as the lock is held and kept for the run, so a stop that comes before the job exists, during the
    // checkout, is kept for the look just before the job is created.
    let stop = match create_stop_event(request.job, console) {
        Ok(stop) => stop,
        Err(status) => {
            no_back_commits(&request, console);
            return status;
        }
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
    /// Has the heartbeat count the CPU of `job` from now on.
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
pub(super) struct Folders<'a> {
    pub(super) work: &'a Path,
    pub(super) place: &'a Path,
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
    let git = WorkGit::ready(request.lock, &lock);
    git.clear_back_refs(request, console).map_err(|failed| {
        let status = said(console, &failed);
        no_back_commits(request, console);
        status
    })?;
    // Refused before the checkout, so a payload naming a folder outside the work folder changes nothing there; the
    // earlier run's back ref goes, as the checkout's would, so the client finds none to fetch.
    let Some((work, place)) = work_and_place(request.tree(), request.subfolder) else {
        console.err(&format!(
            "gate: remote subfolder {} is not a folder inside the work folder",
            escaped(request.subfolder)
        ));
        if request.siblings.is_empty() {
            git.clear_back_ref(request, &request.main, console)
                .map_err(|failed| said(console, &failed))?;
        }
        no_back_commits(request, console);
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

/// The run once the heartbeat beats: the snapshot checked out, the repo's dotnet tools restored, a pnpm command's pnpm
/// packages installed, the outputs listed, the command seen to its end in the place, the tree it left committed back
/// unless the box's ceiling killed it, and, after a passed command, its outputs packed; the status as
/// [`in_work_folder`] gives it, 1 for a passed command whose outputs are not brought back, [`KILLED`] for a killed one.
fn checked_out(
    request: &Request<'_>,
    git: &WorkGit,
    folders: &Folders<'_>,
    beat: &Beat<'_>,
    mut outputs: Option<&mut RunOutputs<'_>>,
    console: &mut Console,
) -> Result<u32, u32> {
    ready(request, git, folders, beat, console)?;
    if let Some(outputs) = outputs.as_deref_mut() {
        outputs.list(folders.work, beat, console);
    }
    let ran = command(request, folders.place, beat, console).inspect_err(|_| back_commits_failed(request, beat, console))?;
    let status = match ran {
        Ran::Exited(status) => status,
        // The PC applies nothing of a run the box killed, and a process its kill could not end may still be writing.
        Ran::Killed => {
            back_commits_failed(request, beat, console);
            return Ok(KILLED);
        }
    };
    let commit = match git.back_commit(request, console) {
        Ok(commit) => commit,
        Err(failed) => {
            back_commits_failed(request, beat, console);
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

/// The run up to its command: the snapshot laid out and checked out, the subfolder found, the repo's dotnet tools
/// restored and, for a pnpm command, its pnpm packages installed ([`packages::update`]); else `Err` with the status the
/// run ends on, its lines said. A refusal is said as such; any other end is followed, on a pair run, by the box's
/// `gate: remote back commits: failed` line ([`ended_before`]).
fn ready(request: &Request<'_>, git: &WorkGit, folders: &Folders<'_>, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
    let held = match git.lay_out(&request.laid(), console) {
        Ok(held) => held,
        Err(NotLaidOut::Failed(failed)) => return Err(ended_before(said(console, &failed), request, beat, console)),
        Err(NotLaidOut::Refused(why)) => return Err(layout_refused(request, &why, beat, console)),
    };
    inputs_matched(request, &held, beat, console)?;
    if let Err(failed) = git.checkout(request, console) {
        return Err(ended_before(said(console, &failed), request, beat, console));
    }
    // Released before the command, which may change the work folder as it likes.
    drop(held);
    if !folders.place.is_dir() {
        console.err(&format!(
            "gate: remote subfolder {} does not exist in the snapshot",
            escaped(request.subfolder)
        ));
        return Err(ended_before(u32::from(USAGE_STATUS), request, beat, console));
    }
    let restored =
        tools::update(request, folders.work, folders.place, beat, console).and_then(|()| packages::update(request, git, folders, beat, console));
    match restored {
        // The tools' refusal, its tag said: the PC runs the command itself and syncs nothing back.
        Err(REFUSED_STATUS) => Err(REFUSED_STATUS),
        Err(status) => Err(ended_before(status, request, beat, console)),
        Ok(()) => Ok(()),
    }
}

/// A run that ended before its command with `status`, its lines said: on a pair run, the box's back commits `failed`
/// line follows them ([`back_commits_failed`]), so a pair run the box did not refuse never ends without a back commits
/// line. `status`.
fn ended_before(status: u32, request: &Request<'_>, beat: &Beat<'_>, console: &mut Console) -> u32 {
    back_commits_failed(request, beat, console);
    status
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

/// On a pair run, says `gate: remote back commits: failed` under the box's output lock, for a run whose back commits
/// were not all pushed: the PC applies nothing, and a back commits line the command printed itself is no longer the
/// last.
fn back_commits_failed(request: &Request<'_>, beat: &Beat<'_>, console: &mut Console) {
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    no_back_commits(request, console);
}

/// On a pair run, says `gate: remote back commits: failed`, for a run that ended before its command without a refusal;
/// the caller holds the box's output lock, or nothing else writes to the box's output yet.
fn no_back_commits(request: &Request<'_>, console: &mut Console) {
    if !request.siblings.is_empty() {
        console.err("gate: remote back commits: failed");
    }
}

/// Says the failed step's line; its status.
fn said(console: &mut Console, failed: &Failed) -> u32 {
    console.err(&failed.line);
    failed.status
}

/// Refuses the run `layout` for `why`: its refusal tag, then the line naming the main repo and why
/// ([`crate::remote::layout_line`]), said together under the box's output lock, so the PC runs the command itself; 75.
fn layout_refused(request: &Request<'_>, why: &str, beat: &Beat<'_>, console: &mut Console) -> u32 {
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    console.err(&refused_line(request.nonce, Refusal::Layout));
    console.err(&crate::remote::layout_line(request.repo_name(), &escaped(why)));
    REFUSED_STATUS
}

/// The line a pair run says once every back commit is pushed: `gate: remote back commits: ` and, for each repo, the
/// main one first, its name and its back commit, or `none` for a repo whose tree is unchanged, comma-separated.
fn back_commits_line(request: &Request<'_>, commits: &[Option<String>]) -> String {
    let named: Vec<String> = request
        .repos()
        .zip(commits)
        .map(|(repo, commit)| format!("{} {}", repo.name.as_deref().unwrap_or_default(), commit.as_deref().unwrap_or("none")))
        .collect();
    format!("gate: remote back commits: {}", named.join(", "))
}

/// Why the work folder could not be laid out for a run ([`WorkGit::lay_out`]).
enum NotLaidOut {
    /// A removal or a layout step failed, for this reason: the run is refused `layout`.
    Refused(String),
    /// A git step readying a tree failed.
    Failed(Failed),
}

impl From<Unready> for NotLaidOut {
    /// A git step's failure keeps its line and status; any other step's refuses the run `layout`.
    fn from(unready: Unready) -> NotLaidOut {
        match unready {
            Unready::Git(failed) => NotLaidOut::Failed(failed),
            Unready::Folder(why) => NotLaidOut::Refused(why),
        }
    }
}

/// The work folder `work` opened as itself and held, sharing reading and writing but not deletion; else why it could
/// not be: a link, a junction or anything not a folder there is refused, never followed.
fn hold_folder(work: &str) -> Result<File, String> {
    let folder = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(work)
        .map_err(|error| format!("cannot open {work}: {error}"))?;
    let metadata = folder.metadata().map_err(|error| format!("cannot read {work}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{work} is a link, a junction or not a folder"));
    }
    Ok(folder)
}

/// Removes every child of the work folder `work`, each as itself ([`preflight::remove_entry`]); else why one could not
/// be removed or the folder not listed.
fn clear_work(work: &Path) -> Result<(), NotLaidOut> {
    let unlisted = |error: io::Error| NotLaidOut::Refused(format!("cannot list {}: {error}", work.display()));
    for entry in fs::read_dir(work).map_err(unlisted)? {
        removed(&entry.map_err(unlisted)?.path())?;
    }
    Ok(())
}

/// Makes `tree` a folder of its own and its `.git`, when it has one, a folder of its own: a link, a junction or a file
/// at either is removed as itself, never what it leads to, and the tree made again.
fn plain_tree(tree: &str) -> Result<(), NotLaidOut> {
    let tree = Path::new(tree);
    if !preflight::is_real_folder(tree) {
        removed(tree)?;
        fs::create_dir(tree).map_err(|error| NotLaidOut::Refused(format!("cannot create {}: {error}", tree.display())))?;
    }
    let git_folder = tree.join(".git");
    if !preflight::is_real_folder(&git_folder) {
        removed(&git_folder)?;
    }
    Ok(())
}

/// Whether the layout record `record` names another layout than `layout`, no record naming a run of one repo's.
fn layout_changed(record: &str, layout: &str) -> bool {
    match tools::read_record(record) {
        Ok(None) => layout != LAYOUT_SINGLE,
        Ok(Some(text)) => !text.eq_ignore_ascii_case(layout),
        // Counted as changed, so the run writes the record before any clear: one that is not a plain file cannot be
        // written either, and refuses the run `layout` there with nothing cleared; one that can, such as one that is
        // not UTF-8, is cleared as after a clear that did not finish.
        Err(_) => true,
    }
}

/// Removes `path` as itself ([`preflight::remove_entry`]); else why it could not be.
fn removed(path: &Path) -> Result<(), NotLaidOut> {
    preflight::remove_entry(path).map_err(|error| NotLaidOut::Refused(format!("cannot remove {}: {error}", path.display())))
}

/// Writes `layout` as the whole of the layout record `record`, as the tools record is written
/// ([`tools::write_record_file`]); else why it could not be.
fn write_layout(record: &str, layout: &str) -> Result<(), NotLaidOut> {
    tools::write_record_file(record, layout).map_err(|error| NotLaidOut::Refused(format!("cannot write {record}: {error}")))
}

/// The work folder's lock: `path` opened for reading and writing, created when missing, shared with no one, never
/// through a link, and only as a plain disk file. `None` when another process has it open; else the line saying why it
/// could not be opened.
pub(super) fn take_lock(path: &str) -> Result<Option<File>, String> {
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
/// nothing; the tool restore and the pnpm install look at it the same way before they run, so such a stop runs neither
/// ([`crate::boxside::tools`], [`crate::boxside::packages`]). The command starts on a UTF-8 console when it can have one ([`start_command`]), whose
/// donor lives until the wait for the command is over, however it ends. The heartbeat watches the job while [`watch`]
/// waits for it. The command's environment is the box's own with the build-server settings of its run set over it
/// ([`Variables::with_build_servers`]), replacing any value the box inherited: the compiler server's pipe and
/// `MSBuild`'s node handshake are named by the run id `gate-box-<Nonce>`, so no other run's command, on the box or on
/// the PC, uses the servers this one starts, and the `MSBuild` server and node reuse are off. Git's `safe.directory` is
/// set to `*` over that ([`with_safe_directory`]), and the Event Log logging provider of a .NET host the command starts
/// is turned off over that ([`with_event_log_off`]). The command runs with a restricted copy of the box's token
/// ([`restricted_token`]).
fn command(request: &Request<'_>, place: &Path, beat: &Beat<'_>, console: &mut Console) -> Result<Ran, u32> {
    let run = format!("gate-box-{}", request.nonce);
    let variables = with_event_log_off(with_safe_directory(Variables::from_process().with_build_servers(&run)));
    let environment = variables
        .block()
        .map_err(|error| failed(console, &format!("cannot build the command's environment: {error}")))?;
    let Some((word, rest)) = request.command.split_first() else {
        return Err(failed(console, "the run has no command"));
    };
    let rest: Vec<OsString> = rest.iter().map(OsString::from).collect();
    let program = match find_program(word, &variables, place) {
        Ok(program) => program,
        Err(message) => return Ok(Ran::Exited(not_started(console, word, None, &message))),
    };
    let launch = match written(&program, &rest, &variables) {
        Written::Refused(line) => {
            console.err(&line);
            return Err(u32::from(USAGE_STATUS));
        }
        Written::NoShell(message) => return Ok(Ran::Exited(not_started(console, word, Some(&program), &message))),
        Written::Line(application, command_line) => PipedLaunch {
            application,
            command_line,
            directory: place.to_owned(),
            environment,
            console: PipedConsole::Hidden,
            token: restricted_token(console),
        },
    };
    before_the_job(beat.stop.look(STARTING_COMMAND), request.job, console)?;
    let job = create_job(request.job, console)?;
    // Started right after the job and kept until the command's wait is over, so its console outlives the command.
    let donor = console_donor(&job, launch.token.as_ref(), beat.seams);
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
        refuse_token: beat.seams.refuse_token,
    };
    let launched = match start_command(&mut start, donor.as_ref().map_err(String::as_str), beat, console) {
        Ok(launched) => launched,
        Err(answer) => return answer.map(Ran::Exited),
    };
    let mut stopper = Stopper { job: &job, armed: true };
    let keep: Vec<u32> = donor.as_ref().map(Donor::pid).into_iter().collect();
    let spec = Spec {
        timeout: request.timeout,
        host: escaped(request.host),
        spoken: escaped(&request.command.join(" ")),
        nonce: request.nonce,
    };
    let ceiling = Ceiling::begin(spec, &job, beat.lock, beat.seams, console);
    let started = Started {
        job: &job,
        launched,
        keep: &keep,
        beat,
        ceiling,
    };
    let watched = watch(started, console);
    stopper.armed = false;
    // Ended only now, so the command's console stays up for as long as the command may use it.
    drop(donor);
    watched
}

/// The variable that counts the config entries git reads from the environment.
const GIT_CONFIG_COUNT: &str = "GIT_CONFIG_COUNT";

/// `variables` with one more config entry for git, `safe.directory` set to `*`, so a git the command runs trusts the
/// work folder whoever owns it: the restricted token's user owns none of the folders the box made. The entry takes the
/// index `GIT_CONFIG_COUNT` holds, 0 when it is unset, and the count grows by one. A count git refuses
/// ([`git_config_count`]), or one at `i32::MAX` that one more entry would take past what git accepts, leaves
/// `variables` as they are.
pub(super) fn with_safe_directory(variables: Variables) -> Variables {
    let index = match variables.get(GIT_CONFIG_COUNT) {
        None => 0,
        Some(count) => match git_config_count(count) {
            Some(index) if index < i32::MAX.cast_unsigned() => index,
            _ => return variables,
        },
    };
    variables
        .with(GIT_CONFIG_COUNT, &(index + 1).to_string())
        .with(&format!("GIT_CONFIG_KEY_{index}"), "safe.directory")
        .with(&format!("GIT_CONFIG_VALUE_{index}"), "*")
}

/// `variables` with the Event Log logging provider of a .NET host turned off: `Logging__EventLog__LogLevel__Default`
/// and the colon spelling `Logging:EventLog:LogLevel:Default`, which .NET reads as the same key, both set to `None`,
/// replacing every value the box inherited for each name, names compared as Windows compares them. A colon is a legal
/// character in a Windows variable name, so the two names are different ones to [`Variables::with`] and an inherited
/// colon form would otherwise survive beside the `None` under the underscore form, whichever of them .NET reads first.
///
/// The command runs with a restricted copy of the box's token ([`restricted_token`]), which holds neither
/// Administrators nor INTERACTIVE, the only groups the Windows Application log lets write that a restricted copy of
/// that token could hold. The variable keeps a default .NET host, whose logging turns the provider on, working on a
/// builder that lacks the grant a builder's setup gives its user to that log (a fresh or reimaged one); there, its
/// first Warning would fail with `Win32Exception 5` and take the host down. Off, the host writes nothing to the Event
/// Log, and its console output, which the gate captures, is unchanged.
fn with_event_log_off(variables: Variables) -> Variables {
    variables
        .with("Logging__EventLog__LogLevel__Default", "None")
        .with("Logging:EventLog:LogLevel:Default", "None")
}

/// The count `GIT_CONFIG_COUNT` holds, read as git reads it, with `strtoul` taking the whole value: an empty value is
/// 0; else leading whitespace is skipped, then an optional `+` and decimal digits to the end of the value, at most
/// `i32::MAX`. `None` for anything else, which git refuses, a value of whitespace alone included, as `strtoul` then
/// converts nothing and leaves the whole value unread.
fn git_config_count(count: &OsStr) -> Option<u32> {
    let text = count.to_str()?;
    if text.is_empty() {
        return Some(0);
    }
    let text = text.trim_start_matches([' ', '\t', '\n', '\x0B', '\x0C', '\r']);
    let digits = text.strip_prefix('+').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok().filter(|&count| count <= i32::MAX.cast_unsigned())
}

/// A restricted copy of the box's token for the command ([`RestrictedToken::of_this_process`]): not elevated, at
/// medium integrity at most, owning the objects it creates as the user, and with the ssh logon's NETWORK group
/// deny-only, so a check for that group, as WMI's check for a remote caller is, no longer finds it, while deny entries
/// naming NETWORK still apply. `None` when it cannot be made, said on the
/// console: the command then runs with the box's own token.
fn restricted_token(console: &mut Console) -> Option<RestrictedToken> {
    match RestrictedToken::of_this_process() {
        Ok(token) => Some(token),
        Err(error) => {
            console.err(&own_token_line(&error));
            None
        }
    }
}

/// The line saying the command runs with the box's own token, as its restricted one could not be made or started
/// with, for `error`.
fn own_token_line(error: &dyn std::fmt::Display) -> String {
    format!("gate: the command runs with the builder session's own token: {error}")
}

/// Win32's `ERROR_ACCESS_DENIED`, the refusal the token seam stands in for.
const ACCESS_DENIED: u32 = 5;

/// The command to start: its launch, its job and the job's stop signal, the word and program a start failure names,
/// the run's nonce its start marker names, and whether the token seam refuses its start with the restricted token.
struct Start<'a> {
    launch: PipedLaunch,
    job: &'a Job,
    stop: &'a StopSignal<'a>,
    word: &'a str,
    program: &'a Path,
    nonce: &'a str,
    refuse_token: bool,
}

/// A command started in its job, and the read ends of its pipes.
struct Launched {
    running: Running,
    pipes: Pipes,
}

/// The console's donor, put in `job` and run with the command's `token` when it has one, or why there is none: the
/// console seam forces that before any donor starts.
fn console_donor(job: &Job, token: Option<&RestrictedToken>, seams: &BoxSeams) -> Result<Donor, String> {
    if seams.fail_console {
        return Err(FORCED.to_owned());
    }
    Donor::start(job, token)
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
/// launch ended, with why the UTF-8 console could not be had, for the caller to say. The console's keeper runs with the
/// command's token when it has one, as the donor does.
fn on_utf8_console(start: &mut Start<'_>, donor: &Donor, beat: &Beat<'_>, console: &mut Console) -> Ended {
    let lock = beat.lock;
    let hold = || match lock.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    };
    match Utf8Console::enter(donor, start.launch.token.as_ref(), hold) {
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
    /// The line saying the command runs with the box's own token, when its start with the restricted one was refused.
    own_token: Option<String>,
    /// The line saying the job's stop signal could not be read, when it could not.
    unread: Option<String>,
}

impl Ended {
    /// Says the fallback, then that the command runs with the box's own token, unless the launch was stopped, so
    /// neither line comes before a stop line; then that the stop signal could not be read.
    fn say(&self, console: &mut Console) {
        let not_stopped = !matches!(self.how, LaunchEnd::Stopped);
        if let Some(reason) = self.fallback.as_deref().filter(|_| not_stopped) {
            say_fallback(console, reason);
        }
        if let Some(line) = self.own_token.as_deref().filter(|_| not_stopped) {
            console.err(line);
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
    let (created, own_token) = create_suspended(start);
    let ended = |how, unread| Ended {
        how,
        fallback: None,
        own_token,
        unread,
    };
    let PipedSuspended { suspended, stdout, stderr } = match created {
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

/// Creates the command suspended: with its restricted token when it has one, and when `CreateProcessAsUserW` refuses
/// that, or the token seam says it does, once more with the box's own token, which it keeps from then on. Only a
/// failure of that second start is the launch's. The command created, or why not, and the line saying it runs with the
/// box's own token when the restricted start was refused and the second one created it.
fn create_suspended(start: &mut Start<'_>) -> (Result<PipedSuspended, LaunchError>, Option<String>) {
    if start.launch.token.is_none() {
        return (start.launch.start_suspended(start.job), None);
    }
    let restricted = if start.refuse_token {
        Err(LaunchError::CreateProcess {
            code: ACCESS_DENIED,
            message: gate_win32::win32_message(ACCESS_DENIED),
        })
    } else {
        start.launch.start_suspended(start.job)
    };
    match restricted {
        Err(error @ LaunchError::CreateProcess { .. }) => {
            start.launch.token = None;
            let created = start.launch.start_suspended(start.job);
            // A second start refused too was not refused for the token, as of a program that is missing: only its
            // own failure is said.
            let line = created.is_ok().then(|| own_token_line(&error));
            (created, line)
        }
        created => (created, None),
    }
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
/// Once the command has exited, what it left in its job but the processes of `keep` is ended ([`end_what_it_left`])
/// before the readers drain, so a leftover holding its output open costs no drain time, and before the outputs are
/// listed. Under the box's ceiling, a command that passes it has its job ended, the readers drain, then the ceiling
/// says the kill, and the run goes on as [`Ran::Killed`]; the ceiling's clock stops as the wait ends, and its adjusted
/// line is said once the readers drained, but for a client gone or a wait that failed. The look that finds a kill
/// hands over the ceiling with it, so a kill is always said by the ceiling that found it.
fn watch(started: Started<'_, '_>, console: &mut Console) -> Result<Ran, u32> {
    let Started {
        job,
        launched,
        keep,
        beat,
        mut ceiling,
    } = started;
    let Launched { running, pipes } = launched;
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
        running: &running,
        forwarders,
        watching: Watching::Command,
    };
    beat.watch(job);
    // No stop is looked for: a stop ends the command through its job, and the status it ends with passes.
    let waited = watched.wait(
        None,
        |console| {
            let kill = ceiling.as_mut()?.look(console)?;
            ceiling.take().map(|ceiling| (kill, ceiling))
        },
        console,
    );
    beat.unwatch();
    match waited {
        WaitEnd::Exited { code, .. } => {
            let ceiling = ceiling.map(Ceiling::closed);
            end_what_it_left(job, keep, console);
            watched.drained(console)?;
            if let Some(ceiling) = &ceiling {
                ceiling.ended(console);
            }
            Ok(Ran::Exited(code))
        }
        WaitEnd::Killed((kill, ceiling)) => killed(watched, kill, &ceiling.closed(), beat.seams, console),
        WaitEnd::ClientGone => Err(watched.client_gone(console)),
        WaitEnd::Failed(status) => Err(watched.wait_failed(status, console)),
    }
}

/// How a command the box started came to its end: it exited with its status, or the box's ceiling killed it.
pub(super) enum Ran {
    Exited(u32),
    Killed,
}

/// The box's ceiling found the command of `watched` past a limit for `kill`: its job is ended and the readers drain
/// ([`Watched::killed`]), a client gone meanwhile answered as such, with no back ref and no outputs; a job that still
/// holds processes, or could not be ended, is ended once more; then the ceiling says the kill, a note first when the
/// job was still not ended, and the run goes on as [`Ran::Killed`], a kill whatever the ending left.
/// `--test-box-kill-leaves` stands in for what that last ending leaves.
fn killed(watched: Watched<'_>, kill: Kill, ceiling: &Ceiling<'_>, seams: &BoxSeams, console: &mut Console) -> Result<Ran, u32> {
    let job = watched.job;
    let ending = match watched.killed(console)? {
        Ok(ending) if ending.left == 0 && seams.kill_leaves.is_none() => Ok(ending),
        _ => terminate_and_wait(job),
    };
    let ending = match seams.kill_leaves {
        Some(left) => Ok(Ending {
            left: usize::try_from(left).unwrap_or(usize::MAX),
            late: None,
        }),
        None => ending,
    };
    ceiling.killed(kill, &ending, console);
    Ok(Ran::Killed)
}

/// A command just started that [`watch`] sees to its end: its job, its main process and pipes, the processes of its
/// job that outlive it by design, the run's output lock and heartbeat, and the box's ceiling on it, `None` for none.
struct Started<'s, 'b> {
    job: &'s Arc<Job>,
    launched: Launched,
    keep: &'s [u32],
    beat: &'s Beat<'b>,
    ceiling: Option<Ceiling<'s>>,
}

/// Ends what the command that exited left in its `job` as a local run does ([`end_leftovers`]), its gates spared,
/// under the box's own state root, resolved as `gate run` resolves it with no `--state-dir`, and the processes of
/// `keep` spared too: the console's donor, which a spared gate may still be using, and which the box ends itself once
/// the wait for the command is over. A root that cannot be resolved is said in a fixed line, naming no path, and the
/// whole job terminated, so no build server outlives the run; the run's status stands.
fn end_what_it_left(job: &Job, keep: &[u32], console: &mut Console) {
    if let Ok(root) = resolve::root(None, &Environment::from_process()) {
        end_leftovers(job, &root.root, keep, console);
    } else {
        console.err("gate: cannot end the run's job: the box's state root cannot be resolved; ending the whole job");
        end_job(job, console);
    }
}

/// Which process the box watches, as its lines name it.
#[derive(Clone, Copy)]
pub(super) enum Watching {
    Command,
    Restore,
    Packages,
}

impl Watching {
    /// Its name in the line of a wait that failed.
    fn what(self) -> &'static str {
        match self {
            Watching::Command => "the command",
            Watching::Restore => "the restore",
            Watching::Packages => "the package restore",
        }
    }

    /// What the box does instead when it cannot read the stop signal while it waits.
    fn while_waiting(self) -> &'static str {
        match self {
            Watching::Command => "watching the command",
            Watching::Restore => "watching the restore",
            Watching::Packages => "watching the package restore",
        }
    }

    /// The line said when a leftover process still holds its output open once its main process exited.
    fn leftover(self) -> &'static str {
        match self {
            Watching::Command => "gate: remote command's output is still held open by a leftover process; stopped reading",
            Watching::Restore => "gate: remote the restore's output is still held open by a leftover process; stopped reading",
            Watching::Packages => "gate: remote the package restore's output is still held open by a leftover process; stopped reading",
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

/// How the wait for a watched process's main process ended, a look for a kill finding a `K`.
pub(super) enum WaitEnd<K> {
    /// It exited with `code`; `stopped` when a stop set the run's signal, looked at only when a signal was given.
    Exited { code: u32, stopped: bool },
    /// The look for a kill found this one first; the process still runs.
    Killed(K),
    /// A write to the box's own output failed first: the client closed the connection.
    ClientGone,
    /// The box could not wait for it: the status 1, its line said and its job terminated.
    Failed(u32),
}

impl Watched<'_> {
    /// Waits for the main process to exit, looking every [`WAIT_STEP`] whether the client is still there and, when
    /// `stop` is given, whether a stop came: a stop terminates the job, so the main process exits and the wait sees it,
    /// and is looked at once more as it exits, so a stop that ended the job before the box looked counts too. Each look
    /// while the process runs also asks `look` for a kill, which ends the wait.
    pub(super) fn wait<K>(
        &self,
        stop: Option<&StopSignal<'_>>,
        mut look: impl FnMut(&mut Console) -> Option<K>,
        console: &mut Console,
    ) -> WaitEnd<K> {
        let mut stopped = false;
        loop {
            if let Some(end) = self.step(stopped, stop, &mut look, console) {
                return end;
            }
            if !stopped && self.stop_seen(stop, console) {
                stopped = true;
                end_job(self.job, console);
            }
        }
    }

    /// One step of [`Watched::wait`]: its end when the main process exited, the client went, `look` found a kill or the
    /// wait failed; `None` while the process runs on. `stopped` is whether a stop was already seen.
    fn step<K>(
        &self,
        stopped: bool,
        stop: Option<&StopSignal<'_>>,
        look: &mut impl FnMut(&mut Console) -> Option<K>,
        console: &mut Console,
    ) -> Option<WaitEnd<K>> {
        match self.running.wait(WAIT_STEP) {
            Ok(Some(code)) => Some(WaitEnd::Exited {
                code,
                stopped: stopped || self.stop_seen(stop, console),
            }),
            Ok(None) if self.forwarders.pipe_closed() => Some(WaitEnd::ClientGone),
            Ok(None) => look(console).map(WaitEnd::Killed),
            Err(error) => {
                let status = failed(console, &format!("cannot wait for {}: {error}", self.watching.what()));
                end_job(self.job, console);
                Some(WaitEnd::Failed(status))
            }
        }
    }

    /// Whether `stop`, when given, says a stop came, the line saying it cannot be read said the first time.
    fn stop_seen(&self, stop: Option<&StopSignal<'_>>, console: &mut Console) -> bool {
        stop.is_some_and(|stop| stop.seen(self.watching.while_waiting(), console))
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

    /// The box's ceiling found the process past a limit: its job is ended first, then the readers drain for at most
    /// [`DRAIN`]; how the ending went, which the ceiling says. A write to the box's own output that failed meanwhile
    /// answers [`CLOSED_STATUS`], and a leftover process still holding the output open is said, as
    /// [`Watched::drained`] does.
    pub(super) fn killed(self, console: &mut Console) -> Result<io::Result<Ending>, u32> {
        let ending = terminate_and_wait(self.job);
        let drained = self.forwarders.drain(DRAIN);
        if drained.pipe_closed {
            return Err(closed_connection(self.job, ending, console));
        }
        if !drained.all_ended {
            console.err(self.watching.leftover());
        }
        Ok(ending)
    }

    /// The client closed the connection: the job is stopped first, then the readers drain for at most [`DRAIN`];
    /// [`CLOSED_STATUS`].
    pub(super) fn client_gone(self, console: &mut Console) -> u32 {
        let ending = terminate_and_wait(self.job);
        self.forwarders.drain(DRAIN);
        closed_connection(self.job, ending, console)
    }

    /// The wait for the main process failed, its line said: the job is ended and waited for, then the readers drain
    /// for at most [`DRAIN`], so no line of the process is forwarded after the box's next line; then what the ending
    /// left is said. `status`, the wait's own.
    pub(super) fn wait_failed(self, status: u32, console: &mut Console) -> u32 {
        let ending = terminate_and_wait(self.job);
        self.forwarders.drain(DRAIN);
        match ending {
            Ok(ending) => ending.say(self.job.name(), console),
            Err(error) => console.err(&format!("gate: remote could not stop job {}: {error}", self.job.name())),
        }
        status
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

/// The failure of a fetch that left the snapshot of `repo` out of its repository: the wrapper's line on a run of one
/// repo, and one naming the repo on a pair run; its status 1.
fn missing_snapshot(repo: &Repo<'_>) -> Failed {
    let sha = repo.snapshot;
    let line = match &repo.name {
        None => format!("gate: remote snapshot {sha} is not in the work folder's repository after the fetch"),
        Some(name) => format!("gate: remote snapshot {sha} is not in the repository of {name} after the fetch"),
    };
    Failed { line, status: FAILED_STATUS }
}

/// The failure of git `step` whose forwarded output found the client gone: the run ends as it ends when the command's
/// forwarded output finds it gone, with [`CLOSED_STATUS`], git having already ended. The line names git once, though a
/// tree-readying step's name starts with it.
fn client_gone(step: &str) -> Failed {
    let step = step.strip_prefix("git ").unwrap_or(step);
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

/// The run readies a tree with git as it runs every other step, under the lock it holds.
impl preflight::TreeGit for WorkGit {
    fn run_step(&self, step: &str, args: &[&str], console: &mut Console) -> Result<(), Failed> {
        self.git_step(step, args, console)
    }

    /// Both of git's streams discarded, so nothing it says lands outside the box's output lock.
    fn run_code(&self, args: &[&str]) -> Result<i32, String> {
        self.git.code(&self.args(args), Stdio::null(), Stdio::null())
    }

    /// A [`WorkGit`] is made only once the run holds the lock ([`WorkGit::ready`]).
    fn holds_the_lock(&self) -> bool {
        true
    }
}

/// git as the box runs it in the work folder: none of the variables that would point it elsewhere, no standard input,
/// its hooks read from a folder that cannot exist, no file-system monitor, and its output forwarded under the box's
/// output lock.
pub(super) struct WorkGit {
    pub(super) git: Git,
    /// `core.hooksPath=<lock>\hooks`: a folder inside the lock file, which cannot exist while the run holds the lock
    /// open sharing nothing, since a file has no children and cannot be deleted, renamed or replaced meanwhile.
    hooks: String,
    /// The box's output lock, which every line git writes is forwarded under.
    output: OutputLock,
}

impl WorkGit {
    /// Finds git, its output to be forwarded under `output`; called only once the caller holds the work folder's lock
    /// file `lock`.
    fn ready(lock: &str, output: &OutputLock) -> WorkGit {
        WorkGit {
            git: Git::find(),
            hooks: format!(r"core.hooksPath={lock}\hooks"),
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
    pub(super) fn args<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut all = vec!["-c", self.hooks.as_str(), "-c", "core.fsmonitor=false"];
        all.extend_from_slice(args);
        all
    }

    /// Runs `step` in the tree of `repo` with `args`, its output forwarded, its line naming the repo on a pair run.
    fn step(&self, step: &str, repo: &Repo<'_>, args: &[&str], console: &mut Console) -> Result<(), Failed> {
        let mut all = repo.at().to_vec();
        all.extend_from_slice(args);
        self.git_step(&repo.step(step), &all, console)
    }

    /// Lays out the work folder for the run, before any checkout: the folder made when missing, opened as itself and
    /// held ([`hold_folder`]); its children all removed when its layout record names another layout than the run's
    /// ([`layout_changed`]), the record saying [`LAYOUT_CHANGING`] first ([`clear_work`]); then each repo's tree
    /// readied ([`WorkGit::ready_trees`]) on a pair run, after a clear, or when the main tree's `.git`, `.git\objects`
    /// or `.git\objects\info` is missing or not a folder of its own, or its `.git\config` is there and not a file of
    /// its own, a link there leading git or the alternates file into another repository ([`preflight::git_is_plain`]);
    /// and the run's layout recorded after a clear. A clear removes the inputs record `{Work}.inputs` too, before the
    /// work folder's children, so no record outlives the inputs it lists. A run of one repo in a work folder of that
    /// layout whose `.git` is plain readies nothing here: the preflight readied its tree. The held folder, which the
    /// caller keeps through the checkouts.
    fn lay_out(&self, laid: &Laid<'_>, console: &mut Console) -> Result<File, NotLaidOut> {
        let work = Path::new(laid.work);
        fs::create_dir_all(work).map_err(|error| NotLaidOut::Refused(format!("cannot create {}: {error}", laid.work)))?;
        let held = hold_folder(laid.work).map_err(NotLaidOut::Refused)?;
        let record = format!("{}.layout", laid.work);
        let layout = laid.layout();
        let changed = layout_changed(&record, &layout);
        if changed {
            write_layout(&record, LAYOUT_CHANGING)?;
            removed(Path::new(&inputs::record_path(laid.work)))?;
            clear_work(work)?;
        }
        if changed || !laid.siblings.is_empty() || !preflight::git_is_plain(Path::new(&laid.main.tree)) {
            self.ready_trees(laid, console)?;
        }
        if changed {
            write_layout(&record, &layout)?;
        }
        Ok(held)
    }

    /// Each repo's tree and its `.git` made folders of their own ([`plain_tree`]) and readied as the preflight readies
    /// a run of one repo's ([`preflight::ready_tree`]), under the lock, so a link at `.git\objects` or
    /// `.git\objects\info` is replaced by a folder too.
    fn ready_trees(&self, laid: &Laid<'_>, console: &mut Console) -> Result<(), NotLaidOut> {
        for repo in laid.repos() {
            plain_tree(&repo.tree)?;
            preflight::ready_tree(self, &repo.bare, &repo.tree, repo.name.as_deref(), console).map_err(NotLaidOut::from)?;
        }
        Ok(())
    }

    /// Each repo's snapshot, one repo after another, the main one first ([`WorkGit::checkout_repo`]).
    fn checkout(&self, request: &Request<'_>, console: &mut Console) -> Result<(), Failed> {
        request.repos().try_for_each(|repo| self.checkout_repo(request, repo, console))
    }

    /// The snapshot of `repo`, fetched from its bare repository at `refs/gate/<key>` and checked out by its commit over
    /// whatever the last run left, untracked files removed and ignored ones (build output) kept, and no back commit of
    /// an earlier run left behind. The commit, never the ref's tip, is what is checked out, so a later push of the key
    /// changes nothing this run runs. A commit the repo's repository lacks after the fetch ends the run with the box's
    /// own line.
    fn checkout_repo(&self, request: &Request<'_>, repo: &Repo<'_>, console: &mut Console) -> Result<(), Failed> {
        let refname = format!("refs/gate/{}", request.key);
        self.step("fetch", repo, &["fetch", "-q", &repo.bare, &refname], console)?;
        let commit = format!("{}^{{commit}}", repo.snapshot);
        match self.quiet(repo, &["cat-file", "-e", &commit]) {
            Ok(0) => {}
            Ok(_) => return Err(missing_snapshot(repo)),
            Err(message) => {
                return Err(Failed {
                    line: format!("gate: remote {} failed: {message}", repo.step("cat-file")),
                    status: FAILED_STATUS,
                });
            }
        }
        self.step("checkout", repo, &["checkout", "-q", "--force", "--detach", repo.snapshot], console)?;
        self.step("clean", repo, &["clean", "-fdq"], console)?;
        // A pair run cleared every repo's back ref as soon as it held the lock ([`WorkGit::clear_back_refs`]).
        if request.siblings.is_empty() {
            self.clear_back_ref(request, repo, console)?;
        }
        Ok(())
    }

    /// On a pair run, every repo's back ref deleted ([`WorkGit::clear_back_ref`]), so no repo keeps an earlier run's
    /// back commit whatever step of this run fails. Nothing on a run of one repo, whose checkout deletes it.
    fn clear_back_refs(&self, request: &Request<'_>, console: &mut Console) -> Result<(), Failed> {
        if request.siblings.is_empty() {
            return Ok(());
        }
        request.repos().try_for_each(|repo| self.clear_back_ref(request, repo, console))
    }

    /// git's exit code for `args` in the tree of `repo`, both of its streams discarded, as the wrapper discards
    /// `cat-file`'s own; else why git could not be started.
    fn quiet(&self, repo: &Repo<'_>, args: &[&str]) -> Result<i32, String> {
        let mut all = repo.at().to_vec();
        all.extend_from_slice(args);
        self.git.code(&self.args(&all), Stdio::null(), Stdio::null())
    }

    /// Deletes `refs/gate-back/<key>` in the bare repository of `repo`, an earlier run's back commit, so the client
    /// finds none to fetch after a run that makes none.
    fn clear_back_ref(&self, request: &Request<'_>, repo: &Repo<'_>, console: &mut Console) -> Result<(), Failed> {
        let bare = format!("--git-dir={}", repo.bare);
        let back = format!("refs/gate-back/{}", request.key);
        self.git_step(&repo.step(BACK_REF_RESET), &[&bare, "update-ref", "-d", &back], console)
    }

    /// After a failed push, the back ref of each repo of `pushed` deleted ([`WorkGit::clear_back_ref`]), every one
    /// tried: a failure is said under the output lock, and the first client gone during one is given back once every
    /// ref has been tried, so the run ends on it as on any client gone ([`client_gone`]).
    fn undo_pushes(&self, request: &Request<'_>, pushed: &[&Repo<'_>], console: &mut Console) -> Option<Failed> {
        let mut gone = None;
        for repo in pushed {
            let Err(undone) = self.clear_back_ref(request, repo, console) else {
                continue;
            };
            if undone.line == client_gone(&repo.step(BACK_REF_RESET)).line {
                gone.get_or_insert(undone);
            } else {
                let _held = self.output.lock().unwrap_or_else(PoisonError::into_inner);
                console.err(&undone.line);
            }
        }
        gone
    }

    /// The back commit of each repo, all or none: every repo's commit made first, the main one first
    /// ([`WorkGit::repo_back_commit`]), then each pushed ([`WorkGit::push_back`]), and a failed push deleting every
    /// back ref this run pushed and, on a pair run, the failing repo's own ([`WorkGit::undo_pushes`]), so the PC finds
    /// a back commit of every changed repo or of none. A pair run then says each repo's commit, or none, on one line
    /// ([`back_commits_line`]). The main repo's commit, `None` when its tree is unchanged; else why one failed.
    fn back_commit(&self, request: &Request<'_>, console: &mut Console) -> Result<Option<String>, BackFailed> {
        let mut commits = Vec::with_capacity(1 + request.siblings.len());
        for repo in request.repos() {
            commits.push(self.repo_back_commit(request, repo, console)?);
        }
        let mut pushed = Vec::with_capacity(commits.len());
        for (repo, commit) in request.repos().zip(&commits) {
            let Some(commit) = commit else { continue };
            if let Err(failed) = self.push_back(request, repo, commit, console) {
                // On a pair run, the failing repo's own ref too: a push that failed after its ref was updated leaves
                // it behind. A run of one repo takes no step it did not take before.
                if !request.siblings.is_empty() {
                    pushed.push(repo);
                }
                return Err(self.undo_pushes(request, &pushed, console).map_or(failed, BackFailed::ClientGone));
            }
            pushed.push(repo);
        }
        if !request.siblings.is_empty() {
            let _held = self.output.lock().unwrap_or_else(PoisonError::into_inner);
            console.err(&back_commits_line(request, &commits));
        }
        Ok(commits.into_iter().next().flatten())
    }

    /// Pushes the back commit `commit` of `repo` to `refs/gate-back/<key>` in its bare repository; else why it failed.
    fn push_back(&self, request: &Request<'_>, repo: &Repo<'_>, commit: &str, console: &mut Console) -> Result<(), BackFailed> {
        let program = self
            .git
            .program
            .as_ref()
            .map_err(|message| BackFailed::Step(format!("git cannot be started: {message}")))?;
        let back = BackGit {
            program,
            request,
            repo,
            work: self,
        };
        let target = format!("{commit}:refs/gate-back/{}", request.key);
        back.status("push", &["push", "-q", &repo.bare, &target], false, console)
    }

    /// The tree the command left in `repo`, committed on `HEAD`, unpushed; nothing when the tree is unchanged. It
    /// starts from a copy of the repo's index, which the checkout just refreshed, so `add -A` skips every unchanged
    /// file; a copy that cannot be removed afterwards is said and fails nothing. The run's one `Index` file is used by
    /// each repo in turn. The commit, `None` when the tree is unchanged; else why it failed.
    fn repo_back_commit(&self, request: &Request<'_>, repo: &Repo<'_>, console: &mut Console) -> Result<Option<String>, BackFailed> {
        let program = self
            .git
            .program
            .as_ref()
            .map_err(|message| BackFailed::Step(format!("git cannot be started: {message}")))?;
        let back = BackGit {
            program,
            request,
            repo,
            work: self,
        };
        let index = Path::new(&repo.tree).join(".git").join("index");
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
        Ok(Some(commit))
    }

    /// The `/`-separated paths the main repo's back commit `commit` changes against `HEAD`; else why they could not be
    /// read.
    fn back_paths(&self, request: &Request<'_>, commit: &str, console: &mut Console) -> Result<Vec<String>, BackFailed> {
        let program = self
            .git
            .program
            .as_ref()
            .map_err(|message| BackFailed::Step(format!("git cannot be started: {message}")))?;
        BackGit {
            program,
            request,
            repo: &request.main,
            work: self,
        }
        .changed(commit, console)
    }
}

/// git as the back commit runs it: in the tree of `repo` as [`WorkGit`] runs it, with the payload's identity, its
/// steps named with the repo's name on a pair run.
struct BackGit<'a> {
    program: &'a Path,
    request: &'a Request<'a>,
    repo: &'a Repo<'a>,
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
        let mut all = self.repo.at().to_vec();
        all.extend_from_slice(args);
        command.args(self.work.args(&all)).stdin(Stdio::null());
        command
    }

    /// Runs `step` to its end, its stderr, and its stdout unless `read_stdout`, forwarded ([`Git::forwarded`]); its
    /// output, else why it could not run or that the client is gone.
    fn run(&self, step: &str, args: &[&str], copied_index: bool, read_stdout: bool, console: &mut Console) -> Result<Output, BackFailed> {
        let step = self.repo.step(step);
        Git::forwarded(self.command(args, copied_index), read_stdout, &self.work.output, console).map_err(|failure| match failure {
            GitFailure::Failed(error) => BackFailed::Step(format!("{step} failed: {error}")),
            GitFailure::ClientGone => BackFailed::ClientGone(client_gone(&step)),
        })
    }

    /// Runs `step`, its output forwarded; else why it failed.
    fn status(&self, step: &str, args: &[&str], copied_index: bool, console: &mut Console) -> Result<(), BackFailed> {
        let output = self.run(step, args, copied_index, false, console)?;
        match exit_code(output.status) {
            0 => Ok(()),
            code => Err(BackFailed::Step(format!("{} failed (status {code})", self.repo.step(step)))),
        }
    }

    /// Runs `step` and returns its stdout, its stderr forwarded; else why it failed.
    fn stdout(&self, step: &str, args: &[&str], copied_index: bool, console: &mut Console) -> Result<Vec<u8>, BackFailed> {
        let output = self.run(step, args, copied_index, true, console)?;
        match exit_code(output.status) {
            0 => Ok(output.stdout),
            code => Err(BackFailed::Step(format!("{} failed (status {code})", self.repo.step(step)))),
        }
    }

    /// Runs `step` and returns the first line of its stdout, its stderr forwarded; else why it failed.
    fn line(&self, step: &str, args: &[&str], copied_index: bool, console: &mut Console) -> Result<String, BackFailed> {
        let stdout = self.stdout(step, args, copied_index, console)?;
        String::from_utf8_lossy(&stdout)
            .lines()
            .next()
            .map(str::to_owned)
            .ok_or_else(|| BackFailed::Step(format!("{} printed nothing", self.repo.step(step))))
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

    use super::{CLOSED_STATUS, GIT_CONFIG_COUNT, Look, STARTING_COMMAND, before_the_job, client_gone, look, with_safe_directory};
    use crate::console::Console;
    use crate::environment::Variables;

    const JOB: &str = r"Local\gate-job-k-0001";

    #[test]
    fn a_client_gone_line_names_its_git_step_once() {
        let gone = |step: &str| client_gone(step).line;
        assert_eq!(gone("git init"), "gate: remote the client closed the connection during git init");
        assert_eq!(
            gone("git config core.autocrlf for lib"),
            "gate: remote the client closed the connection during git config core.autocrlf for lib"
        );
        assert_eq!(gone("fetch lib"), "gate: remote the client closed the connection during git fetch lib");
        assert_eq!(client_gone("git init").status, CLOSED_STATUS);
    }

    /// The box's environment with `GIT_CONFIG_COUNT` set to `count`.
    fn counted(count: &str) -> Variables {
        Variables::from_process().with(GIT_CONFIG_COUNT, count)
    }

    /// The count, key and value `with_safe_directory` leaves at `index` over a `GIT_CONFIG_COUNT` of `count`.
    fn entry_over(count: &str, index: u32) -> [Option<String>; 3] {
        let variables = with_safe_directory(counted(count));
        let read = |name: &str| variables.get(name).map(|value| value.to_string_lossy().into_owned());
        [
            read(GIT_CONFIG_COUNT),
            read(&format!("GIT_CONFIG_KEY_{index}")),
            read(&format!("GIT_CONFIG_VALUE_{index}")),
        ]
    }

    /// The entry `with_safe_directory` adds at `index`, the count then being `count`.
    fn safe_directory_at(count: &str) -> [Option<String>; 3] {
        [Some(count.to_owned()), Some("safe.directory".to_owned()), Some("*".to_owned())]
    }

    #[test]
    fn safe_directory_takes_index_0_over_an_empty_count() {
        assert_eq!(entry_over("", 0), safe_directory_at("1"));
    }

    #[test]
    fn safe_directory_takes_the_index_a_count_after_leading_whitespace_holds() {
        assert_eq!(entry_over(" 2", 2), safe_directory_at("3"));
    }

    #[test]
    #[expect(clippy::expect_used, reason = "an environment whose block cannot be built must fail the case loudly")]
    fn safe_directory_leaves_the_environment_unchanged_over_a_count_git_refuses() {
        for count in [" ", "x", "2x", "2147483648"] {
            let unchanged = format!("{:?}", counted(count).block().expect("the block is built"));
            let after = format!("{:?}", with_safe_directory(counted(count)).block().expect("the block is built"));
            assert_eq!(after, unchanged, "{count:?}");
        }
    }

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
