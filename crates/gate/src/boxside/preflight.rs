//! `gate box preflight`, the first call of every routed run: readies the work folder and its bare repository as the
//! PowerShell wrapper's `Initialize-WorkFolder` does, then says the version, warm, boot and SDK lines, and a line for
//! each tool the payload's `Needs` names.

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
    // Read once, before the folders are readied, which would make a missing `.git` and so a stale record look current.
    let layout = layout_record(work);
    let warm = warm_state(work, &layout);
    let readied = match &pair {
        None => ready(bare, work, &layout, console),
        Some(pair) => ready_bares(bare, work, pair, console),
    };
    if let Err(failed) = readied {
        console.err(&failed.line);
        return failed.status;
    }
    console.out(&format!("gate: remote version {}", build_version()));
    console.out(&format!("gate: remote warm {}", warm.as_deref().unwrap_or("none")));
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

/// The most bytes of a warm record [`warm_state`] reads: one past a hash, so a longer record is seen as not one.
const WARM_READ_LIMIT: u64 = 65;

/// The hash the work folder `work`'s warm record, `{Work}.warm`, holds; `None` unless every tree its layout record
/// `layout` names ([`laid_out_trees`]) has a plain `.git` ([`git_is_plain`]), as a work folder that is missing has
/// not, and `None` when there is no record, when it cannot be opened or read or is not a plain file, and when it holds
/// anything but exactly 64 lower-case hex digits, a line end included. The record is opened sharing reading, writing
/// and deletion, so this read never makes a run's removal of it fail.
fn warm_state(work: &str, layout: &io::Result<Option<String>>) -> Option<String> {
    let trees = laid_out_trees(work, layout)?;
    if !trees.iter().all(|tree| git_is_plain(Path::new(tree))) {
        return None;
    }
    let mut bytes = Vec::new();
    shared_record(&crate::boxside::run::warm_record(work))
        .ok()?
        .take(WARM_READ_LIMIT)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    crate::outputs::hash::is_lower_hex(&text, crate::outputs::hash::SHA256_DIGITS).then_some(text)
}

/// The record `record` beside the work folder opened as the preflight reads one: sharing reading, writing and
/// deletion, without following a link there, and only as a plain file.
fn shared_record(record: &str) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(Share::All.mode())
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(record)?;
    gate_win32::check_plain_file(&file)?;
    Ok(file)
}

/// The text of the work folder `work`'s layout record, `{work}.layout`, read once by the preflight ([`shared_record`]);
/// `Ok(None)` when there is none.
fn layout_record(work: &str) -> io::Result<Option<String>> {
    let mut file = match shared_record(&format!("{work}.layout")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

/// The trees the layout record of the work folder `work`, `layout`, says it holds: `work` itself for a run of one
/// repo's layout or no record, `{work}\{name}` for the leaf and each sibling of a pair's; `None` for a clear in
/// progress, a record that cannot be read, or one that names no layout or a name that is not a folder name.
fn laid_out_trees(work: &str, layout: &io::Result<Option<String>>) -> Option<Vec<String>> {
    let text = match layout {
        Ok(None) => return Some(vec![work.to_owned()]),
        Ok(Some(text)) => text,
        Err(_) => return None,
    };
    if text.eq_ignore_ascii_case(LAYOUT_SINGLE) {
        return Some(vec![work.to_owned()]);
    }
    let names: Vec<&str> = text.strip_prefix("pair ")?.split(' ').collect();
    let named = names.len() >= 2 && names.iter().all(|name| gate_router::is_layout_name(name));
    named.then(|| names.iter().map(|name| format!(r"{work}\{name}")).collect())
}

/// The folder above `work`; the bare repository, said as created; and the work folder's repository ([`ready_tree`],
/// which readies nothing through a link at its `.git`), unless the work folder is laid out for another layout
/// ([`laid_out_for_one_repo`] of its record `layout`): a run of the key changes it under its lock, which the
/// preflight does not take.
fn ready(bare: &str, work: &str, layout: &io::Result<Option<String>>, console: &mut Console) -> Result<(), Failed> {
    let git = Git::find();
    create_parent(work)?;
    create_bare(&git, bare, None, console)?;
    if !laid_out_for_one_repo(layout) {
        return Ok(());
    }
    let warm = crate::boxside::run::warm_record(work);
    let at = TreeAt {
        tree: work,
        bare,
        warm: &warm,
    };
    ready_tree(&git, &at, None, console).map_err(|unready| match unready {
        Unready::Git(failed) => failed,
        Unready::Folder(why) => Failed::without_status(format!("gate: remote {why}")),
    })
}

/// Whether the work folder's layout record `layout` ([`layout_record`]) says the layout of a run of one repo or is
/// missing; a record that says a pair's layout or a clear in progress, or that cannot be read, is not.
fn laid_out_for_one_repo(layout: &io::Result<Option<String>>) -> bool {
    match layout {
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

/// Where [`ready_tree`] readies a repository: the tree, its repo's bare repository, and the warm record of the work
/// folder the tree lies in, `{Work}.warm`.
pub(super) struct TreeAt<'a> {
    pub(super) tree: &'a str,
    pub(super) bare: &'a str,
    pub(super) warm: &'a str,
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
/// A `.git` made anew is no warm tree: the work folder's warm record goes first, before `git init`. A record that
/// cannot be removed readies nothing: the preflight leaves the tree to the run, whose own removal refuses it.
pub(super) fn ready_tree(git: &impl TreeGit, at: &TreeAt<'_>, name: Option<&str>, console: &mut Console) -> Result<(), Unready> {
    let (bare, tree) = (at.bare, at.tree);
    if git.holds_the_lock() {
        repair_git(tree).map_err(Unready::Folder)?;
    } else if has_a_link_in_its_git(Path::new(tree)) {
        return Ok(());
    }
    // A run removes a link, a junction or a file named `.git` before it readies a tree (`plain_tree` in the run).
    let made = fs::symlink_metadata(Path::new(tree).join(".git")).is_err();
    if made {
        if let Err(error) = remove_entry(Path::new(at.warm)) {
            if !git.holds_the_lock() {
                // The preflight leaves the tree to the run, which removes the record under the lock or is refused.
                console.err(&format!(
                    "gate: remote could not remove {}: {}",
                    crate::boxside::payload::escaped(at.warm),
                    crate::boxside::payload::escaped(&error.to_string())
                ));
                return Ok(());
            }
            return Err(Unready::Folder(format!("cannot remove {}: {error}", at.warm)));
        }
        tree_step(git, "git init", name, &["init", "-q", "--", tree], console)?;
    }
    if !made || !fresh_config_written(bare, tree, name)? {
        configure(git, bare, tree, name, console)?;
    }
    if let Some(line) = write_alternates(bare, tree, git.holds_the_lock()).map_err(Unready::Folder)? {
        console.err(&line);
    }
    Ok(())
}

/// The tree `tree`'s `origin`, the bare repository `bare`, added unless it has one, and its line-ending config, each
/// written by a git step through `git`, its line naming the repo `name` when given ([`tree_step`]).
fn configure(git: &impl TreeGit, bare: &str, tree: &str, name: Option<&str>, console: &mut Console) -> Result<(), Unready> {
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
    Ok(())
}

/// The step whose line a failed [`fresh_config_written`] says, as the `git remote add` it stands in for would.
const REMOTE_ADD: &str = "git remote add";
/// The refspec `git remote add` gives a remote's `fetch`.
const ORIGIN_FETCH: &str = "+refs/heads/*:refs/remotes/origin/*";

/// The config's name in the tree's `.git` folder, and the lock git takes to replace it.
const CONFIG: &str = "config";
const CONFIG_LOCK: &str = "config.lock";

/// Writes the config `git init` just made in the tree `tree` once, with the `origin` remote `bare` and the line-ending
/// config in it, as [`configure`]'s git steps would leave it ([`fresh_config`]): whether it did. The tree's `.git` is
/// opened as itself and held, and git's own `config.lock` taken beneath it before the config is read as itself and
/// replaced whole ([`replace_config`]). `false`, with nothing written, so the git steps write it,
/// when `.git` is not a folder itself, the config is missing, not a plain file or not UTF-8, it holds more than `git
/// init`'s own `[core]` section, or its lock is held. A read or a write that fails says the `git remote add` step's
/// line, naming the repo `name` when given, and leaves the config as it was.
fn fresh_config_written(bare: &str, tree: &str, name: Option<&str>) -> Result<bool, Unready> {
    let git_folder = Path::new(tree).join(".git");
    let failed = |why: String| {
        let line = format!("gate: remote {} failed: {why}", for_repo(REMOTE_ADD, name));
        Unready::Git(failed_for(Failed::without_status(line), REMOTE_ADD, name))
    };
    let config = git_folder.join(CONFIG);
    let folder = OpenOptions::new()
        .read(true)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&git_folder)
        .map_err(|error| failed(format!("cannot open {}: {error}", git_folder.display())))?;
    if !is_folder_itself(&folder).map_err(|error| failed(format!("cannot read {}: {error}", git_folder.display())))? {
        return Ok(false);
    }
    replace_config(&folder, bare, |lock, text| lock.write_all(text.as_bytes()))
        .map_err(|error| failed(format!("cannot write {}: {error}", config.display())))
}

/// The text of the config beneath the held `.git` folder `folder`, opened as itself and read through that handle;
/// `None` when it is missing, not a plain file (a link, a junction or a folder) or not UTF-8. Else why it could not be
/// read.
fn plain_config(folder: &File) -> io::Result<Option<String>> {
    let mut config = match gate_win32::open_beneath(folder, CONFIG, Share::All) {
        Ok(config) => config,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = config.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    config.read_to_end(&mut bytes)?;
    Ok(String::from_utf8(bytes).ok())
}

/// Replaces the config beneath the held `.git` folder `folder` whole, in git's order: `config.lock` made new beneath
/// the handle, then the config read as itself ([`plain_config`]) and its new text, with the `origin` remote `bare`
/// ([`fresh_config`]), written into the lock by `fill`, and the lock renamed over the config by handle. `false`, with
/// nothing written, when the lock is there already, a git or another preflight holding it, or when the config cannot
/// be written once. Every way but the rename deletes the lock through its handle and leaves the config as it was;
/// else why it failed.
fn replace_config(folder: &File, bare: &str, fill: impl FnOnce(&mut File, &str) -> io::Result<()>) -> io::Result<bool> {
    let made = gate_win32::open_beneath_as(folder, CONFIG_LOCK, FILE_GENERIC_WRITE | DELETE, Share::NoDelete, Make::NewFile);
    let mut lock = match made {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error),
    };
    let replaced = locked_replace(folder, &mut lock, bare, fill);
    if matches!(replaced, Ok(true)) {
        return replaced;
    }
    match (replaced, gate_win32::delete_opened(&lock)) {
        (replaced, Ok(())) => replaced,
        (Ok(_), Err(left)) => Err(io::Error::new(left.kind(), format!("{CONFIG_LOCK} could not be removed: {left}"))),
        (Err(error), Err(left)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; {CONFIG_LOCK} could not be removed: {left}"),
        )),
    }
}

/// [`replace_config`] once its lock `lock` is held: whether the config was replaced, `false` when it cannot be
/// written once; else why the read, the write or the rename failed.
fn locked_replace(folder: &File, lock: &mut File, bare: &str, fill: impl FnOnce(&mut File, &str) -> io::Result<()>) -> io::Result<bool> {
    let read = plain_config(folder).map_err(|error| io::Error::new(error.kind(), format!("cannot read {CONFIG}: {error}")))?;
    let Some(text) = read.and_then(|text| fresh_config(&text, bare)) else {
        return Ok(false);
    };
    fill(lock, &text)?;
    gate_win32::rename_beneath(lock, folder, CONFIG)?;
    Ok(true)
}

/// The config `text` `git init` wrote, with `core.autocrlf=false` and `core.eol=lf` added to its `[core]` section and
/// the `origin` remote `bare` after it, as `git remote add` and `git config` write them; `None` when `text` holds any
/// section but `[core]`, either of those keys already, a U+FEFF anywhere, which could hide a section's header, or does
/// not end its last line, or ends it with a `\` that would carry it on, which the git steps handle.
fn fresh_config(text: &str, bare: &str) -> Option<String> {
    let headers: Vec<&str> = text.lines().map(str::trim).filter(|line| line.starts_with('[')).collect();
    let set = text.lines().any(|line| {
        let key = line.trim_start().split(['=', ' ', '\t']).next().unwrap_or_default();
        key.eq_ignore_ascii_case("autocrlf") || key.eq_ignore_ascii_case("eol")
    });
    let continued = text.lines().last().is_some_and(|line| line.ends_with('\\'));
    if headers != ["[core]"] || set || continued || text.contains('\u{feff}') || !text.ends_with('\n') {
        return None;
    }
    Some(format!(
        "{text}\tautocrlf = false\n\teol = lf\n[remote \"origin\"]\n\turl = {}\n\tfetch = {ORIGIN_FETCH}\n",
        config_value(bare)
    ))
}

/// The config value `value` as git writes it: its `\` and `"` escaped, a line end and a tab written as `\n` and `\t`,
/// and the whole quoted when it starts or ends with a space or holds a `;`, a `#` or a carriage return.
fn config_value(value: &str) -> String {
    let quoted = value.starts_with(' ') || value.ends_with(' ') || value.contains([';', '#', '\r']);
    let mut said = String::new();
    if quoted {
        said.push('"');
    }
    for character in value.chars() {
        match character {
            '\n' => said.push_str("\\n"),
            '\t' => said.push_str("\\t"),
            '"' | '\\' => {
                said.push('\\');
                said.push(character);
            }
            other => said.push(other),
        }
    }
    if quoted {
        said.push('"');
    }
    said
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
/// `.git\objects\info` are made when missing ([`made_folder`]), and the file written ([`written`]), the line saying
/// where a folder at its name was moved aside, if one was. Else why not: `could not write {file}: ` and the reason.
fn write_alternates(bare: &str, tree: &str, repair: bool) -> Result<Option<String>, String> {
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
/// file read, removed and made beneath that handle ([`written_beneath`]); the line saying where a folder at the name
/// was moved aside, when one was ([`moved_aside_line`]). Else why not, a folder that is a link, a junction or not a
/// folder refused.
pub(super) fn written(file: &Path, text: &str) -> Result<Option<String>, String> {
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
    Ok(written_beneath(&folder, name, text)?.map(|moved| moved_aside_line(file, &moved)))
}

/// The line saying the folder that was at the path `file` is now `moved`, one name, beside it.
pub(super) fn moved_aside_line(file: &Path, moved: &str) -> String {
    format!(
        "gate: remote moved aside the folder {} to {}",
        file.display(),
        file.with_file_name(moved).display()
    )
}

/// Whether the open `opened` is a folder itself: a folder, not a link or a junction.
pub(super) fn is_folder_itself(opened: &File) -> io::Result<bool> {
    let metadata = opened.metadata()?;
    Ok(metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}

/// Writes `text` as the file `name` in the held folder `folder` unless it already holds exactly that: whatever is at the
/// name opened as itself beneath the handle, never followed; a folder of its own, which another account may have
/// filled, moved aside beneath the handle to `{name}.gate-moved-{n}` for the first free `n` and never walked
/// ([`moved_aside`]); a plain file of other bytes, a link, a junction or anything else deleted as itself; and a new
/// file made beneath the handle, refused when anything took the name meanwhile. The name a folder was moved aside to,
/// when one was. Else why not.
pub(super) fn written_beneath(folder: &File, name: &str, text: &str) -> Result<Option<String>, String> {
    let moved = match gate_win32::open_beneath_as(folder, name, FILE_GENERIC_READ | DELETE, Share::All, Make::Open) {
        Ok(found) => match found_at_name(folder, name, found, text)? {
            Found::Holds => return Ok(None),
            Found::Deleted => None,
            Found::MovedAside(moved) => Some(moved),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut made =
        gate_win32::open_beneath_as(folder, name, FILE_GENERIC_WRITE, Share::NoDelete, Make::NewFile).map_err(|error| error.to_string())?;
    made.write_all(text.as_bytes()).map_err(|error| error.to_string())?;
    Ok(moved)
}

/// What [`written_beneath`] found at its name, and did with it.
enum Found {
    /// A plain file holding exactly the text, left as it is.
    Holds,
    /// Anything else but a folder of its own, deleted as itself through its handle.
    Deleted,
    /// A folder of its own, moved aside to this name beside it.
    MovedAside(String),
}

/// Whether `found`, the entry `name` of the held folder `folder` opened as itself with `DELETE` access, is a plain file
/// holding exactly `text`; else a folder of its own is moved aside ([`moved_aside`]) and anything else deleted as
/// itself, through its handle. Else why it could not be moved or deleted.
fn found_at_name(folder: &File, name: &str, mut found: File, text: &str) -> Result<Found, String> {
    let metadata = found.metadata().map_err(|error| error.to_string())?;
    let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if metadata.is_file() && !reparse {
        let mut held = Vec::new();
        if found.read_to_end(&mut held).is_ok() && held == text.as_bytes() {
            return Ok(Found::Holds);
        }
    }
    if metadata.is_dir() && !reparse {
        return moved_aside(folder, name, &found).map(Found::MovedAside);
    }
    gate_win32::delete_opened(&found).map_err(|error| error.to_string())?;
    Ok(Found::Deleted)
}

/// The most names `{name}.gate-moved-{n}` tried for a folder moved aside.
const MOVED_ASIDE_LIMIT: u32 = 100;

/// Renames `found`, the folder `name` of the held folder `folder` opened with `DELETE` access, to
/// `{name}.gate-moved-{n}` beside it, by handle and never replacing what has that name
/// ([`gate_win32::rename_beneath_no_replace`]), for the first `n` from 1 the rename finds free: the new name. Else why
/// not: the rename's error, or every `n` up to [`MOVED_ASIDE_LIMIT`] taken.
fn moved_aside(folder: &File, name: &str, found: &File) -> Result<String, String> {
    for n in 1..=MOVED_ASIDE_LIMIT {
        let moved = format!("{name}.gate-moved-{n}");
        match gate_win32::rename_beneath_no_replace(found, folder, &moved) {
            Ok(()) => return Ok(moved),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("cannot move aside the folder {name}: {error}")),
        }
    }
    Err(format!(
        "cannot move aside the folder {name}: {name}.gate-moved-1 to {name}.gate-moved-{MOVED_ASIDE_LIMIT} are all taken"
    ))
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
    let listing = git::sdk_listing(Variables::from_process(), limit, None, &[])?;
    Ok(sdk_list(&listing))
}

/// The first space-separated word of each non-empty line of `listing`, joined by `,`; `none` when there is none.
fn sdk_list(listing: &str) -> String {
    let versions = crate::remote::parity::sdk_versions(listing);
    if versions.is_empty() { "none".to_owned() } else { versions.join(",") }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::{self, Write as _};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use std::process::{Child, Stdio};
    use std::sync::PoisonError;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    use super::{Git, TreeAt, fresh_config, fresh_config_written, git_failed, ready_tree, replace_config, run_forwarded, sdk_list};
    use crate::boxside::forward::OutputLock;
    use crate::console::Console;

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose git cannot start must fail loudly")]
    fn git_writes_its_stderr_through_the_output_lock() {
        let output = OutputLock::default();
        let missing = crate::test_support::temp_dir().join(format!("gate-preflight-no-such-repository-{}", std::process::id()));
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

    /// The `git config --local --list` of the tree `tree`, and its config file's bytes.
    #[expect(clippy::expect_used, reason = "a test whose git cannot start must fail loudly")]
    fn config_of(git: &Git, tree: &Path) -> (String, Vec<u8>) {
        let git_dir = format!(r"--git-dir={}\.git", tree.display());
        let mut command = git.command(&[&git_dir, "config", "--local", "--list"]).expect("git is on PATH");
        let output = gate_win32::spawn(command.stdout(Stdio::piped()).stderr(Stdio::null()))
            .and_then(Child::wait_with_output)
            .expect("git config runs");
        assert_eq!(output.status.code(), Some(0), "git config --list in {}", tree.display());
        let list = String::from_utf8(output.stdout).expect("the config list is UTF-8");
        (list, fs::read(tree.join(".git").join("config")).expect("the config file reads"))
    }

    /// A tree the preflight makes has the config `git remote add` and `git config` would have given it, written once,
    /// for a bare repository whose path holds a backslash and a space, and characters git escapes or quotes.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose git cannot start must fail loudly")]
    fn a_fresh_tree_s_config_is_what_the_git_steps_write() {
        let git = Git::find();
        for (at, bare) in [r"C:\Box Work\x.git", r#"C:\Box Work\a"b;c #.git"#, r" C:\Box Work\x.git "]
            .into_iter()
            .enumerate()
        {
            let folder = crate::test_support::temp_dir().join(format!("gate-preflight-config-{at}-{}", std::process::id()));
            if folder.exists() {
                fs::remove_dir_all(&folder).expect("an earlier run's folder can be removed");
            }
            let (written, called) = (folder.join("written"), folder.join("called"));
            fs::create_dir_all(&folder).expect("the test's folder can be made");

            let (tree, warm) = (written.to_string_lossy(), folder.join("written.warm").to_string_lossy().into_owned());
            let at = TreeAt {
                tree: &tree,
                bare,
                warm: &warm,
            };
            let readied = ready_tree(&git, &at, None, &mut Console::new());

            assert!(readied.is_ok(), "the tree is readied for {bare}");
            let called_dir = format!(r"--git-dir={}\.git", called.display());
            let called_tree = called.to_string_lossy();
            assert_eq!(
                git.code(&["init", "-q", "--", &called_tree], Stdio::null(), Stdio::null()),
                Ok(0),
                "git init"
            );
            let initialised = fs::read_to_string(called.join(".git").join("config")).expect("git init's config reads");
            assert!(
                fresh_config(&initialised, bare).is_some(),
                "the readied tree took the one write for {bare}: {initialised:?}"
            );
            for args in [
                &[&called_dir, "remote", "add", "--", "origin", bare][..],
                &[&called_dir, "config", "core.autocrlf", "false"],
                &[&called_dir, "config", "core.eol", "lf"],
            ] {
                assert_eq!(git.code(args, Stdio::null(), Stdio::null()), Ok(0), "git {args:?}");
            }
            assert_eq!(config_of(&git, &written), config_of(&git, &called), "{bare}");
            fs::remove_dir_all(&folder).expect("the test's folder can be removed");
        }
    }

    /// The config `git init` writes on this machine's git, for the tests that hand it to the one write.
    const INIT_CONFIG: &str = "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n";

    /// A tree in the unit tests' temp folder for the test `name`, its `.git` folder holding `config` as `config`'s
    /// bytes when given, and `config.lock` when `locked`; the tree's path.
    #[expect(clippy::expect_used, reason = "a test that cannot make its tree must fail loudly")]
    fn planted_tree(name: &str, config: Option<&[u8]>, locked: bool) -> std::path::PathBuf {
        let tree = crate::test_support::temp_dir().join(format!("gate-preflight-{name}-{}", std::process::id()));
        if tree.exists() {
            fs::remove_dir_all(&tree).expect("an earlier run's tree can be removed");
        }
        fs::create_dir_all(tree.join(".git")).expect("the tree's .git can be made");
        if let Some(config) = config {
            fs::write(tree.join(".git").join("config"), config).expect("the config can be planted");
        }
        if locked {
            fs::write(tree.join(".git").join("config.lock"), "held").expect("the lock can be planted");
        }
        tree
    }

    /// The one write leaves to the git steps, writing nothing, a config it cannot take whole: one that is missing, one
    /// that is not UTF-8, one that holds another section, and one whose `config.lock` a git or another preflight holds;
    /// a lock the write took itself is gone.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose tree cannot be read must fail loudly")]
    fn a_config_the_one_write_cannot_take_is_left_to_the_git_steps() {
        let not_utf8: &[u8] = b"[core]\n\tbare = \xff\n";
        let other = format!("{INIT_CONFIG}[user]\n\tname = x\n");
        for (name, config, locked) in [
            ("config-missing", None, false),
            ("config-not-utf8", Some(not_utf8), false),
            ("config-other-section", Some(other.as_bytes()), false),
            ("config-locked", Some(INIT_CONFIG.as_bytes()), true),
        ] {
            let tree = planted_tree(name, config, locked);

            let written = fresh_config_written(r"C:\Box\x.git", &tree.to_string_lossy(), None);

            assert!(matches!(written, Ok(false)), "{name}: left to the git steps");
            let left = fs::read(tree.join(".git").join("config")).ok();
            assert_eq!(left.as_deref(), config, "{name}: the config is as it was");
            assert_eq!(
                tree.join(".git").join("config.lock").exists(),
                locked,
                "{name}: no lock of the write's is left"
            );
            fs::remove_dir_all(&tree).expect("the tree can be removed");
        }
    }

    /// A folder at the config's name is no plain config: the one write leaves it to the git steps and no lock behind.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose tree cannot be made must fail loudly")]
    fn a_folder_at_the_config_s_name_is_left_to_the_git_steps() {
        let tree = planted_tree("config-a-folder", None, false);
        let git_folder = tree.join(".git");
        fs::create_dir(git_folder.join("config")).expect("a folder can be planted at the config's name");

        let written = fresh_config_written(r"C:\Box\x.git", &tree.to_string_lossy(), None);

        assert!(matches!(written, Ok(false)), "left to the git steps");
        assert!(git_folder.join("config").is_dir(), "the folder is as it was");
        assert!(!git_folder.join("config.lock").exists(), "the write's lock is gone");
        fs::remove_dir_all(&tree).expect("the tree can be removed");
    }

    /// A write into `config.lock` that fails before the rename leaves the config byte-identical and no lock behind.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose tree cannot be read must fail loudly")]
    fn a_config_write_that_fails_before_the_rename_leaves_the_config_as_it_was() {
        let tree = planted_tree("config-write-fails", Some(INIT_CONFIG.as_bytes()), false);
        let git_folder = tree.join(".git");
        let folder = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&git_folder)
            .expect("the .git folder opens");
        let replaced = replace_config(&folder, r"C:\Box\x.git", |lock, text| {
            lock.write_all(&text.as_bytes()[..10])?;
            Err(io::Error::other("the disk is full"))
        });

        drop(folder);
        assert_eq!(replaced.map_err(|error| error.to_string()), Err("the disk is full".to_owned()));
        assert_eq!(fs::read(git_folder.join("config")).expect("the config reads"), INIT_CONFIG.as_bytes());
        assert!(!git_folder.join("config.lock").exists(), "the lock is removed");
        fs::remove_dir_all(&tree).expect("the tree can be removed");
    }

    /// A config holding U+FEFF anywhere, which can hide a section header, or whose last line ends with `\`, which would
    /// carry the next line on, is left to the git steps.
    #[test]
    fn a_config_with_a_byte_order_mark_or_a_continued_last_line_is_not_written_once() {
        let bare = r"C:\Box\x.git";
        assert!(fresh_config(INIT_CONFIG, bare).is_some(), "git init's own config is written once");
        assert_eq!(
            fresh_config(&format!("\u{feff}[remote \"x\"]\n{INIT_CONFIG}"), bare),
            None,
            "a hidden header"
        );
        assert_eq!(fresh_config(&format!("{INIT_CONFIG}\u{feff}"), bare), None, "U+FEFF at the end");
        assert_eq!(fresh_config(&format!("{INIT_CONFIG}\tx = y \\\n"), bare), None, "a continued last line");
    }

    #[test]
    fn the_sdk_list_is_the_first_word_of_each_line() {
        let listing = "8.0.404 [C:\\Program Files\\dotnet\\sdk]\r\n\r\n  9.0.100 [C:\\Program Files\\dotnet\\sdk]\r\n";
        assert_eq!(sdk_list(listing), "8.0.404,9.0.100");
        assert_eq!(sdk_list(""), "none");
        assert_eq!(sdk_list("\r\n  \n"), "none");
    }
}
