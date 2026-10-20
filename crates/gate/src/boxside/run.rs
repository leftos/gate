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
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use gate_args::{BoxSeams, EnvRefusal, env_refusal, is_godot_console, over_cap, repeated_name, shown_name};
use gate_watchdog::{Kill, Pump, is_wsl_command};
use gate_win32::{
    Donor, EnvironmentBlock, Job, LaunchError, NamedEvent, PipedConsole, PipedLaunch, PipedSuspended, RestrictedToken, Running, Utf8Console,
};

use crate::answer::USAGE_STATUS;
use crate::boxside::boot;
use crate::boxside::ceiling::{Ceiling, KILLED, Spec};
use crate::boxside::forward::{ClientGone, DRAIN, Forwarders, OutputLock};
use crate::boxside::heartbeat;
use crate::boxside::inputs;
use crate::boxside::lfs::{self, RunLfs};
use crate::boxside::msys;
use crate::boxside::outputs::{self, Asked};
use crate::boxside::packages;
use crate::boxside::payload::{self, PairSibling, Payload, bare_stem, escaped, ordinal_upper, trees};
use crate::boxside::preflight::{self, Failed, GIT_REDIRECTS, Git, GitFailure, Unready, exit_code, git_failed};
use crate::boxside::probe;
use crate::boxside::stop::{Ending, STOPPED, stop_event_name, terminate_and_wait};
use crate::boxside::tools;
use crate::console::Console;
use crate::environment::{BOX_EXE_VARIABLE, BOX_JOB_VARIABLE, Variables};
use crate::outputs::manifest::{Listing, Manifest};
use crate::remote::{Refusal, refused_line, started_line};
use crate::resolve::{self, Environment};
use crate::run::{ERROR_FILE_NOT_FOUND, Written, end_leftovers, find_program, written};

mod desktop_run;

use desktop_run::DesktopEnd;

/// The status of a run refused: another run holds the work folder, the work folder's dotnet tools could not be
/// restored, or the run asked for the builder's desktop and could not have it.
pub(super) const REFUSED_STATUS: u32 = 75;
/// The status of a run the box failed on its own, and of a passed command whose tree could not be committed back.
pub(super) const FAILED_STATUS: u32 = 1;
/// The status of a command that could not be started.
const NOT_STARTED: u32 = 127;
/// The status of a run whose client closed the connection: the box stopped the job and exits as a lost connection does.
const CLOSED_STATUS: u32 = 255;

/// The git step that deletes a repo's back ref ([`WorkGit::clear_back_ref`]).
const BACK_REF_RESET: &str = "back ref reset";
/// The most bytes of standard output the LFS post-check's quiet checkout may print.
const RECHECKOUT_OUTPUT: usize = 64 * 1024;
/// The pathspec of every `.gitattributes` file at any depth, whatever the case of its name, as NTFS opens a file named
/// `.GitAttributes` as a folder's attributes file.
const ATTRIBUTES_FILES: &str = ":(glob,icase)**/.gitattributes";

/// Why a run's checkout did not end with its snapshot checked out.
enum NotCheckedOut {
    /// A git step failed, or the client went, as the line says.
    Failed(Failed),
    /// The run's LFS content is not what its snapshot needs, for this reason: refused `lfs`, so the PC runs it.
    Refused(String),
    /// The run's LFS scratch folder could not be made, for this reason: refused `lfs`, so the PC runs it.
    NoScratch(String),
}

impl From<Failed> for NotCheckedOut {
    fn from(failed: Failed) -> NotCheckedOut {
        NotCheckedOut::Failed(failed)
    }
}
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
pub(super) const LAYOUT_CHANGING: &str = "changing";
/// What the layout record says of a run of one repo's layout, checked out in the work folder itself; no record says the
/// same.
pub(super) const LAYOUT_SINGLE: &str = "single";
/// The warm record of the work folder `work`, `{work}.warm`: the hash of the warm-up file whose commands last passed
/// in it, written once they all did and removed whenever the folder's state changes under it.
pub(super) fn warm_record(work: &str) -> String {
    format!("{work}{WARM_SUFFIX}")
}

/// What follows the work folder's path in its warm record's ([`warm_record`]).
const WARM_SUFFIX: &str = ".warm";
/// What follows the work folder's path in its interrupted-command record's ([`command_record`]).
const COMMAND_SUFFIX: &str = ".command";
/// What follows the interrupted-command record's name in the empty record made beside it to replace one that cannot
/// be read ([`renewed_empty`]).
const RENEWING_SUFFIX: &str = ".new";
/// The SID of the Administrators group, which owns what an elevated box makes.
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

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
    /// Each command's words, its program first: the one command of a run, or each of a warm-up's in order.
    commands: Vec<Vec<String>>,
    /// The warm-up file's hash a warm-up records in `{Work}.warm` once every command passed; `None` for a run.
    warm_hash: Option<&'a str>,
    subfolder: &'a str,
    identity: Vec<(String, String)>,
    /// The caller's variables for the command, `Env`: each a name and a value, set over the box's own environment and
    /// under the gate's own settings; none when the payload gives none.
    env: Vec<(String, String)>,
    /// The version of the Godot install the run uses, `GodotVersion`, `major.minor.patch`; `None` when none is chosen.
    godot: Option<&'a str>,
    /// The Unreal Engine install the run's `UnrealVersion` names, resolved once as the request is read
    /// ([`resolved_engine`]) and handed to every command alike: `Ok(None)` when none is chosen, `Err` with the line its
    /// commands fail to start with when it does not resolve.
    engine: Result<Option<PathBuf>, String>,
    /// The names `EngineEnv` sets to the root of the chosen engine ([`with_unreal`]), as the repo's toolchain manifest
    /// `[env]` names them; none when the payload gives none. Every process the box starts for the run's repo loses the
    /// values it inherited for them ([`box_variables`]).
    pub(super) engine_env: Vec<String>,
    /// The folders the run writes ([`preflight::refused_folders`]): no folder a command runs from may lie inside one,
    /// neither the chosen engine's ([`resolved_engine`]) nor Git's `bin` ([`with_git_bin_first`]).
    refused: Vec<PathBuf>,
    /// The outputs the run brings back, `None` when it has no output globs.
    outputs: Option<Asked<'a>>,
    /// The run's `Nonce`, which its start marker and its refusal tags name.
    pub(super) nonce: &'a str,
    /// The run's ceiling in whole seconds of load-adjusted time, `None` for a run the box gives none.
    timeout: Option<u64>,
    /// Whether the run asks for the builder's desktop, its `Desktop` key; a run without one runs on the box's own
    /// session.
    desktop: bool,
    /// How long the run may wait for the builder's desktop, in whole seconds; `None` on a run that does not ask for
    /// one, whatever its `DesktopWait` said.
    desktop_wait: Option<u64>,
    /// The PC's name for the builder, which the box's kill line names, empty when the payload gives none.
    host: &'a str,
    /// The digest of the inputs manifest the client installed the run's inputs from, `None` when it sent none.
    inputs_digest: Option<&'a str>,
    /// The digest of the manifest of the PC's build the client sent the work folder, `SentDigest`, `None` when it sent
    /// none.
    sent_digest: Option<&'a str>,
    /// The digest of the LFS manifest of the objects the snapshot's pointers name, `LfsDigest`, `None` when the client
    /// sent none. Past the checkout's cover check, a run has one exactly when its snapshot holds LFS pointers.
    lfs_digest: Option<&'a str>,
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
        // A run of the builder's desktop has to say how long it may wait for one; a wait no run asks for is ignored,
        // whatever it says.
        let desktop = payload.desktop()?;
        let desktop_wait = if desktop {
            Some(payload.desktop_wait()?.ok_or_else(|| "gate: box payload lacks DesktopWait".to_owned())?)
        } else {
            None
        };
        let bare = payload.path("Bare")?;
        let lock = payload.beside_work("Lock", work, "lock")?;
        let index = payload.beside_work("Index", work, "index")?;
        let job = payload.job(key)?;
        let commands = payload.commands()?;
        // A warm-up's commands run on the box's own session alone.
        if desktop && commands.warm_hash.is_some() {
            return Err("gate: box payload asks a warm-up for the desktop".to_owned());
        }
        let subfolder = payload.optional_text("Subfolder")?;
        let identity = identity(payload)?;
        let env = caller_env(payload)?;
        let godot = payload.godot_version()?;
        let refused = preflight::refused_folders(payload, work);
        // Before the engine is resolved, so a malformed one is refused before any engine folder is probed.
        let engine_env = payload.engine_env()?;
        let engine = resolved_engine(payload.unreal_version()?, &refused);
        let inputs_digest = payload.inputs_digest()?;
        let sent_digest = payload.sent_digest()?;
        let lfs_digest = payload.lfs_digest()?;
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
            commands: commands.words,
            warm_hash: commands.warm_hash,
            subfolder,
            identity,
            env,
            godot,
            engine,
            engine_env,
            refused,
            outputs,
            nonce,
            timeout,
            desktop,
            desktop_wait,
            host,
            inputs_digest,
            sent_digest,
            lfs_digest,
        })
    }

    /// The main repo's tree, which the command's subfolder, the tool manifests and the outputs are read in.
    pub(super) fn tree(&self) -> &str {
        &self.main.tree
    }

    /// Each command's words, its program first, as the payload gives them: one command for a run, each of a warm-up's
    /// in order.
    pub(super) fn commands(&self) -> &[Vec<String>] {
        &self.commands
    }

    /// Every repo of the run, the main one first.
    fn repos(&self) -> impl Iterator<Item = &Repo<'_>> {
        std::iter::once(&self.main).chain(&self.siblings)
    }

    /// The work folder and the repos the run lays out in it.
    fn laid(&self) -> Laid<'_> {
        Laid {
            work: self.work,
            key: self.key,
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
    /// The payload's key, which names the work folder's jobs ([`payload::is_job_name`]); the work folder's last name
    /// matches it only without regard to case.
    key: &'r str,
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
        let siblings = self.siblings.iter().filter_map(|repo| repo.name.as_deref()).collect();
        layout_text(self.main.name.as_deref(), siblings)
    }
}

/// The layout a layout record names for a run whose main repo's leaf is `leaf` and whose siblings are `siblings`
/// ([`Laid::layout`]): [`LAYOUT_SINGLE`] without a leaf, else `pair`, the leaf, then the siblings' names sorted without
/// regard to ASCII case, one space before each.
pub(super) fn layout_text(leaf: Option<&str>, mut siblings: Vec<&str>) -> String {
    let Some(leaf) = leaf else {
        return LAYOUT_SINGLE.to_owned();
    };
    siblings.sort_by_key(|name| name.to_ascii_lowercase());
    format!("pair {leaf} {}", siblings.join(" "))
}

/// Lays out the work folder `asked` holds for one repo, its bare repository, as a run does before its checkout
/// ([`WorkGit::lay_out`]), under the work folder's lock file, which the caller holds; git's output is forwarded as a
/// run's is. The held work folder; else `Err` with the status, its lines said: a failing git step's, or 75 for a work
/// folder that could not be laid out, its `layout` tag and line said as a run says them.
pub(super) fn lay_out_alone(asked: &inputs::WorkLock<'_>, console: &mut Console) -> Result<File, u32> {
    let main = Repo::new(None, asked.work.to_owned(), asked.bare, "");
    let laid = Laid {
        work: asked.work,
        key: asked.key,
        main: &main,
        siblings: &[],
    };
    let not = match WorkGit::ready(asked.lock, &OutputLock::default(), &ClientGone::default()).lay_out(&laid, console) {
        Ok(held) => return Ok(held),
        Err(not) => not,
    };
    match not.refusal() {
        Ok(why) => {
            console.err(&refused_line(asked.nonce, Refusal::Layout));
            console.err(&crate::remote::layout_line(repo_name_of(asked.bare), &escaped(&why)));
            Err(REFUSED_STATUS)
        }
        Err(failed) => Err(said(console, &failed)),
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
    let why = match inputs::unmatched(inputs::Book::Inputs, request.work, held, sent) {
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

/// Checks the run's `SentDigest`, when it has one, against the work folder `held`'s sent-build record and the copies
/// it holds, and rewrites the tree's exclude file from both records ([`inputs::unmatched`]), once the work folder is
/// laid out and cleared and its inputs checked: a record missing, unreadable or of another digest, a copy changed or
/// gone, or an exclude file that cannot be written refuses the run `sent`, its tag and line said under the box's
/// output lock, so the PC runs it again without its `--no-build`; 75. A folder moved aside from the exclude file's
/// name is said, under the lock too.
fn sent_matched(request: &Request<'_>, held: &File, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
    let Some(sent) = request.sent_digest else {
        return Ok(());
    };
    let why = match inputs::unmatched(inputs::Book::SentBuild, request.work, held, sent) {
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
    console.err(&refused_line(request.nonce, Refusal::Sent));
    console.err(&format!(
        "gate: remote build sent to work folder {} is not the one there: {}",
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

/// The payload's `Env`: the caller's variables for the command, each pair a name and a value the gate can set; none when
/// the key is absent or null. Else the usage line: a list over its cap (`gate_args::MOST_ENV_NAMES`,
/// `gate_args::MOST_ENV_BYTES`), a name the gate sets itself, a `GATE_` name, a machine path, a name or value no
/// environment block carries ([`env_refusal`]), or a name given twice in any ASCII case.
///
/// The cap is read from the raw object before any pair of it is checked, so a request over the cap is refused without
/// its pairs being copied.
fn caller_env(payload: &Payload) -> Result<Vec<(String, String)>, String> {
    let (names, bytes) = payload.pair_size("Env")?.unwrap_or_default();
    if over_cap(names, bytes) {
        return Err(EnvRefusal::OverCap.box_line(""));
    }
    let pairs = payload.pairs("Env")?;
    for (name, value) in &pairs {
        if let Some(refusal) = env_refusal(name, value) {
            return Err(refusal.box_line(&shown_escaped(name)));
        }
    }
    if let Some(twice) = repeated_name(pairs.iter().map(|(name, _)| name.as_str())) {
        return Err(EnvRefusal::Twice.box_line(&shown_escaped(twice)));
    }
    Ok(pairs)
}

/// The name `name` a refusal line shows: shortened to [`shown_name`]'s limit and escaped as the box escapes a payload
/// value, so a long or forged name cannot fill or break the line.
fn shown_escaped(name: &str) -> String {
    escaped(&shown_name(name))
}

/// Runs the payload's command in its work folder and returns the status: the command's own whole 32-bit code, 127
/// when it could not be started, 75 when another run holds the work folder, it could not be laid out for the run's
/// repos, its dotnet tools could not be restored or the run asked for the builder's desktop and could not have it
/// ([`desktop_run`]: the console's session is read before the checkout and again under the desktop lock), 2 for a
/// payload or a word refused, a failing git step's status, 1 when the box failed, the tree of a passed command could
/// not be committed back or a passed command's outputs are not brought back, or 255 when the client closed the
/// connection and the box stopped the job. A warm-up's payload runs its commands in order instead and brings nothing
/// back ([`warm_up`]).
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
    // A run the console cannot take is refused before the checkout; the stage before the command reads it again.
    if request.desktop
        && let Err(status) = desktop_run::console_takes_it(&request, seams, console)
    {
        return status;
    }
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
    /// The run's client-gone flag, which the heartbeat and every forwarded output of the run set and every wait reads.
    pub(super) gone: &'a ClientGone,
    pump: Option<&'a Pump>,
    seams: &'a BoxSeams,
    pub(super) stop: StopSignal<'a>,
    /// The load-adjusted seconds the box's ceiling counted for the last command it watched, set as that ceiling ends;
    /// `None` before any, for a command under no ceiling, and once the sampler failed.
    adjusted: Cell<Option<f64>>,
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
    /// Whether the run's [`ClientGone`] is set: the client's connection is gone.
    pub(super) fn client_gone(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }

    /// Has the heartbeat count the CPU of `job` from now on, and the WSL VM's too for a `wsl` command.
    fn watch(&self, job: &Arc<Job>, wsl: bool) {
        if let Some(pump) = self.pump {
            pump.watch(heartbeat::job_cpu(Arc::clone(job), wsl));
        }
    }

    /// Has the heartbeat write every period again.
    fn unwatch(&self) {
        if let Some(pump) = self.pump {
            pump.unwatch();
        }
    }

    /// Says `line` on stderr holding the box's output lock, so it never lands inside a heartbeat or a forwarded line.
    pub(super) fn say(&self, console: &mut Console, line: &str) {
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
    // Made with the lock, before the run's first git call, so git's forwarded output and the heartbeat share it.
    let gone = ClientGone::default();
    let git = WorkGit::ready(request.lock, &lock, &gone);
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
    let pump = heartbeat::start(&lock, &gone, heartbeat::period(seams))
        .inspect_err(|error| console.err(&format!("gate: remote heartbeat stopped: cannot start the heartbeat: {error}")))
        .ok();
    let beat = Beat {
        lock: &lock,
        gone: &gone,
        pump: pump.as_ref(),
        seams,
        stop: StopSignal::new(stop, request.job),
        adjusted: Cell::new(None),
    };
    // The MSYS keeper (`boxside::msys`): up once the heartbeat beats and before the checkout's first git call, held
    // until the run returns, so no MSYS start of the run can leave Git's shared section locked. A stop ends the wait
    // for it; a signal that cannot be read is no stop here, and the run's own look at it says so.
    let _keeper = msys::kept(&|| stop.is_set().unwrap_or(false))
        .inspect_err(|line| beat.say(console, line))
        .ok();
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

/// The run once the heartbeat beats: the snapshot checked out, the repo's dotnet tools restored, a pnpm, bun or npm
/// command's packages installed, the outputs listed, the command seen to its end in the place, the tree it left committed back
/// unless the box's ceiling killed it, and, after a passed command, its outputs packed; the status as
/// [`in_work_folder`] gives it, 1 for a passed command whose outputs are not brought back, [`KILLED`] for a killed one.
fn checked_out(
    request: &Request<'_>,
    git: &WorkGit,
    folders: &Folders<'_>,
    beat: &Beat<'_>,
    outputs: Option<&mut RunOutputs<'_>>,
    console: &mut Console,
) -> Result<u32, u32> {
    let lfs = ready(request, git, folders, beat, console)?;
    let ended = commanded(request, git, folders, beat, (outputs, &lfs), console);
    lfs.close(console);
    ended
}

/// The run once its checkout is ready, as [`checked_out`] goes on with it, its LFS state `lfs` held throughout and
/// giving the back commit its pins ([`RunLfs::pins`]).
fn commanded(
    request: &Request<'_>,
    git: &WorkGit,
    folders: &Folders<'_>,
    beat: &Beat<'_>,
    (mut outputs, lfs): (Option<&mut RunOutputs<'_>>, &RunLfs),
    console: &mut Console,
) -> Result<u32, u32> {
    if let Some(hash) = request.warm_hash {
        return warm_up(request, hash, folders.place, beat, console);
    }
    if let Some(outputs) = outputs.as_deref_mut() {
        outputs.list(folders.work, beat, console);
    }
    let one = One {
        words: request.commands.first().map_or(&[][..], Vec::as_slice),
        place: folders.place,
        timeout: request.timeout,
        of: Of::Run,
        recorded: true,
    };
    let record = command_record(request.work);
    let ran = ran_command(request, &one, beat, console)?;
    removed_if_clean(&record, &ran, beat, console);
    let status = match ran {
        Ran::Exited(status) => status,
        // The PC applies nothing of a run the box killed, and a process its kill could not end may still be writing.
        Ran::Killed => {
            back_commits_failed(request, beat, console);
            return Ok(KILLED);
        }
    };
    let commit = match git.back_commit(request, lfs, console) {
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

/// The run's command seen to its end: on the builder's desktop for a run that asks for it
/// ([`desktop_run::desktop_command`]), else on the box's own session ([`command`]). `Err` with the status of a run that
/// ends before it, its lines said: a refusal of the builder's desktop, 75, says no back commits line; any other end is
/// followed by the box's back commits `failed` line on a pair run ([`back_commits_failed`]).
fn ran_command(request: &Request<'_>, one: &One<'_>, beat: &Beat<'_>, console: &mut Console) -> Result<Ran, u32> {
    let Some(wait) = request.desktop_wait else {
        return command(request, one, beat, console).inspect_err(|_| back_commits_failed(request, beat, console));
    };
    match desktop_run::desktop_command(request, one, wait, beat, console) {
        Ok(ran) => Ok(ran),
        Err(DesktopEnd::Refused) => Err(REFUSED_STATUS),
        Err(DesktopEnd::Ended(status)) => Err(ended_before(status, request, beat, console)),
    }
}

/// A warm-up once its work folder is ready, its tools and every pnpm, bun or npm lockfile its commands need restored: the warm
/// record `{Work}.warm` removed, the start marker said once, then each command run in `place` in order, as a run's
/// command is ([`command`]), with the stop signal looked at before each. The warm-up's `Timeout` is one budget: each
/// command's ceiling is what the earlier ones left of it ([`remaining_budget`]). The first command that ends non-zero,
/// is killed or finds no budget left ends the warm-up, said, with its status ([`KILLED`] for the last two). Once every
/// command passed, the record is written holding `hash` and `gate: remote warm <hash>` is said, last; 0. Nothing is
/// committed back and no outputs are packed. `Err` with the status of a warm-up that ended otherwise, its line said: 1
/// when the record could not be removed or written, or for a stop that came before a command started. A warm-up that
/// did not pass removes the record once more, so none a command wrote itself outlives it.
fn warm_up(request: &Request<'_>, hash: &str, place: &Path, beat: &Beat<'_>, console: &mut Console) -> Result<u32, u32> {
    let record = warm_record(request.work);
    record_step(preflight::remove_entry(Path::new(&record)), "remove", &record, beat, console)?;
    beat.say(console, &started_line(request.nonce));
    let ended = match warm_commands(request, place, beat, console) {
        Ok(0) => {
            let written = written_retrying(&record, hash, || thread::sleep(WARM_WRITE_WAIT));
            record_step(written, "write", &record, beat, console).map(|()| 0)
        }
        other => other,
    };
    if ended == Ok(0) {
        beat.say(console, &format!("gate: remote warm {hash}"));
    } else {
        // Said when it fails; the warm-up's own status stands.
        let _ = record_step(preflight::remove_entry(Path::new(&record)), "remove", &record, beat, console);
    }
    ended
}

/// How long the warm record's write waits before it tries again while another process has the record open, and how
/// many times it waits.
const WARM_WRITE_WAIT: Duration = Duration::from_millis(100);
const WARM_WRITE_WAITS: u32 = 10;

/// Each of a warm-up's commands run in `place` in order ([`warm_up`]): 0 once all passed, else the status of the first
/// that did not, or of a budget spent before a command, said; `Err` as [`command`] gives it, or [`CLOSED_STATUS`] for a
/// client found gone before a command, which is then not started.
fn warm_commands(request: &Request<'_>, place: &Path, beat: &Beat<'_>, console: &mut Console) -> Result<u32, u32> {
    let count = request.commands.len();
    let mut spent = 0.0;
    for (index, words) in request.commands.iter().enumerate() {
        if beat.client_gone() {
            return Err(CLOSED_STATUS);
        }
        let at = index + 1;
        let budget = request.timeout.map(|budget| (budget, remaining_budget(budget, spent)));
        if let Some((budget, None)) = budget {
            beat.say(
                console,
                &format!("gate: remote warm-up ran out of its {budget} s budget before command {at} of {count}"),
            );
            return Ok(KILLED);
        }
        let timeout = budget.and_then(|(_, left)| left);
        beat.adjusted.set(None);
        let began = Instant::now();
        let one = One {
            words,
            place,
            timeout,
            of: Of::WarmUp,
            recorded: false,
        };
        let ran = warm_command(request, &one, beat, console)?;
        spent += command_spent(beat.adjusted.get(), began.elapsed());
        let status = match ran {
            Ran::Exited(status) => status,
            Ran::Killed => KILLED,
        };
        if status != 0 {
            beat.say(console, &format!("gate: remote warm-up stopped at command {at} of {count}"));
            return Ok(status);
        }
        let left = request.timeout.and_then(|budget| remaining_budget(budget, spent));
        if let (true, Some(left)) = (at < count, left) {
            let waited = Instant::now();
            job_name_freed(request.job, beat, Duration::from_secs(left.saturating_mul(BACKSTOP_TIMES)), console);
            spent += gap_spent(waited.elapsed());
        }
    }
    Ok(0)
}

/// One of a warm-up's commands seen to its end as [`command`] sees it, under the interrupted-command record
/// ([`command_record`]): written just before the command is resumed when there is none ([`One::recorded`]), and then
/// removed once the command ended clean ([`removed_if_clean`]); one already there, or a name that cannot be looked at,
/// is left as it is. `Err` as [`command`] gives it.
fn warm_command(request: &Request<'_>, one: &One<'_>, beat: &Beat<'_>, console: &mut Console) -> Result<Ran, u32> {
    let record = command_record(request.work);
    let wrote = fs::symlink_metadata(&record).is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
    let one = One { recorded: wrote, ..*one };
    let ran = command(request, &one, beat, console)?;
    if wrote {
        removed_if_clean(&record, &ran, beat, console);
    }
    Ok(ran)
}

/// How often a warm-up looks for the name of a command's job to be free before the next.
const JOB_GONE_STEP: Duration = Duration::from_millis(20);
/// How many times the budget left a warm-up waits for a job's name at most, in wall time: the backstop a command's
/// ceiling has.
const BACKSTOP_TIMES: u64 = 5;

/// Returns once no job has the name `job`, a stop came (`beat`'s stop signal, its unread line said the first time
/// under the box's output lock), or `limit` of
/// wall time passed: a process its command left and the box ended ([`ended_whole`]) leaves its job's count before its
/// handles close, an inherited handle to the job among them, so the job can outlive its last process for as long as
/// the machine takes to tear those down. A stop is then the next command's stop outcome ([`before_the_job`]); a job
/// still there, or one that cannot be looked for, is left to the next command's job creation, which refuses its name
/// with its own line. A client gone meanwhile, which the heartbeat finds, is not looked for here: [`warm_commands`]
/// looks for it before the next command.
fn job_name_freed(job: &str, beat: &Beat<'_>, limit: Duration, console: &mut Console) {
    let started = Instant::now();
    while matches!(gate_win32::OpenedJob::open(job), Ok(Some(_))) && started.elapsed() < limit {
        let stopped = {
            // Held, so the line saying the signal cannot be read never lands inside a heartbeat line.
            let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
            beat.stop.seen(STARTING_COMMAND, console)
        };
        if stopped {
            return;
        }
        thread::sleep(JOB_GONE_STEP);
    }
}

/// The seconds of a warm-up's budget a wait of `wall` time between two commands spends: a fifth of it, as a command
/// is charged at least ([`command_spent`]), so the whole warm-up's wall time stays near five budgets.
fn gap_spent(wall: Duration) -> f64 {
    wall.as_secs_f64() / 5.0
}

/// Writes `text` as the whole of the warm record `record` ([`tools::write_record_file`], which shares it with no one),
/// trying again after `between` while another process has it open, as a preflight reading it may, at most
/// [`WARM_WRITE_WAITS`] times; else the last try's error.
fn written_retrying(record: &str, text: &str, between: impl FnMut()) -> io::Result<()> {
    retried(|| tools::write_record_file(record, text), between)
}

/// What `step`, an open of a record beside the work folder, gave, tried again after `between` while another process
/// has the record open, as a preflight reading it may, at most [`WARM_WRITE_WAITS`] times; else the last try's error.
fn retried<T>(mut step: impl FnMut() -> io::Result<T>, mut between: impl FnMut()) -> io::Result<T> {
    let mut waits = 0;
    loop {
        match step() {
            Err(error) if error.raw_os_error() == Some(SHARING_VIOLATION) && waits < WARM_WRITE_WAITS => {
                between();
                waits += 1;
            }
            done => return done,
        }
    }
}

/// The interrupted-command record of the work folder `work`, `{work}.command`: `{UTC start} {job}`, written just before
/// a command starts and removed only once it ended clean ([`ended_clean`]), so a record found by a later run is of a
/// command that was ended mid-run, whose leftovers that run clears ([`WorkGit::cleared_interrupted`]).
pub(super) fn command_record(work: &str) -> String {
    format!("{work}{COMMAND_SUFFIX}")
}

/// Writes the interrupted-command record `record` for a command of the job `job` starting now, flushed to the disk,
/// tried again while another process holds it ([`retried`]).
fn written_command_record(record: &str, job: &str) -> io::Result<()> {
    let text = format!("{} {job}", boot::round_trip(jiff::Timestamp::now()));
    retried(|| tools::write_record_synced(record, &text), || thread::sleep(WARM_WRITE_WAIT))
}

/// Whether the command whose wait just ended with an exit ended clean: the run's stop signal unset when read now, as a
/// stop ends the command with an exit of its own, and the client still there. A signal that cannot be read is no clean
/// end.
fn ended_clean(beat: &Beat<'_>) -> bool {
    !beat.client_gone() && matches!(beat.stop.event.is_set(), Ok(false))
}

/// Removes the interrupted-command record `record` once the command that `ran` ended clean ([`ended_clean`]); a removal
/// that fails is said under the box's output lock and changes nothing else.
fn removed_if_clean(record: &str, ran: &Ran, beat: &Beat<'_>, console: &mut Console) {
    if matches!(ran, Ran::Exited(_)) && ended_clean(beat) {
        // Said when it fails; the run's status stands.
        let _ = record_step(preflight::remove_entry(Path::new(record)), "remove", record, beat, console);
    }
}

/// How much earlier than the recorded start a file may have been written and still count as an interrupted command's:
/// the slack for a clock stepped back and for times a file system keeps coarsely.
const INTERRUPTED_SLACK: Duration = Duration::from_secs(60);
/// How many times the clear tries a file another process holds without delete sharing, and how long it waits between.
const CLEAR_TRIES: u32 = 5;
const CLEAR_WAIT: Duration = Duration::from_millis(500);

/// What an interrupted-command record says: the time from which a file counts as the command's, `None` to count every
/// file, and the job to end first, `None` for none.
#[derive(Debug, PartialEq, Eq)]
struct Interrupted {
    since: Option<SystemTime>,
    job: Option<String>,
}

impl Interrupted {
    /// Reads the record `text` of the work folder whose key is `key` at `now`: `{start} {job}`, the start a UTC time and
    /// the job one of the work folder's job names, the key in any case ([`payload::is_recorded_job_name`]); a file
    /// counts from the earlier of the start and `now`, less [`INTERRUPTED_SLACK`]. A record of any other form, an empty
    /// one included, counts every file and names no job.
    fn read(text: &str, key: &str, now: SystemTime) -> Interrupted {
        let parsed = text.split_once(' ').and_then(|(start, job)| {
            let start: jiff::Timestamp = start.parse().ok()?;
            payload::is_recorded_job_name(job, key).then(|| (SystemTime::from(start), job.to_owned()))
        });
        match parsed {
            Some((start, job)) => Interrupted {
                since: start.min(now).checked_sub(INTERRUPTED_SLACK),
                job: Some(job),
            },
            None => Interrupted { since: None, job: None },
        }
    }
}

/// What the interrupted-command record of the work folder `work`, whose payload's key is `key`, says
/// ([`Interrupted::read`]); `None` when there is none. The record is read as the tools record is, tried again while
/// another process holds it ([`retried`]). One that is not UTF-8 is of no form; one that cannot be read otherwise (a link, a
/// folder, a file of more than one name) is replaced by an empty record ([`renewed_empty`]), so it is of no form too
/// and a clear cut short still finds it: neither ends a job, and the clear takes every file. Else why the record could
/// not be read, removed or written.
fn interrupted(work: &str, key: &str) -> Result<Option<Interrupted>, NotLaidOut> {
    let record = command_record(work);
    let text = match retried(|| tools::read_record(&record), || thread::sleep(WARM_WRITE_WAIT)) {
        Ok(None) => return Ok(None),
        Ok(Some(text)) => text,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => String::new(),
        Err(error) if error.raw_os_error() == Some(SHARING_VIOLATION) => {
            return Err(NotLaidOut::Refused(format!("cannot read {record}: {error}")));
        }
        Err(_) => {
            renewed_empty(&record)?;
            String::new()
        }
    };
    Ok(Some(Interrupted::read(&text, key, SystemTime::now())))
}

/// Puts an empty interrupted-command record, flushed to the disk, at `record` in place of one that cannot be read:
/// made new beside it in its folder, held as itself ([`hold_folder`]), and renamed over it by handle
/// ([`gate_win32::rename_beneath`]), so some record stands at the name at every instant and a clear cut short still
/// finds one. A folder or a link at the name is removed first, as itself, never followed. Else why not.
fn renewed_empty(record: &str) -> Result<(), NotLaidOut> {
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_GENERIC_WRITE};
    let path = Path::new(record);
    let not_written = |error: &dyn std::fmt::Display| NotLaidOut::Refused(format!("cannot write {record}: {error}"));
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(OsStr::to_str)) else {
        return Err(not_written(&"it has no folder or no name"));
    };
    if fs::symlink_metadata(path).is_ok_and(|entry| entry.is_dir() || entry.file_type().is_symlink()) {
        removed(path)?;
    }
    let folder = hold_folder(&parent.to_string_lossy()).map_err(NotLaidOut::Refused)?;
    let beside = format!("{name}{RENEWING_SUFFIX}");
    // One a box that died mid-renewal left is no record: it goes first.
    removed(&parent.join(&beside))?;
    let made = gate_win32::open_beneath_as(
        &folder,
        &beside,
        FILE_GENERIC_WRITE | DELETE,
        gate_win32::Share::NoDelete,
        gate_win32::Make::NewFile,
    )
    .and_then(|file| file.sync_all().map(|()| file))
    .and_then(|file| gate_win32::rename_beneath(&file, &folder, name));
    made.map_err(|error| not_written(&error))
}

/// The line the clear says when the job its record names exists but cannot be opened, `job` escaped.
fn unopened_job_line(job: &str) -> String {
    format!(
        "gate: remote: the recorded job {} could not be opened (access denied); the clear goes on without ending it",
        escaped(job)
    )
}

/// What a step that removes or writes (`what`) the warm record `record` did: on a failure, the line saying it could
/// not, said under the box's output lock, and `Err` with status 1.
fn record_step(done: io::Result<()>, what: &str, record: &str, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
    done.map_err(|error| record_failed(what, record, &error, beat, console))
}

/// A step that removes or writes (`what`) a record `record` beside the work folder failed with `error`: the line
/// saying so, said under the box's output lock; status 1.
pub(super) fn record_failed(what: &str, record: &str, error: &io::Error, beat: &Beat<'_>, console: &mut Console) -> u32 {
    let line = format!("gate: remote could not {what} {}: {}", escaped(record), escaped(&error.to_string()));
    beat.say(console, &line);
    FAILED_STATUS
}

/// Removes the interrupted-command record `record` of a command that was never resumed, so no later run takes it for
/// one ended mid-run; a removal that fails is said ([`record_failed`]) and changes nothing else.
pub(super) fn unstarted_record_removed(record: &str, beat: &Beat<'_>, console: &mut Console) {
    if let Err(error) = preflight::remove_entry(Path::new(record)) {
        // Said; the run's own failure is its answer.
        let _ = record_failed("remove", record, &error, beat, console);
    }
}

/// Ends every process a warm-up's command left in its `job` and waits until it holds none ([`terminate_and_wait`]),
/// so the next command's job, which takes the same name, finds none of that name still alive; what the job still
/// holds, or why it could not be ended, is said under the box's output lock.
fn ended_whole(job: &Job, beat: &Beat<'_>, console: &mut Console) {
    let ending = terminate_and_wait(job);
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    match ending {
        Ok(ending) => ending.say(job.name(), console),
        Err(error) => console.err(&format!("gate: remote could not end job {}: {error}", job.name())),
    }
}

/// The whole seconds of a warm-up's `budget` left once its commands have spent `spent` seconds of it, rounded up and
/// at least 1; `None` once nothing is left.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the payload bounds the budget to 2^31 - 1, which an f64 holds exactly, and what is left is positive and at most the budget"
)]
fn remaining_budget(budget: u64, spent: f64) -> Option<u64> {
    let left = budget as f64 - spent;
    // A positive amount rounded up is at least 1.
    (left > 0.0).then(|| left.ceil() as u64)
}

/// The seconds of a warm-up's budget one command spent: the load-adjusted seconds its ceiling counted, but at least a
/// fifth of its `wall` time, so the whole warm-up's wall time stays near five budgets as each command's backstop
/// keeps one command's; its wall time when there are none because the sampler failed and the clock stopped.
fn command_spent(adjusted: Option<f64>, wall: Duration) -> f64 {
    let wall = wall.as_secs_f64();
    adjusted.map_or(wall, |adjusted| adjusted.max(wall / 5.0))
}

/// One command as [`command`] starts it: its words, its program first; its ceiling in whole seconds of load-adjusted
/// time, `None` for none; and what it is a command of ([`Of`]).
#[derive(Clone, Copy)]
struct One<'w> {
    words: &'w [String],
    /// The folder the command runs in.
    place: &'w Path,
    timeout: Option<u64>,
    of: Of,
    /// Whether the interrupted-command record ([`command_record`]) is written once the command's job exists and just
    /// before the command is resumed, so only a command that can be running has one.
    recorded: bool,
}

impl One<'_> {
    /// Whether the command is a WSL one, by its first word as the local watch tells ([`is_wsl_command`]), so the PC
    /// and the box agree on it.
    fn wsl(&self) -> bool {
        self.words.first().is_some_and(|word| is_wsl_command(word))
    }
}

/// What a command [`command`] starts is a command of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Of {
    /// A run: the start marker is said just before its launch.
    Run,
    /// A warm-up, whose start marker was said before its first command: its job is ended whole once it is over
    /// ([`ended_whole`]), so the next command can take the job's name.
    WarmUp,
}

/// The run up to its command: the snapshot laid out and checked out, the subfolder found, the repo's dotnet tools
/// restored and, for a pnpm, bun or npm command, its packages installed ([`packages::update`]); else `Err` with the status the
/// run ends on, its lines said. A refusal is said as such; any other end is followed, on a pair run, by the box's
/// `gate: remote back commits: failed` line ([`ended_before`]). The run's LFS state, held: its LFS content when it
/// carries an `LfsDigest`, its scratch folder alone, or nothing, otherwise ([`WorkGit::checkout`]); a refusal of it is said as such
/// ([`lfs_refused`]).
fn ready(request: &Request<'_>, git: &WorkGit, folders: &Folders<'_>, beat: &Beat<'_>, console: &mut Console) -> Result<RunLfs, u32> {
    let held = match git.lay_out(&request.laid(), console).map_err(NotLaidOut::refusal) {
        Ok(held) => held,
        Err(Ok(why)) => return Err(layout_refused(request, &why, beat, console)),
        Err(Err(failed)) => return Err(ended_before(said(console, &failed), request, beat, console)),
    };
    inputs_matched(request, &held, beat, console)?;
    sent_matched(request, &held, beat, console)?;
    let lfs = match git.checkout(request, console) {
        Ok(lfs) => lfs,
        Err(NotCheckedOut::Failed(failed)) => return Err(ended_before(said(console, &failed), request, beat, console)),
        Err(NotCheckedOut::Refused(why)) => return Err(lfs_refused(request, &lfs::not_the_snapshots(request.key, &why), beat, console)),
        Err(NotCheckedOut::NoScratch(why)) => return Err(lfs_refused(request, &lfs::no_scratch_line(&why), beat, console)),
    };
    // Released before the command, which may change the work folder as it likes.
    drop(held);
    match restored(request, git, folders, beat, console) {
        Ok(()) => Ok(lfs),
        Err(status) => {
            lfs.close(console);
            Err(status)
        }
    }
}

/// Refuses the run `lfs`, its tag and then `line`, which says why, said under the box's output lock, so the PC runs the
/// command itself; 75.
fn lfs_refused(request: &Request<'_>, line: &str, beat: &Beat<'_>, console: &mut Console) -> u32 {
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    console.err(&lfs::refused_line(request.nonce));
    console.err(line);
    REFUSED_STATUS
}

/// The run after its checkout and up to its command: the subfolder found, the repo's dotnet tools restored and, for a
/// pnpm or bun command, its packages installed; else `Err` with the status the run ends on, its lines said, as
/// [`ready`] gives it.
fn restored(request: &Request<'_>, git: &WorkGit, folders: &Folders<'_>, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
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
/// line; returns `status` unchanged.
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
    // A warm-up brings nothing back, so it never says how its back commits went.
    if !request.siblings.is_empty() && request.warm_hash.is_none() {
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
    /// The interrupted-command clear did not finish, its reason said already on its own line
    /// ([`WorkGit::not_cleared`]): the run is refused `layout`, its layout line saying only [`UNCLEARED`].
    Uncleared,
    /// A git step readying a tree failed.
    Failed(Failed),
}

/// What the layout line of a run refused for a clear that did not finish says, the clear's own line having named the
/// file or the job and the error.
const UNCLEARED: &str = "what an interrupted command left in it could not be cleared";

impl NotLaidOut {
    /// Why the run is refused `layout`, as its layout line says it, [`UNCLEARED`] for a clear that did not finish; else
    /// the failed git step, whose own line and status the run ends on.
    fn refusal(self) -> Result<String, Failed> {
        match self {
            NotLaidOut::Refused(why) => Ok(why),
            NotLaidOut::Uncleared => Ok(UNCLEARED.to_owned()),
            NotLaidOut::Failed(failed) => Err(failed),
        }
    }
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
    names_another_layout(&tools::read_record(record), layout)
}

/// Whether a layout record that read as `read` names another layout than `layout`, as [`layout_changed`] judges it: no
/// record names a run of one repo's, and one that cannot be read counts as another.
pub(super) fn names_another_layout(read: &io::Result<Option<String>>, layout: &str) -> bool {
    match read {
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

/// Starts the command in its place and sees it to its end: `Ok` with its status (127 when it could not be started), or
/// `Err` with the status of a run that ends here, its line said: 2 for a batch word refused, 1 for the box's own
/// failure or a stop that came first, 255 for a client that closed the connection. The job's stop signal is looked at
/// just before the job is created ([`before_the_job`]), so a stop that came while the snapshot was checked out starts
/// nothing; the tool restore and the package install look at it the same way before they run, so such a stop runs neither
/// ([`crate::boxside::tools`], [`crate::boxside::packages`]). The command starts on a UTF-8 console when it can have one ([`start_command`]), whose
/// donor lives until the wait for the command is over, however it ends. The heartbeat watches the job while [`watch`]
/// waits for it. The command's environment is [`command_variables`]'s: the box's own, with the caller's `Env` pairs set
/// over it ([`Variables::with_caller`]) and then the build-server settings of its run
/// ([`Variables::with_build_servers`]), replacing any value the box inherited (the caller can give none of them): the
/// compiler server's pipe and `MSBuild`'s node handshake are named by the run id `gate-box-<Nonce>`, so no other run's
/// command, on the box or on the PC, uses the servers this one starts, and the `MSBuild` server, the compiler server and
/// node reuse are off. Git's `safe.directory` is set to `*` over that ([`with_safe_directory`]), and the Event Log
/// logging provider of a .NET host the command starts is turned off over that ([`with_event_log_off`]). The command runs
/// with a restricted copy of the box's token ([`restricted_token`]). The program and a run's `GODOT_PATH` are
/// [`resolved_command`]'s, and its `UE_ROOT` and `EngineEnv` names [`with_unreal`]'s, through [`prepared`].
fn command(request: &Request<'_>, one: &One<'_>, beat: &Beat<'_>, console: &mut Console) -> Result<Ran, u32> {
    let Prepared {
        word,
        program,
        application,
        command_line,
        environment,
    } = match prepared(request, one, console) {
        Ok(prepared) => prepared,
        Err(answer) => return answer,
    };
    let launch = PipedLaunch {
        application,
        command_line,
        directory: one.place.to_owned(),
        environment,
        console: PipedConsole::Hidden,
        token: command_token(request, console)?,
    };
    before_the_job(beat.stop.look(STARTING_COMMAND), request.job, beat.lock, console)?;
    let job = create_job(request.job, console)?;
    // Started right after the job and kept until the command's wait is over, so its console outlives the command.
    let donor = console_donor(&job, launch.token.as_ref(), beat.seams);
    if let Some(park) = beat.seams.park_launch.as_deref() {
        wait_for_path(park, console);
    }
    let record = command_record(request.work);
    let mut start = Start {
        launch,
        job: &job,
        stop: &beat.stop,
        word,
        program: &program,
        marker: (one.of == Of::Run).then_some(request.nonce),
        record: one.recorded.then_some(record.as_str()),
        refuse_token: beat.seams.refuse_token,
        keep_restricted: request.lfs_digest.is_some(),
    };
    let launched = match start_command(&mut start, donor.as_ref().map_err(String::as_str), beat, console) {
        Ok(launched) => launched,
        Err(answer) => return answer.map(Ran::Exited),
    };
    let mut stopper = Stopper { job: &job, armed: true };
    let keep: Vec<u32> = donor.as_ref().map(Donor::pid).into_iter().collect();
    let spec = Spec {
        timeout: one.timeout,
        host: escaped(request.host),
        spoken: escaped(&one.words.join(" ")),
        nonce: request.nonce,
    };
    let ceiling = Ceiling::begin(spec, &job, beat.lock, beat.seams, console);
    let started = Started {
        job: &job,
        launched,
        keep: &keep,
        beat,
        ceiling,
        wsl: one.wsl(),
    };
    let watched = watch(started, console);
    stopper.armed = false;
    // Ended only now, so the command's console stays up for as long as the command may use it.
    drop(donor);
    if one.of == Of::WarmUp {
        ended_whole(&job, beat, console);
    }
    watched
}

/// A command ready to start: its first word, the program that word resolved to, the application and command line it is
/// started with, and the environment it gets.
struct Prepared<'w> {
    word: &'w str,
    program: PathBuf,
    application: PathBuf,
    command_line: Vec<u16>,
    environment: EnvironmentBlock,
}

/// Resolves `one`'s program in its place and writes its command line, with the environment [`command_variables`]
/// composes, the program and a run's `GODOT_PATH` being [`resolved_command`]'s and its `UE_ROOT` and `EngineEnv` names
/// [`with_unreal`]'s, before anything holds a job or the builder's desktop for it, so a desktop run gets the same
/// program and environment as any run. `Err` with the run's answer when it cannot start: `Ok` with 127 for a chosen Unreal Engine that does not
/// resolve, a program not found or one with no shell to run it, its line said; `Err` with 2 for a batch word refused,
/// or 1 for the box's own failure, its line said.
fn prepared<'w>(request: &Request<'_>, one: &One<'w>, console: &mut Console) -> Result<Prepared<'w>, Result<Ran, u32>> {
    let run = format!("gate-box-{}", request.nonce);
    let Some((word, rest)) = one.words.split_first() else {
        return Err(Err(failed(console, "the run has no command")));
    };
    let exe = std::env::current_exe().map_err(|error| Err(failed(console, &format!("the box's own path cannot be read: {error}"))))?;
    let job = handed_job(request.job, gate_win32::own_session());
    let variables = command_variables(&request.env, &request.engine_env, &run, &job, &exe);
    let variables = with_git_bin_first(variables, &request.refused);
    let (variables, found) = resolved_command(request.godot, word, variables, one.place);
    let variables = match with_unreal(&request.engine, &request.engine_env, variables) {
        Ok(variables) => variables,
        Err(line) => {
            console.err(&line);
            return Err(Ok(Ran::Exited(NOT_STARTED)));
        }
    };
    let environment = variables
        .block()
        .map_err(|error| Err(failed(console, &format!("cannot build the command's environment: {error}"))))?;
    let rest: Vec<OsString> = rest.iter().map(OsString::from).collect();
    let program = match found {
        Ok(program) => program,
        Err(message) => return Err(Ok(Ran::Exited(not_started(console, word, None, &message)))),
    };
    match written(&program, &rest, &variables) {
        Written::Refused(line) => {
            console.err(&line);
            Err(Err(u32::from(USAGE_STATUS)))
        }
        Written::NoShell(message) => Err(Ok(Ran::Exited(not_started(console, word, Some(&program), &message)))),
        Written::Line(application, command_line) => Ok(Prepared {
            word,
            program,
            application,
            command_line,
            environment,
        }),
    }
}

/// The environment the payload's command is started with: the box's own less the `EngineEnv` names `engine_env`
/// ([`box_variables`]), with the caller's `Env` pairs `env` set over it, then the gate's own settings for the run `run`
/// — the build-server settings ([`Variables::with_build_servers`]), git's `safe.directory` ([`with_safe_directory`]),
/// the Event Log logging provider off ([`with_event_log_off`]), and the box run's own names: [`BOX_JOB_VARIABLE`] the job `job` the command runs in, as the command reaches it
/// ([`handed_job`]), and [`BOX_EXE_VARIABLE`] the box's own image `exe`. The caller's pairs stand before every setting
/// the gate applies, so none of them can undo one.
fn command_variables(env: &[(String, String)], engine_env: &[String], run: &str, job: &str, exe: &Path) -> Variables {
    with_event_log_off(with_safe_directory(box_variables(engine_env).with_caller(env).with_build_servers(run)))
        .with(BOX_JOB_VARIABLE, job)
        .with(BOX_EXE_VARIABLE, &exe.to_string_lossy())
}

/// The name of the variable a process reads its program folders from. Windows compares variable names without case, so
/// this is every spelling of it, `Path` among them.
const PATH_VARIABLE: &str = "PATH";

/// The environment `variables` with Git's trusted `bin` at the front of its `PATH` ([`with_path_first`]), unless the
/// box finds none or the one it finds lies inside one of the run's own folders `refused` ([`msys::git_bin`]): then
/// they are left as they are and the command keeps the machine `PATH` it would have had.
fn with_git_bin_first(variables: Variables, refused: &[PathBuf]) -> Variables {
    match msys::git_bin(&Git::find(), refused) {
        Some(bin) => with_path_first(variables, &bin),
        None => variables,
    }
}

/// `variables` with `folder` at the front of their `PATH`, set when there is none, so the command's own first word
/// resolves on the same `PATH` the command gets; a folder already first leaves them as they are, so no second entry of
/// it is added. The rest of the value is kept as it is, entries the box inherited included. A `PATH` or a `folder` that
/// is not Unicode, and a folder holding `;` or `"`, which would split or quote the value, leave `variables` untouched.
fn with_path_first(variables: Variables, folder: &Path) -> Variables {
    let Some(folder) = folder.to_str() else {
        return variables;
    };
    if folder.contains([';', '"']) {
        return variables;
    }
    let Some(existing) = variables.get(PATH_VARIABLE).map(OsStr::to_os_string) else {
        return variables.with(PATH_VARIABLE, folder);
    };
    let Some(existing) = existing.to_str() else {
        return variables;
    };
    if existing.is_empty() {
        return variables.with(PATH_VARIABLE, folder);
    }
    if existing.split(';').next().is_some_and(|first| first.eq_ignore_ascii_case(folder)) {
        return variables;
    }
    variables.with(PATH_VARIABLE, &format!("{folder};{existing}"))
}

/// The name of the run's job `job` as the command reaches the object the box created: in session 0, where the box of
/// an ssh logon runs and where `Local\` is the global namespace, `Global\` in place of a leading `Local\`, so a desktop
/// command in the console session, whose own `Local\` names another object, reaches it too; in any other session, or
/// when the box's own `session` cannot be read, the name as created.
fn handed_job(job: &str, session: io::Result<u32>) -> String {
    match (session, job.strip_prefix(r"Local\")) {
        (Ok(0), Some(rest)) => format!(r"Global\{rest}"),
        _ => job.to_owned(),
    }
}

/// The run's job `name`, made with the security a box run's job is made with ([`gate_win32::box_job_descriptor`]),
/// so the command may open it to list it, and kill-on-close when `kill_on_close` ([`Job::create_kill_on_close`]).
pub(super) fn box_run_job(name: &str, kill_on_close: bool) -> io::Result<Job> {
    let descriptor = gate_win32::token_facts().and_then(|facts| gate_win32::box_job_descriptor(&facts))?;
    if kill_on_close {
        Job::create_kill_on_close_secured(name, &descriptor)
    } else {
        Job::create_secured(name, &descriptor)
    }
}

/// The variable a command of a run with a chosen Godot version reads the Godot install it resolved from.
const GODOT_PATH: &str = "GODOT_PATH";

/// The program the command's first word `word` runs in `place` and the environment `variables` it runs with, for a run
/// whose chosen Godot version is `godot`. With no version, the word is looked for as any other ([`find_program`]) and
/// the environment is left as it is.
///
/// With a version and the box's `GATE_GODOT_ROOT` set, the Godot program is the install of that version under the root
/// ([`probe::installed_godot`]) and nothing else: a Godot console word ([`gate_args::is_godot_console`]) runs it, and
/// with no such install fails to start, never falling back to the `PATH`. With a version and no root, a Godot console
/// word is looked for as any other word, and the Godot program is the one it runs, or, for any other word, the
/// `Godot_console.exe` the `PATH` holds. `GODOT_PATH` is set over every other value the environment holds for it, the
/// caller's having been refused, to the Godot program found; with none found, every value it holds is removed, so an
/// inherited one never passes for the gate's.
fn resolved_command(godot: Option<&str>, word: &str, variables: Variables, place: &Path) -> (Variables, Result<PathBuf, String>) {
    let Some(version) = godot else {
        let found = find_program(word, &variables, place);
        return (variables, found);
    };
    let (found, godot_program) = match (is_godot_console(word), variables.get("GATE_GODOT_ROOT")) {
        (true, Some(root)) => {
            let installed = probe::installed_godot(root, version);
            (
                installed.clone().ok_or_else(|| gate_win32::win32_message(ERROR_FILE_NOT_FOUND)),
                installed,
            )
        }
        (true, None) => {
            let found = find_program(word, &variables, place);
            let godot_program = found.as_ref().ok().cloned();
            (found, godot_program)
        }
        (false, Some(root)) => (find_program(word, &variables, place), probe::installed_godot(root, version)),
        (false, None) => (find_program(word, &variables, place), find_program(probe::GODOT, &variables, place).ok()),
    };
    let variables = match godot_program {
        Some(program) => variables.with(GODOT_PATH, &program.to_string_lossy()),
        None => variables.without(GODOT_PATH),
    };
    (variables, found)
}

/// The variable a command of a run with a chosen Unreal Engine version reads the engine folder it resolved from.
const UE_ROOT: &str = "UE_ROOT";

/// The box's own environment as every process it starts for a run's repo gets it — the command, a package install, a
/// tool restore — before what each sets over it: the box's process environment less any `UE_ROOT` it inherited and any
/// value of the run's `EngineEnv` names `engine_env` ([`without_engine`]), so none of them sees an engine the run did
/// not choose.
pub(super) fn box_variables(engine_env: &[String]) -> Variables {
    without_engine(Variables::from_process(), engine_env)
}

/// `inherited` with every value it holds for `UE_ROOT` and for each of the names `engine_env` removed.
fn without_engine(inherited: Variables, engine_env: &[String]) -> Variables {
    engine_env
        .iter()
        .fold(inherited.without(UE_ROOT), |variables, name| variables.without(name))
}

/// The Unreal Engine install a run whose chosen version is `unreal` uses, resolved once for all its commands: the
/// install of that version under the box's `GATE_UNREAL_ROOT`, a root overlapping none of the run's folders `refused`
/// ([`probe::installed_unreal`]); `Ok(None)` with no version. `Err` with the line
/// `gate: could not resolve unreal <version>: <why>` when the version does not resolve.
fn resolved_engine(unreal: Option<&str>, refused: &[PathBuf]) -> Result<Option<PathBuf>, String> {
    let Some(version) = unreal else {
        return Ok(None);
    };
    let worktrees: Vec<&Path> = refused.iter().map(PathBuf::as_path).collect();
    let variables = box_variables(&[]);
    variables
        .get(probe::UNREAL_ROOT)
        .ok_or_else(|| format!("{} is not set", probe::UNREAL_ROOT))
        .and_then(|root| probe::installed_unreal(root, version, &worktrees))
        .map(Some)
        .map_err(|why| format!("gate: could not resolve unreal {version}: {why}"))
}

/// The environment `variables` of a command of a run whose engine resolved to `engine` ([`resolved_engine`]): with
/// one, `UE_ROOT` set over every other value they hold for it, the caller's having been refused, to that engine's
/// folder, and then each of the `EngineEnv` names `engine_env` set to the same folder, over a caller's pair for it;
/// with none, as they are, the box's inherited values already gone ([`box_variables`]). `Err` with the line the engine
/// failed to resolve with. The command's first word is never read.
fn with_unreal(engine: &Result<Option<PathBuf>, String>, engine_env: &[String], variables: Variables) -> Result<Variables, String> {
    match engine {
        Ok(None) => Ok(variables),
        Ok(Some(folder)) => {
            let folder = folder.to_string_lossy();
            let variables = variables.with(UE_ROOT, &folder);
            Ok(engine_env.iter().fold(variables, |variables, name| variables.with(name, &folder)))
        }
        Err(line) => Err(line.clone()),
    }
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

/// The command's restricted token ([`restricted_token`]); for a run with LFS content, which never runs on the box's own
/// token, a token that cannot be made fails the start instead: `Err` with status 1, its line said.
fn command_token(request: &Request<'_>, console: &mut Console) -> Result<Option<RestrictedToken>, u32> {
    if request.lfs_digest.is_none() {
        return Ok(restricted_token(console));
    }
    RestrictedToken::of_this_process().map(Some).map_err(|error| {
        failed(
            console,
            &format!("cannot make the command's restricted token, and {LFS_OWN_TOKEN}: {error}"),
        )
    })
}

/// Why a run with LFS content does not fall back to the box's own token.
const LFS_OWN_TOKEN: &str = "a run with LFS content never runs with the builder session's own token";

/// The line saying the command runs with the box's own token, as its restricted one could not be made or started
/// with, for `error`.
fn own_token_line(error: &dyn std::fmt::Display) -> String {
    format!("gate: the command runs with the builder session's own token: {error}")
}

/// Win32's `ERROR_ACCESS_DENIED`, the refusal the token seam stands in for.
const ACCESS_DENIED: u32 = 5;

/// The command to start: its launch, its job and the job's stop signal, the word and program a start failure names,
/// the run's nonce its start marker names, `None` for a warm-up's command, whose marker was said before its first
/// command, the interrupted-command record written just before its resume, `None` for none ([`One::recorded`]), and
/// whether the token seam refuses its start with the restricted token.
struct Start<'a> {
    launch: PipedLaunch,
    record: Option<&'a str>,
    job: &'a Job,
    stop: &'a StopSignal<'a>,
    word: &'a str,
    program: &'a Path,
    marker: Option<&'a str>,
    refuse_token: bool,
    /// Whether a refused start with the restricted token fails the start rather than falling back to the box's own
    /// token: a run with LFS content.
    keep_restricted: bool,
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
    if let Some(nonce) = start.marker {
        beat.say(console, &started_line(nonce));
    }
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
        LaunchEnd::Refused(error) => {
            // The record may have been written before a resume that failed: the command never ran.
            if let Some(record) = start.record {
                unstarted_record_removed(record, beat, console);
            }
            Err(start_refused(console, start, error))
        }
        LaunchEnd::Unrecorded(record, error) => {
            let status = record_failed("write", &record, &error, beat, console);
            // A write that failed part way may have left a record: the command never ran.
            unstarted_record_removed(&record, beat, console);
            Err(Err(status))
        }
        LaunchEnd::Stopped => Err(Err(stopped_before_start(start.job, beat.lock, console))),
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
    /// The line saying the command runs with the box's own token, when its start with the restricted one was refused;
    /// for a run with LFS content, the line saying it was not started instead.
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
    /// The interrupted-command record, this one, could not be written before the resume, for this error: the command
    /// never ran.
    Unrecorded(String, io::Error),
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
    if let Err(error) = assigned {
        return ended(LaunchEnd::Refused(error), unread);
    }
    // Written only now that the command is in its job and about to run, so a command that never starts leaves none.
    if let Some(record) = start.record
        && let Err(error) = written_command_record(record, start.job.name())
    {
        return ended(LaunchEnd::Unrecorded(record.to_owned(), error), unread);
    }
    let how = match suspended.resume() {
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
/// box's own token when the restricted start was refused and the second one created it. A run with LFS content
/// (`keep_restricted`) never takes the second start: the restricted start's refusal is the launch's, with the line
/// saying why the command was not started on the box's own token.
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
        Err(error @ LaunchError::CreateProcess { .. }) if start.keep_restricted => {
            let line = format!("gate: remote could not start the command with its restricted token, and {LFS_OWN_TOKEN}");
            (Err(error), Some(line))
        }
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
/// on. Either line is said holding the box's output `lock`, as the heartbeat writes under it.
pub(super) fn before_the_job(seen: Look, job: &str, lock: &OutputLock, console: &mut Console) -> Result<(), u32> {
    let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
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
pub(super) fn stopped_before_start(job: &Job, lock: &OutputLock, console: &mut Console) -> u32 {
    let ending = terminate_and_wait(job);
    said_stopped(job, ending, lock, console)
}

/// Says the run failed for a stop that came before the command started, naming `job`, whose `ending` is already
/// decided; status 1. What the job still holds after the stop and a later terminate that failed ([`Ending::say`]), or
/// why it could not be stopped, is said after. Every line is said holding the box's output `lock`, as the heartbeat
/// writes under it.
pub(super) fn said_stopped(job: &Job, ending: io::Result<Ending>, lock: &OutputLock, console: &mut Console) -> u32 {
    let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
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

/// Creates the run's job `name` with a box run's job's security ([`box_run_job`]); a name some job already has is
/// refused, so a run never shares a job and a stop meant for an earlier run never reaches it; shared, so the
/// heartbeat's thread can read its CPU. Else the status 1, its line said.
fn create_job(name: &str, console: &mut Console) -> Result<Arc<Job>, u32> {
    job_made(box_run_job(name, false), name, console)
}

/// The run's job `name` as `made` gave it, shared, as [`create_job`] gives it; else the status 1, its line said, a
/// name some job already has said as such.
pub(super) fn job_made(made: io::Result<Job>, name: &str, console: &mut Console) -> Result<Arc<Job>, u32> {
    made.map(Arc::new).map_err(|error| {
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

/// Forwards the running command's output, waits for its main process to exit, then drains the readers as
/// [`Forwarders::drain`] does — up to [`DRAIN`], and past it up to its hang guard for a reader that is only behind;
/// the command's exit code. A failed write to the box's own output means the client closed the connection,
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
        wsl,
    } = started;
    let Launched { running, pipes } = launched;
    let forwarders = match Forwarders::start(pipes.stdout, pipes.stderr, beat.lock, beat.gone) {
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
    beat.watch(job, wsl);
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
            beat.adjusted.set(ceiling.as_ref().and_then(Ceiling::adjusted_seconds));
            end_what_it_left(job, keep, console);
            watched.drained(console)?;
            if let Some(ceiling) = &ceiling {
                ceiling.ended(console);
            }
            Ok(Ran::Exited(code))
        }
        WaitEnd::Killed((kill, ceiling)) => {
            let ceiling = ceiling.closed();
            beat.adjusted.set(ceiling.adjusted_seconds());
            killed(watched, kill, &ceiling, beat.seams, console)
        }
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
/// job that outlive it by design, the run's output lock and heartbeat, the box's ceiling on it, `None` for none, and
/// whether it is a WSL command ([`One::wsl`]), whose WSL VM's CPU the heartbeat counts too.
struct Started<'s, 'b> {
    job: &'s Arc<Job>,
    launched: Launched,
    keep: &'s [u32],
    beat: &'s Beat<'b>,
    ceiling: Option<Ceiling<'s>>,
    wsl: bool,
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
    /// The run's [`ClientGone`] was set first, by a forwarded line or a heartbeat line or probe whose write to the box's
    /// own output found its reader gone: the client closed the connection.
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
            Ok(None) if self.forwarders.client_gone() => Some(WaitEnd::ClientGone),
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

    /// Once the main process exited: drains the readers as [`Forwarders::drain`] does — up to [`DRAIN`], and past it
    /// up to its hang guard for a reader that is only behind. A write to the box's own output that failed meanwhile
    /// stops the job and answers [`CLOSED_STATUS`]; a leftover process still holding the output open is said, and the
    /// run goes on.
    pub(super) fn drained(self, console: &mut Console) -> Result<(), u32> {
        let drained = self.forwarders.drain(DRAIN);
        if drained.client_gone {
            return Err(closed_connection(self.job, terminate_and_wait(self.job), console));
        }
        if !drained.all_ended {
            console.err(self.watching.leftover());
        }
        Ok(())
    }

    /// The box's ceiling found the process past a limit: its job is ended first, then the readers drain as
    /// [`Forwarders::drain`] does — up to [`DRAIN`], and past it up to its hang guard for a reader that is only
    /// behind; how the ending went, which the ceiling says. A write to the box's own output that failed meanwhile
    /// answers [`CLOSED_STATUS`], and a leftover process still holding the output open is said, as
    /// [`Watched::drained`] does.
    pub(super) fn killed(self, console: &mut Console) -> Result<io::Result<Ending>, u32> {
        let ending = terminate_and_wait(self.job);
        let drained = self.forwarders.drain(DRAIN);
        if drained.client_gone {
            return Err(closed_connection(self.job, ending, console));
        }
        if !drained.all_ended {
            console.err(self.watching.leftover());
        }
        Ok(ending)
    }

    /// The client closed the connection: the job is stopped first, then the readers drain as [`Forwarders::drain`]
    /// does — up to [`DRAIN`], and past it up to its hang guard for a reader that is only behind; [`CLOSED_STATUS`].
    pub(super) fn client_gone(self, console: &mut Console) -> u32 {
        let ending = terminate_and_wait(self.job);
        self.forwarders.drain(DRAIN);
        closed_connection(self.job, ending, console)
    }

    /// The wait for the main process failed, its line said: the job is ended and waited for, then the readers drain
    /// as [`Forwarders::drain`] does — up to [`DRAIN`], and past it up to its hang guard for a reader that is only
    /// behind — so no line of the process is forwarded after the box's next line; then what the ending left is said.
    /// `status`, the wait's own.
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
    /// The run's client-gone flag, which a failed forward of git's output sets and each git step reads once git ended.
    gone: ClientGone,
}

impl WorkGit {
    /// Finds git, its output to be forwarded under `output`, a failed forward setting `gone`; called only once the
    /// caller holds the work folder's lock file `lock`.
    fn ready(lock: &str, output: &OutputLock, gone: &ClientGone) -> WorkGit {
        WorkGit {
            git: Git::find(),
            hooks: format!(r"core.hooksPath={lock}\hooks"),
            output: Arc::clone(output),
            gone: Arc::clone(gone),
        }
    }

    /// Runs git with the safety options and `args`, both of its streams forwarded ([`Git::forwarded`]); a non-zero
    /// status, or a git that cannot be started, fails `step`, said as [`Git::step`] says it, and a client gone ends the
    /// run ([`client_gone`]).
    fn git_step(&self, step: &str, args: &[&str], console: &mut Console) -> Result<(), Failed> {
        match self.git_code(step, args, console)? {
            0 => Ok(()),
            code => Err(git_failed(step, code)),
        }
    }

    /// git's exit code for the safety options and `args`, both of its streams forwarded ([`Git::forwarded`]); a git that
    /// cannot be started fails `step`, said as [`Git::step`] says it, and a client gone ends the run ([`client_gone`]).
    fn git_code(&self, step: &str, args: &[&str], console: &mut Console) -> Result<i32, Failed> {
        let ran = self
            .git
            .command(&self.args(args))
            .map_err(GitFailure::Failed)
            .and_then(|command| Git::forwarded(command, false, &self.output, &self.gone, console));
        match ran {
            Ok(output) => Ok(exit_code(output.status)),
            Err(GitFailure::Failed(message)) => Err(Failed::without_status(format!("gate: remote {step} failed: {message}"))),
            Err(GitFailure::ClientGone) => Err(client_gone(step)),
        }
    }

    /// The safety options, no `core.attributesFile` among them ([`crate::remote::lfs::NO_ATTRIBUTES_FILE`]), so the
    /// tree's attributes are read from its own `.gitattributes` files and from `<Work>\.git\info\attributes`, then `args`.
    pub(super) fn args<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut all = vec!["-c", self.hooks.as_str(), "-c", "core.fsmonitor=false"];
        all.extend(crate::remote::lfs::NO_ATTRIBUTES_FILE);
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
    /// and the run's layout recorded after a clear. A clear removes the inputs record `{Work}.inputs` and the
    /// sent-build record `{Work}.sent-build` too, before the work folder's children, so no record outlives the files it
    /// lists, and the warm record `{Work}.warm` before it writes [`LAYOUT_CHANGING`], as it is removed too when the
    /// work folder is missing and made here, so no warm record outlives the folder state it describes. A run of one
    /// repo in a work folder of that
    /// layout whose `.git` is plain readies nothing here: the preflight readied its tree. The interrupted-command record
    /// `{Work}.command` is removed with a missing work folder and after a clear, each once the job it names is ended
    /// ([`WorkGit::job_emptied`]) before anything is removed; a work folder of the run's layout that
    /// holds one has what the interrupted command left cleared before any tree is readied
    /// ([`WorkGit::cleared_interrupted`]). The held folder, which the caller keeps through the checkouts.
    fn lay_out(&self, laid: &Laid<'_>, console: &mut Console) -> Result<File, NotLaidOut> {
        let work = Path::new(laid.work);
        let warm = warm_record(laid.work);
        let command = command_record(laid.work);
        if fs::symlink_metadata(work).is_err() {
            removed(Path::new(&warm))?;
            // A command its record names may still run, though its folder is gone: it is ended before the record goes.
            self.recorded_command_ended(laid, console)?;
            removed(Path::new(&command))?;
        }
        fs::create_dir_all(work).map_err(|error| NotLaidOut::Refused(format!("cannot create {}: {error}", laid.work)))?;
        let held = hold_folder(laid.work).map_err(NotLaidOut::Refused)?;
        let record = format!("{}.layout", laid.work);
        let layout = laid.layout();
        let changed = layout_changed(&record, &layout);
        if changed {
            // Ended before anything is cleared, so no command its record names writes into the folder after the clear.
            self.recorded_command_ended(laid, console)?;
            removed(Path::new(&warm))?;
            write_layout(&record, LAYOUT_CHANGING)?;
            removed(Path::new(&inputs::record_path(laid.work)))?;
            removed(Path::new(&inputs::sent_record_path(laid.work)))?;
            clear_work(work)?;
            // Last, so a clear cut short still finds the record and clears again.
            removed(Path::new(&command))?;
        } else {
            self.cleared_interrupted(laid.work, laid.key, &held, console)?;
        }
        // A tree readied here removes the warm record itself when its `.git` is made or repaired ([`WorkGit::ready_trees`]).
        if changed || !laid.siblings.is_empty() || !preflight::git_is_plain(Path::new(&laid.main.tree)) {
            self.ready_trees(laid, console)?;
        }
        if changed {
            write_layout(&record, &layout)?;
        }
        Ok(held)
    }

    /// Clears what an interrupted command left in the work folder `work`, held as `held`, when its interrupted-command
    /// record ([`command_record`]) is there: the warm, packages and tools records removed first, so none of their
    /// skip rules trusts the folder; the recorded job ended until it holds no process, a job no longer there being
    /// fine; then every file below the work folder written since the recorded start ([`Interrupted::read`]) removed
    /// by handle ([`gate_win32::delete_fresh_beneath`]), never inside a `.git` or behind a junction; the record
    /// removed last, so a clear cut short runs again. Its two lines are said under the box's output lock. A file that
    /// will not go, or a job that will not empty, refuses the run `layout` with the record kept ([`WorkGit::not_cleared`]).
    fn cleared_interrupted(&self, work: &str, key: &str, held: &File, console: &mut Console) -> Result<(), NotLaidOut> {
        let Some(interrupted) = interrupted(work, key)? else {
            return Ok(());
        };
        let record = command_record(work);
        for suffix in [WARM_SUFFIX, packages::PACKAGES_SUFFIX, tools::TOOLS_SUFFIX] {
            let other = format!("{work}{suffix}");
            removed(Path::new(&other))?;
        }
        self.say(console, "gate: remote clearing what an interrupted command left in the work folder");
        self.recorded_job_ended(&interrupted, console)?;
        let fresh = gate_win32::Fresh {
            since: interrupted.since,
            tries: CLEAR_TRIES,
            wait: CLEAR_WAIT,
        };
        let count = gate_win32::delete_fresh_beneath(held, fresh)
            .map_err(|not| self.not_cleared(&Path::new(work).join(&not.path).display().to_string(), &not.error, console))?;
        self.say(
            console,
            &format!("gate: remote cleared {count} file(s) an interrupted command left in the work folder"),
        );
        removed(Path::new(&record))
    }

    /// Ends the job the interrupted-command record of the work folder `laid` lays out names, when there is a record
    /// ([`interrupted`]) and it names one ([`WorkGit::recorded_job_ended`]).
    fn recorded_command_ended(&self, laid: &Laid<'_>, console: &mut Console) -> Result<(), NotLaidOut> {
        match interrupted(laid.work, laid.key)? {
            Some(interrupted) => self.recorded_job_ended(&interrupted, console),
            None => Ok(()),
        }
    }

    /// Ends the job the interrupted-command record `interrupted` names, when it names one ([`WorkGit::job_emptied`]).
    fn recorded_job_ended(&self, interrupted: &Interrupted, console: &mut Console) -> Result<(), NotLaidOut> {
        match &interrupted.job {
            Some(job) => self.job_emptied(job, console),
            None => Ok(()),
        }
    }

    /// Ends every process of the job `job` an interrupted command ran in and waits until it holds none
    /// ([`terminate_and_wait`]); a job no longer there is fine, and so is a name an object that is no job holds, as it
    /// holds no process ([`OpenedJob::open_or_not_a_job`]). One that exists but may not be ended (access denied) is
    /// judged as [`WorkGit::unended_job`] says. A job that cannot be opened otherwise or ended, or still holds a
    /// process, refuses the run ([`WorkGit::not_cleared`]).
    fn job_emptied(&self, job: &str, console: &mut Console) -> Result<(), NotLaidOut> {
        let ended = match gate_win32::OpenedJob::open_or_not_a_job(job) {
            Ok(gate_win32::JobOpened::Absent | gate_win32::JobOpened::NotAJob) => return Ok(()),
            Ok(gate_win32::JobOpened::Job(opened)) => terminate_and_wait(&opened).and_then(|ending| match ending.left {
                0 => Ok(()),
                left => Err(io::Error::other(format!("it still has {left} process(es)"))),
            }),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return self.unended_job(job, console),
            Err(error) => Err(error),
        };
        ended.map_err(|error| self.not_cleared(job, &error, console))
    }

    /// Judges the job `job`, which exists but may not be ended, as a process whose wait is refused is judged by what
    /// query access alone shows: opened to be listed, one that holds a process may be the run's own command still
    /// running and refuses the run ([`WorkGit::not_cleared`]), and one that holds none is left as it is, said under the
    /// box's output lock ([`unopened_job_line`]), and the clear goes on. One that may not be listed either is judged
    /// by its owner ([`WorkGit::unlisted_job`]).
    fn unended_job(&self, job: &str, console: &mut Console) -> Result<(), NotLaidOut> {
        let held = match gate_win32::OpenedJob::open_to_list(job) {
            Ok(None) => return Ok(()),
            Ok(Some(listed)) => listed.process_ids().map(|ids| ids.len()),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return self.unlisted_job(job, console),
            Err(error) => Err(error),
        };
        match held {
            Ok(0) => {
                self.say(console, &unopened_job_line(job));
                Ok(())
            }
            Ok(left) => {
                let still = io::Error::other(format!("it may not be ended and still has {left} process(es)"));
                Err(self.not_cleared(job, &still, console))
            }
            Err(error) => Err(self.not_cleared(job, &error, console)),
        }
    }

    /// Judges the job `job`, which exists but may be neither ended nor listed, by its owner, which the system lets read
    /// a job's security whatever its DACL says: one owned by the box's user or by Administrators, as the box's own jobs
    /// are, may be the run's own and refuses the run ([`WorkGit::not_cleared`]); one whose owner is another, or that
    /// does not let the box read its owner, is another principal's of the same name, left as it is, said under the
    /// box's output lock ([`unopened_job_line`]), and the clear goes on.
    fn unlisted_job(&self, job: &str, console: &mut Console) -> Result<(), NotLaidOut> {
        let owner = match gate_win32::OpenedJob::owner_of(job) {
            Ok(None) => return Ok(()),
            Ok(Some(owner)) => owner,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                self.say(console, &unopened_job_line(job));
                return Ok(());
            }
            Err(error) => return Err(self.not_cleared(job, &error, console)),
        };
        match gate_win32::current_user_sid() {
            Ok(user) if owner == user || owner == ADMINISTRATORS_SID => {
                let own = io::Error::other(format!(
                    "it may be neither ended nor listed, and its owner {owner} owns the box's own jobs"
                ));
                Err(self.not_cleared(job, &own, console))
            }
            Ok(_) => {
                self.say(console, &unopened_job_line(job));
                Ok(())
            }
            Err(error) => Err(self.not_cleared(job, &error, console)),
        }
    }

    /// The refusal of a clear that could not remove `what`, a file's path or a job's name, for `error`: its line said
    /// under the box's output lock, `what` and `error` escaped, and the run refused `layout` ([`NotLaidOut::Uncleared`]),
    /// its layout line not saying the reason again.
    fn not_cleared(&self, what: &str, error: &io::Error, console: &mut Console) -> NotLaidOut {
        self.say(
            console,
            &format!(
                "gate: remote could not clear what an interrupted command left in the work folder: {}: {}",
                escaped(what),
                escaped(&error.to_string())
            ),
        );
        NotLaidOut::Uncleared
    }

    /// Says `line` on stderr holding the box's output lock.
    fn say(&self, console: &mut Console, line: &str) {
        let _held = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        console.err(line);
    }

    /// Each repo's tree and its `.git` made folders of their own ([`plain_tree`]) and readied as the preflight readies
    /// a run of one repo's ([`preflight::ready_tree`]), under the lock, so a link at `.git\objects` or
    /// `.git\objects\info` is replaced by a folder too.
    fn ready_trees(&self, laid: &Laid<'_>, console: &mut Console) -> Result<(), NotLaidOut> {
        let warm = warm_record(laid.work);
        // A tree whose `.git` is made or repaired here is no longer the one the warm record was written for.
        if laid.repos().any(|repo| !preflight::git_is_plain(Path::new(&repo.tree))) {
            removed(Path::new(&warm))?;
        }
        for repo in laid.repos() {
            plain_tree(&repo.tree)?;
            let at = preflight::TreeAt {
                tree: &repo.tree,
                bare: &repo.bare,
                warm: &warm,
            };
            preflight::ready_tree(self, &at, repo.name.as_deref(), console).map_err(NotLaidOut::from)?;
        }
        Ok(())
    }

    /// Each repo's snapshot, fetched from its bare repository at `refs/gate/<key>` ([`WorkGit::fetched`]) and checked
    /// out, one repo after another, the main one first ([`WorkGit::checked_out_repo`]); between the main repo's fetch
    /// and its checkout, the run's LFS state is readied ([`WorkGit::run_lfs`]) and given back held, let go on a
    /// failure. A pair run whose payload carries an `LfsDigest` is refused `lfs` before any checkout, as no pair takes
    /// LFS content.
    fn checkout(&self, request: &Request<'_>, console: &mut Console) -> Result<RunLfs, NotCheckedOut> {
        if !request.siblings.is_empty() && request.lfs_digest.is_some() {
            return Err(NotCheckedOut::Refused(
                "a sibling pair's run carries an LfsDigest, and no pair takes LFS content".to_owned(),
            ));
        }
        self.fetched(request, &request.main, console)?;
        let mut lfs = self.run_lfs(request, console)?;
        let ended = request.repos().enumerate().try_for_each(|(at, repo)| {
            if at > 0 {
                self.fetched(request, repo, console)?;
            }
            self.lfs_scratch(request, repo, &mut lfs, console)?;
            self.checked_out_repo(request, repo, &lfs, console)
        });
        match ended {
            Ok(()) => Ok(lfs),
            Err(not) => {
                lfs.close(console);
                Err(not)
            }
        }
    }

    /// git's standard output for `args` in the tree of `repo`, fed `feed` on its standard input when given, at most
    /// `cap` bytes, its stderr forwarded; else the failure of `step`, said as [`WorkGit::git_step`] says it.
    fn output(&self, step: &str, repo: &Repo<'_>, args: &[&str], feed: Option<&[u8]>, cap: usize, console: &mut Console) -> Result<Vec<u8>, Failed> {
        let step = repo.step(step);
        let cannot = |message: String| Failed::without_status(format!("gate: remote {step} failed: {message}"));
        let mut all = repo.at().to_vec();
        all.extend_from_slice(args);
        let mut command = self.git.command(&self.args(&all)).map_err(cannot)?;
        let writer = match feed {
            None => None,
            Some(bytes) => {
                let (reader, mut writer) = io::pipe().map_err(|error| cannot(format!("cannot make the pipe of its input: {error}")))?;
                command.stdin(reader);
                let bytes = bytes.to_vec();
                Some(thread::spawn(move || writer.write_all(&bytes)))
            }
        };
        let ran = Git::forwarded(command, true, &self.output, &self.gone, console);
        let written = writer.map(thread::JoinHandle::join);
        let output = match ran {
            Ok(output) => output,
            Err(GitFailure::Failed(message)) => return Err(cannot(message)),
            Err(GitFailure::ClientGone) => return Err(client_gone(&step)),
        };
        match exit_code(output.status) {
            0 => {}
            code => return Err(git_failed(&step, code)),
        }
        match written {
            Some(Ok(Err(error))) => return Err(cannot(format!("its input could not be written: {error}"))),
            Some(Err(_)) => return Err(cannot("the writer of its input panicked".to_owned())),
            Some(Ok(Ok(()))) | None => {}
        }
        if output.stdout.len() > cap {
            return Err(cannot(format!("it printed more than {cap} bytes")));
        }
        Ok(output.stdout)
    }

    /// The run's LFS state, held from before its first checkout through its back commit. For a run that carries an
    /// `LfsDigest`, which only a run of one repo may, the LFS content of its fetched snapshot, covered and held
    /// ([`lfs::covered`]): its pointers read from the snapshot itself as the PC reads them
    /// ([`crate::remote::lfs::pointers_of`]) and checked against the digest; then its scratch folder made
    /// ([`RunLfs::with_scratch`]). For a run with none, which lists nothing, no scratch folder yet
    /// ([`RunLfs::without_content`]): each repo's checkout makes it when its snapshot names the LFS filter
    /// ([`WorkGit::lfs_scratch`]). Else the git step's failure, or why the run is refused `lfs`: its content, or a scratch
    /// folder that cannot be made.
    fn run_lfs(&self, request: &Request<'_>, console: &mut Console) -> Result<RunLfs, NotCheckedOut> {
        let Some(digest) = request.lfs_digest else {
            return Ok(RunLfs::without_content());
        };
        let repo = &request.main;
        let unreadable = |why: String| Failed::without_status(format!("gate: remote lfs listing failed: {why}"));
        let listed = crate::remote::lfs::pointers_of(
            repo.snapshot,
            &mut |args, feed, cap| self.output("lfs listing", repo, args, feed, cap, console),
            unreadable,
        )?;
        let mut lfs = lfs::covered(&repo.bare, digest, &listed).map_err(NotCheckedOut::Refused)?;
        lfs.with_scratch(request.work, (request.key, request.nonce), console)
            .map_err(NotCheckedOut::NoScratch)?;
        Ok(lfs)
    }

    /// The run's scratch folder made before the checkout of `repo` ([`RunLfs::with_scratch`]) when the run has none yet
    /// and a `.gitattributes` file of `repo`'s snapshot, at any depth and whatever the case of its name, holds the text
    /// `filter=lfs` (`git grep`), so git-lfs reads and writes the run's own `lfs.storage` for every file the snapshot's
    /// attributes give the filter; a comment naming it makes a scratch folder too, which costs nothing. A scratch folder
    /// that cannot be made refuses the run `lfs`, as its command has not run; a `git grep` that fails is the step's
    /// failure, its line ending in the first line git said.
    fn lfs_scratch(&self, request: &Request<'_>, repo: &Repo<'_>, lfs: &mut RunLfs, console: &mut Console) -> Result<(), NotCheckedOut> {
        if lfs.has_scratch() {
            return Ok(());
        }
        let step = repo.step("lfs listing");
        let args = ["grep", "-q", "-F", "-e", "filter=lfs", repo.snapshot, "--", ATTRIBUTES_FILES];
        match self.quiet_saying(repo, &args) {
            Ok((0, _)) => lfs
                .with_scratch(request.work, (request.key, request.nonce), console)
                .map_err(NotCheckedOut::NoScratch),
            Ok((1, _)) => Ok(()),
            Ok((code, said)) => {
                let mut failed = git_failed(&step, code);
                if !said.is_empty() {
                    failed.line = format!("{}: {}", failed.line, escaped(&said));
                }
                Err(failed.into())
            }
            Err(message) => Err(Failed::without_status(format!("gate: remote {step} failed: {message}")).into()),
        }
    }

    /// The checkout of `repo`'s snapshot over its tree, pinned by `lfs`'s pins ([`RunLfs::pins`]), its `lfs.storage`
    /// the run's scratch folder when it has one, else a folder below the run's lock that cannot be made, and then each
    /// LFS file checked ([`WorkGit::lfs_post_check`]). A run with no LFS content has no pointer to check and its scratch
    /// folder, when it has one, is empty: a file committed as its own bytes under a `filter=lfs` rule checks out as
    /// those bytes, and a pointer finds no object there and no server to fetch it from, so its smudge fails the
    /// checkout. A run with no scratch folder has no `filter=lfs` in its snapshot's attributes: a file the filter still
    /// reaches, through attributes the snapshot does not hold, fails the checkout, which says so when the tree's
    /// attributes name the filter ([`WorkGit::checkout_failed`]).
    fn checked_out_tree(&self, request: &Request<'_>, repo: &Repo<'_>, lfs: &RunLfs, console: &mut Console) -> Result<(), NotCheckedOut> {
        let no_storage = lfs::no_storage(request.lock);
        let pins = lfs.pins(&no_storage);
        let mut args = repo.at().to_vec();
        args.extend_from_slice(&pins);
        args.extend(["checkout", "-q", "--force", "--detach", repo.snapshot]);
        let step = repo.step("checkout");
        match self.git_code(&step, &args, console)? {
            0 => self.lfs_post_check(repo, lfs, &pins, console),
            code => Err(self.checkout_failed(repo, lfs, (&step, code), console).into()),
        }
    }

    /// The failure of `repo`'s checkout `step`, which exited with `code`: on a run with no scratch folder whose tree's
    /// attributes name the LFS filter ([`WorkGit::names_lfs_in_tree`]), the line saying a file under it was found that
    /// the snapshot does not declare ([`lfs::NO_STORAGE_CHECKOUT`]), with git's status, once the tree's untracked files
    /// are cleaned (`clean -fdq`), as a passing checkout's are, so an attributes file a command left there does not
    /// fail every later run; a clean that fails is said and leaves that line as it is. Else git's own ([`git_failed`]).
    fn checkout_failed(&self, repo: &Repo<'_>, lfs: &RunLfs, (step, code): (&str, i32), console: &mut Console) -> Failed {
        if lfs.has_scratch() || !self.names_lfs_in_tree(repo, console) {
            return git_failed(step, code);
        }
        if let Err(unclean) = self.step("clean", repo, &["clean", "-fdq"], console) {
            let _held = self.output.lock().unwrap_or_else(PoisonError::into_inner);
            console.err(&unclean.line);
        }
        Failed {
            line: format!("gate: remote {step} failed: {}", lfs::NO_STORAGE_CHECKOUT),
            status: code.cast_unsigned(),
        }
    }

    /// Whether a `.gitattributes` file of `repo`'s tree, at any depth and whatever the case of its name, tracked or
    /// untracked, holds the text `filter=lfs` (`git grep`), searched in the tree itself ([`WorkGit::quiet_in_tree`]); a
    /// file in an ignored folder is passed over, as `add -A` reads no attributes there and a package's own, in
    /// `node_modules` say, would name a filter the run never ran. A `git grep` that fails, or cannot be started, says so
    /// on the run's stderr and answers no, so the caller keeps git's own failure.
    fn names_lfs_in_tree(&self, repo: &Repo<'_>, console: &mut Console) -> bool {
        let args = ["grep", "-q", "-F", "-e", "filter=lfs", "--untracked", "--", ATTRIBUTES_FILES];
        match self.quiet_in_tree(repo, &args) {
            Ok(0) => true,
            Ok(_) => false,
            Err(message) => {
                let _held = self.output.lock().unwrap_or_else(PoisonError::into_inner);
                console.err(&format!("gate: remote leftover-LFS check could not run: {message}"));
                false
            }
        }
    }

    /// Every LFS pointer's path in `repo`'s tree checked to be a plain file of the pointer's size, as a checkout whose
    /// stat finds a file clean leaves a pointer an older run left: the files that are not deleted and checked out again
    /// once, with the same pins `pins`, and checked again. Else the git step's failure, or the refusal naming the first
    /// still wrong.
    fn lfs_post_check(&self, repo: &Repo<'_>, lfs: &RunLfs, pins: &[&str], console: &mut Console) -> Result<(), NotCheckedOut> {
        let wrong = lfs::wrong_files(&repo.tree, &lfs.pointers);
        if wrong.is_empty() {
            return Ok(());
        }
        lfs::deleted_files(&repo.tree, &wrong).map_err(NotCheckedOut::Refused)?;
        let feed: Vec<u8> = wrong.iter().flat_map(|pointer| pointer.path.bytes().chain([0])).collect();
        let mut args = vec!["--literal-pathspecs"];
        args.extend_from_slice(pins);
        args.extend([
            "checkout",
            "-q",
            "--force",
            repo.snapshot,
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]);
        self.output("lfs checkout", repo, &args, Some(&feed), RECHECKOUT_OUTPUT, console)?;
        let still = lfs::wrong_files(&repo.tree, &lfs.pointers);
        match still.first() {
            None => Ok(()),
            Some(first) => Err(NotCheckedOut::Refused(format!(
                "{} LFS file(s) are not their content after the checkout, {} among them",
                still.len(),
                escaped(&first.path)
            ))),
        }
    }

    /// The fetched snapshot of `repo` checked out by its commit over whatever the last run left, pinned by the run's
    /// LFS state `lfs` ([`WorkGit::checked_out_tree`]), untracked files removed and ignored ones (build output) kept,
    /// and no back commit of an earlier run left behind. The commit, never the ref's tip, is what is checked out, so a
    /// later push of the key changes nothing this run runs.
    fn checked_out_repo(&self, request: &Request<'_>, repo: &Repo<'_>, lfs: &RunLfs, console: &mut Console) -> Result<(), NotCheckedOut> {
        self.checked_out_tree(request, repo, lfs, console)?;
        self.step("clean", repo, &["clean", "-fdq"], console)?;
        // A pair run cleared every repo's back ref as soon as it held the lock ([`WorkGit::clear_back_refs`]).
        if request.siblings.is_empty() {
            self.clear_back_ref(request, repo, console)?;
        }
        Ok(())
    }

    /// The snapshot of `repo` fetched from its bare repository at `refs/gate/<key>`, and its commit found; else the
    /// failure, the box's own line for a commit the repository lacks after the fetch.
    fn fetched(&self, request: &Request<'_>, repo: &Repo<'_>, console: &mut Console) -> Result<(), Failed> {
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

    /// As [`WorkGit::quiet`], run in the tree itself, the folder its `--work-tree` names: a `git grep --untracked` walks
    /// the folder it was started in, never that one, so from the box's own working directory, under sshd the ssh user's
    /// profile, it would answer about the profile instead of the tree. Only a step that walks the tree takes it, and a
    /// tree folder that is missing or unreachable fails the start instead of reading the wrong folder
    /// ([`WorkGit::names_lfs_in_tree`] says so).
    fn quiet_in_tree(&self, repo: &Repo<'_>, args: &[&str]) -> Result<i32, String> {
        let mut all = repo.at().to_vec();
        all.extend_from_slice(args);
        let mut command = self.git.command(&self.args(&all))?;
        command.current_dir(&repo.tree).stdout(Stdio::null()).stderr(Stdio::null());
        Git::code_of(&mut command)
    }

    /// git's exit code for `args` and the first line of its standard error, empty when it said none, its standard output
    /// discarded, so a failure can say git's reason; else why git could not be started. `args` name the commit they
    /// search, so no working directory is set.
    fn quiet_saying(&self, repo: &Repo<'_>, args: &[&str]) -> Result<(i32, String), String> {
        let mut all = repo.at().to_vec();
        all.extend_from_slice(args);
        let mut command = self.git.command(&self.args(&all))?;
        // Started under the spawn lock, so it inherits no launch's handles.
        let output = gate_win32::spawn(command.stdout(Stdio::null()).stderr(Stdio::piped()))
            .and_then(Child::wait_with_output)
            .map_err(|error| error.to_string())?;
        let said = String::from_utf8_lossy(&output.stderr).lines().next().unwrap_or_default().to_owned();
        Ok((exit_code(output.status), said))
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
    /// Each repo's `add -A` takes the run's pins ([`RunLfs::pins`]), so a clean writes nothing into the store
    /// or the work folder's `.git\lfs`.
    fn back_commit(&self, request: &Request<'_>, lfs: &RunLfs, console: &mut Console) -> Result<Option<String>, BackFailed> {
        let mut commits = Vec::with_capacity(1 + request.siblings.len());
        for repo in request.repos() {
            commits.push(self.repo_back_commit(request, repo, lfs, console)?);
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
    fn repo_back_commit(&self, request: &Request<'_>, repo: &Repo<'_>, lfs: &RunLfs, console: &mut Console) -> Result<Option<String>, BackFailed> {
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
        let no_storage = lfs::no_storage(request.lock);
        let mut add = lfs.pins(&no_storage);
        add.extend(["add", "-A"]);
        let added = back.status("add", &add, true, console).map_err(|failed| match failed {
            // With no scratch folder, git-lfs's clean has no storage it can make: the cause when the tree's attributes
            // name the filter, and git's own line otherwise.
            BackFailed::Step(_) if !lfs.has_scratch() && self.names_lfs_in_tree(repo, console) => BackFailed::Step(lfs::NO_STORAGE_ADD.to_owned()),
            other => other,
        });
        let tree = added.and_then(|()| back.line("write-tree", &["write-tree"], true, console));
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
        command.env(crate::remote::lfs::NO_SYSTEM_ATTRIBUTES, "1");
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
        Git::forwarded(self.command(args, copied_index), read_stdout, &self.work.output, &self.work.gone, console).map_err(|failure| match failure {
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
mod interrupted_tests {
    use std::time::{Duration, SystemTime};

    use super::{INTERRUPTED_SLACK, Interrupted};

    const KEY: &str = "app-1a2b3c4d";
    const JOB: &str = r"Local\gate-remote-app-1a2b3c4d-0123456789ab";

    /// The UTC time of the record text `2026-10-19T10:00:00.0000000Z`.
    fn ten_o_clock() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_hours(497_890)
    }

    #[test]
    fn a_record_of_a_past_start_counts_files_from_its_start_less_the_slack() {
        let text = format!("2026-10-19T10:00:00.0000000Z {JOB}");
        let read = Interrupted::read(&text, KEY, ten_o_clock() + Duration::from_hours(1));
        assert_eq!(
            read,
            Interrupted {
                since: Some(ten_o_clock() - INTERRUPTED_SLACK),
                job: Some(JOB.to_owned()),
            }
        );
    }

    #[test]
    fn a_record_dated_in_the_future_counts_files_from_now() {
        let text = format!("2026-10-19T10:00:00.0000000Z {JOB}");
        let now = ten_o_clock() - Duration::from_hours(1);
        let read = Interrupted::read(&text, KEY, now);
        assert_eq!(read.since, Some(now - INTERRUPTED_SLACK), "the start is past now, so now is used");
        assert_eq!(read.job.as_deref(), Some(JOB));
    }

    #[test]
    fn an_empty_or_malformed_record_counts_every_file_and_names_no_job() {
        let everything = Interrupted { since: None, job: None };
        let other_job = r"Local\gate-remote-other-1a2b3c4d-0123456789ab";
        let cases = [
            ("empty", String::new()),
            ("no-job", "2026-10-19T10:00:00.0000000Z".to_owned()),
            ("not-a-time", format!("yesterday {JOB}")),
            ("another-work-folder's-job", format!("2026-10-19T10:00:00.0000000Z {other_job}")),
            ("short-nonce", format!("2026-10-19T10:00:00.0000000Z {}", &JOB[..JOB.len() - 1])),
            ("trailing-line-end", format!("2026-10-19T10:00:00.0000000Z {JOB}\n")),
        ];
        for (name, text) in cases {
            assert_eq!(Interrupted::read(&text, KEY, ten_o_clock()), everything, "{name}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;

    use super::{
        BackGit, CLOSED_STATUS, GIT_CONFIG_COUNT, GODOT_PATH, Look, Of, One, PATH_VARIABLE, Payload, Request, SHARING_VIOLATION, STARTING_COMMAND,
        UE_ROOT, WorkGit, before_the_job, client_gone, command_spent, command_variables, gap_spent, handed_job, look, prepared, remaining_budget,
        resolved_command, with_git_bin_first, with_path_first, with_safe_directory, with_unreal, without_engine, written_retrying,
    };
    use crate::boxside::payload::engine_env_name_refused;
    use gate_args::{EnvRefusal, env_refusal};

    use crate::boxside::forward::{ClientGone, OutputLock};
    use crate::console::Console;
    use crate::environment::{BOX_EXE_VARIABLE, BOX_JOB_VARIABLE, Variables};

    #[test]
    fn a_warm_up_budget_left_is_rounded_up_to_whole_seconds() {
        assert_eq!(remaining_budget(10, 0.0), Some(10), "nothing spent");
        assert_eq!(remaining_budget(10, 2.5), Some(8), "7.5 s left round up");
        assert_eq!(remaining_budget(10, 3.0), Some(7), "whole seconds stay as they are");
        assert_eq!(remaining_budget(10, 9.999), Some(1), "a sliver left is at least 1 s");
    }

    #[test]
    fn a_warm_up_budget_spent_leaves_none() {
        assert_eq!(remaining_budget(10, 10.0), None, "spent exactly");
        assert_eq!(remaining_budget(10, 12.25), None, "overspent by the last command's drain");
    }

    #[test]
    fn a_command_spends_its_adjusted_seconds_or_its_wall_time_once_the_sampler_failed() {
        let wall = Duration::from_millis(4_500);
        assert!(
            (command_spent(Some(1.25), wall) - 1.25).abs() < 1e-9,
            "adjusted seconds above a fifth of the wall time"
        );
        assert!(
            (command_spent(Some(0.5), wall) - 0.9).abs() < 1e-9,
            "a fifth of the wall time above the adjusted seconds"
        );
        assert!((command_spent(None, wall) - 4.5).abs() < 1e-9, "the wall time once the clock stopped");
        assert_eq!(
            remaining_budget(5, command_spent(None, wall)),
            Some(1),
            "the wall time runs the budget down"
        );
    }

    #[test]
    fn a_wait_between_commands_spends_a_fifth_of_its_wall_time() {
        assert!((gap_spent(Duration::from_millis(4_500)) - 0.9).abs() < 1e-9, "a fifth");
        assert!(gap_spent(Duration::ZERO).abs() < 1e-9, "no wait spends nothing");
    }

    /// A scratch record path for the test `test`, its folder made and no record in it.
    #[expect(clippy::expect_used, reason = "a test that cannot make its folder must fail loudly")]
    fn scratch_record(test: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-run-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder.join("k.warm")
    }

    /// `path` opened as a preflight reads a warm record: sharing reading, writing and deletion.
    #[expect(clippy::expect_used, reason = "a test that cannot make its record must fail loudly")]
    fn held_open(path: &Path) -> File {
        fs::write(path, "old").expect("the record can be made");
        OpenOptions::new()
            .read(true)
            .share_mode(gate_win32::Share::All.mode())
            .open(path)
            .expect("the record can be opened")
    }

    #[test]
    fn the_warm_record_write_waits_out_a_reader_that_holds_it() {
        let record = scratch_record("warm-retry");
        let mut reader = Some(held_open(&record));
        let mut waits = 0;

        let written = written_retrying(&record.to_string_lossy(), "hash", || {
            waits += 1;
            reader = None;
        });

        assert!(written.is_ok(), "{written:?}");
        assert!(reader.is_none(), "the reader was let go");
        assert_eq!(waits, 1, "one wait, then the reader is gone");
        assert_eq!(fs::read_to_string(&record).ok().as_deref(), Some("hash"));
    }

    #[test]
    fn the_warm_record_write_gives_up_after_ten_waits() {
        let record = scratch_record("warm-retry-give-up");
        let _reader = held_open(&record);
        let mut waits = 0;

        let written = written_retrying(&record.to_string_lossy(), "hash", || waits += 1);

        assert_eq!(
            written.map_err(|error| error.raw_os_error()),
            Err(Some(SHARING_VIOLATION)),
            "a sharing violation"
        );
        assert_eq!(waits, 10, "ten waits between eleven tries");
    }

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
        assert_eq!(
            before_the_job(look(|| Ok(true), JOB, STARTING_COMMAND), JOB, &OutputLock::default(), &mut console),
            Err(1)
        );
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
        assert_eq!(
            before_the_job(look(|| Ok(false), JOB, STARTING_COMMAND), JOB, &OutputLock::default(), &mut console),
            Ok(())
        );
        assert!(console.take_notes().is_empty(), "nothing is said");
    }

    #[test]
    fn run_that_cannot_read_the_stop_signal_before_its_job_exists_says_so_and_goes_on() {
        let mut console = Console::new();
        let seen = look(|| Err(io::Error::other("the wait failed")), JOB, STARTING_COMMAND);
        assert_eq!(before_the_job(seen, JOB, &OutputLock::default(), &mut console), Ok(()));
        assert_eq!(console.take_notes(), [unread()]);
    }

    /// A payload of a run of one repo as `gate box run` reads it, with the object `desktop`'s keys, `Desktop` and
    /// `DesktopWait`, merged in; every other key is the least `Request::read` accepts.
    #[expect(clippy::expect_used, reason = "a payload the box cannot decode must fail the case loudly")]
    fn run_payload(desktop: &serde_json::Value) -> Payload {
        let mut json = json!({
            "Snapshot": "0123456789abcdef0123456789abcdef01234567",
            "Key": "app-1a2b3c4d",
            "Work": r"C:\gate-remote\work\app-1a2b3c4d",
            "OutputFolder": r"C:\gate-remote\outputs",
            "Bare": r"C:\gate-remote\app.git",
            "Lock": r"C:\gate-remote\work\app-1a2b3c4d.lock",
            "Index": r"C:\gate-remote\work\app-1a2b3c4d.index",
            "Job": r"Local\gate-remote-app-1a2b3c4d-0123456789ab",
            "Nonce": "0123456789abcdef0123456789abcdef",
            "Command": ["cmd"],
        });
        if let Some(keys) = desktop.as_object() {
            for (key, value) in keys {
                json[key] = value.clone();
            }
        }
        Payload::decode(&STANDARD.encode(json.to_string())).expect("the payload decodes")
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a payload the box cannot read must fail the case loudly")]
    fn desktop_true_without_a_wait_is_refused() {
        let asking = run_payload(&json!({ "Desktop": true }));
        assert_eq!(Request::read(&asking).err(), Some("gate: box payload lacks DesktopWait".to_owned()));

        let not_a_bool = run_payload(&json!({ "Desktop": "yes" }));
        assert_eq!(
            Request::read(&not_a_bool).err(),
            Some("gate: box payload Desktop is not true or false".to_owned())
        );

        let waited = run_payload(&json!({ "Desktop": true, "DesktopWait": 30 }));
        let request = Request::read(&waited).expect("a run of the builder's desktop is read");
        assert!(request.desktop, "the payload asks for the builder's desktop");
        assert_eq!(request.desktop_wait, Some(30), "the whole seconds it gives");

        let ignored = run_payload(&json!({ "DesktopWait": "30" }));
        let request = Request::read(&ignored).expect("a run that asks for no desktop is read");
        assert!(!request.desktop, "the payload asks for no desktop");
        assert_eq!(request.desktop_wait, None, "a wait no run asks for is ignored");
    }

    #[test]
    fn a_warm_up_that_asks_for_the_desktop_is_refused() {
        let warm = json!({
            "Command": null,
            "WarmCommands": [["cmd"]],
            "WarmHash": "5f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0",
            "Timeout": 600,
        });
        let mut asking = warm.clone();
        asking["Desktop"] = json!(true);
        asking["DesktopWait"] = json!(30);
        assert_eq!(
            Request::read(&run_payload(&asking)).err(),
            Some("gate: box payload asks a warm-up for the desktop".to_owned())
        );

        let mut not_asking = warm;
        not_asking["Desktop"] = json!(false);
        assert_eq!(
            Request::read(&run_payload(&not_asking)).err(),
            None,
            "a warm-up of the box's own session is read"
        );
    }

    #[test]
    fn caller_pairs_never_undo_the_gates_own_settings() {
        // A payload cannot reach this: the box refuses a name the gate sets. The caller's pairs stand before every
        // setting the chain applies, so a reordering inside it is what this test would catch.
        let caller = [
            ("MSBUILDUSESERVER".to_owned(), "1".to_owned()),
            ("DOTNET_CLI_USE_MSBUILD_SERVER".to_owned(), "1".to_owned()),
            ("MSBUILDDISABLENODEREUSE".to_owned(), "0".to_owned()),
            ("Logging__EventLog__LogLevel__Default".to_owned(), "Warning".to_owned()),
        ];

        let variables = command_variables(&caller, &[], "gate-box-ab", RUN_JOB, Path::new(BOX_IMAGE));

        assert_eq!(variables.get("MSBUILDUSESERVER"), Some(OsStr::new("0")), "the gate's value wins");
        assert_eq!(variables.get("DOTNET_CLI_USE_MSBUILD_SERVER"), Some(OsStr::new("0")));
        assert_eq!(variables.get("MSBUILDDISABLENODEREUSE"), Some(OsStr::new("1")), "node reuse is off");
        assert_eq!(variables.get("SharedCompilationId"), Some(OsStr::new("gate-box-ab")), "the run's own id");
        assert_eq!(variables.get("Logging__EventLog__LogLevel__Default"), Some(OsStr::new("None")));
    }

    /// The job a payload names and the box's own image, as these cases give them to [`command_variables`].
    const RUN_JOB: &str = r"Local\gate-remote-k-ab12cd34ef56";
    const BOX_IMAGE: &str = r"C:\Program Files\gate\bin\gate.exe";

    #[test]
    fn command_variables_tell_the_command_the_box_s_exe_and_its_job_verbatim() {
        let variables = command_variables(&[], &[], "gate-box-ab", RUN_JOB, Path::new(BOX_IMAGE));

        assert_eq!(
            variables.get(BOX_JOB_VARIABLE),
            Some(OsStr::new(RUN_JOB)),
            "the job's name as the payload gave it"
        );
        assert_eq!(variables.get(BOX_EXE_VARIABLE), Some(OsStr::new(BOX_IMAGE)), "the box's own image");
    }

    #[test]
    fn command_variables_hand_a_session_0_box_s_job_by_its_global_name() {
        assert_eq!(
            handed_job(RUN_JOB, Ok(0)),
            r"Global\gate-remote-k-ab12cd34ef56",
            "session 0's Local\\ is Global\\"
        );
        assert_eq!(handed_job(RUN_JOB, Ok(2)), RUN_JOB, "another session's own Local\\");
        assert_eq!(
            handed_job(RUN_JOB, Err(io::Error::other("no session"))),
            RUN_JOB,
            "a session that cannot be read"
        );
        assert_eq!(
            handed_job(r"Global\gate-remote-k-ab12cd34ef56", Ok(0)),
            r"Global\gate-remote-k-ab12cd34ef56",
            "a name of another namespace as it is"
        );
    }

    #[test]
    fn command_variables_keep_the_box_names_over_a_caller_pair_the_box_refuses_first() {
        for name in [BOX_JOB_VARIABLE, BOX_EXE_VARIABLE, "gate_box_job", "Gate_Box_Exe"] {
            assert_eq!(
                env_refusal(name, "forged"),
                Some(EnvRefusal::GateName),
                "the box refuses a payload's {name} before any command is composed"
            );
            // Composed anyway, as an inherited value stands where a caller's pair would.
            let caller = [(name.to_owned(), "forged".to_owned())];

            let variables = command_variables(&caller, &[], "gate-box-ab", RUN_JOB, Path::new(BOX_IMAGE));

            assert_eq!(variables.get(BOX_JOB_VARIABLE), Some(OsStr::new(RUN_JOB)), "{name}");
            assert_eq!(variables.get(BOX_EXE_VARIABLE), Some(OsStr::new(BOX_IMAGE)), "{name}");
        }
    }

    #[test]
    fn caller_git_config_entries_come_before_the_gates_safe_directory() {
        // A payload cannot reach this: the box refuses a `GIT_CONFIG_COUNT` a caller gives. An inherited count is what
        // the box composes over, and the caller's pair stands exactly where an inherited one does.
        let caller = [("GIT_CONFIG_COUNT".to_owned(), "9".to_owned())];

        let variables = command_variables(&caller, &[], "gate-box-ab", RUN_JOB, Path::new(BOX_IMAGE));

        assert_eq!(
            variables.get(GIT_CONFIG_COUNT),
            Some(OsStr::new("10")),
            "the gate's entry takes the index after the caller's entries"
        );
        assert_eq!(variables.get("GIT_CONFIG_KEY_9"), Some(OsStr::new("safe.directory")));
        assert_eq!(variables.get("GIT_CONFIG_VALUE_9"), Some(OsStr::new("*")));
    }

    /// No variable the box sets on a command can be a manifest `[env]` name, so none of the gate's settings can be undone
    /// by one: every name the command's environment sets over the box's own ([`command_variables`],
    /// [`with_git_bin_first`], [`resolved_command`], [`with_unreal`]) is refused by [`engine_env_name_refused`].
    #[test]
    fn every_variable_the_box_sets_on_a_command_is_refused_as_an_engine_env_name() {
        let caller = vec![("ZZ_CALLER".to_owned(), "1".to_owned())];
        let variables = command_variables(&caller, &[], "gate-box-ab", RUN_JOB, Path::new(BOX_IMAGE));
        let variables = with_path_first(with_git_bin_first(variables, &[]), Path::new(r"C:\gate-guard\bin"));
        let place = std::env::temp_dir();
        let (variables, _) = resolved_command(Some("4.7.2"), "tool.exe", variables.with("GATE_GODOT_ROOT", r"C:\gate-guard"), &place);
        let engine = Ok(Some(PathBuf::from(r"C:\gate-guard\UE_5.8")));
        let named = with_unreal(&engine, &[], variables).map(|variables| variables.names());
        assert!(named.is_ok(), "an engine that resolved sets its variables");

        let inherited = Variables::from_process();
        let set: Vec<String> = named
            .unwrap_or_default()
            .into_iter()
            .filter(|(name, value)| inherited.get(name).is_none_or(|was| was != OsStr::new(value)))
            .map(|(name, _)| name)
            .filter(|name| name != "ZZ_CALLER")
            .collect();

        assert!(set.iter().any(|name| name == UE_ROOT), "the guard sees what the gate sets: {set:?}");
        for name in set.iter().map(String::as_str).chain([GODOT_PATH, UE_ROOT, PATH_VARIABLE]) {
            assert!(
                engine_env_name_refused(name),
                "{name} is set by the gate but not refused as an [env] name"
            );
        }
    }

    /// The base every process the box starts for a run's repo gets holds no `UE_ROOT` the box inherited, nor a value
    /// of a name the run's `EngineEnv` gives.
    #[test]
    fn the_box_s_base_environment_holds_no_ue_root() {
        let inherited = Variables::from_process()
            .with("UE_ROOT", r"C:\stale\UE_5.0")
            .with("UE_MCP_TEST_ENGINE_ROOT", r"C:\stale\engine")
            .with("ZZ_KEPT", "1");

        let base = without_engine(inherited, &["ue_mcp_test_engine_root".to_owned()]);

        assert_eq!(base.get("UE_ROOT"), None);
        assert_eq!(base.get("UE_MCP_TEST_ENGINE_ROOT"), None, "removed in any ASCII case");
        assert_eq!(base.get("ZZ_KEPT"), Some(OsStr::new("1")), "the rest is kept");
    }

    /// The `PATH` entries of `variables`, as the environment block a command is created with carries them, whatever the
    /// name's case.
    #[expect(clippy::expect_used, reason = "an environment block the case builds must fail it loudly")]
    fn path_entries(variables: &Variables) -> Vec<String> {
        let block = variables.block().expect("the environment block is built");
        String::from_utf16_lossy(block.as_wide())
            .split('\0')
            .filter(|entry| entry.split('=').next().is_some_and(|name| name.eq_ignore_ascii_case("PATH")))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn a_path_first_prepend_sets_a_path_when_there_is_none() {
        let variables = with_path_first(Variables::from_process().without("PATH"), Path::new(r"C:\Git\bin"));

        assert_eq!(path_entries(&variables), [r"PATH=C:\Git\bin"]);
    }

    #[test]
    fn a_path_first_prepend_replaces_a_path_spelled_another_way() {
        let variables = Variables::from_process().without("PATH").with("Path", r"C:\a;C:\b");

        let variables = with_path_first(variables, Path::new(r"C:\Git\bin"));

        assert_eq!(
            path_entries(&variables),
            [r"PATH=C:\Git\bin;C:\a;C:\b"],
            "one entry, whatever its name's case"
        );
    }

    #[test]
    fn a_path_first_prepend_leaves_a_bin_already_first() {
        let variables = Variables::from_process().without("PATH").with("PATH", r"C:\Git\bin;C:\a");

        let variables = with_path_first(variables, Path::new(r"C:\Git\bin"));

        assert_eq!(path_entries(&variables), [r"PATH=C:\Git\bin;C:\a"]);
    }

    #[test]
    fn a_path_first_prepend_sets_an_empty_path_to_the_bin() {
        let variables = Variables::from_process().without("PATH").with("PATH", "");

        let variables = with_path_first(variables, Path::new(r"C:\Git\bin"));

        assert_eq!(path_entries(&variables), [r"PATH=C:\Git\bin"], "never the bin and an empty entry");
    }

    #[test]
    fn a_path_first_prepend_refuses_a_folder_that_would_split_or_quote_the_value() {
        for folder in [r"C:\Git;bin", r#"C:\Git"bin"#] {
            let variables = Variables::from_process().without("PATH").with("PATH", r"C:\a;C:\b");

            let variables = with_path_first(variables, Path::new(folder));

            assert_eq!(path_entries(&variables), [r"PATH=C:\a;C:\b"], "{folder}");
        }
    }

    #[test]
    fn a_path_first_prepend_leaves_a_folder_that_is_not_unicode_alone() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt as _;

        let mut wide: Vec<u16> = r"C:\Git".encode_utf16().collect();
        wide.push(0xd800);
        let folder = PathBuf::from(OsString::from_wide(&wide));
        let variables = Variables::from_process().without("PATH").with("PATH", r"C:\a");

        let variables = with_path_first(variables, &folder);

        assert_eq!(path_entries(&variables), [r"PATH=C:\a"]);
    }

    #[test]
    fn a_command_is_a_wsl_one_by_its_first_word_alone() {
        let place = std::env::temp_dir();
        let rows: [(&[&str], bool); 6] = [
            (&["wsl", "-d", "Ubuntu"], true),
            (&[r"C:\Windows\System32\wsl.exe", "-d", "Ubuntu"], true),
            (&["WSL.EXE"], true),
            (&["pwsh", "-c", "wsl"], false),
            (&["wslconfig", "/l"], false),
            (&[], false),
        ];
        for (words, wsl) in rows {
            let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
            let one = One {
                words: &words,
                place: &place,
                timeout: None,
                of: Of::Run,
                recorded: false,
            };
            assert_eq!(one.wsl(), wsl, "{words:?}");
        }
    }

    /// Every git call the run makes in the work folder, and every one of its back commit's, reads a path's attributes
    /// from the tree's own files alone: no `core.attributesFile`, no system-wide attributes file.
    #[test]
    #[expect(clippy::expect_used, reason = "a payload the box cannot read must fail the case loudly")]
    fn every_work_and_back_git_call_reads_the_tree_s_own_attributes_alone() {
        let output = OutputLock::default();
        let work = WorkGit::ready(r"C:\gate-remote\work\app-1a2b3c4d.lock", &output, &ClientGone::default());
        let pinned = |args: &[&OsStr]| args.windows(2).any(|pair| pair == [OsStr::new("-c"), OsStr::new("core.attributesFile=")]);
        let payload = run_payload(&json!({}));
        let request = Request::read(&payload).expect("a run is read");
        let back = BackGit {
            program: Path::new("git.exe"),
            request: &request,
            repo: &request.main,
            work: &work,
        };

        let command = back.command(&["add", "-A"], true);

        assert!(
            command
                .get_envs()
                .any(|pair| pair == (OsStr::new("GIT_ATTR_NOSYSTEM"), Some(OsStr::new("1")))),
            "the back commit reads no system-wide attributes file: {:?}",
            command.get_envs().collect::<Vec<_>>()
        );
        let back_args: Vec<&OsStr> = command.get_args().collect();
        assert!(pinned(&back_args), "{back_args:?}");
        let args: Vec<&OsStr> = work.args(&["status"]).into_iter().map(OsStr::new).collect();
        assert!(pinned(&args), "{args:?}");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a payload the box cannot read must fail the case loudly")]
    fn a_prepared_command_carries_the_callers_env_pairs() {
        // A desktop run sends the prepared environment to the helper, so this is the environment it starts with too.
        let payload = run_payload(&json!({ "Env": { "NuGetAudit": "false" } }));
        let request = Request::read(&payload).expect("a run with a caller pair is read");
        let words = ["cmd".to_owned()];
        let place = std::env::temp_dir();
        let one = One {
            words: &words,
            place: &place,
            timeout: None,
            of: Of::Run,
            recorded: false,
        };
        let mut console = Console::new();

        let environment = prepared(&request, &one, &mut console).map(|prepared| prepared.environment).ok();

        let entries = environment.map(|block| String::from_utf16_lossy(block.as_wide()));
        assert!(
            entries.is_some_and(|entries| entries.split('\0').any(|entry| entry == "NuGetAudit=false")),
            "the caller's pair is in the environment: {:?}",
            console.take_notes()
        );
    }
}
