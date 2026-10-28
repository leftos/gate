//! Where a run's command runs: the facts [`gate_router::decide`] asks for, read from the routing file, git and the
//! drive table, then, for a command that would still route in a repo whose warm-up is set up, the hash of its warm-up
//! file, and the lines they say. git runs on the run thread, before any command is started.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use gate_args::{GATE_SET_NAMES, RunRequest, Slot, TestSeams, env_refusal};
use gate_router::warm::FileRead;
use gate_router::windows::{self, WindowsChange};
use gate_router::{
    Config, ConfigProblem, Facts, FolderId, GitignoredInput, Inputs, LaterCandidate, LocalReason, Pattern, PinRefusal, Place, RemoteRun, Route,
    SiblingFolder, SiblingRoot, Target, decide, dropped_env, dropped_env_line, escaped, is_input_file, is_project_or_solution, repo_name,
};
use gate_win32::Share;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetDriveTypeW,
};

use crate::console::Console;
use crate::dotnet_projects;
use crate::no_build;
use crate::outputs::glob::Globs;
use crate::outputs::walk::walk;
use crate::remote::inputs::{EXCLUDED, nested_folder, relative};
use crate::remote::{self, git, siblings};
use crate::resolve::{self, Environment};
use crate::warm;
use gate_inputs::names::PATH_DEPTH_LIMIT;

/// The `notRouted` value of a heavy gate its caller kept here with `--local`.
const LOCAL_FLAG: &str = "--local";
/// The `notRouted` value of a matched command kept here because its outside path lies in another git worktree.
const ANOTHER_WORKTREE: &str = "another worktree";
/// The `notRouted` value of a matched command its repo's `.gate/local.toml` keeps here.
const DECLARED_LOCAL: &str = "declared local";
/// The `notRouted` value of a matched command kept here because a word names a gitignored project input.
const GITIGNORED_INPUT: &str = "gitignored input";
/// The declaration file of the prefixes of the caller's variables a repo's commands read.
const ENV_NAME: &str = "env.toml";
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
/// How long the git calls of one read of the change against `.gate/windows.toml`'s paths may take together, for the
/// worktree and every sibling it reads ([`Facts::windows_change`]); each call's own limit is what is left of it.
const CHANGE_DEADLINE: Duration = Duration::from_secs(10);
/// The ref the default branch's upstream is taken from when `refs/remotes/origin/HEAD` names none.
const ORIGIN_MAIN: &str = "refs/remotes/origin/main";
/// The ref the default branch's upstream is first taken from, followed when it is symbolic.
const ORIGIN_HEAD: &str = "refs/remotes/origin/HEAD";

/// How long the git calls of one read of a change may take together: the `--test-git-limit-ms` seam when it is given,
/// so a case on a loaded machine never reaches it, else [`CHANGE_DEADLINE`].
fn change_deadline(seams: &TestSeams) -> Duration {
    seams.git_limit_ms.map_or(CHANGE_DEADLINE, Duration::from_millis)
}

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
/// the routed run's warm-up file when its repo's warm-up is set up ([`consented_hash`]), and what the decision saw,
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
    /// `--builder <host>`, the one builder the run is pinned to.
    pin: Option<String>,
    /// The state root the run's records are under, the built-here record among them ([`Facts::built_here`]).
    state: PathBuf,
}

impl Decision {
    /// What the decision of a run with `request` sees under `covering_parent`, read from `environment`, its records
    /// under the state root `state`.
    fn of(request: &RunRequest, environment: &Environment, covering_parent: bool, state: &Path) -> Decision {
        Decision {
            state: state.to_owned(),
            covering_parent,
            local: request.local,
            git_hook: environment.git_index_file.as_deref().is_some_and(|value| !value.is_empty()),
            words: request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect(),
            unicode: request.command.iter().all(|word| word.to_str().is_some()),
            local_app_data: environment.local_app_data.clone(),
            pin: request.builder.clone(),
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
            pin: self.pin.as_deref(),
        }
    }
}

/// Decides where `request`'s command runs and says the decision's lines, on stderr and in the log: why a command runs
/// here when there is a reason to say, or the lines of a routed run, then the problem of its repo's `.gate/env.toml`
/// when it has one ([`declared_prefixes`]), and the line of the watched variables it does not send ([`dropped_line`]),
/// the names the repo's declared prefixes start among them, after them. A command that would route but has a word that
/// is not valid Unicode runs here, saying so. A gate under a `covering_parent`'s slot runs here, saying nothing. With
/// the route, what the decision learnt of the gate's place in its git worktree, and, for a routed run of a repo whose
/// warm-up is set up, the hash of its warm-up file ([`consented_hash`]).
pub(crate) fn route(request: &RunRequest, environment: &Environment, covering_parent: bool, state: &Path, console: &mut Console) -> Decided {
    let decision = Decision::of(request, environment, covering_parent, state);
    let mut machine = Machine::new(environment, &request.test_seams).with_state(&decision);
    machine.run_log = run_log(request);
    let route = match decide(&decision.inputs(slot(request.slot)), &machine) {
        Route::Routed(_) if !decision.unicode => match &decision.pin {
            Some(host) => Route::Refused(PinRefusal::CannotRoute {
                host: host.clone(),
                why: NOT_UNICODE.to_owned(),
            }),
            None => Route::Local {
                reason: Some(LocalReason::Failed {
                    reason: NOT_UNICODE.to_owned(),
                }),
            },
        },
        route => route,
    };
    let (warm_hash, declared) = match &route {
        Route::Routed(remote) => (consented_hash(remote, &machine), declared_prefixes(remote, &machine)),
        Route::Local { .. } | Route::Refused(_) => (None, Ok(Vec::new())),
    };
    // A refused pin never readies its log, so the notes it would have written there are said now.
    let refused = matches!(route, Route::Refused(_));
    for line in machine.unsaid_notes() {
        if refused {
            console.err(&line);
        } else {
            console.note(&line);
        }
    }
    match &route {
        Route::Local { reason: Some(reason) } => console.err(&reason.line()),
        Route::Local { reason: None } => {}
        Route::Refused(refusal) => console.err(&refusal.line()),
        Route::Routed(run) => say_routed(request, run, declared, console),
    }
    Decided {
        route,
        placed: machine.found.into_inner(),
        warm_hash,
        decision,
    }
}

/// The log `request`'s run writes, absolute against the working folder the run has entered, as the run resolved it.
/// `None` when it cannot be made absolute, which the run's own resolution of the same words refused before any
/// decision.
fn run_log(request: &RunRequest) -> Option<PathBuf> {
    resolve::log(&request.log).ok()
}

/// Says the lines of `request`'s routed `run`: its route lines, the problem of its repo's `.gate/env.toml` when
/// `declared` is one ([`declared_prefixes`]), and the line of the watched variables it does not send ([`dropped_line`]).
fn say_routed(request: &RunRequest, run: &RemoteRun, declared: Result<Vec<String>, String>, console: &mut Console) {
    run.lines().iter().for_each(|line| console.err(line));
    let prefixes = declared.unwrap_or_else(|line| {
        console.err(&line);
        Vec::new()
    });
    if let Some(line) = dropped_line(request, &prefixes) {
        console.err(&line);
    }
}

/// The routed run a light gate held back behind a critical request gets when its command would route on heavy: the
/// decision `decision` saw asked again with the heavy slot ([`would_route_on_heavy`]'s question), with the git limits
/// and the `HEAD` a routed run reads. Says that the critical request of gate `by` sends the run to its builder, then
/// the routed run's lines as [`route`] does, and notes the decision's log lines. `None`, saying nothing, when the
/// decision would keep the run here or a word is not valid Unicode. With the run, the hash of its repo's warm-up file
/// when its warm-up is set up ([`consented_hash`]).
pub(crate) fn route_as_heavy(request: &RunRequest, decision: &Decision, by: u32, console: &mut Console) -> Option<(RemoteRun, Option<String>)> {
    let machine = Machine::of(decision.local_app_data.as_deref(), &request.test_seams).with_state(decision);
    let routed = match decide(&decision.inputs(gate_router::Slot::Heavy), &machine) {
        Route::Routed(run) if decision.unicode => Some(run),
        Route::Routed(_) | Route::Local { .. } | Route::Refused(_) => None,
    };
    let Some(run) = routed else {
        for line in machine.unsaid_notes() {
            console.note(&line);
        }
        return None;
    };
    let warm_hash = consented_hash(&run, &machine);
    let declared = declared_prefixes(&run, &machine);
    for line in machine.unsaid_notes() {
        console.note(&line);
    }
    console.err(&format!(
        "gate: held back by the critical request of gate {by}; sending this light run to {}",
        run.builder.alias
    ));
    say_routed(request, &run, declared, console);
    Some((*run, warm_hash))
}

/// The routed run of `request` on `candidate`, a later builder of the run its decision `decision` routed
/// ([`gate_router::later_run`]), its facts asked again of this machine as the decision asked them with the slot
/// `asked`: the run's own, or heavy for a light run a critical request sent to a builder. The log lines of the
/// declaration files it reads are noted for the log. `Err(None)` when the candidate is steered off, `Err(Some(why))`
/// when it is skipped for a reason ([`gate_router::later_run`]).
pub(crate) fn later_run(
    request: &RunRequest,
    decision: &Decision,
    candidate: &LaterCandidate,
    asked: Slot,
    console: &mut Console,
) -> Result<RemoteRun, Option<String>> {
    let machine = Machine::of(decision.local_app_data.as_deref(), &request.test_seams).with_state(decision);
    let run = gate_router::later_run(&decision.inputs(slot(asked)), &machine, candidate);
    for line in machine.unsaid_notes() {
        console.note(&line);
    }
    run
}

/// Whether a light run whose routing decision saw `decision` would route on heavy: the decision asked again with the
/// heavy slot and every other input exactly as the run's own had it, routed only when every word of the command is valid
/// Unicode (the [`NOT_UNICODE`] rule [`route`] applies). Its git calls run under [`HINT_GIT_LIMIT`], since the run still
/// holds its light slot, and a call that cannot answer in time means no hint. It reads the routing file and git as the
/// decision would, and never reads the warm-up file ([`consented_hash`]) or says a line; nor does it look up the
/// worktree an outside path lies in, which only changes the line of a run kept here.
pub(crate) fn would_route_on_heavy(decision: &Decision, seams: &TestSeams) -> bool {
    let machine = Machine::hint(decision.local_app_data.as_deref(), seams).with_state(decision);
    hint_holds(decision.unicode, &machine, |machine| {
        matches!(decide(&decision.inputs(gate_router::Slot::Heavy), machine), Route::Routed(_))
    })
}

/// Whether the hint's decision gives a hint: only for a command whose words are all valid Unicode (`unicode`), which
/// alone asks `routes` whether the decision on `machine` routes it, and only when it does and the read of the change
/// against `.gate/windows.toml`'s paths, if `machine` made one, could tell it.
fn hint_holds(unicode: bool, machine: &Machine<'_>, routes: impl FnOnce(&Machine<'_>) -> bool) -> bool {
    unicode && routes(machine) && !machine.change_unknown.get()
}

/// The line a routed run says for the watched variables of the gate's environment it does not send
/// ([`gate_router::dropped_env`]), the names starting one of the repo's `declared` prefixes among them: those no
/// `--env` pair of `request` names and the gate does not set itself. A name that is not valid Unicode is left out, and
/// so is one `--env` refuses ([`env_refusal`]), whose advice the line could not give. `None` when it sends them all.
fn dropped_line(request: &RunRequest, declared: &[String]) -> Option<String> {
    let caller: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| env_refusal(name, "").is_none())
        .collect();
    let sent: Vec<&str> = request.env.iter().map(|(name, _)| name.as_str()).collect();
    dropped_env_line(&dropped_env(caller.iter().map(String::as_str), &sent, GATE_SET_NAMES, declared))
}

/// The prefixes the routed `remote`'s repo declares in `.gate/env.toml` ([`gate_router::env::read_env`]), read through
/// [`read_declaration`] with the declarations folder the routing file the decision read names, its log lines kept
/// among `machine`'s notes; none when neither place has the file. Read only for a routed run, after the decision, which
/// it never changes.
///
/// # Errors
///
/// The line to say when the file cannot be read or has a problem ([`gate_router::env::problem_line`]), naming it as
/// [`Source::said`] does; the run then routes watching the build variables alone.
fn declared_prefixes(remote: &RemoteRun, machine: &Machine<'_>) -> Result<Vec<String>, String> {
    let (read, source) = read_declaration(Path::new(&remote.root), &remote.repo, machine.config.borrow().as_ref(), ENV_NAME);
    machine.notes.borrow_mut().extend(source.log_lines(&remote.repo, ENV_NAME));
    let problem = match read {
        Ok(None) => return Ok(Vec::new()),
        Ok(Some(bytes)) => match gate_router::env::read_env(&bytes) {
            Ok(prefixes) => return Ok(prefixes),
            Err(problem) => problem,
        },
        Err(problem) => problem,
    };
    Err(gate_router::env::problem_line(&source.said(ENV_NAME), &problem))
}

/// The hash of `remote`'s warm-up file when its repo has warm-up consent and the file parsed; else `None`. Consent is
/// read from the routing file the decision read, not read again, and without it the file is not read. Neither the
/// warm-up record nor its lock is read: the run routes either way, and the hash only words the cold note and guards the
/// record a `layout` refusal drops.
fn consented_hash(remote: &RemoteRun, machine: &Machine<'_>) -> Option<String> {
    let config = machine.config.borrow();
    let consented = config.as_ref().is_some_and(|config| config.warmup_consented(&remote.repo) == Ok(true));
    if !consented {
        return None;
    }
    let (read, source) = warm::read_warmup_file(Path::new(&remote.root), &remote.repo, config.as_ref());
    machine.notes.borrow_mut().extend(source.log_lines(&remote.repo, warm::WARMUP_NAME));
    match read {
        FileRead::Parsed { hash, .. } => Some(hash),
        FileRead::Absent | FileRead::Problem(_) => None,
    }
}

/// The queue line's `notRouted`: `--local` for a heavy gate its caller kept here with `--local`, `another worktree`
/// for a matched command whose outside path lies in another git worktree, `declared local` for one its repo's
/// `.gate/local.toml` keeps here, by a pattern or by a problem, `gitignored input` for one whose word names a gitignored
/// project input, else none.
pub(crate) fn not_routed(route: &Route) -> Option<String> {
    match route {
        Route::Local {
            reason: Some(LocalReason::LocalFlag),
        } => Some(LOCAL_FLAG.to_owned()),
        Route::Local {
            reason: Some(LocalReason::OtherWorktree { .. }),
        } => Some(ANOTHER_WORKTREE.to_owned()),
        Route::Local {
            reason: Some(LocalReason::DeclaredLocal { .. } | LocalReason::LocalFile { .. }),
        } => Some(DECLARED_LOCAL.to_owned()),
        Route::Local {
            reason: Some(LocalReason::GitignoredPastCap { .. } | LocalReason::GitignoredPair { .. } | LocalReason::GitignoredUnsent { .. }),
        } => Some(GITIGNORED_INPUT.to_owned()),
        _ => None,
    }
}

/// The exclusions of the input globs a word's project input is matched against ([`Machine::uncovered`]): none, a
/// regex no path matches. The inputs' own exclusions hold only for paths no project input has.
const NOTHING_EXCLUDED: &str = r"[^\s\S]";

/// The folders a sweep ([`swept_folder`]) never enters, at any depth below the swept folder, in any ASCII letter case.
const NOT_SWEPT: &str = r"(^|/)(bin|obj)/";

/// Why a gitignored candidate whose project files lie in another repository cannot be carried.
const IN_NESTED: &str = "it is inside a nested repository";

/// Why a gitignored candidate whose folder is, or lies under, a link or a junction cannot be carried.
const IN_LINK: &str = "the folder is a link or junction";

/// The files the gitignored candidate `path`, `/`-separated below `root`, carries to the builder, each with its size:
/// a folder, and a project or solution file ([`is_project_or_solution`]) below the root, carry their folder's files
/// ([`swept_folder`]); any other file candidate carries only itself.
///
/// # Errors
///
/// Why the candidate cannot be carried whole: its folder is, or lies under, a link or a junction ([`IN_LINK`]), one of
/// its project files `own` lies in another repository ([`IN_NESTED`]) or is left out of its folder's files, or its
/// folder or file cannot be read.
fn swept(root: &Path, path: &str, own: &[String], globs: &Globs, log: Option<&str>) -> Result<Vec<(String, u64)>, String> {
    let file = is_input_file(path);
    let folder = if file {
        path.rsplit_once('/').map_or("", |(folder, _)| folder)
    } else {
        path
    };
    if under_link(root, folder) {
        return Err(IN_LINK.to_owned());
    }
    let mut known = HashMap::new();
    if own.iter().any(|own| nested_folder(root, own, &mut known).is_some()) {
        return Err(IN_NESTED.to_owned());
    }
    if file && (folder.is_empty() || !is_project_or_solution(path)) {
        let metadata = fs::metadata(root.join(path)).map_err(|error| format!("cannot read {}: {error}", escaped(path)))?;
        return Ok(vec![(path.to_owned(), metadata.len())]);
    }
    let carried = swept_folder(root, folder, globs, log, &mut known)?;
    match own.iter().find(|own| !carried.iter().any(|(file, _)| file.eq_ignore_ascii_case(own))) {
        Some(left) => Err(format!("{} is left out of the files its folder sends", escaped(left))),
        None => Ok(carried),
    }
}

/// Every file at any depth below `folder`, `/`-separated below `root` (the root itself when empty), that a walk lists
/// ([`walk`]: links and junctions neither followed nor listed), with its size, but those below a `bin` or `obj` folder
/// of it ([`NOT_SWEPT`]), those the inputs never send ([`EXCLUDED`]), those of more than [`PATH_DEPTH_LIMIT`] parts
/// from the root, those inside another repository ([`nested_folder`], its answers kept in `known`), those one of
/// the input `globs` already carries, and the run's own `log` and its `.err` file ([`is_run_log`]). Which of them git
/// ignores is not asked here.
///
/// # Errors
///
/// Why the inputs' exclusions cannot be matched, or the folder walked.
fn swept_folder(
    root: &Path,
    folder: &str,
    globs: &Globs,
    log: Option<&str>,
    known: &mut HashMap<String, bool>,
) -> Result<Vec<(String, u64)>, String> {
    let sweep = Globs::inputs(&["**".to_owned()], &format!("(?i){EXCLUDED}|{NOT_SWEPT}"))?;
    let found = walk(&root.join(folder), &sweep).map_err(|error| error.to_string())?;
    Ok(found
        .into_iter()
        .map(|file| {
            let full = if folder.is_empty() {
                file.path
            } else {
                format!("{folder}/{}", file.path)
            };
            (full, file.stamp.size)
        })
        .filter(|(file, _)| !is_run_log(file, log))
        .filter(|(file, _)| file.split('/').count() <= PATH_DEPTH_LIMIT && !globs.matches(file) && nested_folder(root, file, known).is_none())
        .collect())
}

/// Whether `file` is the run's own `log` or its `.err` file, both `/`-separated below the root, compared without regard
/// to ASCII letter case as the inputs compare it ([`relative`]); never when the log lies outside the worktree.
fn is_run_log(file: &str, log: Option<&str>) -> bool {
    log.is_some_and(|log| file.eq_ignore_ascii_case(log) || file.eq_ignore_ascii_case(&format!("{log}.err")))
}

/// Whether `folder`, `/`-separated below `root`, or a folder above it below the root, is a link, a junction or another
/// reparse point. One whose attributes cannot be read counts as none: its walk says why it cannot be read.
fn under_link(root: &Path, folder: &str) -> bool {
    let mut at = root.to_path_buf();
    folder.split('/').filter(|part| !part.is_empty()).any(|part| {
        at.push(part);
        fs::symlink_metadata(&at).is_ok_and(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    })
}

/// A candidate [`Machine::gitignored_inputs`] judges: its path and the project files it names ([`input_files`]).
type Judged<'p> = (&'p String, Vec<String>);

/// A gitignored candidate [`Machine::gitignored_inputs`] sweeps: its path, and the files it carries or why it cannot be
/// carried ([`swept`]).
type Swept<'p> = (&'p String, Result<Vec<(String, u64)>, String>);

/// Each file the candidates `swept` carry, once each.
fn carried_of<'s>(swept: &[&'s Swept<'_>]) -> Vec<&'s String> {
    let mut seen = HashSet::new();
    swept
        .iter()
        .filter_map(|(_, carried)| carried.as_ref().ok())
        .flatten()
        .map(|(file, _)| file)
        .filter(|file| seen.insert(file.as_str()))
        .collect()
}

/// The files of `carried` that are in `ignored`.
fn kept_ignored(carried: &[(String, u64)], ignored: &HashSet<String>) -> Vec<(String, u64)> {
    carried.iter().filter(|(file, _)| ignored.contains(file)).cloned().collect()
}

/// The files the candidate `path`, `/`-separated below `root`, names as a build's input: itself when it has an input
/// file's extension ([`is_input_file`]) and is a file; else, when it is a folder, each file directly in it that is a
/// project or solution file ([`is_project_or_solution`]), in the order the folder lists them; else none.
///
/// # Errors
///
/// Why the folder `path` names cannot be listed.
fn input_files(root: &Path, path: &str) -> Result<Vec<String>, String> {
    let on_disk = root.join(path);
    if is_input_file(path) {
        return Ok(if on_disk.is_file() { vec![path.to_owned()] } else { Vec::new() });
    }
    if !on_disk.is_dir() {
        return Ok(Vec::new());
    }
    let cannot = |error: io::Error| format!("cannot list {}: {error}", escaped(path));
    let mut files = Vec::new();
    for entry in fs::read_dir(&on_disk).map_err(cannot)? {
        let entry = entry.map_err(cannot)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_project_or_solution(name) && entry.file_type().map_err(cannot)?.is_file() {
            files.push(format!("{path}/{name}"));
        }
    }
    Ok(files)
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
    /// The most bytes of a change listing git may write before it is ended and the change cannot be told: the git
    /// module's [`git::OUTPUT_CAP`].
    listing_cap: usize,
    /// How long the git calls of one read of the change against `.gate/windows.toml`'s paths may take together
    /// ([`Facts::windows_change`]): [`CHANGE_DEADLINE`], or the `--test-git-limit-ms` seam when it is given; the hint's
    /// decision's own git limit for the hint's ([`would_route_on_heavy`]).
    change_deadline: Duration,
    /// Whether a read of the change could not tell it ([`Facts::windows_change`]), which words are gitignored could not
    /// be told for every candidate, or a word names a gitignored project input ([`Facts::gitignored_inputs`]): the
    /// hint's decision then gives no hint ([`would_route_on_heavy`]), so a light run naming a gitignored project says
    /// nothing whether heavy would carry it or keep it here.
    change_unknown: Cell<bool>,
    /// Whether a placement call's readers get as long as git itself may take once it exits by itself
    /// ([`git::placing_drain`]): true for the decision, false for the hint's, which keeps the ordinary drain while the
    /// run holds its light slot ([`would_route_on_heavy`]).
    placing: bool,
    found: RefCell<Placed>,
    /// The routing file the decision read, when it read one: the warm-up's judgement reads its consent from this.
    config: RefCell<Option<Config>>,
    /// Whether the worktree an outside path lies in is looked up ([`Facts::worktree_of`]): only a decision that may say
    /// the line naming it does.
    look_up_trees: bool,
    /// Whether the worktree's `HEAD` is read ([`Facts::head_sha`]): the route hint, which throws the route away, does
    /// not, and answers no sha.
    read_head: bool,
    /// The log lines of the declaration files the decision read ([`Source::log_lines`]), for the log alone.
    notes: RefCell<Vec<String>>,
    /// Each declaration file the decision read, by name, with how the lines name it ([`Source::said`]).
    said: RefCell<Vec<(String, String)>>,
    /// The remote transport the test seams name, whose builder root the box trees lie under ([`Facts::box_trees`]);
    /// `None` for the ssh transport.
    remote_transport: Option<String>,
    /// The absolute path of the log the run writes, which a sweep leaves out ([`swept_folder`]); `None` for a decision
    /// that runs nothing (`gate warm`, the route hint).
    run_log: Option<PathBuf>,
    /// The state root whose built-here record says which builds ran here ([`Facts::built_here`]); `None` for a
    /// decision that routes no command (`gate warm`), which then finds no build here.
    state: Option<PathBuf>,
}

impl<'a> Machine<'a> {
    /// The facts of the gate's working folder now, with `environment`'s LOCALAPPDATA and the git limits and remote
    /// transport `seams` set.
    pub(crate) fn new(environment: &'a Environment, seams: &TestSeams) -> Machine<'a> {
        Machine::of(environment.local_app_data.as_deref(), seams)
    }

    /// The facts of the gate's working folder now, reading `local_app_data`'s routing file, with the git limits and
    /// remote transport `seams` set.
    fn of(local_app_data: Option<&'a OsStr>, seams: &TestSeams) -> Machine<'a> {
        Machine {
            remote_transport: seams.remote_transport.clone(),
            change_deadline: change_deadline(seams),
            ..Machine::at(local_app_data, git::limit(seams), git::drain(seams))
        }
    }

    /// The facts of the gate's working folder now, reading `local_app_data`'s routing file, with the decision's git
    /// calls under `git_limit` and their readers under `git_drain`.
    fn at(local_app_data: Option<&'a OsStr>, git_limit: Duration, git_drain: Duration) -> Machine<'a> {
        Machine {
            folder: std::env::current_dir(),
            local_app_data,
            git_limit,
            git_drain,
            change_deadline: CHANGE_DEADLINE,
            change_unknown: Cell::new(false),
            listing_cap: git::OUTPUT_CAP,
            placing: true,
            found: RefCell::new(Placed::NotAsked),
            config: RefCell::new(None),
            look_up_trees: true,
            read_head: true,
            notes: RefCell::new(Vec::new()),
            said: RefCell::new(Vec::new()),
            remote_transport: None,
            run_log: None,
            state: None,
        }
    }

    /// These facts with the built-here record read under the state root `decision` saw.
    fn with_state(self, decision: &Decision) -> Machine<'a> {
        Machine {
            state: Some(decision.state.clone()),
            ..self
        }
    }

    /// The facts of the hint's decision ([`would_route_on_heavy`]): the routing file `local_app_data` names, its git
    /// calls under [`HINT_GIT_LIMIT`] or the seams' limit when that is shorter, its readers under the seams' drain,
    /// which its placement call keeps, and neither the worktree of an outside path nor the worktree's `HEAD` read.
    fn hint(local_app_data: Option<&'a OsStr>, seams: &TestSeams) -> Machine<'a> {
        let limit = git::limit(seams).min(HINT_GIT_LIMIT);
        Machine {
            look_up_trees: false,
            read_head: false,
            placing: false,
            change_deadline: limit,
            ..Machine::at(local_app_data, limit, git::drain(seams))
        }
    }

    /// The notes gathered so far that this process has not said yet ([`unsaid`]), taken: lines for the log alone.
    pub(crate) fn unsaid_notes(&self) -> Vec<String> {
        unsaid(self.notes.take())
    }

    /// Holds `config` as the routing file the decision read, as [`Facts::config`] holds the one it reads: `gate warm`
    /// reads its own and hands it over, so the declaration files are read through the folder it names.
    pub(crate) fn hold_config(&self, config: &Config) {
        self.config.replace(Some(config.clone()));
    }

    /// The declaration file `name` of the repo at `place` ([`read_declaration`]), its declarations folder the one the
    /// routing file the decision read names, its log lines kept among the notes and how the lines name it kept.
    fn declaration(&self, place: &Place, name: &str) -> Result<Option<Vec<u8>>, String> {
        let repo = repo_name(&place.checkout);
        let (read, source) = read_declaration(Path::new(&place.root), &repo, self.config.borrow().as_ref(), name);
        self.notes.borrow_mut().extend(source.log_lines(&repo, name));
        let mut said = self.said.borrow_mut();
        said.retain(|(read_name, _)| read_name != name);
        said.push((name.to_owned(), source.said(name)));
        read
    }

    /// Which of `paths`, relative to the root of `place`, one `git ls-files --cached --others --exclude-standard` of the
    /// gate's folder lists as a file or as a folder holding one, compared byte for byte ([`Facts::snapshot_carries`]).
    /// The only pathspec is the folder's prefix, as git spells it (none at the root): a pathspec naming a candidate
    /// in another letter case would be listed in that case on a disk that ignores it.
    fn listed_paths(&self, place: &Place, paths: &[String]) -> Result<Vec<String>, String> {
        let root = Path::new(&place.root);
        let mut args = vec!["--literal-pathspecs", "ls-files", "-z", "--cached", "--others", "--exclude-standard"];
        if !place.prefix.is_empty() {
            args.extend(["--", place.prefix.as_str()]);
        }
        let mut ran = git::Git::find(root)?.run(&self.git_call(root, &args))?;
        if !ran.status.success() {
            let code = ran.status.code().map_or_else(|| "unknown".to_owned(), |code| code.to_string());
            return Err(format!("git ls-files failed with status {code}"));
        }
        let mut listing = Vec::new();
        ran.stdout
            .read_to_end(&mut listing)
            .map_err(|error| format!("cannot read git ls-files' output: {error}"))?;
        let listed: Vec<&[u8]> = listing.split(|byte| *byte == 0).filter(|path| !path.is_empty()).collect();
        Ok(paths
            .iter()
            .filter(|path| listed.iter().any(|file| lists_path(file, path.as_bytes())))
            .cloned()
            .collect())
    }

    /// Each of `paths`, relative to `root`, that names files ([`input_files`]) none of `globs` matches, with those files:
    /// a path naming no file, or a file an input glob carries, is left out, and so is one whose folder cannot be
    /// listed, saying why ([`Machine::cannot_tell`]).
    fn uncovered<'p>(&self, root: &Path, paths: &'p [String], globs: &Globs) -> Vec<(&'p String, Vec<String>)> {
        let mut judged = Vec::new();
        for path in paths {
            match input_files(root, path) {
                Ok(files) if !files.is_empty() && !files.iter().any(|file| globs.matches(file)) => judged.push((path, files)),
                Ok(_) => {}
                Err(why) => self.cannot_tell(&why),
            }
        }
        judged
    }

    /// Which of `files`, relative to `root`, one `git check-ignore -z --stdin` in the root reports ignored; a tracked
    /// file is never reported.
    ///
    /// # Errors
    ///
    /// Why git cannot be started, failed, with its status and last error lines ([`git::git_failure`]), or wrote what
    /// cannot be read.
    fn check_ignore(&self, root: &Path, files: &[&String]) -> Result<Vec<String>, String> {
        let input: Vec<u8> = files.iter().flat_map(|file| file.bytes().chain([0])).collect();
        let args = ["check-ignore", "-z", "--stdin"];
        let call = git::Call {
            stdin: Some(&input),
            ..self.git_call(root, &args)
        };
        let (mut ran, errors) = git::Git::find(root)?.run_kept(&call)?;
        // 1 says no path is ignored; anything but 0 and 1 is git's own failure.
        if !matches!(ran.status.code(), Some(0 | 1)) {
            return Err(format!("git check-ignore failed: {}", git::git_failure(ran.status.code(), &errors)));
        }
        let mut listing = Vec::new();
        ran.stdout
            .read_to_end(&mut listing)
            .map_err(|error| format!("cannot read git check-ignore's output: {error}"))?;
        let listing = String::from_utf8(listing).map_err(|_| "git check-ignore's output is not valid UTF-8".to_owned())?;
        Ok(listing.split('\0').filter(|file| !file.is_empty()).map(str::to_owned).collect())
    }

    /// Says in the log why which words are gitignored cannot be told, and marks the decision as one that gives no
    /// route hint ([`Machine::change_unknown`]).
    fn cannot_tell(&self, why: &str) {
        self.notes
            .borrow_mut()
            .push(format!("gate: cannot tell which words are gitignored: {why}"));
        self.change_unknown.set(true);
    }

    /// The candidates of `judged` all of whose project files one `git check-ignore` in `root` reports ignored
    /// ([`Machine::check_ignore`]). When that call fails, each candidate's files are asked alone, and each candidate
    /// whose call fails too is left out, saying why ([`Machine::cannot_tell`]): one path git refuses (one in a
    /// submodule) hides no other.
    fn ignored_candidates<'j, 'p>(&self, root: &Path, judged: &'j [Judged<'p>]) -> Vec<&'j Judged<'p>> {
        let all_ignored = |files: &[String], ignored: &[String]| files.iter().all(|file| ignored.contains(file));
        let every: Vec<&String> = judged.iter().flat_map(|(_, files)| files).collect();
        match self.check_ignore(root, &every) {
            Ok(ignored) => judged.iter().filter(|(_, files)| all_ignored(files, &ignored)).collect(),
            Err(_) => judged
                .iter()
                .filter(|(_, files)| match self.check_ignore(root, &files.iter().collect::<Vec<_>>()) {
                    Ok(ignored) => all_ignored(files, &ignored),
                    Err(why) => {
                        self.cannot_tell(&why);
                        false
                    }
                })
                .collect(),
        }
    }

    /// Each candidate of `swept` as a gitignored input: the files it carries that one `git check-ignore` in `root`
    /// reports ignored ([`Machine::check_ignore`]), or why it cannot be carried. When that call fails, each candidate's
    /// files are asked alone, and a candidate whose call fails too cannot be carried, for git's reason.
    fn carried_ignored(&self, root: &Path, swept: &[Swept<'_>]) -> Vec<GitignoredInput> {
        let set = |ignored: Vec<String>| ignored.into_iter().collect::<HashSet<_>>();
        let every = carried_of(&swept.iter().collect::<Vec<_>>());
        if every.is_empty() {
            return swept
                .iter()
                .map(|(path, files)| GitignoredInput {
                    path: (*path).clone(),
                    files: files.clone(),
                })
                .collect();
        }
        let whole = self.check_ignore(root, &every).map(set);
        swept
            .iter()
            .map(|one| {
                let files = match (&one.1, &whole) {
                    (Err(why), _) => Err(why.clone()),
                    (Ok(carried), Ok(ignored)) => Ok(kept_ignored(carried, ignored)),
                    (Ok(carried), Err(_)) => self
                        .check_ignore(root, &carried_of(&[one]))
                        .map(|ignored| kept_ignored(carried, &set(ignored))),
                };
                GitignoredInput { path: one.0.clone(), files }
            })
            .collect()
    }

    /// What the change of the worktree at `place`, and of each of `siblings` an entry names, is to the paths its
    /// `.gate/windows.toml` declares ([`Facts::windows_change`]): untouched with no file or no entry, else the first path
    /// a declared path matches of the worktree's change, read only when an entry is not a sibling's, then of each named
    /// sibling's in declared order, its paths prefixed `../{name}/` ([`Machine::touched_in`]). A repo is read only while
    /// no earlier path is touched. Every git call shares one deadline of [`Machine::change_deadline`].
    ///
    /// # Errors
    ///
    /// Why it cannot be told: the file cannot be read or has a problem, git cannot be found, or a repo's change cannot
    /// be read.
    fn read_change(&self, place: &Place, siblings: &[SiblingRoot], siblings_problem: Option<&str>) -> Result<WindowsChange, String> {
        let Some(bytes) = self.declaration(place, windows::WINDOWS_NAME)? else {
            return Ok(WindowsChange::Untouched);
        };
        let names: Vec<&str> = siblings.iter().map(|sibling| sibling.name.as_str()).collect();
        let declared = siblings_problem.map_or(Ok(names.as_slice()), Err);
        let patterns = windows::read_windows(&bytes, declared)?;
        if patterns.is_empty() {
            return Ok(WindowsChange::Untouched);
        }
        let deadline = Instant::now() + self.change_deadline;
        let own = std::iter::once(ChangedRepo {
            root: Path::new(&place.root),
            sibling: None,
        });
        let named = siblings.iter().map(|sibling| ChangedRepo {
            root: Path::new(&sibling.root),
            sibling: Some(&sibling.name),
        });
        let git = git::Git::find(Path::new(&place.root))?;
        for repo in own.chain(named) {
            let entries = windows::entries_for(&patterns, repo.sibling);
            if entries.is_empty() {
                continue;
            }
            if let Some(path) = self.touched_in(&git, &repo, &entries, deadline)? {
                return Ok(WindowsChange::Touched { path });
            }
        }
        Ok(WindowsChange::Untouched)
    }

    /// The first path of `repo`'s change that one of its `entries` ([`windows::entries_for`]) matches, its path prefixed
    /// `../{name}/` for a sibling: the tracked paths `git diff --name-only --no-renames` lists against the merge-base
    /// ([`Machine::change_base`]), committed, staged or not, then, only when none of them is touched, the untracked ones
    /// `git ls-files --others --exclude-standard` lists, each in git's order and read up to [`Machine::listing_cap`].
    /// Each git call runs under what is left of `deadline`.
    ///
    /// # Errors
    ///
    /// Why the change cannot be read ([`Machine::change_base`]), a listing git refuses, writes past the cap or writes
    /// with a path that is not valid UTF-8, or the deadline passed.
    fn touched_in(&self, git: &git::Git, repo: &ChangedRepo<'_>, entries: &[Pattern], deadline: Instant) -> Result<Option<String>, String> {
        let base = self.change_base(git, repo.root, deadline)?;
        let diff = ["--no-optional-locks", "diff", "--name-only", "--no-renames", "-z", base.as_str(), "--"];
        let others = ["--no-optional-locks", "ls-files", "--others", "--exclude-standard", "-z"];
        for args in [&diff[..], &others[..]] {
            let listing = self
                .change_answer(git, repo.root, args, deadline, self.listing_cap)?
                .map_err(|refused| refused.line)?;
            let mut paths = nul_separated(&listing, repo.root)?;
            if let Some(name) = repo.sibling {
                paths = paths.iter().map(|path| windows::sibling_path(name, path)).collect();
            }
            if let Some(path) = windows::first_touched(entries, &paths) {
                return Ok(Some(path.to_owned()));
            }
        }
        Ok(None)
    }

    /// The merge-base of the `HEAD` of the repo at `root` with the default branch's upstream ([`Machine::upstream`]).
    ///
    /// # Errors
    ///
    /// A shallow clone, a detached or unborn `HEAD`, a `HEAD` git cannot read (its failure line), no upstream ref, no
    /// merge-base, a git call that fails or does not answer, or the deadline passed.
    fn change_base(&self, git: &git::Git, root: &Path, deadline: Instant) -> Result<String, String> {
        let shown = root.display();
        let head_args = [
            "--no-optional-locks",
            "rev-parse",
            "--is-shallow-repository",
            "--symbolic-full-name",
            "HEAD",
        ];
        let head = match self.change_answer(git, root, &head_args, deadline, git::OUTPUT_CAP)? {
            Ok(head) => utf8_answer(head, "rev-parse", root)?,
            Err(refused) => return Err(self.head_failure(git, root, refused, deadline)?),
        };
        let mut lines = head.lines();
        if lines.next() != Some("false") {
            return Err(format!("{shown} is a shallow clone"));
        }
        if !lines.next().is_some_and(|name| name.starts_with("refs/")) {
            return Err(format!("HEAD is detached in {shown}"));
        }
        let upstream = self.upstream(git, root, deadline)?;
        let no_base = || format!("HEAD and {upstream} have no merge-base in {shown}");
        let merge_base = ["--no-optional-locks", "merge-base", "HEAD", upstream];
        match self.change_answer(git, root, &merge_base, deadline, git::OUTPUT_CAP)? {
            Ok(base) => Some(utf8_answer(base, "merge-base", root)?.trim().to_owned())
                .filter(|base| !base.is_empty())
                .ok_or_else(no_base),
            Err(Refused { code: Some(1), .. }) => Err(no_base()),
            Err(refused) => Err(refused.line),
        }
    }

    /// Why the `HEAD` of the repo at `root` could not be read, git having `refused` it: that it names no commit when
    /// `HEAD` names a branch (an unborn one), else git's own failure line, as for a pruned worktree or a folder git
    /// will not read.
    ///
    /// # Errors
    ///
    /// The deadline passed, or git cannot be started or does not answer within its limit.
    fn head_failure(&self, git: &git::Git, root: &Path, refused: Refused, deadline: Instant) -> Result<String, String> {
        let branch = ["--no-optional-locks", "symbolic-ref", "--quiet", "HEAD"];
        Ok(match self.change_answer(git, root, &branch, deadline, git::OUTPUT_CAP)? {
            Ok(_) => format!("HEAD names no commit in {}", root.display()),
            Err(_) => refused.line,
        })
    }

    /// The default branch's upstream of the repo at `root`, as the ref that names it: [`ORIGIN_HEAD`] when it names a
    /// commit, a symbolic ref followed, else [`ORIGIN_MAIN`] when it names one, as after the target of `origin/HEAD` was
    /// pruned. Each is asked with `git rev-parse --verify --quiet <ref>^{commit}`, whose exit 1 says there is none.
    ///
    /// # Errors
    ///
    /// Neither ref names a commit, a rev-parse fails otherwise (its failure line), or a git call does not answer.
    fn upstream(&self, git: &git::Git, root: &Path, deadline: Instant) -> Result<&'static str, String> {
        for name in [ORIGIN_HEAD, ORIGIN_MAIN] {
            let commit = format!("{name}^{{commit}}");
            let verify = ["--no-optional-locks", "rev-parse", "--verify", "--quiet", commit.as_str()];
            match self.change_answer(git, root, &verify, deadline, git::OUTPUT_CAP)? {
                Ok(_) => return Ok(name),
                Err(Refused { code: Some(1), .. }) => {}
                Err(refused) => return Err(refused.line),
            }
        }
        Err(format!("{} has no {ORIGIN_HEAD} or {ORIGIN_MAIN}", root.display()))
    }

    /// What one git call of a change's read with `args` in `root` wrote on standard output, read up to `cap` bytes by a
    /// reader that ends git as soon as it passes them ([`git::Git::status_within`]); `Err` with the call's status and
    /// failure line when git exits with a failure status. The call's limit is what is left of `deadline`, never more
    /// than the decision's own ([`change_limit`]).
    ///
    /// # Errors
    ///
    /// The deadline passed, or the call was ended at what was left of it ([`change_error`]); git cannot be started,
    /// does not answer within its own limit, or writes more than `cap`.
    fn change_answer(&self, git: &git::Git, root: &Path, args: &[&str], deadline: Instant, cap: usize) -> Result<Result<Vec<u8>, Refused>, String> {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(limit) = change_limit(left, self.git_limit) else {
            return Err(deadline_line(self.change_deadline));
        };
        let call = git::Call {
            drain: self.git_drain,
            ..git::Call::plain(root, args, limit)
        };
        let (status, output, errors) = git
            .status_within(&call, cap)
            .map_err(|error| change_error(error, limit, limit < self.git_limit, self.change_deadline))?;
        if !status.success() {
            let code = status.code();
            return Ok(Err(Refused {
                code,
                line: git::failed_line(&call, code, &errors, &output),
            }));
        }
        Ok(Ok(output))
    }

    /// The decision's git call in `folder` with `args`, under the decision's limit and drain.
    fn git_call<'b>(&self, folder: &'b Path, args: &'b [&'b str]) -> git::Call<'b> {
        git::Call {
            drain: self.git_drain,
            ..git::Call::plain(folder, args, self.git_limit)
        }
    }

    /// The decision's placement call in `folder`: [`PLACE_ARGS`] asked and built as every placement call is
    /// ([`git::Call::placing`]), but for the hint's decision, which keeps the ordinary drain while the run holds its
    /// light slot ([`would_route_on_heavy`]).
    fn place_call<'b>(&self, folder: &'b Path) -> git::Call<'b> {
        if self.placing {
            git::Call::placing(folder, &PLACE_ARGS, self.git_limit, self.git_drain, None)
        } else {
            self.git_call(folder, &PLACE_ARGS)
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
                read_place(&git, &self.place_call(folder))
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
        if !self.read_head {
            return Ok(None);
        }
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

    /// Empty when the trees cannot be told or a PC root cannot stand in the word, saying why in the log.
    fn box_trees(&self, target: &Target) -> Vec<(String, String)> {
        remote::box_trees(self.remote_transport.as_deref(), target).unwrap_or_else(|why| {
            self.notes.borrow_mut().push(format!("gate: no PathMap word: {why}"));
            Vec::new()
        })
    }

    /// A build file that cannot be read counts as moving it, saying why in the log.
    fn moves_build_output(&self, place: &Place, folder: &str, file: Option<&str>, default_configuration: bool) -> bool {
        no_build::moves_output(Path::new(&place.root), folder, file, default_configuration).unwrap_or_else(|why| {
            self.notes.borrow_mut().push(format!("gate: not sending the PC's build: {why}"));
            true
        })
    }

    /// A folder that cannot be listed holds none, saying why in the log.
    fn project_files(&self, place: &Place, folder: &str) -> Vec<String> {
        let path = folder
            .split('/')
            .filter(|segment| !segment.is_empty())
            .fold(PathBuf::from(&place.root), |path, segment| path.join(segment));
        match no_build::project_files(&path) {
            Ok(files) => files
                .iter()
                .filter_map(|file| file.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect(),
            Err(error) => {
                let line = format!(
                    "gate: not sending the PC's build: cannot read {}: {error}",
                    escaped(&path.to_string_lossy())
                );
                self.notes.borrow_mut().push(line);
                Vec::new()
            }
        }
    }

    /// The rule a run here keeps `--no-build` by ([`no_build::last_build_ran_here`]); no state root finds no build.
    fn built_here(&self, root: &str, words: &[String]) -> Option<String> {
        no_build::last_build_ran_here(self.state.as_deref()?, root, words)
    }

    /// A project that cannot be read counts as Windows-only, saying why in the log.
    fn windows_only_tests(&self, place: &Place, words: &[String]) -> bool {
        dotnet_projects::windows_only_tests(Path::new(&place.root), &place.prefix, words).unwrap_or_else(|why| {
            self.notes.borrow_mut().push(format!("gate: kept off the macOS builder: {why}"));
            true
        })
    }

    /// A change that touches a declared path, or cannot be told to, says why in the log ([`Machine::read_change`]).
    /// The hint's machine notes an unknown change apart, which means no hint ([`would_route_on_heavy`]).
    fn windows_change(&self, place: &Place, siblings: &[SiblingRoot], siblings_problem: Option<&str>) -> WindowsChange {
        let change = self
            .read_change(place, siblings, siblings_problem)
            .unwrap_or_else(|why| WindowsChange::Unknown { why });
        if matches!(change, WindowsChange::Unknown { .. }) {
            self.change_unknown.set(true);
        }
        if let Some(reason) = change.reason(&self.declaration_said(place, windows::WINDOWS_NAME)) {
            self.notes.borrow_mut().push(format!("gate: kept off the macOS builder: {reason}"));
        }
        change
    }

    fn file_in_folder(&self, name: &str) -> bool {
        self.folder.as_ref().is_ok_and(|folder| folder.join(name).is_file())
    }

    /// One `git ls-files` of the gate's folder, each path matched against what it lists in its letter case
    /// ([`Machine::listed_paths`]). A git that cannot be started, fails or writes what cannot be read carries none, which
    /// keeps the command off the Mac, saying why in the log.
    fn snapshot_carries(&self, place: &Place, paths: &[String]) -> Vec<String> {
        self.listed_paths(place, paths).unwrap_or_else(|why| {
            self.notes
                .borrow_mut()
                .push(format!("gate: cannot tell what the snapshot carries: {why}"));
            Vec::new()
        })
    }

    /// The project files each path names that no input glob of `inputs` matches, as the inputs' own matcher matches
    /// them ([`Machine::uncovered`]), are asked of one `git check-ignore` ([`Machine::ignored_candidates`]), made only
    /// when there is such a file; a path is given back when git reports every one of its project files ignored, and only
    /// such a path is swept, carrying its swept files git reports ignored, or why it cannot be carried
    /// ([`Machine::carried_ignored`]). A decision given any path back gives no route hint ([`Machine::change_unknown`]).
    fn gitignored_inputs(&self, place: &Place, paths: &[String], inputs: &[String]) -> Vec<GitignoredInput> {
        let root = Path::new(&place.root);
        let globs = match Globs::inputs(inputs, NOTHING_EXCLUDED) {
            Ok(globs) => globs,
            Err(why) => {
                self.cannot_tell(&why);
                return Vec::new();
            }
        };
        let judged = self.uncovered(root, paths, &globs);
        if judged.is_empty() {
            return Vec::new();
        }
        let log = self.run_log.as_deref().and_then(|log| relative(root, log));
        let swept: Vec<Swept<'_>> = self
            .ignored_candidates(root, &judged)
            .into_iter()
            .map(|(path, own)| (*path, swept(root, path, own, &globs, log.as_deref())))
            .collect();
        let found = self.carried_ignored(root, &swept);
        // A light run naming a gitignored project input gives no route hint, whether or not heavy would carry it.
        if !found.is_empty() {
            self.change_unknown.set(true);
        }
        found
    }

    fn subst_target(&self, letter: char) -> Option<String> {
        // The decision takes no error here: a letter whose device cannot be read is no subst drive, and a word on it
        // is judged as written.
        gate_win32::device_target(letter).ok().flatten()
    }

    fn siblings(&self, place: &Place) -> Result<Vec<String>, String> {
        read_siblings(self.declaration(place, "siblings.toml"))
    }

    fn desktop(&self, place: &Place) -> Result<Vec<Pattern>, String> {
        read_patterns(self.declaration(place, "desktop.toml"), gate_router::desktop::read_desktop)
    }

    fn local(&self, place: &Place) -> Result<Vec<Pattern>, String> {
        read_patterns(self.declaration(place, "local.toml"), gate_router::local::read_local)
    }

    /// As the last read of `name` named it; `.gate/<name>` when it was not read.
    fn declaration_said(&self, _place: &Place, name: &str) -> String {
        self.said
            .borrow()
            .iter()
            .find(|(read_name, _)| read_name == name)
            .map_or_else(|| format!(".gate/{name}"), |(_, said)| said.clone())
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
/// What `GetDriveTypeW` says of a fixed local drive.
const DRIVE_FIXED: u32 = 3;
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
    let kind = drive_type(char::from(letter));
    kind != DRIVE_NO_ROOT_DIR && kind != DRIVE_REMOTE
}

/// What `GetDriveTypeW` says of the root of the drive `letter`.
fn drive_type(letter: char) -> u32 {
    let root: Vec<u16> = format!(r"{letter}:\").encode_utf16().chain([0]).collect();
    // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call, which only reads it.
    unsafe { GetDriveTypeW(root.as_ptr()) }
}

/// The folder name of the main checkout of the worktree at `tree`, as git's common folder gives the run's own: the
/// parent of its common folder ([`common_folder_of`]). No git is started.
fn checkout_of(tree: &Path) -> Option<String> {
    common_folder_of(tree)?.parent()?.file_name()?.to_str().map(str::to_owned)
}

/// The git common folder of the worktree at `tree`, made absolute: `tree\.git` when that is a folder; for a `.git`
/// file, the common folder of the git folder its `gitdir:` line names ([`common_folder`]). `None` when any of it cannot
/// be read, the `.git` is a link or a junction, or the folder the `gitdir:` line names is on no local drive
/// ([`on_local_drive`]). No git is started.
pub(crate) fn common_folder_of(tree: &Path) -> Option<PathBuf> {
    let dot_git = tree.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        std::path::absolute(dot_git).ok()
    } else if metadata.is_file() {
        common_folder(&linked_git_folder(tree, &dot_git)?)
    } else {
        None
    }
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

/// The siblings a `siblings.toml` declares, from its `read` ([`read_declaration`]): no file declares none, and a file
/// that cannot be read, does not parse or has any problem gives its first problem.
fn read_siblings(read: Result<Option<Vec<u8>>, String>) -> Result<Vec<String>, String> {
    let Some(bytes) = read? else {
        return Ok(Vec::new());
    };
    let declared = siblings::read_bytes(&bytes)?;
    match declared.problems.into_iter().next() {
        Some(problem) => Err(problem),
        None => Ok(declared.names),
    }
}

/// `answer`, the output of the git call `name` in `root`, as UTF-8 text.
fn utf8_answer(answer: Vec<u8>, name: &str, root: &Path) -> Result<String, String> {
    String::from_utf8(answer).map_err(|_| format!("the output of git {name} in {} is not valid UTF-8", root.display()))
}

/// The non-empty paths of the `-z` listing git wrote of the repo at `root`, in order; `Err` naming the first path that
/// is not valid UTF-8, which cannot be judged.
fn nul_separated(listing: &[u8], root: &Path) -> Result<Vec<String>, String> {
    listing
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec()).map_err(|_| {
                format!(
                    "git listed a path that is not valid UTF-8 in {}: {}",
                    root.display(),
                    String::from_utf8_lossy(path)
                )
            })
        })
        .collect()
}

/// How long the next git call of a change's read may take with `left` of its deadline left: all of it, never more
/// than `per_call`; `None` once nothing is left.
fn change_limit(left: Duration, per_call: Duration) -> Option<Duration> {
    (!left.is_zero()).then(|| left.min(per_call))
}

/// What a git call of a change's read that git `error` ended says: the deadline's own line when the call was killed at
/// its `limit` and that limit was the time left of the shared `deadline` (`by_deadline`), else the error's line.
fn change_error(error: git::GitError, limit: Duration, by_deadline: bool, deadline: Duration) -> String {
    let why = String::from(error);
    if by_deadline && why == git::late_git_line(limit) {
        deadline_line(deadline)
    } else {
        why
    }
}

/// Why a change's read stopped once its git calls together passed `deadline`.
fn deadline_line(deadline: Duration) -> String {
    format!("its git calls did not answer within {}", git::spelled_limit(deadline))
}

/// A repo whose change a read of `.gate/windows.toml`'s paths asks git for: its root, and the sibling name its paths
/// are prefixed with, `None` for the worktree's own.
struct ChangedRepo<'a> {
    root: &'a Path,
    sibling: Option<&'a str>,
}

/// A git call of a change's read that git answered with a failure status: the status, and the failure line naming the
/// call, its folder, its status and its last lines.
struct Refused {
    code: Option<i32>,
    line: String,
}

/// The patterns a pattern file (`desktop.toml` or `local.toml`) declares, from its `read` ([`read_declaration`]) and
/// its file's reader `parse`: no file declares none, and a file that cannot be read, does not parse or has any problem
/// gives its first problem.
fn read_patterns(read: Result<Option<Vec<u8>>, String>, parse: fn(&[u8]) -> Result<Vec<Pattern>, String>) -> Result<Vec<Pattern>, String> {
    match read? {
        Some(bytes) => parse(&bytes),
        None => Ok(Vec::new()),
    }
}

/// Where a declaration file came from, or where its problem is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The worktree's own `.gate` folder; also a file neither place has.
    Repo,
    /// The repo's declarations folder that `remote.json` names: the file's full path there.
    Declarations(PathBuf),
}

/// Where a read of a declaration file went: the file's origin, why the repo's declarations folder could not be used
/// when the read reached it, and the copy there that the worktree's own file shadows, when that copy exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub origin: Origin,
    pub unusable: Option<String>,
    pub shadows: Option<PathBuf>,
}

impl Source {
    /// A read that went to the repo's own `.gate` alone, or found the file nowhere.
    const REPO: Source = Source {
        origin: Origin::Repo,
        unusable: None,
        shadows: None,
    };

    /// A read that found no file, the declarations folder unusable for the reason `why` when there is one.
    fn nothing(why: Option<String>) -> (Result<Option<Vec<u8>>, String>, Source) {
        (
            Ok(None),
            Source {
                unusable: why,
                ..Source::REPO
            },
        )
    }

    /// The log lines of this read of the file `name` of the repo `repo`: why its declarations folder was not read,
    /// then where a file taken from it was read from, or the copy the worktree's own file shadows.
    pub(crate) fn log_lines(&self, repo: &str, name: &str) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(why) = &self.unusable {
            lines.push(format!("gate: declarations for {} not read: {}", escaped(repo), escaped(why)));
        }
        if let Origin::Declarations(path) = &self.origin {
            lines.push(format!("gate: .gate/{name} read from {}", escaped(&path.to_string_lossy())));
        }
        if let Some(path) = &self.shadows {
            lines.push(shadows_line(name, path));
        }
        lines
    }

    /// How a line names the file `name` this read found: `.gate/<name>` for the repo's own, else its full path.
    pub(crate) fn said(&self, name: &str) -> String {
        match &self.origin {
            Origin::Repo => format!(".gate/{name}"),
            Origin::Declarations(path) => path.to_string_lossy().into_owned(),
        }
    }
}

/// The bytes of the declaration file `name` of the worktree at `root`, and where they came from. The repo's own
/// `.gate` answers first ([`read_repo_file`]): its file wins, its problem included, and a `.gate` that is not a folder
/// answers that there is none. Only when `.gate` is absent, or is a folder without the file, is the copy in the
/// declarations folder `config` names for `repo` read ([`read_declarations_copy`]). A file of the worktree's own, its
/// problem included, notes the copy it shadows, when there is one ([`declared_copy`]). Routing, `gate warm` and `gate
/// check` read `siblings.toml`, `desktop.toml` and `warmup.toml` through this, routing `local.toml` and `env.toml` too,
/// and `gate check` `toolchain.toml`.
pub(crate) fn read_declaration(root: &Path, repo: &str, config: Option<&Config>, name: &str) -> (Result<Option<Vec<u8>>, String>, Source) {
    match read_repo_file(root, name) {
        RepoRead::File(Ok(None)) => read_declarations_copy(repo, config, name),
        RepoRead::File(read) => {
            let shadows = declared_copy(repo, config, name);
            (read, Source { shadows, ..Source::REPO })
        }
        RepoRead::GateNotFolder => (Ok(None), Source::REPO),
    }
}

/// The full path of the copy of the declaration file `name` in the declarations folder `config` names for `repo`
/// ([`hold_declarations`]), when anything is there by that name: it is opened beneath the folder, never read, so a
/// file of the worktree's own learns of the copy it shadows without reading it. `None` when no folder is named, the
/// folder cannot be used, or it holds nothing by that name.
pub(crate) fn declared_copy(repo: &str, config: Option<&Config>, name: &str) -> Option<PathBuf> {
    let Ok(Some((folder, held))) = hold_declarations(repo, config) else {
        return None;
    };
    match gate_win32::open_beneath(&held, name, Share::All) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        _ => Some(Path::new(folder).join(name)),
    }
}

/// The log line saying that the worktree's own declaration file `name` shadows its copy at `path`.
pub(crate) fn shadows_line(name: &str, path: &Path) -> String {
    format!("gate: .gate/{name} shadows {}", escaped(&path.to_string_lossy()))
}

/// Why the declarations folder `config` names for `repo` cannot be used, judged as [`read_declarations_copy`] judges
/// it before it reads; `None` when it names none or the folder can be used. `gate check` judges the entry through this
/// whether or not a read reaches the folder.
pub(crate) fn judge_declarations(repo: &str, config: Option<&Config>) -> Option<String> {
    hold_declarations(repo, config).err()
}

/// The declarations folder `config` names for `repo`, matched in any letter case, as written and opened as itself;
/// `None` when it names none.
///
/// # Errors
///
/// Why the folder cannot be used: `declarations` cannot be used, or the folder is not on a fixed local drive
/// ([`on_fixed_drive`]), is missing, is a link or a junction, is not a folder or cannot be opened.
fn hold_declarations<'c>(repo: &str, config: Option<&'c Config>) -> Result<Option<(&'c str, fs::File)>, String> {
    let folder = match config.map(|config| config.declarations_folder(repo)) {
        None | Some(Ok(None)) => return Ok(None),
        Some(Err(problem)) => return Err(format!("remote.json's declarations cannot be used: {problem}")),
        Some(Ok(Some(folder))) => folder,
    };
    if !on_fixed_drive(folder) {
        return Err(format!("'{folder}' is not a folder on a fixed local drive"));
    }
    match hold_folder(Path::new(folder)) {
        Err(error) => Err(format!("'{folder}' cannot be read: {error}")),
        Ok(Held::Missing) => Err(format!("'{folder}' does not exist")),
        Ok(Held::Link) => Err(format!("'{folder}' is a link or a junction")),
        Ok(Held::NotFolder) => Err(format!("'{folder}' is not a folder")),
        Ok(Held::Folder(held)) => Ok(Some((folder, held))),
    }
}

/// The bytes of the file `name` in the declarations folder `config` names for `repo` ([`hold_declarations`]), and
/// where they came from: `None` when it names none or the folder holds no such file. A folder that cannot be used is
/// not read, and the source says why. The folder is held open while the file is read beneath it, as
/// [`read_repo_file`] reads one of `.gate`, and a problem of the file is that file's, at its full path.
pub(crate) fn read_declarations_copy(repo: &str, config: Option<&Config>, name: &str) -> (Result<Option<Vec<u8>>, String>, Source) {
    let (folder, held) = match hold_declarations(repo, config) {
        Ok(None) => return Source::nothing(None),
        Err(why) => return Source::nothing(Some(why)),
        Ok(Some(held)) => held,
    };
    let read = read_beneath(&held, name);
    drop(held);
    let origin = match read {
        Ok(None) => Origin::Repo,
        _ => Origin::Declarations(Path::new(folder).join(name)),
    };
    (read, Source { origin, ..Source::REPO })
}

/// Whether `folder` is an absolute drive path ([`is_drive_path`]) on a fixed local drive: `GetDriveTypeW` says its
/// root is fixed, and its letter is no `subst` drive standing for a share ([`is_share_target`]). A letter whose device
/// cannot be read is taken as no `subst` drive, as routing's facts take it ([`Facts::subst_target`]): a service's
/// session may be refused the lookup of a plain volume's letter, which `GetDriveTypeW` has already found fixed.
fn on_fixed_drive(folder: &str) -> bool {
    let Some(letter) = folder.chars().next().filter(|_| is_drive_path(folder)) else {
        return false;
    };
    let letter = letter.to_ascii_uppercase();
    is_fixed_local(drive_type(letter), gate_win32::device_target(letter).ok().flatten().as_deref())
}

/// Whether a drive whose root `GetDriveTypeW` calls `kind`, its letter standing for the device `target` (`None` for no
/// `subst` drive), is a fixed local drive: `kind` is fixed and `target` is no share ([`is_share_target`]).
fn is_fixed_local(kind: u32, target: Option<&str>) -> bool {
    kind == DRIVE_FIXED && !target.is_some_and(is_share_target)
}

/// Whether the device `target` a drive letter stands for ([`gate_win32::device_target`]) is a share: `\??\UNC\…`, as
/// a `subst` onto a share gives, or `\\…`.
fn is_share_target(target: &str) -> bool {
    let unc = r"\??\UNC\";
    target.get(..unc.len()).is_some_and(|start| start.eq_ignore_ascii_case(unc)) || target.starts_with(r"\\")
}

/// Whether `folder` is an absolute drive path: a letter, `:` and a separator, then at least one name, each separator
/// `\` or `/`, and every name one that means only itself ([`is_plain_name`]). A share, device or `\\?\` path is not,
/// nor is a drive-relative one.
fn is_drive_path(folder: &str) -> bool {
    let mut chars = folder.chars();
    let (Some(letter), Some(':'), Some('\\' | '/')) = (chars.next(), chars.next(), chars.next()) else {
        return false;
    };
    let names: Vec<&str> = chars.as_str().split(['\\', '/']).collect();
    letter.is_ascii_alphabetic() && names.iter().any(|name| !name.is_empty()) && names.iter().all(|name| is_plain_name(name))
}

/// Whether the path name `name` means only itself: it does not end in `.` or a space, which Win32 trims (so `.` and
/// `..` are refused too), and holds no `:`, which names an alternate data stream.
fn is_plain_name(name: &str) -> bool {
    !name.ends_with(['.', ' ']) && !name.contains(':')
}

/// A folder opened as itself, or what is there instead.
enum Held {
    Missing,
    /// A link or a junction: any name-surrogate reparse point.
    Link,
    NotFolder,
    Folder(fs::File),
}

/// The folder at `path` opened as itself ([`open_as_itself`]), a link or a junction not followed, or what is there
/// instead.
fn hold_folder(path: &Path) -> io::Result<Held> {
    let Some((folder, metadata)) = open_as_itself(path)? else {
        return Ok(Held::Missing);
    };
    Ok(if metadata.file_type().is_symlink() {
        Held::Link
    } else if metadata.is_dir() {
        Held::Folder(folder)
    } else {
        Held::NotFolder
    })
}

/// The bytes of the file `name` inside the folder object `folder` holds ([`gate_win32::open_beneath`]), opened as
/// itself and read by [`read_whole`]; `None` when there is no such file.
fn read_beneath(folder: &fs::File, name: &str) -> Result<Option<Vec<u8>>, String> {
    let opened = with_metadata(gate_win32::open_beneath(folder, name, Share::All)).map_err(|error| format!("cannot read it: {error}"))?;
    opened.map(|(file, metadata)| read_whole(file, &metadata)).transpose()
}

/// What `<root>\.gate` answers for its file `name` as it stands on disk: the file's bytes, `None` when there is no such
/// file, or its problem; or, told apart, a `.gate` that is not a folder. The declarations reader
/// ([`read_declaration`]) reads the repo's own files through this.
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
fn read_repo_file(root: &Path, name: &str) -> RepoRead {
    let held = match hold_folder(&root.join(".gate")) {
        Ok(held) => held,
        Err(error) => return RepoRead::File(Err(format!(".gate: cannot read it: {error}"))),
    };
    RepoRead::File(match held {
        Held::Missing => Ok(None),
        Held::NotFolder => return RepoRead::GateNotFolder,
        Held::Link => Err(".gate is a link or a junction".to_owned()),
        Held::Folder(gate_folder) => {
            let bytes = read_beneath(&gate_folder, name);
            drop(gate_folder);
            bytes
        }
    })
}

/// What the repo's own `.gate` answers for one of its files ([`read_repo_file`]).
enum RepoRead {
    /// `.gate` is there as something other than a folder: the repo's answer, with no file.
    GateNotFolder,
    /// The file's bytes; `None` when `.gate` is absent or is a folder without the file; else the file's problem.
    File(Result<Option<Vec<u8>>, String>),
}

/// Notes already said by this process.
static SAID_NOTES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The lines of `lines` this process has not said yet, each kept as said from now on: a declarations note is said once
/// a run, however many of its reads reach the same folder.
pub(crate) fn unsaid(lines: Vec<String>) -> Vec<String> {
    let mut said = SAID_NOTES.lock().unwrap_or_else(PoisonError::into_inner);
    lines
        .into_iter()
        .filter(|line| {
            if said.contains(line) {
                return false;
            }
            said.push(line.clone());
            true
        })
        .collect()
}

/// Whether `<root>\.gate` is there as something other than a folder, so no file of it can be read; `false` when it is
/// absent or is a folder. A link or a junction is refused by [`read_repo_file`] by name before this is asked.
pub(crate) fn gate_is_not_a_folder(root: &Path) -> bool {
    fs::symlink_metadata(root.join(".gate")).is_ok_and(|metadata| !metadata.is_dir())
}

/// The bytes of the file at `path`, opened as itself so a link or a junction there is not followed, and read as
/// [`read_repo_file`] reads a file of `.gate`: a link, a junction or a folder is the not-a-file problem, and a file
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

/// Whether the listed `file` is `path` or lies in the folder `path`.
fn lists_path(file: &[u8], path: &[u8]) -> bool {
    file.strip_prefix(path).is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
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
    use std::time::Duration;

    use gate_router::windows::WindowsChange;
    use gate_router::{CarriedSibling, Config, Facts, FolderId, GitignoredInput, LocalReason, Place, Route, SiblingFolder, SiblingRoot, Target};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    use super::{
        CHANGE_DEADLINE, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_REMOTE, GATE_FILE_NOT_A_FILE, HINT_GIT_LIMIT, Machine, Origin, SIBLING_GIT_LINK,
        SIBLING_LINK, SIBLING_NOT_A_FOLDER, Source, change_error, change_limit, checkout_of, hint_holds, holds_existing_path, is_fixed_local,
        is_share_target, judge_declarations, not_routed, nul_separated, parse_head, parse_place, read_declaration, read_siblings, sibling_folder,
        worktree_of,
    };
    use crate::remote::box_trees;
    use crate::remote::git;
    use crate::test_support::junction;
    use gate_args::TestSeams;
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

    /// The folder the place cases are asked of, spelled on a subst drive as a gate started there spells it.
    const FOLDER: &str = r"S:\src";

    /// A target of the worktree at `root`, its folder named `leaf`, in the work folder `k`, carrying each of
    /// `siblings`, a folder name and its root.
    fn target_at(root: &str, leaf: &str, siblings: &[(&str, &str)]) -> Target {
        let id = FolderId { volume: 0, file: [0; 16] };
        Target {
            builder: gate_router::Builder {
                alias: "builder".to_owned(),
                os: gate_router::BuilderOs::Windows,
                root: None,
                slots: 1,
                id: None,
                data: None,
            },
            place: Place {
                root: root.to_owned(),
                prefix: String::new(),
                checkout: leaf.to_owned(),
                folder: root.to_owned(),
            },
            repo: leaf.to_owned(),
            leaf: leaf.to_owned(),
            root_id: id,
            siblings: siblings
                .iter()
                .map(|(name, root)| CarriedSibling {
                    name: (*name).to_owned(),
                    root: (*root).to_owned(),
                    id,
                })
                .collect(),
            key: "k".to_owned(),
            inputs: Vec::new(),
        }
    }

    /// The pairs of each `(box tree, PC root)` as owned text.
    fn pairs(trees: &[(&str, &str)]) -> Vec<(String, String)> {
        trees.iter().map(|(tree, root)| ((*tree).to_owned(), (*root).to_owned())).collect()
    }

    /// Asserts a run of `target` on the ssh transport gets no box tree at all, so no path-map word, and the decision's
    /// notes say once that the PC root `root` holds `held`.
    fn assert_refused(target: &Target, root: &str, held: char) {
        let machine = Machine::at(None, Duration::from_secs(1), Duration::from_secs(1));
        assert_eq!(machine.box_trees(target), Vec::<(String, String)>::new(), "no tree, not one left out");
        let line = format!("gate: no PathMap word: the root {root} holds '{held}', which the builder's command line cannot carry");
        assert_eq!(machine.notes.take(), [line]);
    }

    #[test]
    fn the_placement_call_gets_the_whole_limit_once_git_exits_by_itself_and_the_drain_otherwise() {
        let folder = Path::new(FOLDER);
        let starved = Machine::at(None, Duration::from_mins(15), Duration::from_secs(2));
        assert_eq!(
            starved.place_call(folder).drain_once_exited(),
            Duration::from_mins(15),
            "a starved gate's readers get the whole git limit once git exits by itself"
        );
        assert_eq!(
            starved.place_call(folder).drain,
            Duration::from_secs(2),
            "a call its interrupt ends keeps the ordinary drain"
        );
        let longer = Machine::at(None, Duration::from_secs(2), Duration::from_secs(30));
        assert_eq!(
            longer.place_call(folder).drain_once_exited(),
            Duration::from_secs(30),
            "a drain above the limit still wins"
        );
    }

    #[test]
    fn an_ordinary_git_call_keeps_its_drain_once_git_exits_by_itself() {
        let machine = Machine::at(None, Duration::from_secs(60), Duration::from_secs(600));
        let call = machine.git_call(Path::new(FOLDER), &["ls-files"]);
        assert_eq!(call.exit_drain, None, "only a placement call sets a drain of its own after git's exit");
        assert_eq!(call.drain, Duration::from_secs(600));
        assert_eq!(
            call.drain_once_exited(),
            Duration::from_secs(600),
            "the drain an ordinary git call was given is kept after a clean exit too"
        );
    }

    #[test]
    fn the_hint_s_placement_call_keeps_the_ordinary_drain() {
        let hint = Machine::hint(None, &TestSeams::default());
        let call = hint.place_call(Path::new(FOLDER));
        assert_eq!(
            call.limit, HINT_GIT_LIMIT,
            "the hint holds its light slot, so its calls keep the hint's limit"
        );
        assert_eq!(call.drain, git::GitBounds::DEFAULT.drain, "the hint's readers get the seams' drain");
        assert_eq!(call.exit_drain, None, "the hint's placement call is built as an ordinary git call");
        assert_eq!(
            call.drain_once_exited(),
            git::GitBounds::DEFAULT.drain,
            "the hint keeps the ordinary drain once git exits by itself too"
        );
    }

    #[test]
    fn a_root_with_a_reordering_mark_is_named_escaped_in_the_note() {
        let machine = Machine::at(None, Duration::from_secs(1), Duration::from_secs(1));
        assert_eq!(
            machine.box_trees(&target_at("X:\\dev\\a\u{202e}b", "a\u{202e}b", &[])),
            Vec::<(String, String)>::new()
        );
        let line = r"gate: no PathMap word: the root X:\dev\a\u{202e}b holds '\u{202e}', which the builder's command line cannot carry";
        assert_eq!(machine.notes.take(), [line.to_owned()]);
    }

    #[test]
    fn a_root_with_a_space_gets_no_path_map() {
        assert_refused(&target_at(r"X:\dev\my app", "my app", &[]), r"X:\dev\my app", ' ');
    }

    #[test]
    fn a_sibling_root_with_a_semicolon_leaves_out_the_whole_path_map() {
        let target = target_at("X:/dev/app", "app", &[("lib", r"X:\dev\x;y")]);
        assert_refused(&target, r"X:\dev\x;y", ';');
    }

    #[test]
    fn a_root_with_a_comma_gets_no_path_map() {
        assert_refused(&target_at(r"X:\dev\notes,old", "notes,old", &[]), r"X:\dev\notes,old", ',');
    }

    #[test]
    fn a_root_with_an_equals_sign_gets_no_path_map() {
        assert_refused(&target_at(r"X:\dev\a=b", "a=b", &[]), r"X:\dev\a=b", '=');
    }

    #[test]
    fn a_root_with_a_percent_sign_gets_no_path_map() {
        assert_refused(&target_at(r"X:\dev\100%41x", "100%41x", &[]), r"X:\dev\100%41x", '%');
    }

    #[test]
    fn a_root_with_a_letter_outside_ascii_gets_no_path_map() {
        assert_refused(&target_at(r"X:\dev\café", "café", &[]), r"X:\dev\café", 'é');
    }

    #[test]
    fn a_root_with_a_colon_past_the_drive_gets_no_path_map() {
        assert_refused(&target_at(r"X:\dev\a:b", "a:b", &[]), r"X:\dev\a:b", ':');
    }

    #[test]
    fn a_verbatim_or_device_root_gets_no_path_map() {
        assert_refused(&target_at(r"\\?\X:\dev\app", "app", &[]), r"\\?\X:\dev\app", '?');
        assert_refused(&target_at(r"\\.\X:\dev\app", "app", &[]), r"\\.\X:\dev\app", '.');
    }

    #[test]
    fn a_pair_maps_its_main_tree_then_each_sibling_in_carried_order() {
        let target = target_at("X:/dev/app.wt/a", "a", &[("lib", "X:/dev/lib"), ("tools", "X:/dev/tools")]);
        assert_eq!(
            box_trees(None, &target),
            Ok(pairs(&[
                (r"D:\gate-remote\work\k\a", r"X:\dev\app.wt\a"),
                (r"D:\gate-remote\work\k\lib", r"X:\dev\lib"),
                (r"D:\gate-remote\work\k\tools", r"X:\dev\tools"),
            ]))
        );
        assert_eq!(
            box_trees(None, &target_at("X:/dev/app.wt/a", "a", &[])),
            Ok(pairs(&[(r"D:\gate-remote\work\k", r"X:\dev\app.wt\a")])),
            "a run with no sibling is checked out in the work folder itself"
        );
    }

    #[test]
    fn a_share_root_and_one_of_the_allowed_marks_get_their_trees() {
        let target = target_at(r"\\server\share\x", "x", &[("a.b_c-d~1", "X:/dev/a.b_c-d~1")]);
        assert_eq!(
            box_trees(None, &target),
            Ok(pairs(&[
                (r"D:\gate-remote\work\k\x", r"\\server\share\x"),
                (r"D:\gate-remote\work\k\a.b_c-d~1", r"X:\dev\a.b_c-d~1"),
            ]))
        );
    }

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

    /// Runs git with `args` in `folder`, failing the test when it fails.
    #[expect(clippy::disallowed_methods, reason = "a test's git setup takes no spawn lock")]
    fn git_in(folder: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(folder)
            .args(args)
            .status()
            .expect("git can be run");
        assert!(status.success(), "git {args:?} failed");
    }

    /// The place of a gate at the root of the worktree `root`.
    fn place_of(root: &Path) -> Place {
        let root = root.to_string_lossy().into_owned();
        Place {
            prefix: String::new(),
            checkout: "g".to_owned(),
            folder: root.clone(),
            root,
        }
    }

    /// Writes a file at the `/`-separated `path` below `root`, its folders made.
    fn put(root: &Path, path: &str) {
        let full = root.join(path);
        fs::create_dir_all(full.parent().expect("the file has a folder")).expect("the folder can be made");
        fs::write(full, "x").expect("the file can be written");
    }

    #[test]
    fn the_snapshot_carries_listed_files_and_folders_holding_one_and_nothing_ignored() {
        let (root, _removed) = scratch("snapshot");
        git_in(&root, &["init", "--quiet"]);
        for path in ["src/Duel.sln", "tests/a.cs", "bin/x", ".gitignore"] {
            put(&root, path);
        }
        fs::write(root.join(".gitignore"), "bin/\n").expect("the ignore file can be written");
        git_in(&root, &["add", "--", "src/Duel.sln"]);
        let machine = Machine::at(None, Duration::from_secs(60), Duration::from_secs(5));
        let asked = ["src/Duel.sln", "src", "tests", "bin/x", "nope"].map(str::to_owned);
        assert_eq!(machine.snapshot_carries(&place_of(&root), &asked), ["src/Duel.sln", "src", "tests"]);
    }

    /// The gitignored input `path` carrying each of `files`, a path and its size.
    fn carrying(path: &str, files: &[(&str, u64)]) -> GitignoredInput {
        GitignoredInput {
            path: path.to_owned(),
            files: Ok(files.iter().map(|(file, size)| ((*file).to_owned(), *size)).collect()),
        }
    }

    /// The gitignored input `path` that cannot be carried, for the reason `why`.
    fn unsent(path: &str, why: &str) -> GitignoredInput {
        GitignoredInput {
            path: path.to_owned(),
            files: Err(why.to_owned()),
        }
    }

    /// A worktree at a fresh scratch folder for the case `name` whose `.gitignore` is `ignore`, holding a file at each
    /// of `files`, and a machine to ask about it.
    fn ignoring_tree(name: &str, ignore: &str, files: &[&str]) -> (PathBuf, Removed, Machine<'static>) {
        let (root, removed) = scratch(name);
        git_in(&root, &["init", "--quiet"]);
        fs::write(root.join(".gitignore"), ignore).expect("the ignore file can be written");
        for path in files {
            put(&root, path);
        }
        (root, removed, Machine::at(None, Duration::from_secs(60), Duration::from_secs(5)))
    }

    /// Holds the folder `path` open sharing nothing, so no listing of it can be read while the handle lives.
    fn held_open(path: &Path) -> fs::File {
        fs::create_dir_all(path).expect("the folder can be made");
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("the folder can be held open sharing nothing")
    }

    #[test]
    fn no_check_ignore_runs_when_no_candidate_swept_a_file() {
        let (root, _removed) = scratch("gitignored-swept-nothing");
        // Every git call in the folder fails, so a candidate given back carrying nothing was given back without one.
        fs::write(root.join(".git"), "gitdir: missing\n").expect("a broken .git file can be written");
        let machine = Machine::at(None, Duration::from_secs(60), Duration::from_secs(5));
        let (empty, gone) = ("probe".to_owned(), "gone".to_owned());
        let swept = [(&empty, Ok(Vec::new())), (&gone, Err("why".to_owned()))];
        assert_eq!(machine.carried_ignored(&root, &swept), [carrying("probe", &[]), unsent("gone", "why")]);
    }

    #[test]
    fn a_tracked_candidate_is_never_swept() {
        let (root, _removed, machine) = ignoring_tree("gitignored-tracked", "*.log\n", &["src/A.csproj"]);
        git_in(&root, &["add", "--", "src/A.csproj"]);
        let held = held_open(&root.join("src").join("locked"));
        let found = machine.gitignored_inputs(&place_of(&root), &["src/A.csproj".to_owned(), "src".to_owned()], &[]);
        drop(held);
        assert_eq!(found, []);
        assert_eq!(*machine.notes.borrow(), Vec::<String>::new(), "no folder was walked, so none failed");
    }

    #[test]
    fn an_ignored_settings_file_carries_only_itself() {
        let files = [".tmp/probe/x.props", ".tmp/probe/Program.cs"];
        let (root, _removed, machine) = ignoring_tree("gitignored-settings", ".tmp/\n", &files);
        assert_eq!(
            machine.gitignored_inputs(&place_of(&root), &[".tmp/probe/x.props".to_owned()], &[]),
            [carrying(".tmp/probe/x.props", &[(".tmp/probe/x.props", 1)])]
        );
    }

    #[test]
    fn an_ignored_project_at_the_root_carries_only_itself() {
        let (root, _removed, machine) = ignoring_tree("gitignored-root", "A.csproj\nother.txt\n", &["A.csproj", "other.txt"]);
        assert_eq!(
            machine.gitignored_inputs(&place_of(&root), &["A.csproj".to_owned()], &[]),
            [carrying("A.csproj", &[("A.csproj", 1)])]
        );
    }

    #[test]
    fn a_candidate_in_a_nested_clone_stays_here_saying_why() {
        let (root, _removed, machine) = ignoring_tree("gitignored-nested", ".tmp/\n", &[".tmp/clone/P.csproj"]);
        git_in(&root.join(".tmp").join("clone"), &["init", "--quiet"]);
        assert_eq!(
            machine.gitignored_inputs(&place_of(&root), &[".tmp/clone/P.csproj".to_owned()], &[]),
            [unsent(".tmp/clone/P.csproj", "it is inside a nested repository")]
        );
    }

    #[test]
    fn a_candidate_whose_sweep_fails_stays_here_saying_why() {
        let (root, _removed, machine) = ignoring_tree("gitignored-unswept", ".tmp/\n", &[".tmp/probe/P.csproj"]);
        let held = held_open(&root.join(".tmp").join("probe").join("locked"));
        let found = machine.gitignored_inputs(&place_of(&root), &[".tmp/probe/P.csproj".to_owned()], &[]);
        drop(held);
        let [GitignoredInput { path, files: Err(why) }] = found.as_slice() else {
            panic!("the candidate cannot be carried: {found:?}");
        };
        assert_eq!(path, ".tmp/probe/P.csproj");
        assert!(why.contains("locked"), "the walk's own error names the folder: {why}");
    }

    #[test]
    fn a_candidate_under_a_junction_stays_here_saying_why() {
        let (root, _removed, machine) = ignoring_tree("gitignored-junction", ".tmp/\n", &[".tmp/real/P.csproj"]);
        junction(&root.join(".tmp").join("link"), &root.join(".tmp").join("real"));
        assert_eq!(
            machine.gitignored_inputs(&place_of(&root), &[".tmp/link/P.csproj".to_owned()], &[]),
            [unsent(".tmp/link/P.csproj", "the folder is a link or junction")]
        );
    }

    #[test]
    fn a_folder_candidate_lists_its_ignored_files_but_bin_and_obj() {
        let (root, _removed) = scratch("gitignored-folder");
        git_in(&root, &["init", "--quiet"]);
        fs::write(root.join(".gitignore"), "probe/*\n!probe/Readme.md\n").expect("the ignore file can be written");
        for path in [
            "probe/Probe.csproj",
            "probe/sub/deep.props",
            "probe/bin/y.dll",
            "probe/OBJ/x.json",
            "probe/src/Obj/z.json",
            "probe/keys.json",
            "probe/.env.local",
            "probe/nested/.git",
            "probe/nested/a.cs",
            "probe/Readme.md",
        ] {
            put(&root, path);
        }
        fs::write(root.join("probe").join("Program.cs"), "12345").expect("the file can be written");
        let machine = Machine::at(None, Duration::from_secs(60), Duration::from_secs(5));
        let globs = ["probe/keys.json".to_owned()];
        assert_eq!(
            machine.gitignored_inputs(&place_of(&root), &["probe".to_owned()], &globs),
            [carrying(
                "probe",
                &[("probe/Probe.csproj", 1), ("probe/Program.cs", 5), ("probe/sub/deep.props", 1)]
            )],
            "neither bin, obj, a glob's file, an env file, another repository's file nor a file git keeps is carried"
        );
    }

    #[test]
    fn a_file_candidate_carries_its_folder_s_ignored_files() {
        let (root, _removed) = scratch("gitignored-file");
        git_in(&root, &["init", "--quiet"]);
        fs::write(root.join(".gitignore"), ".tmp/\n").expect("the ignore file can be written");
        for path in [
            ".tmp/probe/Probe.csproj",
            ".tmp/probe/Program.cs",
            ".tmp/probe/obj/x.json",
            ".tmp/other/z.cs",
        ] {
            put(&root, path);
        }
        let machine = Machine::at(None, Duration::from_secs(60), Duration::from_secs(5));
        assert_eq!(
            machine.gitignored_inputs(&place_of(&root), &[".tmp/probe/Probe.csproj".to_owned()], &[]),
            [carrying(
                ".tmp/probe/Probe.csproj",
                &[(".tmp/probe/Probe.csproj", 1), (".tmp/probe/Program.cs", 1)]
            )]
        );
    }

    #[test]
    fn a_git_that_fails_carries_nothing() {
        let (root, _removed) = scratch("snapshot-not-a-repo");
        fs::write(root.join(".git"), "gitdir: missing\n").expect("a broken .git file can be written");
        put(&root, "a.txt");
        let machine = Machine::at(None, Duration::from_secs(60), Duration::from_secs(5));
        assert_eq!(machine.snapshot_carries(&place_of(&root), &["a.txt".to_owned()]), Vec::<String>::new());
        let notes = machine.notes.take();
        assert_eq!(notes.len(), 1, "one line says why: {notes:?}");
        assert!(notes[0].starts_with("gate: cannot tell what the snapshot carries: "), "{notes:?}");
    }

    #[test]
    fn a_test_whose_projects_cannot_be_read_is_windows_only_and_notes_why() {
        let (root, _removed) = scratch("windows-only-unreadable");
        let machine = Machine::at(None, Duration::from_secs(60), Duration::from_secs(5));
        assert!(machine.windows_only_tests(&place_of(&root), &["dotnet".to_owned(), "test".to_owned()]));
        let notes = machine.notes.take();
        assert_eq!(notes.len(), 1, "one line says why: {notes:?}");
        assert!(notes[0].starts_with("gate: kept off the macOS builder: "), "{notes:?}");
    }

    /// Commits every file of the repo at `root` with the message `message`, as a committer of the test's own, with no
    /// hook and no signing.
    fn commit_all(root: &Path, message: &str) {
        git_in(root, &["add", "--all"]);
        let identity = [
            "-c",
            "user.name=gate test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ];
        let commit = [
            "-c",
            "core.hooksPath=.no-hooks",
            "commit",
            "--quiet",
            "--allow-empty",
            "--message",
            message,
        ];
        git_in(root, &[&identity[..], &commit[..]].concat());
    }

    /// Makes `root` a repo whose `.gate/windows.toml` declares `paths`, its one commit `refs/remotes/origin/main`.
    fn change_repo(root: &Path, paths: &str) {
        git_in(root, &["init", "--quiet"]);
        put(root, ".gate/windows.toml");
        fs::write(root.join(".gate").join("windows.toml"), format!("paths = {paths}\n")).expect("the declaration can be written");
        commit_all(root, "base");
        git_in(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    }

    /// What the decision's machine reads of the change of the worktree at `root` carrying `siblings`.
    fn change_of(root: &Path, siblings: &[SiblingRoot]) -> WindowsChange {
        change_machine().windows_change(&place_of(root), siblings, None)
    }

    /// The decision's machine with its git calls, readers and change deadline under [`git::TEST_HANG_GUARD`], so a read
    /// on a loaded machine never reaches the production deadline.
    fn change_machine() -> Machine<'static> {
        Machine {
            change_deadline: git::TEST_HANG_GUARD,
            ..Machine::at(None, git::TEST_HANG_GUARD, git::TEST_HANG_GUARD)
        }
    }

    #[test]
    fn a_call_killed_at_the_time_left_of_the_deadline_says_the_deadline() {
        let left = Duration::from_millis(3);
        let late = || git::GitError::Failed(git::late_git_line(left));
        let deadline = Duration::from_secs(10);
        assert_eq!(change_error(late(), left, true, deadline), "its git calls did not answer within 10 s");
        assert_eq!(
            change_error(late(), left, false, deadline),
            "git did not answer within 3 ms",
            "its own limit"
        );
        assert_eq!(change_error(git::GitError::Failed("other".to_owned()), left, true, deadline), "other");
    }

    #[test]
    fn a_hint_reads_nothing_for_a_word_not_unicode_and_none_holds_for_an_unknown_change() {
        let machine = Machine::hint(None, &TestSeams::default());
        assert!(!hint_holds(false, &machine, |_| panic!("the decision is not asked")));
        assert!(hint_holds(true, &machine, |_| true), "a routed decision with a change told");
        assert!(!hint_holds(true, &machine, |_| false));
        let unknown = |machine: &Machine<'_>| {
            machine.change_unknown.set(true);
            true
        };
        assert!(
            !hint_holds(true, &Machine::hint(None, &TestSeams::default()), unknown),
            "a routed decision, change unknown"
        );
    }

    #[test]
    fn a_head_git_cannot_read_says_git_failure_line_and_an_unborn_one_names_no_commit() {
        let (broken, _removed) = scratch("change-broken-git");
        put(&broken, ".gate/windows.toml");
        fs::write(broken.join(".gate").join("windows.toml"), "paths = ['a/*']\n").expect("the declaration can be written");
        fs::write(broken.join(".git"), "gitdir: missing\n").expect("a broken .git file can be written");
        let WindowsChange::Unknown { why } = change_of(&broken, &[]) else {
            panic!("a broken .git cannot be read");
        };
        assert!(why.starts_with("git --no-optional-locks rev-parse --is-shallow-repository"), "{why}");
        assert!(why.contains("(status 128)"), "{why}");

        let (unborn, _removed_unborn) = scratch("change-unborn");
        git_in(&unborn, &["init", "--quiet"]);
        put(&unborn, ".gate/windows.toml");
        fs::write(unborn.join(".gate").join("windows.toml"), "paths = ['a/*']\n").expect("the declaration can be written");
        assert_eq!(change_of(&unborn, &[]), unknown(format!("HEAD names no commit in {}", unborn.display())));
    }

    #[test]
    fn a_plain_origin_head_naming_a_commit_is_the_base_with_no_origin_main() {
        let (root, _removed) = scratch("change-plain-origin-head");
        change_repo(&root, "['tools/Hook/*']");
        git_in(&root, &["update-ref", "refs/remotes/origin/HEAD", "HEAD"]);
        git_in(&root, &["update-ref", "-d", "refs/remotes/origin/main"]);
        put(&root, "tools/Hook/a.cs");
        commit_all(&root, "hook");
        assert_eq!(
            change_of(&root, &[]),
            WindowsChange::Touched {
                path: "tools/Hook/a.cs".to_owned()
            }
        );
    }

    #[test]
    fn a_listing_past_the_cap_ends_git_and_cannot_be_told() {
        let (root, _removed) = scratch("change-over-cap");
        change_repo(&root, "['tools/Hook/*']");
        put(&root, "docs/a-long-untracked-name.md");
        let machine = Machine {
            listing_cap: 8,
            ..change_machine()
        };
        let WindowsChange::Unknown { why } = machine.windows_change(&place_of(&root), &[], None) else {
            panic!("a listing past the cap cannot be judged");
        };
        assert!(why.starts_with("git --no-optional-locks ls-files"), "{why}");
        assert!(why.ends_with("its output passed 8 bytes"), "{why}");
    }

    /// A change that cannot be told, for `why`.
    fn unknown(why: String) -> WindowsChange {
        WindowsChange::Unknown { why }
    }

    /// The sibling `name` at `root`.
    fn sibling_at(name: &str, root: &Path) -> SiblingRoot {
        SiblingRoot {
            name: name.to_owned(),
            root: root.to_string_lossy().into_owned(),
        }
    }

    #[test]
    fn the_limit_of_a_change_call_is_what_is_left_and_never_above_the_per_call_limit() {
        let per_call = Duration::from_secs(60);
        assert_eq!(change_limit(Duration::from_secs(3), per_call), Some(Duration::from_secs(3)));
        assert_eq!(change_limit(Duration::from_secs(90), per_call), Some(per_call));
        assert_eq!(change_limit(Duration::ZERO, per_call), None, "the deadline passed");
    }

    #[test]
    fn the_hint_reads_the_change_under_its_own_limit_and_an_unknown_one_means_no_hint() {
        let seams = TestSeams::default();
        let hint = Machine::hint(None, &seams);
        assert_eq!((hint.change_deadline, hint.git_limit), (HINT_GIT_LIMIT, HINT_GIT_LIMIT));
        assert_eq!(Machine::of(None, &seams).change_deadline, CHANGE_DEADLINE);
        let (root, _removed) = scratch("change-hint");
        git_in(&root, &["init", "--quiet"]);
        put(&root, ".gate/windows.toml");
        fs::write(root.join(".gate").join("windows.toml"), "paths = ['a/*']\n").expect("the declaration can be written");
        let machine = Machine::hint(None, &seams);
        assert!(matches!(
            machine.windows_change(&place_of(&root), &[], None),
            WindowsChange::Unknown { .. }
        ));
        assert!(machine.change_unknown.get(), "the hint's decision gives no hint");
    }

    #[test]
    fn a_detached_head_a_shallow_clone_and_no_merge_base_are_unknown_saying_so() {
        let (root, _removed) = scratch("change-detached");
        change_repo(&root, "['a/*']");
        git_in(&root, &["checkout", "--quiet", "--detach"]);
        assert_eq!(change_of(&root, &[]), unknown(format!("HEAD is detached in {}", root.display())));

        let (from, _removed_from) = scratch("change-shallow-from");
        change_repo(&from, "['a/*']");
        commit_all(&from, "second");
        let (shallow, _removed_shallow) = scratch("change-shallow");
        let url = format!("file:///{}", from.to_string_lossy().replace('\\', "/"));
        let into = shallow.to_string_lossy().into_owned();
        git_in(&from, &["clone", "--quiet", "--depth", "1", &url, &into]);
        assert_eq!(change_of(&shallow, &[]), unknown(format!("{} is a shallow clone", shallow.display())));

        let (unrelated, _removed_unrelated) = scratch("change-unrelated");
        change_repo(&unrelated, "['a/*']");
        git_in(&unrelated, &["branch", "--move", "--force", "work"]);
        git_in(&unrelated, &["checkout", "--quiet", "--orphan", "lone"]);
        commit_all(&unrelated, "lone");
        git_in(&unrelated, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git_in(&unrelated, &["checkout", "--quiet", "work"]);
        assert_eq!(
            change_of(&unrelated, &[]),
            unknown(format!("HEAD and refs/remotes/origin/main have no merge-base in {}", unrelated.display()))
        );
    }

    #[test]
    fn a_dangling_origin_head_falls_back_to_origin_main() {
        let (root, _removed) = scratch("change-dangling-origin-head");
        change_repo(&root, "['tools/Hook/*']");
        git_in(&root, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/pruned"]);
        put(&root, "tools/Hook/a.cs");
        commit_all(&root, "hook");
        assert_eq!(
            change_of(&root, &[]),
            WindowsChange::Touched {
                path: "tools/Hook/a.cs".to_owned()
            }
        );
    }

    #[test]
    fn a_sibling_only_file_reads_the_sibling_alone() {
        let (root, _removed) = scratch("change-sibling-root");
        git_in(&root, &["init", "--quiet"]);
        put(&root, ".gate/windows.toml");
        fs::write(root.join(".gate").join("windows.toml"), "paths = ['../s/tools/*']\n").expect("the declaration can be written");

        let (sibling, _removed_sibling) = scratch("change-sibling");
        change_repo(&sibling, "[]");
        put(&sibling, "tools/a.cs");
        commit_all(&sibling, "tools");
        assert_eq!(
            change_of(&root, &[sibling_at("s", &sibling)]),
            WindowsChange::Touched {
                path: "../s/tools/a.cs".to_owned()
            },
            "the root, with no commit and no origin, is never read"
        );
    }

    #[test]
    fn a_listed_path_that_is_not_utf8_cannot_be_judged() {
        let root = Path::new("X:/r");
        assert_eq!(nul_separated(b"a.cs\0b/c.cs\0", root), Ok(vec!["a.cs".to_owned(), "b/c.cs".to_owned()]));
        assert_eq!(
            nul_separated(b"a.cs\0\xff.cs\0", root),
            Err("git listed a path that is not valid UTF-8 in X:/r: \u{fffd}.cs".to_owned())
        );
    }

    #[test]
    fn the_snapshot_carries_no_path_written_in_another_letter_case() {
        let (root, _removed) = scratch("snapshot-case");
        git_in(&root, &["init", "--quiet"]);
        for path in ["src/Duel.sln", "src/a.props", "tests/a.cs"] {
            put(&root, path);
        }
        git_in(&root, &["add", "--", "src/Duel.sln", "src/a.props"]);
        let machine = Machine::at(None, Duration::from_secs(60), Duration::from_secs(5));
        let place = place_of(&root);
        let carried = |asked: &[&str]| machine.snapshot_carries(&place, &asked.iter().map(|path| (*path).to_owned()).collect::<Vec<_>>());
        assert_eq!(
            carried(&["Src/Duel.sln"]),
            Vec::<String>::new(),
            "a file below a wrong-case folder, alone"
        );
        assert_eq!(
            carried(&["Src/Duel.sln", "Src/a.props"]),
            Vec::<String>::new(),
            "two sharing a wrong-case folder"
        );
        assert_eq!(
            carried(&["Tests/a.cs"]),
            Vec::<String>::new(),
            "an untracked file below a wrong-case folder"
        );
        assert_eq!(carried(&["SRC", "TESTS"]), Vec::<String>::new(), "wrong-case folders");
        assert_eq!(carried(&["src/Duel.sln", "tests"]), ["src/Duel.sln", "tests"]);
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
        let read_siblings = |root: &Path| read_siblings(read_declaration(root, "g", None, "siblings.toml").0);
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

    /// A routing file whose `declarations` names `folder` for the repo `g`.
    fn naming(folder: &str) -> Config {
        let text = serde_json::json!({"host": "builder-1", "repos": {"g": ["x"]}, "declarations": {"G": folder}}).to_string();
        Config::parse(&text).expect("the routing file is usable")
    }

    /// A fresh scratch folder for the case `name`, removed when the returned guard drops.
    fn scratch(name: &str) -> (PathBuf, Removed) {
        let folder = crate::test_support::temp_dir().join(format!("gate-route-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        fs::create_dir_all(&folder).expect("the test folder can be made");
        (folder.clone(), Removed(vec![folder]))
    }

    #[test]
    fn the_repo_s_own_file_wins_and_the_declarations_folder_is_read_only_when_the_repo_has_none() {
        let (base, _removed) = scratch("declarations");
        let root = base.join("repo");
        let pc = base.join("pc");
        fs::create_dir_all(root.join(".gate")).expect("the .gate folder can be made");
        fs::create_dir_all(&pc).expect("the declarations folder can be made");
        fs::write(pc.join("siblings.toml"), "siblings = ['pc']\n").expect("the PC copy can be written");
        let config = naming(&pc.to_string_lossy());
        let read = |name: &str| read_declaration(&root, "g", Some(&config), name);
        let from_pc = |name: &str| Source {
            origin: Origin::Declarations(pc.join(name)),
            unusable: None,
            shadows: None,
        };

        assert_eq!(
            read("siblings.toml"),
            (Ok(Some(b"siblings = ['pc']\n".to_vec())), from_pc("siblings.toml")),
            "no repo file"
        );
        assert_eq!(read("desktop.toml"), (Ok(None), Source::REPO), "neither place has the file");
        assert_eq!(
            read_declaration(&root, "other", Some(&config), "siblings.toml"),
            (Ok(None), Source::REPO),
            "a repo the declarations do not name"
        );
        assert_eq!(
            read_declaration(&root, "g", None, "siblings.toml"),
            (Ok(None), Source::REPO),
            "no routing file"
        );

        fs::write(root.join(".gate").join("siblings.toml"), "siblings = ['repo']\n").expect("the repo's file can be written");
        fs::write(root.join(".gate").join("desktop.toml"), "patterns = []\n").expect("the repo's file can be written");
        let shadowing = Source {
            shadows: Some(pc.join("siblings.toml")),
            ..Source::REPO
        };
        assert_eq!(
            read("siblings.toml"),
            (Ok(Some(b"siblings = ['repo']\n".to_vec())), shadowing.clone()),
            "the repo's file wins, shadowing the PC copy"
        );
        assert_eq!(
            shadowing.log_lines("g", "siblings.toml"),
            [format!("gate: .gate/siblings.toml shadows {}", pc.join("siblings.toml").display())]
        );
        assert_eq!(
            read("desktop.toml"),
            (Ok(Some(b"patterns = []\n".to_vec())), Source::REPO),
            "a repo file with no PC copy shadows nothing"
        );
        fs::remove_file(root.join(".gate").join("desktop.toml")).expect("the repo's file can be removed");
        fs::remove_file(root.join(".gate").join("siblings.toml")).expect("the repo's file can be removed");
        fs::create_dir(root.join(".gate").join("siblings.toml")).expect("a folder named like the file can be made");
        assert_eq!(
            read("siblings.toml"),
            (Err(GATE_FILE_NOT_A_FILE.to_owned()), shadowing.clone()),
            "a problem of the repo's file never falls back, and still notes the copy it shadows"
        );

        fs::write(pc.join("warmup.toml"), "x".repeat(64 * 1024 + 1)).expect("a big PC copy can be written");
        assert_eq!(
            read("warmup.toml"),
            (Err("is larger than 64 KiB".to_owned()), from_pc("warmup.toml")),
            "a PC copy's problem is its own, at its path"
        );
        assert_eq!(
            from_pc("warmup.toml").log_lines("g", "warmup.toml"),
            [format!("gate: .gate/warmup.toml read from {}", pc.join("warmup.toml").display())]
        );
        assert_eq!(from_pc("warmup.toml").said("warmup.toml"), pc.join("warmup.toml").to_string_lossy());
        assert_eq!(Source::REPO.said("warmup.toml"), ".gate/warmup.toml");

        fs::remove_dir_all(root.join(".gate")).expect("the .gate folder can be removed");
        fs::write(root.join(".gate"), "not a folder\n").expect("a file named .gate can be written");
        assert_eq!(
            read("siblings.toml"),
            (Ok(None), Source::REPO),
            "a .gate that is not a folder is the repo's answer, with no fallback"
        );
    }

    #[test]
    fn an_unusable_declarations_folder_reads_nothing_and_says_why() {
        let (base, _removed) = scratch("declarations-unusable");
        let root = base.join("repo");
        fs::create_dir_all(&root).expect("the worktree can be made");
        let file = base.join("file");
        fs::write(&file, "x").expect("a file can be written");
        let target = base.join("target");
        fs::create_dir_all(&target).expect("a junction's target can be made");
        fs::write(target.join("siblings.toml"), "siblings = ['b']\n").expect("the target's file can be written");
        let linked = base.join("linked");
        junction(&linked, &target);
        let missing = base.join("missing");
        let at = |path: &Path| path.to_string_lossy().into_owned();
        let not_fixed = "is not a folder on a fixed local drive";
        // SAFETY: GetLogicalDrives takes no arguments and only reads the drive table.
        let drives = unsafe { GetLogicalDrives() };
        let absent = (b'D'..=b'Z')
            .find(|letter| drives & (1 << (letter - b'A')) == 0)
            .map(char::from)
            .expect("a drive letter is free");
        let cases = [
            (r"notes\gate".to_owned(), not_fixed),
            ("C:".to_owned(), not_fixed),
            (r"C:\".to_owned(), not_fixed),
            (r"\\host\share\gate".to_owned(), not_fixed),
            (format!(r"\\?\{}", at(&target)), not_fixed),
            (format!(r"{}\..\target", at(&missing)), not_fixed),
            (format!(r"{}\.", at(&target)), not_fixed),
            (format!("{}.", at(&target)), not_fixed),
            (format!("{} ", at(&target)), not_fixed),
            (format!(r"{}\..\target ", at(&missing)), not_fixed),
            (format!("{}:stream", at(&target)), not_fixed),
            (format!(r"{absent}:\gate"), not_fixed),
            (at(&missing), "does not exist"),
            (at(&file), "is not a folder"),
            (at(&linked), "is a link or a junction"),
        ];
        for (folder, why) in cases {
            let unusable = format!("'{folder}' {why}");
            let (read, source) = read_declaration(&root, "g", Some(&naming(&folder)), "siblings.toml");
            assert_eq!(read, Ok(None), "{folder}");
            assert_eq!(
                source,
                Source {
                    unusable: Some(unusable.clone()),
                    ..Source::REPO
                },
                "{folder}"
            );
            assert_eq!(
                source.log_lines("g", "siblings.toml"),
                [format!("gate: declarations for g not read: {unusable}")],
                "{folder}"
            );
        }
        let bad = Config::parse(r#"{"host": "builder-1", "repos": {}, "declarations": ["x"]}"#).expect("usable");
        assert_eq!(
            read_declaration(&root, "g", Some(&bad), "siblings.toml").1.unusable.as_deref(),
            Some("remote.json's declarations cannot be used: declarations must be a JSON object"),
            "a declarations value that cannot be used"
        );
        fs::create_dir_all(root.join(".gate")).expect("the .gate folder can be made");
        fs::write(root.join(".gate").join("siblings.toml"), "siblings = ['a']\n").expect("the repo's file can be written");
        let config = naming(&at(&missing));
        assert_eq!(
            read_declaration(&root, "g", Some(&config), "siblings.toml").1,
            Source::REPO,
            "a read of a repo with its own file says nothing of the folder"
        );
        assert_eq!(
            judge_declarations("g", Some(&config)),
            Some(format!("'{}' does not exist", at(&missing))),
            "the entry is judged whether or not a read reaches it"
        );
        assert_eq!(judge_declarations("g", Some(&naming(&at(&target)))), None, "a usable folder");
        assert_eq!(judge_declarations("other", Some(&config)), None, "a repo the declarations do not name");
    }

    #[test]
    fn a_subst_drive_onto_a_share_is_a_share() {
        assert!(is_share_target(r"\??\UNC\host\share\gate"));
        assert!(is_share_target(r"\??\unc\host\share"));
        assert!(is_share_target(r"\\host\share"));
        assert!(!is_share_target(r"\??\X:\dev"));
        assert!(!is_share_target(r"\Device\HarddiskVolume3"));
        assert!(!is_share_target(r"\??\UN"));
    }

    #[test]
    fn only_a_fixed_drive_standing_for_no_share_is_a_fixed_local_drive() {
        /// What `GetDriveTypeW` says of a removable drive.
        const DRIVE_REMOVABLE: u32 = 2;
        assert!(is_fixed_local(DRIVE_FIXED, None), "a plain fixed volume");
        assert!(is_fixed_local(DRIVE_FIXED, Some(r"\??\C:\dev")), "a subst onto a local folder");
        assert!(!is_fixed_local(DRIVE_REMOTE, None), "a network drive");
        assert!(!is_fixed_local(DRIVE_REMOVABLE, None), "a removable drive");
        assert!(!is_fixed_local(DRIVE_NO_ROOT_DIR, None), "no volume at the root");
        assert!(!is_fixed_local(DRIVE_FIXED, Some(r"\??\UNC\host\share\gate")), "a subst onto a share");
        assert!(!is_fixed_local(DRIVE_FIXED, Some(r"\\host\share")), "a device standing for a share");
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
    fn only_the_local_flag_another_worktree_and_the_local_file_write_not_routed() {
        let flag = Route::Local {
            reason: Some(LocalReason::LocalFlag),
        };
        let declared = Route::Local {
            reason: Some(LocalReason::DeclaredLocal {
                file: ".gate/local.toml".to_owned(),
                pattern: "x*".to_owned(),
            }),
        };
        let unusable = Route::Local {
            reason: Some(LocalReason::LocalFile {
                file: ".gate/local.toml".to_owned(),
                problem: "x".to_owned(),
            }),
        };
        let desktop = Route::Local {
            reason: Some(LocalReason::DesktopFile {
                file: ".gate/desktop.toml".to_owned(),
                problem: "x".to_owned(),
            }),
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
        assert_eq!(not_routed(&declared).as_deref(), Some("declared local"));
        assert_eq!(
            not_routed(&unusable).as_deref(),
            Some("declared local"),
            "a file the gate cannot use keeps it here too"
        );
        assert_eq!(not_routed(&desktop), None);
        assert_eq!(not_routed(&outside), None, "an outside word in no other worktree writes none");
        assert_eq!(not_routed(&failed), None);
        assert_eq!(not_routed(&Route::Local { reason: None }), None);
    }
}
