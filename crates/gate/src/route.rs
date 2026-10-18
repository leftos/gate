//! Where a run's command runs: the facts [`gate_router::decide`] asks for, read from the routing file, git and the
//! drive table, then, for a command that would still route, the judgement of its work folder's warm-up
//! ([`gate_router::warm::judge`]), and the lines they say. git runs on the run thread, before any command is started.

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::{Duration, SystemTime};

use gate_args::{GATE_SET_NAMES, RunRequest, Slot, TestSeams, command_line};
use gate_desktop::version::build_version;
use gate_router::warm::{
    FileRead, Judgement, RecordRead, Situation, failed_line, inside_job_line, judge, lock_problem_line, start_failed_line, starting_line,
    warming_line,
};
use gate_router::{
    Config, ConfigProblem, Facts, FolderId, Inputs, LocalReason, Pattern, Place, RemoteRun, Route, SiblingFolder, decide, dropped_env,
    dropped_env_line,
};
use gate_state::v1;
use gate_win32::{Detached, DetachedLaunch, Share};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetDriveTypeW,
};

use crate::console::Console;
use crate::environment::Variables;
use crate::remote::{git, siblings};
use crate::resolve::{Environment, ResolvedRoot};
use crate::warm;

/// The `notRouted` value of a heavy gate its caller kept here with `--local`.
const LOCAL_FLAG: &str = "--local";
/// The `notRouted` value of a matched command kept here because its outside path lies in another git worktree.
const ANOTHER_WORKTREE: &str = "another worktree";
/// The largest file of `.gate` the gate reads: 64 KiB.
const GATE_FILE_MOST_BYTES: usize = 64 * 1024;
/// The problem of a file of `.gate` that is a link, a junction or a folder.
const GATE_FILE_NOT_A_FILE: &str = "is a link, a junction or not a file";
/// Why a sibling folder that is a link or a junction is refused, read after its name.
pub(crate) const SIBLING_LINK: &str = "is a link or a junction";
/// Why a sibling that is a file is refused, read after its name.
pub(crate) const SIBLING_NOT_A_FOLDER: &str = "is not a folder";
/// Why a sibling whose `.git` is a link or a junction is refused, read after its name.
pub(crate) const SIBLING_GIT_LINK: &str = "has a .git that is a link or a junction";
/// Why a command that would route runs here when one of its words is not valid Unicode.
const NOT_UNICODE: &str = "a word of the command is not valid Unicode";
/// How long a git call of the hint's decision may take while the run still holds its light slot: a call that cannot
/// answer in time means no hint, silently ([`would_route_on_heavy`]).
const HINT_GIT_LIMIT: Duration = Duration::from_secs(5);

/// The arguments of the git call that places a folder: its worktree root, the folder below it, and the common git
/// folder, whose parent's name is the checkout's.
pub(crate) const PLACE_ARGS: [&str; 5] = [
    "rev-parse",
    "--show-toplevel",
    "--show-prefix",
    "--path-format=absolute",
    "--git-common-dir",
];

/// What the routing decision learnt of the gate's place in its git worktree.
pub(crate) enum Placed {
    /// The decision did not ask git.
    NotAsked,
    /// The decision asked git: the place git gave, `None` when it gave none or the call failed.
    Asked(Option<Place>),
}

impl Placed {
    /// The place git gave, when the decision asked and git gave one.
    pub(crate) fn place(&self) -> Option<&Place> {
        match self {
            Placed::Asked(place) => place.as_ref(),
            Placed::NotAsked => None,
        }
    }
}

/// Where a run's command runs ([`route`]), what the decision learnt of the gate's place in its git worktree, the hash of
/// the warm-up file the routed run's work folder must be warm with when the warm-up asks one, and what the decision saw,
/// which a passed light run asks again as a heavy one ([`would_route_on_heavy`]).
pub(crate) struct Decided {
    pub route: Route,
    pub placed: Placed,
    pub warm_hash: Option<String>,
    pub decision: Decision,
}

/// What the routing decision saw of the run, kept so a passed light run can ask whether its command would route on heavy
/// without reading the routing file or git on any other run: the [`Inputs`] the run's own decision had, less the slot it
/// asked for, and what [`Machine::new`] read of the environment.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent input of the decision, read by name; no two form a state machine"
)]
pub(crate) struct Decision {
    covering_parent: bool,
    local: bool,
    git_hook: bool,
    words: Vec<String>,
    unicode: bool,
    local_app_data: Option<OsString>,
}

impl Decision {
    /// What the decision of a run with `request` sees under `covering_parent`, read from `environment`.
    fn of(request: &RunRequest, environment: &Environment, covering_parent: bool) -> Decision {
        Decision {
            covering_parent,
            local: request.local,
            git_hook: environment.git_index_file.as_deref().is_some_and(|value| !value.is_empty()),
            words: request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect(),
            unicode: request.command.iter().all(|word| word.to_str().is_some()),
            local_app_data: environment.local_app_data.clone(),
        }
    }

    /// The inputs the decision had, with `slot` in place of the run's own.
    fn inputs(&self, slot: gate_router::Slot) -> Inputs<'_> {
        Inputs {
            slot,
            covering_parent: self.covering_parent,
            local: self.local,
            git_hook: self.git_hook,
            words: &self.words,
        }
    }
}

/// Decides where `request`'s command runs and says the decision's lines, on stderr and in the log: why a command runs
/// here when there is a reason to say, or the lines of a routed run, the line of the watched variables it does not send
/// ([`dropped_line`]) after them. A command that would route but has a word that is not valid Unicode runs here, saying
/// so. A command that would still route then has its work folder's warm-up judged against the state root `root`
/// ([`warmed`]). A gate under a `covering_parent`'s slot runs here, saying nothing. With the route, what the decision
/// learnt of the gate's place in its git worktree, and the hash of the warm-up file the routed run's work folder must be
/// warm with, when the warm-up asks one.
pub(crate) fn route(request: &RunRequest, environment: &Environment, covering_parent: bool, root: &ResolvedRoot, console: &mut Console) -> Decided {
    let decision = Decision::of(request, environment, covering_parent);
    let machine = Machine::new(environment, &request.test_seams);
    let route = match decide(&decision.inputs(slot(request.slot)), &machine) {
        Route::Routed(_) if !decision.unicode => Route::Local {
            reason: Some(LocalReason::Failed {
                reason: NOT_UNICODE.to_owned(),
            }),
        },
        route => route,
    };
    let (route, warm_hash) = match route {
        Route::Routed(remote) => warmed(remote, &machine, root, &request.test_seams, console),
        local @ Route::Local { .. } => (local, None),
    };
    match &route {
        Route::Local { reason: Some(reason) } => console.err(&reason.line()),
        Route::Local { reason: None } => {}
        Route::Routed(run) => {
            run.lines().iter().for_each(|line| console.err(line));
            if let Some(line) = dropped_line(request) {
                console.err(&line);
            }
        }
    }
    Decided {
        route,
        placed: machine.found.into_inner(),
        warm_hash,
        decision,
    }
}

/// Whether a light run whose routing decision saw `decision` would route on heavy: the decision asked again with the
/// heavy slot and every other input exactly as the run's own had it, routed only when every word of the command is valid
/// Unicode (the [`NOT_UNICODE`] rule [`route`] applies). Its git calls run under [`HINT_GIT_LIMIT`], since the run still
/// holds its light slot, and a call that cannot answer in time means no hint. It reads the routing file and git as the
/// decision would, and never judges the work folder's warm-up ([`warmed`]), starts one, or says a line; nor does it
/// look up the worktree an outside path lies in, which only changes the line of a run kept here.
pub(crate) fn would_route_on_heavy(decision: &Decision, seams: &TestSeams) -> bool {
    let machine = Machine {
        look_up_trees: false,
        ..Machine::at(
            decision.local_app_data.as_deref(),
            git::limit(seams).min(HINT_GIT_LIMIT),
            git::drain(seams),
        )
    };
    decision.unicode && matches!(decide(&decision.inputs(gate_router::Slot::Heavy), &machine), Route::Routed(_))
}

/// The line a routed run says for the watched variables of the gate's environment it does not send
/// ([`gate_router::dropped_env`]): those no `--env` pair of `request` names and the gate does not set itself. A name
/// that is not valid Unicode is left out. `None` when it sends them all.
fn dropped_line(request: &RunRequest) -> Option<String> {
    let caller: Vec<String> = std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()).collect();
    let sent: Vec<&str> = request.env.iter().map(|(name, _)| name.as_str()).collect();
    dropped_env_line(&dropped_env(caller.iter().map(String::as_str), &sent, GATE_SET_NAMES))
}

/// Whether the warm-up lock at `lock` is held ([`warm::lock_held`]); else the line of a lock that cannot be probed.
fn probed_lock(lock: &Path) -> Result<bool, String> {
    warm::lock_held(lock).map_err(|error| lock_problem_line(&lock.to_string_lossy(), &error.to_string()))
}

/// The route of `remote`, a command that would otherwise route, once its work folder's warm-up is judged
/// ([`gate_router::warm::judge`]): routed as it was, with the warm-up file's hash when the work folder is warm for it
/// or with none when the repo has no consent or no warm-up file; else here, the judgement's line said. A work folder
/// that needs a warm-up has one started ([`start_warm_up`]) before its lines are said; the run stays here either way.
fn warmed(remote: Box<RemoteRun>, machine: &Machine<'_>, root: &ResolvedRoot, seams: &TestSeams, console: &mut Console) -> (Route, Option<String>) {
    let (judgement, hash) = judged(&remote, machine, root);
    let lines = match judgement {
        Judgement::Route => return (Route::Routed(remote), None),
        Judgement::Ready => return (Route::Routed(remote), hash),
        Judgement::Warming { host } => vec![warming_line(&host)],
        Judgement::Start => start_warm_up(&remote, root, seams),
        Judgement::Failed { minutes_ago, reason } => vec![failed_line(minutes_ago, &reason)],
        Judgement::Problem(line) => vec![line],
    };
    for line in &lines {
        console.err(line);
    }
    (Route::Local { reason: None }, None)
}

/// Starts `gate warm --background --state-dir <root>`, this gate's own exe, in `remote`'s worktree root with this
/// gate's whole environment, detached ([`DetachedLaunch`]), and returns the lines to say: [`starting_line`], with
/// [`inside_job_line`] below it when the warm-up runs inside this run's job, or [`start_failed_line`] when it could not
/// be started. The start is a request, never a wait: the warm-up takes its own lock, and one that finds it held exits.
/// With the spawn seam (`--test-warm-spawn`), the command line is appended to the seam's file in place of a start.
fn start_warm_up(remote: &RemoteRun, root: &ResolvedRoot, seams: &TestSeams) -> Vec<String> {
    let directory = Path::new(&remote.root);
    let started = std::env::current_exe()
        .map_err(|error| format!("cannot find this gate's own exe: {error}"))
        .and_then(|program| {
            let words = [
                program.as_os_str(),
                OsStr::new("warm"),
                OsStr::new("--background"),
                OsStr::new("--state-dir"),
                root.path.as_os_str(),
            ];
            match &seams.warm_spawn {
                Some(file) => record_spawn(file, &words, directory),
                None => launch_detached(&program, &words, directory),
            }
        });
    match started {
        Ok(Detachment::OutsideJob) => vec![starting_line(&remote.host)],
        Ok(Detachment::InsideJob) => vec![starting_line(&remote.host), inside_job_line()],
        Err(error) => vec![start_failed_line(&remote.host, &error)],
    }
}

/// Where a started warm-up runs, as [`Detached`] says it.
enum Detachment {
    OutsideJob,
    InsideJob,
}

/// Starts `program` with the command line of `words`, the program's own first, detached in `directory` with this
/// gate's whole environment; else the error.
fn launch_detached(program: &Path, words: &[&OsStr], directory: &Path) -> Result<Detachment, String> {
    let wide: Vec<Vec<u16>> = words.iter().map(|word| word.encode_wide().collect()).collect();
    let launch = DetachedLaunch {
        application: program.to_path_buf(),
        command_line: command_line(&wide),
        directory: directory.to_path_buf(),
        environment: Variables::from_process()
            .block()
            .map_err(|error| format!("cannot build the environment: {error}"))?,
    };
    match launch.start().map_err(|error| error.to_string())? {
        Detached::OutsideJob(_) => Ok(Detachment::OutsideJob),
        Detached::InsideJob(_) => Ok(Detachment::InsideJob),
    }
}

/// The spawn seam: appends `words`, then `cwd=<directory>`, separated by tabs, as one line to `file`, as a warm-up
/// started outside the job.
fn record_spawn(file: &Path, words: &[&OsStr], directory: &Path) -> Result<Detachment, String> {
    let mut fields: Vec<String> = words.iter().map(|word| word.to_string_lossy().into_owned()).collect();
    fields.push(format!("cwd={}", directory.display()));
    let cannot = |error: io::Error| format!("cannot write the spawn file {}: {error}", file.display());
    let mut opened = fs::OpenOptions::new().create(true).append(true).open(file).map_err(cannot)?;
    writeln!(opened, "{}", fields.join("\t")).map_err(cannot)?;
    Ok(Detachment::OutsideJob)
}

/// The judgement of `remote`'s work folder's warm-up, with the hash of the warm-up file when one parsed. Consent is
/// read from the routing file the decision read, not read again; without it nothing more is read, and without a file that parsed neither the record nor the
/// lock is: the lock is probed, never taken, and the record read, each in the state root `root`. A lock that cannot be
/// probed keeps the run here with its line.
fn judged(remote: &RemoteRun, machine: &Machine<'_>, root: &ResolvedRoot) -> (Judgement, Option<String>) {
    let consented = machine
        .config
        .borrow()
        .as_ref()
        .is_some_and(|config| config.warmup_consented(&remote.repo) == Ok(true));
    if !consented {
        return (Judgement::Route, None);
    }
    let file = warm::read_warmup_file(Path::new(&remote.root));
    let gate = build_version();
    let judge_with = |record: &RecordRead, lock_held: bool| {
        judge(&Situation {
            consented,
            file: &file,
            record,
            lock_held,
            host: &remote.host,
            gate: &gate,
            now: SystemTime::now(),
        })
    };
    let FileRead::Parsed { hash, .. } = &file else {
        return (judge_with(&RecordRead::Absent, false), None);
    };
    let lock_held = match probed_lock(&v1::warm_lock_path(&root.path, &remote.key)) {
        Ok(held) => held,
        Err(line) => return (Judgement::Problem(line), None),
    };
    let record = warm::read_warm_record(&v1::warm_record_path(&root.path, &remote.key));
    (judge_with(&record, lock_held), Some(hash.clone()))
}

/// The queue line's `notRouted`: `--local` for a heavy gate its caller kept here with `--local`, `another worktree`
/// for a matched command whose outside path lies in another git worktree, else none.
pub(crate) fn not_routed(route: &Route) -> Option<String> {
    match route {
        Route::Local {
            reason: Some(LocalReason::LocalFlag),
        } => Some(LOCAL_FLAG.to_owned()),
        Route::Local {
            reason: Some(LocalReason::OtherWorktree { .. }),
        } => Some(ANOTHER_WORKTREE.to_owned()),
        _ => None,
    }
}

fn slot(slot: Slot) -> gate_router::Slot {
    match slot {
        Slot::Heavy => gate_router::Slot::Heavy,
        Slot::Light => gate_router::Slot::Light,
        Slot::Critical => gate_router::Slot::Critical,
        Slot::None => gate_router::Slot::None,
    }
}

/// The routing file at `path`: `None` when there is none; else why it cannot be used: not a file, unreadable, or
/// refused by [`Config::parse`]. Routing and `gate push` read it through this.
pub(crate) fn read_remote_json(path: &Path) -> Result<Option<Config>, ConfigProblem> {
    let problem = |reason: String| ConfigProblem {
        path: path.to_path_buf(),
        reason,
    };
    match fs::metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(problem(error.to_string())),
        Ok(metadata) if !metadata.is_file() => return Err(problem("not a file".to_owned())),
        Ok(_) => {}
    }
    let bytes = fs::read(path).map_err(|error| problem(error.to_string()))?;
    Config::parse(&String::from_utf8_lossy(&bytes)).map(Some).map_err(problem)
}

/// Where the routing file is: `<local_app_data>\gate\remote.json`; `None`, no routing file, when LOCALAPPDATA is unset,
/// empty or relative. Routing and `gate check` look for it here.
pub(crate) fn remote_json_path(local_app_data: Option<&OsStr>) -> Option<PathBuf> {
    let folder = local_app_data.map(Path::new).filter(|folder| folder.is_absolute())?;
    Some(folder.join("gate").join("remote.json"))
}

/// This machine's facts: the gate's working folder, or why it cannot be read, the LOCALAPPDATA folder that holds the
/// routing file, how long each of the decision's git calls may take, how long its readers get once git has exited, and
/// what git said of the gate's place when the decision asked.
pub(crate) struct Machine<'a> {
    folder: io::Result<PathBuf>,
    local_app_data: Option<&'a OsStr>,
    git_limit: Duration,
    git_drain: Duration,
    found: RefCell<Placed>,
    /// The routing file the decision read, when it read one: the warm-up's judgement reads its consent from this.
    config: RefCell<Option<Config>>,
    /// Whether the worktree an outside path lies in is looked up ([`Facts::worktree_of`]): only a decision that may say
    /// the line naming it does.
    look_up_trees: bool,
}

impl<'a> Machine<'a> {
    /// The facts of the gate's working folder now, with `environment`'s LOCALAPPDATA and the git limits `seams` set.
    pub(crate) fn new(environment: &'a Environment, seams: &TestSeams) -> Machine<'a> {
        Machine::at(environment.local_app_data.as_deref(), git::limit(seams), git::drain(seams))
    }

    /// The facts of the gate's working folder now, reading `local_app_data`'s routing file, with the decision's git
    /// calls under `git_limit` and their readers under `git_drain`.
    fn at(local_app_data: Option<&'a OsStr>, git_limit: Duration, git_drain: Duration) -> Machine<'a> {
        Machine {
            folder: std::env::current_dir(),
            local_app_data,
            git_limit,
            git_drain,
            found: RefCell::new(Placed::NotAsked),
            config: RefCell::new(None),
            look_up_trees: true,
        }
    }

    /// The decision's git call in `folder` with `args`, under the decision's limit and drain.
    fn git_call<'b>(&self, folder: &'b Path, args: &'b [&'b str]) -> git::Call<'b> {
        git::Call {
            drain: self.git_drain,
            ..git::Call::plain(folder, args, self.git_limit)
        }
    }
}

impl Facts for Machine<'_> {
    /// `<LOCALAPPDATA>\gate\remote.json`; with LOCALAPPDATA unset, empty or relative there is no routing file. A file
    /// that reads is kept for the warm-up's judgement, so the route reads it once.
    fn config(&self) -> Result<Option<Config>, ConfigProblem> {
        let config = remote_json_path(self.local_app_data).map_or(Ok(None), |path| read_remote_json(&path));
        if let Ok(Some(read)) = &config {
            self.config.replace(Some(read.clone()));
        }
        config
    }

    fn place(&self) -> Result<Option<Place>, String> {
        let placed = self
            .folder
            .as_ref()
            .map_err(|error| format!("cannot read the working directory: {error}"))
            .and_then(|folder| {
                let git = git::Git::find(folder)?;
                read_place(&git, &self.git_call(folder, &PLACE_ARGS))
            });
        self.found.replace(Placed::Asked(placed.as_ref().ok().and_then(Option::clone)));
        placed
    }

    /// The first path `git ls-files` lists, tracked or untracked and not ignored, that is still on disk: an unstaged
    /// delete sends nothing. The prefix is a literal path, its spaces and wildcards as written.
    fn folder_has_files(&self, place: &Place) -> Result<bool, String> {
        let root = Path::new(&place.root);
        let args = [
            "--literal-pathspecs",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            &place.prefix,
        ];
        let ran = git::Git::find(root)?.run(&self.git_call(root, &args))?;
        if !ran.status.success() {
            let code = ran.status.code().map_or_else(|| "unknown".to_owned(), |code| code.to_string());
            return Err(format!("git ls-files failed with status {code}"));
        }
        holds_existing_path(BufReader::new(ran.stdout), root).map_err(|error| format!("cannot read git ls-files' output: {error}"))
    }

    /// The root's `HEAD` sha, `Ok(None)` when the worktree has none: a `git rev-parse --verify -q HEAD` that fails (an
    /// unborn branch among them) or writes anything but one sha is no sha.
    fn head_sha(&self, place: &Place) -> Result<Option<String>, String> {
        let root = Path::new(&place.root);
        let args = ["rev-parse", "--verify", "-q", "HEAD"];
        let mut ran = git::Git::find(root)?.run(&self.git_call(root, &args))?;
        if !ran.status.success() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        ran.stdout
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read git rev-parse's output: {error}"))?;
        let text = String::from_utf8(bytes).map_err(|_| "git rev-parse's output is not valid UTF-8".to_owned())?;
        Ok(parse_head(&text))
    }

    fn file_in_folder(&self, name: &str) -> bool {
        self.folder.as_ref().is_ok_and(|folder| folder.join(name).is_file())
    }

    fn subst_target(&self, letter: char) -> Option<String> {
        // The decision takes no error here: a letter whose device cannot be read is no subst drive, and a word on it
        // is judged as written.
        gate_win32::device_target(letter).ok().flatten()
    }

    fn siblings(&self, place: &Place) -> Result<Vec<String>, String> {
        read_siblings(Path::new(&place.root))
    }

    fn desktop(&self, place: &Place) -> Result<Vec<Pattern>, String> {
        read_desktop(Path::new(&place.root))
    }

    fn sibling_folder(&self, root: &str) -> Result<SiblingFolder, String> {
        sibling_folder(root)
    }

    fn root_id(&self, root: &str) -> Result<FolderId, String> {
        root_id(root)
    }

    /// None without looking when the machine does not look up trees (the heavy-route hint's).
    fn worktree_of(&self, path: &Path) -> Option<PathBuf> {
        if !self.look_up_trees {
            return None;
        }
        worktree_of(path)
    }

    fn checkout_of(&self, tree: &Path) -> Option<String> {
        checkout_of(tree)
    }
}

/// What `GetDriveTypeW` says of a root that names no volume.
const DRIVE_NO_ROOT_DIR: u32 = 1;
/// What `GetDriveTypeW` says of a network drive.
const DRIVE_REMOTE: u32 = 4;
/// The most of a `.git` file or a `commondir` file read: a line naming a folder.
const GIT_FILE_LIMIT: u64 = 4096;

/// The git worktree `path` lies in: the nearest of `path` and the folders above it that holds a `.git` folder, a main
/// checkout's, or file, a linked worktree's, read from its metadata without following a link, so a `.git` that is a link
/// or a junction holds none; `None` when none on the way does, and when `path` is not on a local drive
/// ([`on_local_drive`]). No git is started.
fn worktree_of(path: &Path) -> Option<PathBuf> {
    if !on_local_drive(path) {
        return None;
    }
    path.ancestors()
        .find(|folder| fs::symlink_metadata(folder.join(".git")).is_ok_and(|metadata| metadata.is_dir() || metadata.is_file()))
        .map(Path::to_path_buf)
}

/// Whether `path` is absolute on a drive letter (`X:\`, `\\?\X:\`) whose drive is there and not a network drive, as
/// `GetDriveTypeW` says of its root. A share or device path, or a path relative in any way, is not; nothing on the
/// network is asked.
fn on_local_drive(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
        return false;
    };
    let root: Vec<u16> = format!(r"{}:\", char::from(letter)).encode_utf16().chain([0]).collect();
    // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call, which only reads it.
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    kind != DRIVE_NO_ROOT_DIR && kind != DRIVE_REMOTE
}

/// The folder name of the main checkout of the worktree at `tree`, as git's common folder gives the run's own: the
/// parent of `tree\.git` when that is a folder; for a `.git` file, the parent of the common folder of the git folder its
/// `gitdir:` line names ([`common_folder`]). `None` when any of it cannot be read, the `.git` is a link or a junction,
/// or the folder the `gitdir:` line names is on no local drive ([`on_local_drive`]). No git is started.
fn checkout_of(tree: &Path) -> Option<String> {
    let dot_git = tree.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    let common = if metadata.is_dir() {
        dot_git
    } else if metadata.is_file() {
        common_folder(&linked_git_folder(tree, &dot_git)?)?
    } else {
        return None;
    };
    common.parent()?.file_name()?.to_str().map(str::to_owned)
}

/// The git folder the `.git` file `dot_git` of the worktree `tree` names on its first line, `gitdir: <folder>`, read
/// against `tree` when relative; `None` when there is no such line, or the folder is not one on a local drive.
fn linked_git_folder(tree: &Path, dot_git: &Path) -> Option<PathBuf> {
    let text = read_git_file(dot_git)?;
    let named = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    let folder = std::path::absolute(tree.join(named)).ok()?;
    let is_folder = on_local_drive(&folder) && fs::symlink_metadata(&folder).is_ok_and(|metadata| metadata.is_dir());
    is_folder.then_some(folder)
}

/// The common folder of the git folder `git`, as git reads it: the folder its `commondir` file names, read against `git`
/// when relative, else `git` itself when there is no such file; `None` when it cannot be read, or is not a file: a link
/// or a junction is not followed.
fn common_folder(git: &Path) -> Option<PathBuf> {
    let file = git.join("commondir");
    match fs::symlink_metadata(&file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(git.to_path_buf()),
        Ok(metadata) if metadata.is_file() => {}
        _ => return None,
    }
    let named = read_git_file(&file)?;
    std::path::absolute(git.join(named.lines().next()?.trim())).ok()
}

/// The text of the small file at `path`, its first [`GIT_FILE_LIMIT`] bytes; `None` when it cannot be read or is not
/// UTF-8.
fn read_git_file(path: &Path) -> Option<String> {
    let mut text = String::new();
    fs::File::open(path).ok()?.take(GIT_FILE_LIMIT).read_to_string(&mut text).ok()?;
    Some(text)
}

/// The id of the worktree folder at `root`, opened following links and sharing everything: routing reads it for every
/// routed run, and the sync-back names a worktree's lock by it.
///
/// # Errors
///
/// Why the id cannot be read, naming the folder.
pub(crate) fn root_id(root: &str) -> Result<FolderId, String> {
    let unreadable = |error: io::Error| format!("cannot read the id of the worktree folder {root}: {error}");
    let folder = open_followed(Path::new(root), Share::All).map_err(unreadable)?;
    let (volume, file) = gate_win32::file_id(&folder).map_err(unreadable)?;
    Ok(FolderId { volume, file })
}

/// The folder of the sibling whose root is `root`, the worktree root's parent and the sibling's name: the parent opened
/// following links, then the name opened inside the folder that handle holds without following a link
/// ([`gate_win32::open_beneath`]), so it is a direct child of that folder. Missing when nothing has the name or it
/// holds no `.git`; refused when it is a link or a junction (a name-surrogate reparse point), not a folder, or its
/// `.git`, a folder or a linked worktree's file, is a link or a junction; else present with its id. Routing and `gate
/// check` look for a sibling by this rule.
///
/// # Errors
///
/// What could not be read, naming the folder.
pub(crate) fn sibling_folder(root: &str) -> Result<SiblingFolder, String> {
    let unreadable = |error: io::Error| format!("cannot read the sibling folder {root}: {error}");
    let path = Path::new(root);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(OsStr::to_str)) else {
        return Err(format!("cannot read the sibling folder {root}: it names no folder inside another"));
    };
    let parent = match open_followed(parent, Share::All) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SiblingFolder::Missing),
        parent => parent.map_err(unreadable)?,
    };
    let Some((folder, metadata)) = with_metadata(gate_win32::open_beneath(&parent, name, Share::All)).map_err(unreadable)? else {
        return Ok(SiblingFolder::Missing);
    };
    if metadata.file_type().is_symlink() {
        return Ok(SiblingFolder::Refused(SIBLING_LINK.to_owned()));
    }
    if !metadata.is_dir() {
        return Ok(SiblingFolder::Refused(SIBLING_NOT_A_FOLDER.to_owned()));
    }
    let Some((_git, git_metadata)) = with_metadata(gate_win32::open_beneath(&folder, ".git", Share::All)).map_err(unreadable)? else {
        return Ok(SiblingFolder::Missing);
    };
    if git_metadata.file_type().is_symlink() {
        return Ok(SiblingFolder::Refused(SIBLING_GIT_LINK.to_owned()));
    }
    let (volume, file) = gate_win32::file_id(&folder).map_err(unreadable)?;
    Ok(SiblingFolder::Present(FolderId { volume, file }))
}

/// The folder at `path` opened for reading, a link or a junction on the way followed, sharing what `share` says.
pub(crate) fn open_followed(path: &Path, share: Share) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .share_mode(share.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// The siblings `<root>\.gate\siblings.toml` declares as it stands on disk, read by [`read_gate_file`]: no file
/// declares none, and a file that cannot be read, does not parse or has any problem gives its first problem.
fn read_siblings(root: &Path) -> Result<Vec<String>, String> {
    let Some(bytes) = read_gate_file(root, "siblings.toml")? else {
        return Ok(Vec::new());
    };
    let declared = siblings::read_bytes(&bytes)?;
    match declared.problems.into_iter().next() {
        Some(problem) => Err(problem),
        None => Ok(declared.names),
    }
}

/// The patterns `<root>\.gate\desktop.toml` declares as it stands on disk, read by [`read_gate_file`]: no file declares
/// none, and a file that cannot be read, does not parse or has any problem gives its first problem.
fn read_desktop(root: &Path) -> Result<Vec<Pattern>, String> {
    match read_gate_file(root, "desktop.toml")? {
        Some(bytes) => gate_router::desktop::read_desktop(&bytes),
        None => Ok(Vec::new()),
    }
}

/// The bytes of the file `name` of `<root>\.gate` as it stands on disk; `None` when there is no such file. Routing reads
/// `siblings.toml` and `desktop.toml`, and `gate check` reads `toolchain.toml` and `siblings.toml` through this.
///
/// `.gate` is opened as itself, not following a link, and held open until the file is read, sharing reading, writing
/// and deletion so it never blocks a checkout. A `.gate` that is a link or a junction (any name-surrogate reparse
/// point) is a problem naming `.gate`, and so is one that cannot be opened; a `.gate` that is not a folder holds no
/// file. The file is opened by its name inside the folder object that handle holds ([`gate_win32::open_beneath`]), so
/// it is that folder's file whatever the path to `.gate` names by then, matched in any letter case unless the folder
/// is case-sensitive. It is opened as itself too, sharing the same, so a checkout or an editor's save is never blocked
/// by it, and read from that handle. A file that is a link, a junction or a folder is a problem, and so is one larger
/// than [`GATE_FILE_MOST_BYTES`]. A file that is any other reparse point, such as a deduplicated one, is read through
/// its filter, from a handle opened again that must be of the same file ([`gate_win32::same_file`]); one of any other
/// is the not-a-file problem.
pub(crate) fn read_gate_file(root: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
    let gate = root.join(".gate");
    let held = open_as_itself(&gate).map_err(|error| format!(".gate: cannot read it: {error}"))?;
    let Some((gate_folder, gate_metadata)) = held else {
        return Ok(None);
    };
    if gate_metadata.file_type().is_symlink() {
        return Err(".gate is a link or a junction".to_owned());
    }
    if !gate_metadata.is_dir() {
        return Ok(None);
    }
    let opened = with_metadata(gate_win32::open_beneath(&gate_folder, name, Share::All)).map_err(|error| format!("cannot read it: {error}"))?;
    let Some((file, metadata)) = opened else {
        return Ok(None);
    };
    let bytes = read_whole(file, &metadata);
    drop(gate_folder);
    bytes.map(Some)
}

/// Whether `<root>\.gate` is there as something other than a folder, so no file of it can be read; `false` when it is
/// absent or is a folder. A link or a junction is refused by [`read_gate_file`] by name before this is asked.
pub(crate) fn gate_is_not_a_folder(root: &Path) -> bool {
    fs::symlink_metadata(root.join(".gate")).is_ok_and(|metadata| !metadata.is_dir())
}

/// The bytes of the file at `path`, opened as itself so a link or a junction there is not followed, and read as
/// [`read_gate_file`] reads a file of `.gate`: a link, a junction or a folder is the not-a-file problem, and a file
/// larger than [`GATE_FILE_MOST_BYTES`] is a problem too; `None` when nothing is there. `gate check` reads the root
/// `global.json` through this.
pub(crate) fn read_unfollowed(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let opened = open_as_itself(path).map_err(|error| format!("cannot read it: {error}"))?;
    opened.map(|(file, metadata)| read_whole(file, &metadata)).transpose()
}

/// All of `file`, whose handle says `metadata`, when it is a file of at most [`GATE_FILE_MOST_BYTES`]; else its
/// problem.
fn read_whole(file: fs::File, metadata: &fs::Metadata) -> Result<Vec<u8>, String> {
    if !metadata.is_file() {
        return Err(GATE_FILE_NOT_A_FILE.to_owned());
    }
    let bytes = read_capped(file, metadata)?;
    if bytes.len() > GATE_FILE_MOST_BYTES {
        return Err("is larger than 64 KiB".to_owned());
    }
    Ok(bytes)
}

/// The file or folder at `path` opened for reading as itself, a link or a junction not followed, sharing reading,
/// writing and deletion, and what the handle says it is; `None` when nothing is there.
fn open_as_itself(path: &Path) -> io::Result<Option<(fs::File, fs::Metadata)>> {
    with_metadata(
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path),
    )
}

/// The file `opened` and what its handle says it is; `None` when nothing was there to open.
fn with_metadata(opened: io::Result<fs::File>) -> io::Result<Option<(fs::File, fs::Metadata)>> {
    let file = match opened {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        opened => opened?,
    };
    let metadata = file.metadata()?;
    Ok(Some((file, metadata)))
}

/// At most [`GATE_FILE_MOST_BYTES`] and one more of `file`, whose handle says `metadata`: read from that handle for a
/// plain file, and through the filter for a reparse point, which [`gate_win32::open_beneath`] opened as the point
/// itself: a handle opened again that is not of the same file is the not-a-file problem, and an error is the
/// unreadable one.
fn read_capped(file: fs::File, metadata: &fs::Metadata) -> Result<Vec<u8>, String> {
    let unreadable = |error: io::Error| format!("cannot read it: {error}");
    let file = if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        file
    } else {
        let filtered = gate_win32::reopen_through_filter(&file).map_err(unreadable)?;
        if !gate_win32::same_file(&file, &filtered).map_err(unreadable)? {
            return Err(GATE_FILE_NOT_A_FILE.to_owned());
        }
        filtered
    };
    let mut bytes = Vec::new();
    file.take((GATE_FILE_MOST_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(unreadable)?;
    Ok(bytes)
}

/// Runs `call`, git asked [`PLACE_ARGS`] in a folder, and reads the place it printed: `Ok(None)` for any non-zero exit
/// (the folder is in no worktree, among others); else why the call failed or its output does not read as a place.
pub(crate) fn read_place(git: &git::Git, call: &git::Call<'_>) -> Result<Option<Place>, String> {
    let mut ran = git.run(call)?;
    if !ran.status.success() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    ran.stdout
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read git rev-parse's output: {error}"))?;
    let text = String::from_utf8(bytes).map_err(|_| "git rev-parse's output is not valid UTF-8".to_owned())?;
    parse_place(&text, call.folder).map(Some)
}

/// The place of `folder`, as the gate spells it, that `git rev-parse --show-toplevel --show-prefix
/// --path-format=absolute --git-common-dir` printed, in exactly three lines: the root, the prefix (empty at the root)
/// and the common git folder, whose parent's name is the checkout's.
fn parse_place(text: &str, folder: &Path) -> Result<Place, String> {
    let lines: Vec<&str> = text.lines().collect();
    let [root, prefix, common] = lines.as_slice() else {
        return Err(format!(
            "cannot read git rev-parse's output (routing needs git 2.31 or later): {}",
            text.trim()
        ));
    };
    let checkout = Path::new(common)
        .parent()
        .and_then(Path::file_name)
        .ok_or_else(|| format!("cannot name the checkout of the git folder {common}"))?;
    Ok(Place {
        root: (*root).to_owned(),
        prefix: (*prefix).to_owned(),
        checkout: checkout.to_string_lossy().into_owned(),
        folder: folder.to_string_lossy().into_owned(),
    })
}

/// The one sha `git rev-parse HEAD` wrote: 40 lower-case hex digits on a line of their own, nothing else.
fn parse_head(text: &str) -> Option<String> {
    let sha = text.trim();
    (sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))).then(|| sha.to_owned())
}

/// Whether a NUL-separated `git ls-files -z` listing, its paths relative to `root`, names a path still on disk; it
/// reads no further than the first that does.
fn holds_existing_path(listing: impl BufRead, root: &Path) -> io::Result<bool> {
    for path in listing.split(0) {
        let path = path?;
        if !path.is_empty() && fs::symlink_metadata(root.join(String::from_utf8_lossy(&path).as_ref())).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test that cannot make its folders, or whose folder is not present, must fail loudly"
)]
mod tests {
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    use gate_router::{LocalReason, Place, Route, SiblingFolder};
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    use super::{
        GATE_FILE_NOT_A_FILE, SIBLING_GIT_LINK, SIBLING_LINK, SIBLING_NOT_A_FOLDER, checkout_of, holds_existing_path, not_routed, parse_head,
        parse_place, probed_lock, read_siblings, sibling_folder, worktree_of,
    };
    use crate::test_support::junction;
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

    /// The folder the place cases are asked of, spelled on a subst drive as a gate started there spells it.
    const FOLDER: &str = r"S:\src";

    fn place(root: &str, prefix: &str, checkout: &str) -> Place {
        Place {
            root: root.to_owned(),
            prefix: prefix.to_owned(),
            checkout: checkout.to_owned(),
            folder: FOLDER.to_owned(),
        }
    }

    #[test]
    fn rev_parse_output_gives_the_root_the_prefix_and_the_checkout() {
        let parse = |text: &str| parse_place(text, Path::new(FOLDER));
        assert_eq!(
            parse("C:/work/app\n\nC:/work/app/.git\n"),
            Ok(place("C:/work/app", "", "app")),
            "at the root the prefix is empty"
        );
        assert_eq!(
            parse("C:/work/app-wt\r\nsrc/my dir/\r\nC:/work/app/.git\r\n"),
            Ok(place("C:/work/app-wt", "src/my dir/", "app")),
            "a worktree below a folder names the main checkout"
        );
        assert!(parse("C:/work/app\n\n").is_err(), "a missing common folder");
        assert!(
            parse("C:/work/app\n\n--path-format=absolute\nC:/work/app/.git\n").is_err(),
            "a git too old for --path-format prints a fourth line"
        );
        assert!(parse("C:/work/app\n\n/\n").is_err(), "a common folder with no parent");
    }

    #[test]
    fn a_head_is_one_line_of_forty_lower_case_hex_digits() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(parse_head(&format!("{sha}\n")), Some(sha.to_owned()), "an LF ending");
        assert_eq!(parse_head(&format!("{sha}\r\n")), Some(sha.to_owned()), "a CRLF ending");
        assert_eq!(parse_head(&sha.to_uppercase()), None, "upper case");
        assert_eq!(parse_head(&format!("{sha}abcdef0123456789abcdef")), None, "a sha-256's 64 digits");
        assert_eq!(parse_head(&format!("{sha}\n{sha}\n")), None, "two lines");
        assert_eq!(parse_head(""), None, "nothing");
        assert_eq!(parse_head("fatal: not a git repository (or any parent)\n"), None, "junk");
    }

    #[test]
    fn only_a_listed_path_still_on_disk_counts() {
        let root: PathBuf = crate::test_support::temp_dir().join(format!("gate-route-listing-{}", std::process::id()));
        fs::create_dir_all(root.join("sub")).expect("the test folder can be made");
        fs::write(root.join("sub").join("here.txt"), "x").expect("a file can be written");

        let found = |listing: &[u8]| holds_existing_path(listing, &root).expect("a listing in memory can be read");
        assert!(found(b"sub/gone.txt\0sub/here.txt\0"), "one file still there");
        assert!(!found(b"sub/gone.txt\0"), "an unstaged delete");
        assert!(!found(b""), "nothing listed");
        fs::remove_dir_all(&root).expect("the test folder can be removed");
    }

    /// Folders a test made, removed when it ends, failed or not: a junction inside one is removed as the link, its
    /// target left to its own entry.
    struct Removed(Vec<PathBuf>);

    impl Drop for Removed {
        #[expect(clippy::print_stderr, reason = "a case that cannot clean up says so without failing the test it ends")]
        fn drop(&mut self) {
            for folder in &self.0 {
                match fs::remove_dir_all(folder) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        eprintln!("cannot remove the test folder {}: {error}", folder.display());
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn the_siblings_file_is_read_only_as_a_small_regular_file_under_a_real_folder() {
        let root: PathBuf = crate::test_support::temp_dir().join(format!("gate-route-siblings-{}", std::process::id()));
        let elsewhere = root.with_extension("elsewhere");
        for folder in [&root, &elsewhere] {
            if folder.exists() {
                fs::remove_dir_all(folder).expect("a folder left by an earlier run can be removed");
            }
        }
        let _removed = Removed(vec![root.clone(), elsewhere.clone()]);
        let gate = root.join(".gate");
        let file = gate.join("siblings.toml");
        assert_eq!(read_siblings(&root), Ok(Vec::new()), "no .gate folder");
        fs::create_dir_all(&root).expect("the test folder can be made");
        fs::write(&gate, "").expect("a file named .gate can be written");
        assert_eq!(read_siblings(&root), Ok(Vec::new()), "a .gate that is a file holds no siblings file");
        fs::remove_file(&gate).expect("the .gate file can be removed");
        fs::create_dir_all(&gate).expect("the .gate folder can be made");
        assert_eq!(read_siblings(&root), Ok(Vec::new()), "a .gate folder that holds no siblings file");

        fs::create_dir_all(&file).expect("a folder named like the file can be made");
        let not_a_file = Err(GATE_FILE_NOT_A_FILE.to_owned());
        assert_eq!(read_siblings(&root), not_a_file, "a folder");
        fs::remove_dir(&file).expect("the folder can be removed");

        fs::write(&file, format!("siblings = ['b']\n#{}\n", "x".repeat(64 * 1024 - 19))).expect("the file can be written");
        assert_eq!(fs::metadata(&file).expect("the file is there").len(), 64 * 1024);
        assert_eq!(read_siblings(&root), Ok(vec!["b".to_owned()]), "64 KiB reads");
        fs::write(&file, format!("siblings = ['b']\n#{}\n", "x".repeat(64 * 1024 - 18))).expect("the file can be written");
        assert_eq!(read_siblings(&root), Err("is larger than 64 KiB".to_owned()));

        fs::write(&file, "siblings = ['b']\n").expect("the file can be written");
        let held = fs::OpenOptions::new()
            .access_mode(FILE_GENERIC_READ | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&file)
            .expect("the file can be held open for deletion, as an editor's atomic save does");
        assert_eq!(read_siblings(&root), Ok(vec!["b".to_owned()]), "a file held open for deletion reads");
        drop(held);

        fs::create_dir_all(elsewhere.join(".gate")).expect("the junction's target can be made");
        fs::write(elsewhere.join(".gate").join("siblings.toml"), "siblings = ['b']\n").expect("the target file can be written");
        fs::remove_dir_all(&gate).expect("the real .gate can be removed");
        junction(&gate, &elsewhere.join(".gate"));
        assert_eq!(read_siblings(&root), Err(".gate is a link or a junction".to_owned()), "a junction");
    }

    #[test]
    fn a_sibling_folder_is_only_a_real_folder_holding_a_real_git() {
        let parent: PathBuf = crate::test_support::temp_dir().join(format!("gate-route-sibling-folder-{}", std::process::id()));
        if parent.exists() {
            fs::remove_dir_all(&parent).expect("a folder left by an earlier run can be removed");
        }
        let _removed = Removed(vec![parent.clone()]);
        fs::create_dir_all(parent.join("real").join(".git")).expect("a real sibling can be made");
        fs::create_dir_all(parent.join("other").join(".git")).expect("a second real sibling can be made");
        fs::create_dir_all(parent.join("bare")).expect("a folder with no .git can be made");
        fs::create_dir_all(parent.join("git-linked")).expect("a sibling whose .git is a junction can be made");
        junction(&parent.join("git-linked").join(".git"), &parent.join("real").join(".git"));
        junction(&parent.join("linked"), &parent.join("real"));
        fs::write(parent.join("file"), "x").expect("a file named like a sibling can be written");
        let check = |name: &str| sibling_folder(&parent.join(name).to_string_lossy());
        let id = |name: &str| match check(name) {
            Ok(SiblingFolder::Present(id)) => id,
            other => panic!("{name} is not present: {other:?}"),
        };

        assert_eq!(id("real"), id("real"), "one folder has one id");
        assert_ne!(id("real"), id("other"), "two folders have two ids");
        assert_eq!(
            sibling_folder(&format!("{}/real", parent.to_string_lossy().replace('\\', "/"))).map(|_| ()),
            Ok(()),
            "a root spelled with forward slashes, as git spells it"
        );
        let refused = |reason: &str| Ok(SiblingFolder::Refused(reason.to_owned()));
        assert_eq!(check("linked"), refused(SIBLING_LINK), "a junction to a real sibling");
        assert_eq!(check("file"), refused(SIBLING_NOT_A_FOLDER), "a file");
        assert_eq!(check("git-linked"), refused(SIBLING_GIT_LINK), "a .git that is a junction");
        assert_eq!(check("bare"), Ok(SiblingFolder::Missing), "a folder with no .git");
        assert_eq!(check("gone"), Ok(SiblingFolder::Missing), "nothing");
        assert_eq!(
            sibling_folder(&parent.join("gone").join("b").to_string_lossy()),
            Ok(SiblingFolder::Missing),
            "a parent that is not there"
        );
    }

    #[test]
    fn a_path_in_a_linked_worktree_names_its_root() {
        let parent: PathBuf = crate::test_support::temp_dir().join(format!("gate-route-worktree-of-{}", std::process::id()));
        if parent.exists() {
            fs::remove_dir_all(&parent).expect("a folder left by an earlier run can be removed");
        }
        let _removed = Removed(vec![parent.clone()]);
        let tree = parent.join("repo.wt").join("a");
        let deep = tree.join("src").join("deep");
        fs::create_dir_all(&deep).expect("the worktree's folders can be made");
        let git_folder = parent.join("repo").join(".git").join("worktrees").join("a");
        fs::create_dir_all(&git_folder).expect("the main checkout's git folder for the worktree can be made");
        fs::write(git_folder.join("commondir"), "../..\n").expect("the commondir file can be written");
        let gitdir = git_folder.to_string_lossy().replace('\\', "/");
        fs::write(tree.join(".git"), format!("gitdir: {gitdir}\n")).expect("a linked worktree's .git file can be written");
        fs::write(deep.join("x.cs"), "x").expect("a file can be written");
        let nested = tree.join("vendor").join("lib");
        fs::create_dir_all(nested.join(".git")).expect("a nested checkout can be made");
        let linked = tree.join("linked");
        fs::create_dir_all(&linked).expect("a folder whose .git is a junction can be made");
        junction(&linked.join(".git"), &nested.join(".git"));

        assert_eq!(worktree_of(&deep.join("x.cs")), Some(tree.clone()), "a file below the root");
        assert_eq!(worktree_of(&deep), Some(tree.clone()), "a folder below the root");
        assert_eq!(worktree_of(&tree), Some(tree.clone()), "the root itself");
        assert_eq!(
            worktree_of(&deep.join("gone").join("y")),
            Some(tree.clone()),
            "a path not on disk below the root"
        );
        assert_eq!(worktree_of(&nested.join("z")), Some(nested.clone()), "the nearest .git wins");
        assert_eq!(worktree_of(&linked.join("z")), Some(tree.clone()), "a .git that is a junction holds none");
        assert_eq!(
            worktree_of(&parent.join("plain").join("deep")),
            worktree_of(&parent),
            "a folder holding no .git is in the tree its parent is in"
        );

        assert_eq!(checkout_of(&tree).as_deref(), Some("repo"), "a linked worktree's main checkout");
        assert_eq!(checkout_of(&nested).as_deref(), Some("lib"), "a checkout with a .git folder is its own");
        assert_eq!(checkout_of(&linked), None, "a .git that is a junction");

        let commondir = git_folder.join("commondir");
        fs::remove_file(&commondir).expect("the commondir file can be removed");
        let named = parent.join("named-commondir");
        fs::write(&named, "../..\n").expect("a file a link can name can be written");
        // A file symbolic link needs Developer Mode or elevation; where it cannot be made, the folder case stands alone.
        if std::os::windows::fs::symlink_file(&named, &commondir).is_ok() {
            assert_eq!(checkout_of(&tree), None, "a commondir that is a link is not followed");
            fs::remove_file(&commondir).expect("the commondir link can be removed");
        }
        fs::create_dir(&commondir).expect("a folder named commondir can be made");
        assert_eq!(checkout_of(&tree), None, "a commondir that is a folder");
        fs::remove_dir(&commondir).expect("the commondir folder can be removed");
        assert_eq!(
            checkout_of(&tree).as_deref(),
            Some("worktrees"),
            "a git folder with no commondir is its own common folder"
        );
        fs::remove_dir_all(&git_folder).expect("the git folder can be removed");
        assert_eq!(checkout_of(&tree), None, "a .git file naming no folder");

        // SAFETY: `GetLogicalDrives` takes nothing and only reads the system's drive table.
        let drives = unsafe { GetLogicalDrives() };
        let free = (b'D'..=b'Z')
            .rev()
            .find(|letter| drives & (1 << (letter - b'A')) == 0)
            .expect("a drive letter is free");
        let free = PathBuf::from(format!(r"{}:\a\b", char::from(free)));
        assert_eq!(worktree_of(&free), None, "a path on a drive that is not there");
        assert_eq!(worktree_of(Path::new(r"\??\UNC\host\share\x")), None, "a path that is not absolute");
        assert_eq!(worktree_of(Path::new(r"\\host\share\x")), None, "a share path, never asked");
        let device = PathBuf::from(format!(r"\\.\{}", deep.to_string_lossy()));
        assert_eq!(worktree_of(&device), None, "the tree spelled as a device path is on no drive letter");
    }

    #[test]
    fn a_lock_that_is_a_folder_or_a_junction_is_a_lock_problem() {
        let folder: PathBuf = crate::test_support::temp_dir().join(format!("gate-route-lock-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        let _removed = Removed(vec![folder.clone()]);
        let target = folder.join("target");
        fs::create_dir_all(&target).expect("the junction's target can be made");
        let made = folder.join("folder.lock");
        fs::create_dir(&made).expect("a folder named like the lock can be made");
        let linked = folder.join("linked.lock");
        junction(&linked, &target);
        for lock in [made, linked] {
            let line = probed_lock(&lock).expect_err("a folder or a junction is no lock");
            let start = format!("gate: not routed: the warm-up lock {} cannot be read: ", lock.to_string_lossy());
            assert!(line.starts_with(&start), "{line}");
        }
        assert_eq!(probed_lock(&folder.join("missing.lock")), Ok(false), "a missing lock is free");
    }

    #[test]
    fn only_the_local_flag_and_another_worktree_write_not_routed() {
        let flag = Route::Local {
            reason: Some(LocalReason::LocalFlag),
        };
        let other = Route::Local {
            reason: Some(LocalReason::OtherWorktree {
                tree: PathBuf::from(r"X:\dev\other"),
            }),
        };
        let outside = Route::Local {
            reason: Some(LocalReason::Outside { word: r"X:\x".to_owned() }),
        };
        let failed = Route::Local {
            reason: Some(LocalReason::Failed { reason: "x".to_owned() }),
        };

        assert_eq!(not_routed(&flag).as_deref(), Some("--local"));
        assert_eq!(not_routed(&other).as_deref(), Some("another worktree"));
        assert_eq!(not_routed(&outside), None, "an outside word in no other worktree writes none");
        assert_eq!(not_routed(&failed), None);
        assert_eq!(not_routed(&Route::Local { reason: None }), None);
    }
}
