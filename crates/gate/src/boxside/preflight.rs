//! `gate box preflight`, the first call of every routed run: readies the work folder and its bare repository as the
//! PowerShell wrapper's `Initialize-WorkFolder` does, then says the version, boot and SDK lines, and a line for each
//! tool the payload's `Needs` names.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::PoisonError;
use std::thread;
use std::time::Duration;

use gate_args::BoxSeams;
use gate_win32::{Make, Share};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

use crate::answer::USAGE_STATUS;
use crate::boxside::boot;
use crate::boxside::forward::{ChildPipes, DRAIN, Drained, OutputLock};
use crate::boxside::payload::{Pair, Payload};
use crate::boxside::probe;
use crate::boxside::run::{FILE_FLAG_OPEN_REPARSE_POINT, LAYOUT_SINGLE};
use crate::boxside::tools;
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::git;
use crate::run::{ERROR_FILE_NOT_FOUND, find_program};
use crate::version::build_version;

/// The status of a step that failed with no status of its own: git or a folder that could not be made or written.
const FAILED_STATUS: u32 = 1;

/// The git variables that would point git at another repository, index, object store or config than the work folder's.
pub(crate) const GIT_REDIRECTS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// A preflight step that failed: the line saying so, and the status the gate exits with.
pub(crate) struct Failed {
    pub(crate) line: String,
    pub(crate) status: u32,
}

impl Failed {
    pub(crate) fn without_status(line: String) -> Failed {
        Failed { line, status: FAILED_STATUS }
    }
}

/// The failure of git `step` that exited with `code`: the line shows the code signed, as `gate run` and the wrapper's
/// `$LASTEXITCODE` show it, and the gate exits with its whole 32 bits.
pub(crate) fn git_failed(step: &str, code: i32) -> Failed {
    Failed {
        line: format!("gate: remote {step} failed (status {code})"),
        status: code.cast_unsigned(),
    }
}

/// Readies the folders the payload's `Bare` and `Work` name, then says the three trailing lines, the SDK list read
/// within `--test-dotnet-limit-ms` when `seams` gives it, else [`git::DOTNET_LIMIT`], and the tool lines, probed within
/// `--test-probe-limit-ms` when `seams` gives it, else [`probe::PROBE_LIMIT`]; returns the status.
pub(crate) fn preflight(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u32 {
    let read = payload
        .path("Bare")
        .and_then(|bare| Ok((bare, payload.work_folder()?.1, payload.pair()?)));
    let (bare, work, pair) = match read {
        Ok(folders) => folders,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    let readied = match &pair {
        None => ready(bare, work, console),
        Some(pair) => ready_bares(bare, work, pair, console),
    };
    if let Err(failed) = readied {
        console.err(&failed.line);
        return failed.status;
    }
    console.out(&format!("gate: remote version {}", build_version()));
    boot::say(console);
    say_sdks_and_tools(payload, seams, console);
    0
}

/// Says the SDK line, the list read within `--test-dotnet-limit-ms` when `seams` gives it, else
/// [`git::DOTNET_LIMIT`], then one line on stdout for each tool the payload's `Needs` names, in its order. The tools are
/// probed within `--test-probe-limit-ms` when `seams` gives it, else [`probe::PROBE_LIMIT`], while dotnet lists the
/// SDKs. A `Needs` that is not an object of texts is said on stderr, probes nothing, and fails nothing. The minimum each
/// need carries is not read: the box reports, the client compares.
fn say_sdks_and_tools(payload: &Payload, seams: &BoxSeams, console: &mut Console) {
    let names: Result<Vec<String>, String> = payload
        .pairs("Needs")
        .map(|needs| needs.into_iter().map(|(name, _)| name).collect())
        .map_err(|line| format!("{line}; probing nothing"));
    let probe_limit = seams.probe_limit_ms.map_or(probe::PROBE_LIMIT, Duration::from_millis);
    thread::scope(|scope| {
        let probes = names.as_ref().ok().filter(|names| !names.is_empty()).map(|names| {
            (
                names,
                scope.spawn(move || probe::probe(names, probe_limit, Ok(Variables::from_process()), None)),
            )
        });
        say_sdks(seams.dotnet_limit_ms.map_or(git::DOTNET_LIMIT, Duration::from_millis), console);
        if let Err(line) = &names {
            console.err(line);
        }
        let Some((names, probes)) = probes else {
            return;
        };
        let found = probes.join().unwrap_or_else(|panic| {
            let why = probe::panic_reason(&*panic);
            names.iter().map(|name| (name.clone(), probe::Probed::Skipped(why.clone()))).collect()
        });
        for (name, probed) in found {
            console.out(&probe::tool_line(&name, &probed));
        }
    });
}

/// The folder above `work`; the bare repository, said as created; and the work folder's repository ([`ready_tree`],
/// which readies nothing through a link at its `.git`), unless the work folder is laid out for another layout
/// ([`laid_out_for_one_repo`]): a run of the key changes it under its lock, which the preflight does not take.
fn ready(bare: &str, work: &str, console: &mut Console) -> Result<(), Failed> {
    let git = Git::find();
    create_parent(work)?;
    create_bare(&git, bare, None, console)?;
    if !laid_out_for_one_repo(work) {
        return Ok(());
    }
    ready_tree(&git, bare, work, None, console).map_err(|unready| match unready {
        Unready::Git(failed) => failed,
        Unready::Folder(why) => Failed::without_status(format!("gate: remote {why}")),
    })
}

/// Whether the layout record of the work folder `work`, `{work}.layout`, read without following a link, says the
/// layout of a run of one repo or is missing; a record that says a pair's layout or a clear in progress, or that cannot
/// be read, is not.
fn laid_out_for_one_repo(work: &str) -> bool {
    match tools::read_record(&format!("{work}.layout")) {
        Ok(None) => true,
        Ok(Some(text)) => text.eq_ignore_ascii_case(LAYOUT_SINGLE),
        Err(_) => false,
    }
}

/// For a run of the sibling pair `pair`: the folder above `work`, and the bare repository of the main checkout,
/// `bare`, and of each sibling, each said as created, a failure naming its repo. Nothing inside `work`, which only a
/// run holding its lock changes.
fn ready_bares(bare: &str, work: &str, pair: &Pair, console: &mut Console) -> Result<(), Failed> {
    let git = Git::find();
    create_parent(work)?;
    create_bare(&git, bare, Some(pair.leaf()), console)?;
    for sibling in pair.siblings() {
        create_bare(&git, sibling.bare(), Some(sibling.name()), console)?;
    }
    Ok(())
}

/// Creates the folder above `path` when it has one.
fn create_parent(path: &str) -> Result<(), Failed> {
    match Path::new(path).parent().filter(|parent| !parent.as_os_str().is_empty()) {
        Some(parent) => create_folder(parent),
        None => Ok(()),
    }
}

/// Creates `folder` and the folders above it; else the line saying it could not.
fn create_folder(folder: &Path) -> Result<(), Failed> {
    fs::create_dir_all(folder)
        .map_err(|error| Failed::without_status(format!("gate: remote could not create the folder {}: {error}", folder.display())))
}

/// Creates the bare repository `bare` when it is missing, said as created; a failure's line names the repo `name` on a
/// pair run ([`failed_for`]).
fn create_bare(git: &Git, bare: &str, name: Option<&str>, console: &mut Console) -> Result<(), Failed> {
    if !Path::new(bare).exists() {
        let step = "git init --bare";
        git.step(&for_repo(step, name), &["init", "--bare", "-q", "--", bare])
            .map_err(|failed| failed_for(failed, step, name))?;
        console.out(&format!("gate: remote created {bare}"));
    }
    Ok(())
}

/// How [`ready_tree`] runs git: the preflight's [`Git`], or the run's, with its safety options and its output forwarded
/// under the box's output lock.
pub(super) trait TreeGit {
    /// Runs git with `args`; a non-zero status, or a git that cannot be started, fails `step`.
    fn run_step(&self, step: &str, args: &[&str], console: &mut Console) -> Result<(), Failed>;
    /// git's exit code for `args`, its stdout discarded; else why it could not be started.
    fn run_code(&self, args: &[&str]) -> Result<i32, String>;
    /// Whether the caller holds the work folder's lock, so a link where a folder should be may be removed and the
    /// folder made again ([`write_alternates`]).
    fn holds_the_lock(&self) -> bool;
}

impl TreeGit for Git {
    fn run_step(&self, step: &str, args: &[&str], _console: &mut Console) -> Result<(), Failed> {
        self.step(step, args)
    }

    fn run_code(&self, args: &[&str]) -> Result<i32, String> {
        self.code(args, Stdio::null(), Stdio::inherit())
    }

    /// The preflight takes no lock.
    fn holds_the_lock(&self) -> bool {
        false
    }
}

/// Readies the repository of the tree `tree`: created when nothing has the name `.git` there, read without following a
/// link, its `origin` the bare repository `bare`, its line-ending config, and its alternates file pointing at `bare`'s
/// objects ([`write_alternates`]). Without the lock, a link, a junction or a file at the tree's `.git`, `.git\objects`
/// or `.git\objects\info`, or anything but a file at its `.git\config` ([`has_a_link_in_its_git`]), readies nothing:
/// no git step runs and nothing is written, and the run repairs it under the lock. When `git` holds the work folder's
/// lock, those are repaired before the first git step ([`repair_git`]). So each git call goes through `git` and names
/// the tree's own `.git` folder, config and the tree, and no other repository's config, objects or work tree is
/// reached. A step's line names the repo `name` when given, as a pair run's lines do ([`tree_step`]).
pub(super) fn ready_tree(git: &impl TreeGit, bare: &str, tree: &str, name: Option<&str>, console: &mut Console) -> Result<(), Unready> {
    if git.holds_the_lock() {
        repair_git(tree).map_err(Unready::Folder)?;
    } else if has_a_link_in_its_git(Path::new(tree)) {
        return Ok(());
    }
    // A run removes a link, a junction or a file named `.git` before it readies a tree (`plain_tree` in the run).
    if fs::symlink_metadata(Path::new(tree).join(".git")).is_err() {
        tree_step(git, "git init", name, &["init", "-q", "--", tree], console)?;
    }
    let git_dir = format!(r"--git-dir={tree}\.git");
    let work_tree = format!("--work-tree={tree}");
    let (git_dir, work_tree) = (git_dir.as_str(), work_tree.as_str());
    // Checked apart from .git, so a preflight cut short between the two steps is finished by the next one. A git that
    // cannot be started here fails the next step, which names it.
    if git.run_code(&[git_dir, work_tree, "config", "--get", "remote.origin.url"]) != Ok(0) {
        let args = [git_dir, work_tree, "remote", "add", "--", "origin", bare];
        tree_step(git, "git remote add", name, &args, console)?;
    }
    // The tree's own config, so the checkout and the sync-back keep the snapshot's bytes whatever the builder's system
    // and global git config say.
    for (setting, value) in [("core.autocrlf", "false"), ("core.eol", "lf")] {
        let args = [git_dir, work_tree, "config", setting, value];
        tree_step(git, &format!("git config {setting}"), name, &args, console)?;
    }
    write_alternates(bare, tree, git.holds_the_lock()).map_err(Unready::Folder)
}

/// Runs the git step `step` with `args` through `git`, named for the repo `name` on a pair run ([`for_repo`]), its
/// failure's line as [`failed_for`] gives it.
fn tree_step(git: &impl TreeGit, step: &str, name: Option<&str>, args: &[&str], console: &mut Console) -> Result<(), Unready> {
    git.run_step(&for_repo(step, name), args, console)
        .map_err(|failed| Unready::Git(failed_for(failed, step, name)))
}

/// The step `step` as a pair run's git step naming the repo `name` is said: `{step} for {name}`, which cannot be read
/// as one of the step's own words; `step` as it is on a run of one repo.
fn for_repo(step: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{step} for {name}"),
        None => step.to_owned(),
    }
}

/// The failure `failed` of the git step `step`, run as [`for_repo`] names it: its line says `{step} failed for {name}`
/// on a pair run, and is as it is on a run of one repo.
fn failed_for(failed: Failed, step: &str, name: Option<&str>) -> Failed {
    let Some(name) = name else {
        return failed;
    };
    let line = failed
        .line
        .replacen(&format!("{step} for {name} failed"), &format!("{step} failed for {name}"), 1);
    Failed { line, ..failed }
}

/// The step `step` as its lines name it: followed by the repo's name `name` on a pair run, as it is on a run of one
/// repo.
pub(super) fn named_step(step: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{step} {name}"),
        None => step.to_owned(),
    }
}

/// Why a tree could not be readied ([`ready_tree`]).
pub(super) enum Unready {
    /// A git step failed.
    Git(Failed),
    /// A step on the file system failed, for this reason, which says what could not be done.
    Folder(String),
}

/// The tree `tree`'s `.git`, `.git\objects` and `.git\objects\info`.
fn git_folders(tree: &Path) -> [PathBuf; 3] {
    let git = tree.join(".git");
    let objects = git.join("objects");
    let info = objects.join("info");
    [git, objects, info]
}

/// Whether the config file `config` is missing or a file itself, read without following a link: a link there would
/// lead git's config writes, which replace the file through its lock file, into another repository's config.
fn config_is_plain(config: &Path) -> bool {
    !fs::symlink_metadata(config).is_ok_and(|metadata| !metadata.is_file())
}

/// Whether any of the tree `tree`'s `.git`, `.git\objects` and `.git\objects\info` is there and is not a folder itself,
/// or its `.git\config` is there and is not a file itself, read without following a link: a link, a junction or a
/// file, through which git or the alternates file would reach another repository.
fn has_a_link_in_its_git(tree: &Path) -> bool {
    let [git, objects, info] = git_folders(tree);
    let not_a_folder = |folder: &PathBuf| fs::symlink_metadata(folder).is_ok_and(|metadata| !metadata.is_dir());
    [&git, &objects, &info].into_iter().any(not_a_folder) || !config_is_plain(&git.join("config"))
}

/// Whether the tree `tree`'s `.git`, `.git\objects` and `.git\objects\info` are each a folder itself
/// ([`is_real_folder`]) and its `.git\config` is missing or a file itself ([`config_is_plain`]).
pub(super) fn git_is_plain(tree: &Path) -> bool {
    let folders = git_folders(tree);
    folders.iter().all(|folder| is_real_folder(folder)) && config_is_plain(&folders[0].join("config"))
}

/// Under the work folder's lock, before any git step: a `.git\config` that is not a file itself removed as itself,
/// for git to write afresh, and `.git\objects` and `.git\objects\info` made folders of their own ([`made_folder`]), so
/// no git step reads or writes through a link there. Nothing when the tree has no `.git` folder, which `git init` then
/// makes. Else why not.
fn repair_git(tree: &str) -> Result<(), String> {
    let [git, objects, info] = git_folders(Path::new(tree));
    if !is_real_folder(&git) {
        return Ok(());
    }
    let config = git.join("config");
    if !config_is_plain(&config) {
        remove_entry(&config).map_err(|error| format!("cannot remove {}: {error}", config.display()))?;
    }
    let alternates = info.join("alternates");
    for folder in [&objects, &info] {
        made_folder(folder, true).map_err(|why| format!("could not write {}: {why}", alternates.display()))?;
    }
    Ok(())
}

/// Writes `<bare with / separators>/objects` and LF, UTF-8 without a BOM, as the tree's alternates file unless it
/// already holds exactly that, every name read without following a link. The folders `.git\objects` and
/// `.git\objects\info` are made when missing ([`made_folder`]), and the file written ([`written`]). Else why not:
/// `could not write {file}: ` and the reason.
fn write_alternates(bare: &str, tree: &str, repair: bool) -> Result<(), String> {
    let objects = Path::new(tree).join(".git").join("objects");
    let info = objects.join("info");
    let alternates = info.join("alternates");
    let cannot = |why: String| format!("could not write {}: {why}", alternates.display());
    for folder in [&objects, &info] {
        made_folder(folder, repair).map_err(cannot)?;
    }
    let text = format!("{}/objects\n", bare.replace('\\', "/"));
    written(&alternates, &text).map_err(cannot)
}

/// Makes the folder `folder` when nothing has its name, read without following a link; one that is a link, a junction
/// or a file is removed as itself and made again when `repair`, else refused. Else why not.
pub(super) fn made_folder(folder: &Path, repair: bool) -> Result<(), String> {
    match fs::symlink_metadata(folder) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) if !repair => return Err(format!("{} is a link, a junction or a file", folder.display())),
        Ok(_) => remove_entry(folder).map_err(|error| format!("cannot remove {}: {error}", folder.display()))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    fs::create_dir(folder).map_err(|error| format!("cannot create {}: {error}", folder.display()))
}

/// Writes `text` as the file `file` unless it already holds exactly that, its folder opened as itself and held, and the
/// file read, removed and made beneath that handle ([`written_beneath`]). Else why not, a folder that is a link, a
/// junction or not a folder refused.
pub(super) fn written(file: &Path, text: &str) -> Result<(), String> {
    let (Some(parent), Some(name)) = (file.parent(), file.file_name().and_then(|name| name.to_str())) else {
        return Err(format!("{} names no file in a folder", file.display()));
    };
    let folder = OpenOptions::new()
        .read(true)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(parent)
        .map_err(|error| error.to_string())?;
    if !is_folder_itself(&folder).map_err(|error| error.to_string())? {
        return Err(format!("{} is a link, a junction or not a folder", parent.display()));
    }
    written_beneath(&folder, name, text)
}

/// Whether the open `opened` is a folder itself: a folder, not a link or a junction.
pub(super) fn is_folder_itself(opened: &File) -> io::Result<bool> {
    let metadata = opened.metadata()?;
    Ok(metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}

/// Writes `text` as the file `name` in the held folder `folder` unless it already holds exactly that: whatever is at the
/// name opened as itself beneath the handle, never followed; a plain file of other bytes, a link, a junction or
/// anything else deleted as itself (a folder of its own emptied beneath its handle first,
/// [`gate_win32::delete_tree_opened`]), and a new file made beneath the handle, refused when anything took the name
/// meanwhile. Else why not.
pub(super) fn written_beneath(folder: &File, name: &str, text: &str) -> Result<(), String> {
    match gate_win32::open_beneath_as(folder, name, FILE_GENERIC_READ | DELETE, Share::All, Make::Open) {
        Ok(found) => {
            if holds_already(found, text)? {
                return Ok(());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let mut made =
        gate_win32::open_beneath_as(folder, name, FILE_GENERIC_WRITE, Share::NoDelete, Make::NewFile).map_err(|error| error.to_string())?;
    made.write_all(text.as_bytes()).map_err(|error| error.to_string())
}

/// Whether `found`, an entry opened as itself with `DELETE` access, is a plain file holding exactly `text`; anything
/// else is deleted as itself, through its handle, a folder of its own emptied beneath that handle first. Else why it
/// could not be deleted.
fn holds_already(mut found: File, text: &str) -> Result<bool, String> {
    let metadata = found.metadata().map_err(|error| error.to_string())?;
    let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if metadata.is_file() && !reparse {
        let mut held = Vec::new();
        if found.read_to_end(&mut held).is_ok() && held == text.as_bytes() {
            return Ok(true);
        }
    }
    gate_win32::delete_tree_opened(&found).map_err(|error| error.to_string())?;
    Ok(false)
}

/// Removes the entry `path` as itself, never what a link or a junction there leads to: a folder, or a link or a
/// junction to one, with std's `remove_dir_all`, which removes a link it meets rather than following it; anything else
/// with `remove_file`. Nothing when nothing has the name.
pub(super) fn remove_entry(path: &Path) -> io::Result<()> {
    let kind = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if kind.is_dir() || kind.is_symlink_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Whether `path` is a folder itself, read without following a link: a link or a junction there is none.
pub(super) fn is_real_folder(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// git, found once along the gate's own `PATH` and `PATHEXT`.
pub(crate) struct Git {
    pub(crate) program: Result<PathBuf, String>,
}

impl Git {
    pub(crate) fn find() -> Git {
        let program = locate("git").and_then(|found| found.ok_or_else(|| gate_win32::win32_message(ERROR_FILE_NOT_FOUND)));
        Git { program }
    }

    /// Runs git with `args`, its output passed through; a non-zero status, or a git that cannot be started, fails
    /// `step`, named as the wrapper names it.
    pub(crate) fn step(&self, step: &str, args: &[&str]) -> Result<(), Failed> {
        match self.code(args, Stdio::inherit(), Stdio::inherit()) {
            Ok(0) => Ok(()),
            Ok(code) => Err(git_failed(step, code)),
            Err(message) => Err(Failed::without_status(format!("gate: remote {step} failed: {message}"))),
        }
    }

    /// git's exit code for `args`, its stdout sent to `stdout` and its stderr to `stderr`; else why it could not be
    /// started.
    pub(crate) fn code(&self, args: &[&str], stdout: Stdio, stderr: Stdio) -> Result<i32, String> {
        let mut command = self.command(args)?;
        // Started under the spawn lock, so it inherits no launch's handles.
        let status = gate_win32::spawn(command.stdout(stdout).stderr(stderr))
            .and_then(|mut child| child.wait())
            .map_err(|error| error.to_string())?;
        Ok(exit_code(status))
    }

    /// git with `args`, reading no standard input and none of the variables that would point it elsewhere; else why
    /// it cannot be started.
    pub(crate) fn command(&self, args: &[&str]) -> Result<Command, String> {
        let program = self.program.as_ref().map_err(Clone::clone)?;
        let mut command = Command::new(program);
        for name in GIT_REDIRECTS {
            command.env_remove(name);
        }
        command.args(args).stdin(Stdio::null());
        Ok(command)
    }

    /// Starts the git `command` under the spawn lock, its stderr, and its stdout unless `read_stdout`, forwarded line by
    /// line under `lock` ([`ChildPipes`]), so no heartbeat lands inside a line of git's, and waits for it to end. A
    /// process still holding git's output once the readers have drained for [`DRAIN`] is said under `lock`, and the
    /// run goes on. git's output, its stdout empty unless read; else why it could not be started or waited for, or that
    /// a line of git's could not be written to the box's own output: the client closed the connection.
    pub(crate) fn forwarded(command: Command, read_stdout: bool, lock: &OutputLock, console: &mut Console) -> Result<Output, GitFailure> {
        let (output, drained) = run_forwarded(command, read_stdout, lock).map_err(GitFailure::Failed)?;
        if drained.pipe_closed {
            return Err(GitFailure::ClientGone);
        }
        if !drained.all_ended {
            let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
            console.err(GIT_LEFTOVER);
        }
        Ok(output)
    }
}

/// The line said when a leftover process still holds git's output open once git exited.
const GIT_LEFTOVER: &str = "gate: remote git's output is still held open by a leftover process; stopped reading";

/// Why a git whose output the box forwards gave no output.
pub(crate) enum GitFailure {
    /// It could not be started or waited for, for this reason.
    Failed(String),
    /// A line of its could not be written to the box's own output: the client closed the connection.
    ClientGone,
}

/// [`Git::forwarded`] up to the drain: git's output, and what the readers' drain found; else why it could not be
/// started or waited for. `command` is dropped once git is started, so the readers see the end of its pipes once git
/// and whatever it started have closed them.
fn run_forwarded(mut command: Command, read_stdout: bool, lock: &OutputLock) -> Result<(Output, Drained), String> {
    let ChildPipes { stdout, stderr, forwarders } = ChildPipes::open("git's", !read_stdout, lock).map_err(|error| error.to_string())?;
    // Started under the spawn lock, so it inherits no launch's handles.
    let started = gate_win32::spawn(command.stdout(stdout).stderr(stderr));
    drop(command);
    let output = started.and_then(Child::wait_with_output);
    let drained = forwarders.drain(DRAIN);
    Ok((output.map_err(|error| error.to_string())?, drained))
}

/// The exit code of a process that ran to its end; on Windows every such process has one.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(FAILED_STATUS.cast_signed())
}

/// The file `word` names, found as `gate run` finds a command's program: along the gate's own `PATH` and `PATHEXT`,
/// never in the working folder unless `PATH` names it. `None` when it is not found; else why it could not be looked
/// for.
pub(crate) fn locate(word: &str) -> Result<Option<PathBuf>, String> {
    let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
    match find_program(word, &Variables::from_process(), &folder) {
        Ok(program) => Ok(Some(program)),
        // `find_program` says a word it does not find with this message, and every other failure with another.
        Err(message) if message == gate_win32::win32_message(ERROR_FILE_NOT_FOUND) => Ok(None),
        Err(message) => Err(message),
    }
}

/// Says the SDK line on stdout, the list read within `limit`; a list that cannot be read is said on stderr instead, and
/// fails nothing.
fn say_sdks(limit: Duration, console: &mut Console) {
    match sdks(limit) {
        Ok(list) => console.out(&format!("gate: remote sdks {list}")),
        Err(message) => console.err(&format!("gate: remote could not read the SDK list: {message}")),
    }
}

/// The SDK list of `dotnet --list-sdks`, `none` when no dotnet is found along the gate's own `PATH`; else why it could
/// not be read. dotnet is run as [`git::sdk_listing`] runs it, with no console interrupt and no worktree to keep it out
/// of, ended with its job past `limit`.
fn sdks(limit: Duration) -> Result<String, String> {
    let listing = git::sdk_listing(Variables::from_process(), limit, None, None)?;
    Ok(sdk_list(&listing))
}

/// The first space-separated word of each non-empty line of `listing`, joined by `,`; `none` when there is none.
fn sdk_list(listing: &str) -> String {
    let versions = crate::remote::parity::sdk_versions(listing);
    if versions.is_empty() { "none".to_owned() } else { versions.join(",") }
}

#[cfg(test)]
mod tests {
    use std::sync::PoisonError;

    use super::{Git, git_failed, run_forwarded, sdk_list};
    use crate::boxside::forward::OutputLock;

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose git cannot start must fail loudly")]
    fn git_writes_its_stderr_through_the_output_lock() {
        let output = OutputLock::default();
        let missing = std::env::temp_dir().join(format!("gate-preflight-no-such-repository-{}", std::process::id()));
        let git_dir = format!("--git-dir={}", missing.display());
        let command = Git::find()
            .command(&[&git_dir, "rev-parse", "HEAD"])
            .expect("git is on PATH; the unit tests need it");
        let held = output.lock().unwrap_or_else(PoisonError::into_inner);

        // git says on stderr that the repository does not exist, then exits.
        let ran = run_forwarded(command, false, &output);
        drop(held);

        let (ran, drained) = ran.expect("git starts");
        assert_eq!(ran.status.code(), Some(128), "git's own status");
        assert!(
            !drained.all_ended,
            "the reader of git's stderr waits for the held output lock, so it cannot end within the drain"
        );
    }

    #[test]
    fn a_git_status_is_said_signed_and_exited_with_whole() {
        let failed = git_failed("git init", -1_073_741_819);
        assert_eq!(failed.line, "gate: remote git init failed (status -1073741819)");
        assert_eq!(failed.status, 3_221_225_477);
        let failed = git_failed("git config core.eol", 128);
        assert_eq!(failed.line, "gate: remote git config core.eol failed (status 128)");
        assert_eq!(failed.status, 128);
    }

    #[test]
    fn the_sdk_list_is_the_first_word_of_each_line() {
        let listing = "8.0.404 [C:\\Program Files\\dotnet\\sdk]\r\n\r\n  9.0.100 [C:\\Program Files\\dotnet\\sdk]\r\n";
        assert_eq!(sdk_list(listing), "8.0.404,9.0.100");
        assert_eq!(sdk_list(""), "none");
        assert_eq!(sdk_list("\r\n  \n"), "none");
    }
}
