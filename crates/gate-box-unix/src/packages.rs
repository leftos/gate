//! The rules of the pnpm, bun and npm package restore on a macOS builder, as the Windows box has them: which command
//! is a package manager's, which folder its walk starts in, where the nearest lockfile lies from there up to the work
//! folder, the hash of that folder's package files and the `<work>.packages` record of the hashes whose install
//! passed. Pure: every read of the disk is a reader the caller passes, so the rules build and are tested on every
//! target. Record keys compare as bytes, `/` between a folder's components and `.` for the work folder itself.

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};

use gate_wire::escaped;
use sha2::{Digest, Sha256};

/// What follows the work folder's path in its packages record's name, `{work}.packages`.
pub const PACKAGES_SUFFIX: &str = ".packages";

/// A package manager whose packages the box installs before a routed command of it.
pub struct Manager {
    /// Its name in the box's lines.
    pub name: &'static str,
    /// The program its install runs, found on the login shell's `PATH`.
    pub program: &'static str,
    /// The words its install runs, after its program.
    pub install_words: &'static [&'static str],
    /// The file stems of a command's first word that make the command this manager's, compared in any case.
    stems: &'static [&'static str],
    /// The lockfiles whose folder it installs in: a folder holding any of them is the lockfile folder.
    lockfiles: &'static [&'static str],
    /// The files of a lockfile's folder the record hashes, in hash order, each only when it exists.
    hashed: &'static [&'static str],
    /// The stems of its commands that run a bin looked up from the folder they run in, whatever a start-folder option
    /// says, so their walk starts at the place.
    bin_stems: &'static [&'static str],
    /// The folder a command of it is told to start in by the words after its first.
    start_word: fn(&[String]) -> Option<&str>,
    /// What its folders' keys in the record start with, so a folder holds one line for each manager.
    key_prefix: &'static str,
}

/// pnpm, for a `pnpm` command.
const PNPM: Manager = Manager {
    name: "pnpm",
    program: "pnpm",
    install_words: &["install", "--frozen-lockfile"],
    stems: &["pnpm"],
    lockfiles: &["pnpm-lock.yaml"],
    hashed: &["pnpm-lock.yaml", "package.json", "pnpm-workspace.yaml", ".npmrc"],
    bin_stems: &[],
    start_word,
    key_prefix: "",
};

/// bun, for a `bun` or `bunx` command, installing with `bun` for both.
const BUN: Manager = Manager {
    name: "bun",
    program: "bun",
    install_words: &["install", "--frozen-lockfile"],
    stems: &["bun", "bunx"],
    lockfiles: &["bun.lock", "bun.lockb"],
    hashed: &["bun.lock", "bun.lockb", "package.json", "bunfig.toml", ".npmrc"],
    bin_stems: &["bunx"],
    start_word: bun_start_word,
    key_prefix: "bun:",
};

/// npm, for an `npm` or `npx` command, installing with `npm` for both.
const NPM: Manager = Manager {
    name: "npm",
    program: "npm",
    install_words: &["ci", "--ignore-scripts", "--no-audit", "--no-fund"],
    stems: &["npm", "npx"],
    lockfiles: &["package-lock.json", "npm-shrinkwrap.json"],
    hashed: &["package-lock.json", "npm-shrinkwrap.json", "package.json", ".npmrc"],
    bin_stems: &[],
    start_word: no_start_word,
    key_prefix: "npm:",
};

/// The package managers the box installs for, in the order a command is matched against them.
const MANAGERS: [&Manager; 3] = [&PNPM, &BUN, &NPM];

/// The hex digit of each hash byte's high nibble, the low nibble reading from the same table.
const HEX: &[u8; 16] = b"0123456789abcdef";
/// The path git is asked about: with its trailing separator, so a folder pattern such as `node_modules/` matches it
/// before the folder exists.
pub const NODE_MODULES: &str = "node_modules/";

/// The line the box says when the work folder `key` could not restore the packages of the manager `name`, just after
/// its refusal tag.
#[must_use]
pub fn packages_line(name: &str, key: &str) -> String {
    format!("gate: remote work folder {key} could not restore its {name} packages")
}

/// The file name of the command's first word `word`, after any folder or drive, less its last extension.
fn stem(word: &str) -> &str {
    let name = word.rsplit(['\\', '/']).next().unwrap_or(word);
    let name = match name.as_bytes().get(1) {
        Some(b':') => &name[2..],
        _ => name,
    };
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// Whether the [`stem`] of the command's first word `word` is one of `stems`, in any case.
fn stem_in(stems: &[&str], word: &str) -> bool {
    let stem = stem(word);
    stems.iter().any(|known| stem.eq_ignore_ascii_case(known))
}

/// Whether the command's first word `word` names `manager`: its [`stem`] is one of the manager's stems in any case.
fn is_command(manager: &Manager, word: &str) -> bool {
    stem_in(manager.stems, word)
}

/// No start-folder option: such a manager's walk starts at the place, whatever the words after its first say.
fn no_start_word(_words: &[String]) -> Option<&str> {
    None
}

/// The folder `manager`'s command of first word `first` and the words `rest` after it is told to start in: none for a
/// command that runs a bin from the folder it runs in, else what the manager's start word reads in `rest`.
fn start_named<'a>(manager: &Manager, first: &str, rest: &'a [String]) -> Option<&'a str> {
    if stem_in(manager.bin_stems, first) {
        return None;
    }
    (manager.start_word)(rest)
}

/// The folder bun is told to start in by `words`, the command's words after its first: the value of the last
/// `--cwd=<d>` among bun's own options before its subcommand, a bare `--cwd` there ending the scan as bun reads it as
/// valueless; and, after the subcommand `run` or `test`, of the last `--cwd <d>` or `--cwd=<d>` up to the script or
/// test path. None for `bun x`, which looks its bin up from the folder it runs in, whatever `--cwd` says; none when
/// there is none or its value is empty.
fn bun_start_word(words: &[String]) -> Option<&str> {
    let mut named = None;
    let stop = cwd_options(words, false, &mut named);
    let subcommand = stop.and_then(|at| words.get(at)).map(String::as_str);
    match subcommand {
        Some("x") => None,
        Some("run" | "test") => {
            let after = stop.and_then(|at| words.get(at + 1..)).unwrap_or_default();
            cwd_options(after, true, &mut named);
            named
        }
        _ => named,
    }
}

/// Reads bun's options at the start of `words` into `named`, the last non-empty `--cwd=<d>` value winning and, when
/// `spaced`, a `--cwd <d>` too; a bare `--cwd` when not `spaced`, or one without a value, ends them. The index of the
/// first word that is not an option, none when they end otherwise: at a lone `--`, at such a `--cwd`, or at the end.
/// Another option's value is not an option, so it ends them too (`test -t smoke --cwd web` names no folder): the
/// conservative limit [`start_word`] has for pnpm, which leaves the walk at the place.
fn cwd_options<'a>(words: &'a [String], spaced: bool, named: &mut Option<&'a str>) -> Option<usize> {
    let mut index = 0;
    while let Some(word) = words.get(index) {
        if word == "--" {
            return None;
        }
        if !word.starts_with('-') {
            return Some(index);
        }
        let value = if word == "--cwd" {
            index += 1;
            match words.get(index) {
                Some(value) if spaced && value != "--" => Some(value.as_str()),
                _ => return None,
            }
        } else {
            word.strip_prefix("--cwd=")
        };
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            *named = Some(value);
        }
        index += 1;
    }
    None
}

/// The folder pnpm is told to start in by `words`, the command's words after its first: the value of the last
/// `--dir <d>`, `--dir=<d>`, `-C <d>` or `-C<d>` among pnpm's own options. They end at the first word that is neither
/// an option nor the value of `--dir` or `-C` (the script, the bin or `exec`'s program, whose words follow it), at a
/// lone `--`, or at a `--dir` or `-C` without a value. None when there is none or its value is empty.
fn start_word(words: &[String]) -> Option<&str> {
    let mut named = None;
    let mut index = 0;
    while let Some(word) = words.get(index) {
        if word == "--" || !word.starts_with('-') {
            break;
        }
        let value = if word == "--dir" || word == "-C" {
            index += 1;
            match words.get(index) {
                Some(value) if value != "--" => Some(value.as_str()),
                _ => break,
            }
        } else {
            word.strip_prefix("--dir=").or_else(|| word.strip_prefix("-C"))
        };
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            named = Some(value);
        }
        index += 1;
    }
    named
}

/// Whether `path` is the folder `work` or a folder below it, both absolute and folded, compared component by
/// component as bytes.
fn is_within(work: &Path, path: &Path) -> bool {
    path.starts_with(work)
}

/// `path` folded without reading the disk: each `.` dropped and each `..` dropping the name before it, a `..` at the
/// root staying there.
fn folded(path: &Path) -> PathBuf {
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(folded.components().next_back(), Some(Component::Normal(_))) {
                    folded.pop();
                }
            }
            other => folded.push(other),
        }
    }
    folded
}

/// The folder the lockfile walk starts in: the folder `named` by the command's start-folder option, joined to `place`
/// when relative and [`folded`], else `place`; `Err` with the folded folder when it lies outside the work root `work`.
fn start_folder(work: &Path, place: &Path, named: Option<&str>) -> Result<PathBuf, PathBuf> {
    let Some(named) = named else {
        return Ok(place.to_owned());
    };
    let full = folded(&place.join(named));
    if is_within(work, &full) { Ok(full) } else { Err(full) }
}

/// The folder of the first of `manager`'s lockfiles that `present` says is there, from `start` up to the work
/// root `work` inclusive, never above it; none when there is none. A lockfile `present` cannot tell of is an error,
/// the line saying so.
fn lockfile_folder(manager: &Manager, work: &Path, start: &Path, present: &dyn Fn(&Path) -> io::Result<bool>) -> Result<Option<PathBuf>, String> {
    let mut here = start.to_owned();
    while is_within(work, &here) {
        if holds_lockfile(manager, &here, present)? {
            return Ok(Some(here));
        }
        if !here.pop() {
            break;
        }
    }
    Ok(None)
}

/// Whether `folder` holds one of `manager`'s lockfiles, as `present` says; `Err` with the line saying
/// `present` could not tell of one.
fn holds_lockfile(manager: &Manager, folder: &Path, present: &dyn Fn(&Path) -> io::Result<bool>) -> Result<bool, String> {
    for name in manager.lockfiles {
        let path = folder.join(name);
        match present(&path) {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) => return Err(unreadable(manager, &path, name, &error)),
        }
    }
    Ok(false)
}

/// Whether `folder` also holds a pnpm or a bun lockfile, whose manager owns its `node_modules`, so npm restores nothing
/// there; `Err` with the line saying `present` could not tell of one.
fn holds_other_lockfile(folder: &Path, present: &dyn Fn(&Path) -> io::Result<bool>) -> Result<bool, String> {
    for manager in [&PNPM, &BUN] {
        if holds_lockfile(manager, folder, present)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The line saying `manager`'s package file `name` at `path` could not be read for `error`.
fn unreadable(manager: &Manager, path: &Path, name: &str, error: &io::Error) -> String {
    let what = if manager.lockfiles.contains(&name) {
        "the lockfile"
    } else {
        "the package file"
    };
    format!(
        "gate: remote could not read {what} {}: {}",
        escaped(&path.to_string_lossy()),
        escaped(&error.to_string())
    )
}

/// The first folder from the work root `work` down to `folder`, both included, that `real_folder` says is not a folder
/// itself (a link, say), relative to the work root; none when every one is a folder. `Err` with the line saying
/// `real_folder` could not tell of one.
fn linked(work: &Path, folder: &Path, real_folder: &dyn Fn(&Path) -> io::Result<bool>) -> Result<Option<String>, String> {
    let mut steps: Vec<&Path> = folder.ancestors().take_while(|here| is_within(work, here)).collect();
    steps.reverse();
    for here in steps {
        let real = real_folder(here).map_err(|error| {
            format!(
                "gate: remote could not read the folder {}: {}",
                escaped(&here.to_string_lossy()),
                escaped(&error.to_string())
            )
        })?;
        if !real {
            return Ok(Some(relative(work, here).unwrap_or_else(|| here.to_string_lossy().into_owned())));
        }
    }
    Ok(None)
}

/// The `<work>.packages` record: for each folder whose install passed, its key ([`record_key`]) and the hash of its
/// package files then, one `{key}\t{hash}` line each, sorted by key.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Record(BTreeMap<String, String>);

impl Record {
    /// The record of `text`, a line that is not a key, a tab and 64 lower-case hex digits dropped.
    #[must_use]
    pub fn parse(text: &str) -> Record {
        let lines = text.lines().filter_map(|line| {
            let (folder, digest) = line.split_once('\t')?;
            let valid = !folder.is_empty() && digest.len() == 64 && digest.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
            valid.then(|| (folder.to_owned(), digest.to_owned()))
        });
        Record(lines.collect())
    }

    /// The hash on the line of `folder`, its key compared as bytes.
    #[must_use]
    pub fn find(&self, folder: &str) -> Option<&String> {
        self.0.get(folder)
    }

    /// Whether the line of `folder` holds `digest`.
    #[must_use]
    pub fn holds(&self, folder: &str, digest: &str) -> bool {
        self.find(folder).is_some_and(|held| held == digest)
    }

    /// Sets the line of `folder` to `digest`, replacing the one it had, the others kept.
    pub fn set(&mut self, folder: &str, digest: &str) {
        self.0.insert(folder.to_owned(), digest.to_owned());
    }

    /// Drops the line of `folder`, its key compared as bytes, the others kept; whether there was one.
    pub fn remove(&mut self, folder: &str) -> bool {
        self.0.remove(folder).is_some()
    }

    /// The record's text, a line for each key, each ending in LF.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::new();
        for (folder, digest) in &self.0 {
            text.push_str(folder);
            text.push('\t');
            text.push_str(digest);
            text.push('\n');
        }
        text
    }
}

/// Where the install of one command of a package manager leads ([`lockfile_for`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    /// No install: no lockfile is found up to the work root.
    Nothing,
    /// No install: the folder the command starts in lies outside the work root.
    Outside(PathBuf),
    /// No install: this folder from the work root down to the lockfile's, relative to the work root, is not a folder
    /// itself.
    Linked(String),
    /// No install: this npm lockfile folder also holds a pnpm or a bun lockfile, whose manager owns its `node_modules`.
    OtherManager(PathBuf),
    /// The folder of the lockfile to install from.
    Lockfile(PathBuf),
}

/// The package manager of the command of `words`, run in the folder `place` at or below the work folder `work`, both
/// absolute and folded, and where it leads its install ([`Found`]), nothing said; none when the command is no
/// manager's. `present` says whether a lockfile is at a path, `real_folder` whether one is a folder itself.
///
/// # Errors
///
/// The line saying `present` could not tell of a lockfile, or `real_folder` of a folder.
pub fn lockfile_for(
    words: &[String],
    work: &Path,
    place: &Path,
    present: &dyn Fn(&Path) -> io::Result<bool>,
    real_folder: &dyn Fn(&Path) -> io::Result<bool>,
) -> Result<Option<(&'static Manager, Found)>, String> {
    let Some((word, rest)) = words.split_first() else {
        return Ok(None);
    };
    let Some(manager) = MANAGERS.into_iter().find(|manager| is_command(manager, word)) else {
        return Ok(None);
    };
    let start = match start_folder(work, place, start_named(manager, word, rest)) {
        Ok(start) => start,
        Err(outside) => return Ok(Some((manager, Found::Outside(outside)))),
    };
    let found = match lockfile_folder(manager, work, &start, present)? {
        None => Found::Nothing,
        Some(folder) => match linked(work, &folder, real_folder)? {
            Some(link) => Found::Linked(link),
            None if manager.name == NPM.name && holds_other_lockfile(&folder, present)? => Found::OtherManager(folder),
            None => Found::Lockfile(folder),
        },
    };
    Ok(Some((manager, found)))
}

/// The hash of `folder`'s package files: for each of `manager`'s hashed files that `read` finds, its name, a NUL byte,
/// its bytes and a NUL byte, hashed with SHA-256 and written as lower-case hex. `read` gives a file's bytes, `None`
/// when there is none.
///
/// # Errors
///
/// The line saying which file `read` could not read.
pub fn hash(manager: &Manager, folder: &Path, read: &dyn Fn(&Path) -> io::Result<Option<Vec<u8>>>) -> Result<String, String> {
    let mut hasher = Sha256::new();
    for name in manager.hashed {
        let path = folder.join(name);
        let content = match read(&path) {
            Ok(Some(content)) => content,
            Ok(None) => continue,
            Err(error) => return Err(unreadable(manager, &path, name, &error)),
        };
        hasher.update(name.as_bytes());
        hasher.update([0_u8]);
        hasher.update(&content);
        hasher.update([0_u8]);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(hex)
}

/// `folder`'s path relative to the work root `work`, its components joined by `/`, `.` for the root itself; none when
/// `folder` does not lie at or below `work`.
#[must_use]
pub fn relative(work: &Path, folder: &Path) -> Option<String> {
    let below = folder.strip_prefix(work).ok()?;
    let steps: Vec<_> = below.components().map(|step| step.as_os_str().to_string_lossy()).collect();
    Some(if steps.is_empty() { ".".to_owned() } else { steps.join("/") })
}

/// Whether a line of the record can hold the key of `folder` below the work root `work`: each of its components, read
/// as it is on the disk and not as [`relative`] shows it, is UTF-8 and holds no control character, a tab or a newline
/// among them. A folder outside the work root has no key.
#[must_use]
pub fn key_holdable(work: &Path, folder: &Path) -> bool {
    let Ok(below) = folder.strip_prefix(work) else {
        return false;
    };
    below
        .components()
        .all(|step| step.as_os_str().to_str().is_some_and(|name| !name.chars().any(char::is_control)))
}

/// The key of `manager`'s line for the folder `relative` (as [`relative`] names it) in the record: the folder after
/// the manager's prefix, so pnpm's keys are bare folders and bun's start `bun:` and npm's `npm:`.
#[must_use]
pub fn record_key(manager: &Manager, relative: &str) -> String {
    format!("{}{relative}", manager.key_prefix)
}

#[cfg(unix)]
pub(crate) use restore::{packages_group_record, update};

/// The restore itself on a macOS builder, as the dotnet tool restore runs (`tools.rs`): before the command of a routed
/// run whose program is pnpm, bun(x) or npm(x), the manager's install runs in the folder of the lockfile its command
/// leads to ([`lockfile_for`]), along the login shell's `PATH` as the leader of a process group of its own recorded in
/// `<work>.packages.pgid` so `box stop` ends it, both its streams forwarded like the command's, with no deadline; a
/// client gone ends it. The folder's hash ([`hash`]) is written to `<work>.packages` once the install passes, so an
/// unchanged folder whose `node_modules` is there skips it. An install that fails or cannot start refuses the run with
/// status 75 and the `tools` tag, so the PC runs the command itself.
#[cfg(unix)]
mod restore {
    use std::cell::Cell;
    use std::fs;
    use std::io;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use gate_wire::{Refusal, escaped, refused_line};

    use super::{
        Found, MANAGERS, Manager, NODE_MODULES, PACKAGES_SUFFIX, Record, hash, key_holdable, lockfile_for, packages_line, record_key, relative,
    };
    use crate::command::signals_reset;
    use crate::console::{Forwarding, GAT, LineSink, Output, Stamp, Stream};
    use crate::files::{self, HeldLock};
    use crate::git::{CLOSED_STATUS, GitClass, RunGit, exit_failed};
    use crate::lines::{Along, FAILED_STATUS};
    use crate::process::{self, EndedBy, Ending, Forward, GroupRecord, NotRan, Watch, run_watched};
    use crate::run::{REFUSED_STATUS, Request, Unready};

    /// How long the install's group has, once asked to end with `SIGTERM`, before `SIGKILL`: the command's grace.
    const END_GRACE: Duration = Duration::from_secs(2);

    /// The group record of the install in the work folder `work`, `<work>.packages.pgid`, which `box stop` reads and
    /// the next run looks at before its first git step.
    pub(crate) fn packages_group_record(work: &str) -> String {
        format!("{work}.packages.pgid")
    }

    /// The most a package file may hold: one longer fails the run.
    const PACKAGE_FILE_LIMIT: u64 = 256 * 1024 * 1024;

    /// Whether `path` is a plain file or a link, looked at as itself and never followed: a link stops the walk as the
    /// Windows box's does, and its read, which never follows one, fails the run. A folder or anything else at it is not,
    /// nor is a missing path or one below a file; else why it could not be looked at.
    fn present(path: &Path) -> io::Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(metadata.is_file() || metadata.file_type().is_symlink()),
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The whole of the plain file of one name at `path` ([`files::read_whole`]), at most [`PACKAGE_FILE_LIMIT`]
    /// bytes, `None` when there is none.
    fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
        match files::read_whole(path, PACKAGE_FILE_LIMIT) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether `path` is a folder itself, looked at as itself: a link or a missing path is not; else why it could not
    /// be looked at.
    fn real_folder(path: &Path) -> io::Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The line saying the folder `path` could not be looked at for `error`.
    fn folder_unread(path: &Path, error: &io::Error) -> String {
        format!(
            "gate: remote could not read the folder {}: {}",
            escaped(&path.to_string_lossy()),
            escaped(&error.to_string())
        )
    }

    /// What one restore runs with: the run's request, the work lock held, the login shell's `PATH` and the run's
    /// `TMPDIR`, the run's git and its output.
    struct Restore<'r, 'a, S: LineSink + 'static> {
        request: &'r Request<'a>,
        lock: &'r HeldLock,
        along: Along<'r>,
        git: &'r RunGit<'r, S>,
        output: &'r Arc<Output<S>>,
    }

    /// How running the install ended.
    enum Installed {
        /// It exited with this status.
        Exited(i32),
        /// A signal ended it, a `box stop`'s among them.
        Signalled(i32),
        /// It could not be started, for this.
        NotStarted(String),
        /// The run ends with this status, its lines said: the client is gone, or the box could not see the install to
        /// its end.
        Ended(u8),
    }

    /// Restores the packages of the run's command before it runs, under the work lock `lock`: nothing for a command
    /// that is no package manager's or leads to no lockfile; a line saying why the install is skipped when the folder
    /// it starts in lies outside the main tree, a folder down to the lockfile's is a link, an npm lockfile's folder
    /// holds a pnpm or bun one, or the lockfile's folder has a name a line of the record cannot hold ([`key_holdable`]);
    /// else the folder's install ([`update_folder`]), run in `place`'s lockfile folder along `along`, git's
    /// `check-ignore` asked through `git`. Its lines are said through `output`.
    ///
    /// # Errors
    ///
    /// How the run ends, its lines said: refused when the install exited nonzero or could not start, after the failed
    /// line, the `tools` tag and [`packages_line`], said together; status 1 when a lockfile, a package file or a folder
    /// could not be read, the record could not be written, a signal ended git's `check-ignore` or the install, or the
    /// box could not see the install to its end; 255 when the client closed the connection during it or during git's
    /// `check-ignore`.
    pub(crate) fn update<S: LineSink + 'static>(
        request: &Request<'_>,
        place: &Path,
        lock: &HeldLock,
        along: Along<'_>,
        git: &RunGit<'_, S>,
        output: &Arc<Output<S>>,
    ) -> Result<(), Unready> {
        // Bounded by the main repo's tree: the work folder itself on a run of one repo, `<work>/<leaf>` on a pair run.
        let work = Path::new(request.tree());
        let found = lockfile_for(&request.command, work, place, &present, &real_folder)
            .map_err(|line| Unready::Ended(said(output, &[&line], FAILED_STATUS)))?;
        let Some((manager, found)) = found else {
            return Ok(());
        };
        let skipped = match found {
            Found::Nothing => return Ok(()),
            Found::Outside(outside) => format!("{} is outside the work root", escaped(&outside.to_string_lossy())),
            Found::Linked(link) => format!("{} is reached through a link", escaped(&link)),
            Found::OtherManager(folder) => format!("{} holds a pnpm or bun lockfile", escaped(&key_of(work, &folder))),
            Found::Lockfile(folder) if !key_holdable(work, &folder) => {
                format!("{} has a name the packages record cannot hold", escaped(&key_of(work, &folder)))
            }
            Found::Lockfile(folder) => {
                let restore = Restore {
                    request,
                    lock,
                    along,
                    git,
                    output,
                };
                return update_folder(&restore, manager, &folder);
            }
        };
        // A note: the command runs with the packages it has.
        let _ = output.say(Stream::Err, GAT, &format!("gate: remote {} install skipped: {skipped}", manager.name));
        Ok(())
    }

    /// `folder`'s path relative to the main tree `work` ([`relative`]); the folder itself, which never happens for the
    /// folders the walk finds, when it lies elsewhere.
    fn key_of(work: &Path, folder: &Path) -> String {
        relative(work, folder).unwrap_or_else(|| folder.to_string_lossy().into_owned())
    }

    /// Installs the packages of the lockfile folder `folder` with `manager`: nothing when the record `<work>.packages`
    /// holds the folder's hash in the manager's line ([`record_key`]) and its `node_modules` is a folder itself, when
    /// its `node_modules` is there but is not a folder itself ([`modules_linked`], said), or when git does not ignore
    /// `node_modules` there ([`ignored`], said); else the manager's install in the folder ([`ran_install`]), the
    /// manager's line for the folder rewritten when it passes. Every manager's line for the folder, this one's included,
    /// is dropped, and the record written, before the install starts, since whatever its end it rewrites the
    /// `node_modules` they were for, so one that fails or is stopped leaves the folder unrecorded; a record without such
    /// a line is not written then.
    fn update_folder<S: LineSink + 'static>(restore: &Restore<'_, '_, S>, manager: &'static Manager, folder: &Path) -> Result<(), Unready> {
        let (request, output) = (restore.request, restore.output);
        let digest = hash(manager, folder, &read).map_err(|line| Unready::Ended(said(output, &[&line], FAILED_STATUS)))?;
        let place = key_of(Path::new(request.tree()), folder);
        let key = record_key(manager, &place);
        let path = format!("{}{PACKAGES_SUFFIX}", request.work);
        let mut record = record_read(&path, output);
        if installed(&record, &key, &digest, folder) || modules_linked(output, manager, folder, &place)? || !ignored(restore, manager, &place)? {
            return Ok(());
        }
        let mut dropped = false;
        for each in MANAGERS {
            dropped |= record.remove(&record_key(each, &place));
        }
        if dropped {
            record_written(&path, &record, restore.lock, output).map_err(Unready::Ended)?;
        }
        let status = match ran_install(restore, manager, folder, &place) {
            Installed::Exited(0) => {
                record.set(&key, &digest);
                return record_written(&path, &record, restore.lock, output).map_err(Unready::Ended);
            }
            Installed::Exited(status) => status,
            Installed::Signalled(signal) => {
                let line = format!("gate: remote package restore failed (signal {signal})");
                return Err(Unready::Ended(said(output, &[&line], FAILED_STATUS)));
            }
            Installed::NotStarted(why) => {
                let line = format!("gate: remote could not start the {} install: {}", manager.name, escaped(&why));
                let _ = output.say(Stream::Err, GAT, &line);
                i32::from(FAILED_STATUS)
            }
            Installed::Ended(status) => return Err(Unready::Ended(status)),
        };
        Err(refused(request, manager, status, output))
    }

    /// Refuses the run for `manager`'s install that ended with `status`: the failed line, then the `tools` tag and
    /// [`packages_line`] said together.
    fn refused<S: LineSink>(request: &Request<'_>, manager: &Manager, status: i32, output: &Output<S>) -> Unready {
        let _ = output.say(Stream::Err, GAT, &format!("gate: remote package restore failed (status {status})"));
        // Said together, so no heartbeat lands between the tag and the line it tags.
        let tag = refused_line(request.nonce, Refusal::Tools);
        said_as(
            output,
            &[(Stamp::Bare, &tag), (GAT, &packages_line(manager.name, request.key))],
            REFUSED_STATUS,
        );
        Unready::Refused
    }

    /// The record at `path`, empty when there is none; one that cannot be read is said, and read as empty.
    fn record_read<S: LineSink>(path: &str, output: &Output<S>) -> Record {
        match files::read_plain(Path::new(path), files::RECORD_LIMIT) {
            Ok(bytes) => Record::parse(&String::from_utf8_lossy(&bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Record::default(),
            Err(error) => {
                let line = format!(
                    "gate: remote could not read {}: {}; installing the packages",
                    escaped(path),
                    escaped(&error.to_string())
                );
                // A note: the install runs, and its record is written anew.
                let _ = output.say(Stream::Err, GAT, &line);
                Record::default()
            }
        }
    }

    /// Whether the install in `folder`, named `key` in `record`, can be skipped: the record holds `digest` for it and
    /// its `node_modules` is a folder itself, not a link.
    fn installed(record: &Record, key: &str, digest: &str, folder: &Path) -> bool {
        record.holds(key, digest) && files::plain_folder(&folder.join("node_modules"))
    }

    /// Whether the `node_modules` of `folder`, named `place` in the main tree, is there but is not a folder itself (a
    /// link, a file, a FIFO), so `manager`'s install would write through it: when it is, the line saying the install is
    /// skipped is said and the command runs with what it has. A missing one or a folder is not.
    ///
    /// # Errors
    ///
    /// Status 1, the line said, when it could not be looked at.
    fn modules_linked<S: LineSink>(output: &Output<S>, manager: &Manager, folder: &Path, place: &str) -> Result<bool, Unready> {
        let modules = folder.join("node_modules");
        let linked = match fs::symlink_metadata(&modules) {
            Ok(metadata) => !metadata.is_dir(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(Unready::Ended(said(output, &[&folder_unread(&modules, &error)], FAILED_STATUS))),
        };
        if linked {
            let shown = if place == "." {
                "node_modules".to_owned()
            } else {
                format!("{place}/node_modules")
            };
            let line = format!(
                "gate: remote {} install skipped: {} is reached through a link",
                manager.name,
                escaped(&shown)
            );
            // A note: the command runs with the packages it has.
            let _ = output.say(Stream::Err, GAT, &line);
        }
        Ok(linked)
    }

    /// Whether git ignores `node_modules` in the folder `place` of the main tree, asked with the run's git as its other
    /// read-only steps ask, its repository and work tree named; when it does not, or cannot say, the line saying why
    /// `manager`'s install is skipped is said. A client gone while git runs is `Err` with the status the run ends on,
    /// its lines said, so no install starts.
    fn ignored<S: LineSink + 'static>(restore: &Restore<'_, '_, S>, manager: &Manager, place: &str) -> Result<bool, Unready> {
        let output = restore.output;
        // `:(top)` reads the path from the work tree's top, wherever git's own working folder is.
        let path = if place == "." {
            format!(":(top){NODE_MODULES}")
        } else {
            format!(":(top){place}/{NODE_MODULES}")
        };
        let main = &restore.request.main;
        let step = main.step("check-ignore");
        let tree = main.git_tree();
        let why = match restore.git.quiet(&step, &tree.args(&["check-ignore", "-q", &path]), GitClass::Short) {
            Ok(ran) => match ran.status.code() {
                Some(0) => return Ok(true),
                Some(1) => {
                    let line = format!(
                        "gate: remote {} install skipped: node_modules is not ignored in {}",
                        manager.name,
                        escaped(place)
                    );
                    let _ = output.say(Stream::Err, GAT, &line);
                    return Ok(false);
                }
                Some(status) => format!("status {status}"),
                // A git a signal ended ends the run, as at every other git step.
                None => {
                    let failed = exit_failed(&step, ran.status);
                    let lines: Vec<&str> = failed.lines.iter().map(String::as_str).collect();
                    return Err(Unready::Ended(said(output, &lines, failed.status)));
                }
            },
            Err(failed) if failed.status == CLOSED_STATUS => {
                let lines: Vec<&str> = failed.lines.iter().map(String::as_str).collect();
                return Err(Unready::Ended(said(output, &lines, CLOSED_STATUS)));
            }
            Err(failed) => {
                let unran = format!("gate: remote {step} failed: ");
                let whys: Vec<&str> = failed.lines.iter().map(|line| line.strip_prefix(&unran).unwrap_or(line)).collect();
                whys.join("; ")
            }
        };
        let line = format!(
            "gate: remote could not ask git whether node_modules is ignored in {}: {why}",
            escaped(place)
        );
        let _ = output.say(Stream::Err, GAT, &line);
        Ok(false)
    }

    /// Writes `record` as the whole of the record file `path` while the work lock `lock` is held; a failure is said and
    /// fails the run with status 1.
    fn record_written<S: LineSink>(path: &str, record: &Record, lock: &HeldLock, output: &Output<S>) -> Result<(), u8> {
        let written = match lock.still_held() {
            Ok(true) => files::replace_plain(Path::new(path), record.text().as_bytes()).map_err(|failure| failure.to_string()),
            Ok(false) => Err(format!("the work folder lock {} is no longer held", lock.path().display())),
            Err(error) => Err(format!("the work folder lock {} could not be looked at: {error}", lock.path().display())),
        };
        written.map_err(|why| {
            let line = format!("gate: remote could not write {}: {}", escaped(path), escaped(&why));
            said(output, &[&line], FAILED_STATUS)
        })
    }

    /// Runs `manager`'s install in `folder`, named `place` in the main tree, found along `along.path`, with
    /// `CI=true` and its `TMPDIR` `along.tmp` when given, as the leader of a process group of its own recorded in
    /// `<work>.packages.pgid` under the job `<job>-packages`, which `box stop` reads; both its streams forwarded
    /// through `output`, no deadline, and a client gone ending its group. An install that exited or was ended by a
    /// signal is followed by a line saying how long it ran.
    fn ran_install<S: LineSink + 'static>(restore: &Restore<'_, '_, S>, manager: &Manager, folder: &Path, place: &str) -> Installed {
        let (request, output) = (restore.request, restore.output);
        let job = format!("{}-packages", request.job);
        let record = GroupRecord::new(PathBuf::from(packages_group_record(request.work)), &job, restore.lock);
        let mut command = Command::new(manager.program);
        command.args(manager.install_words).current_dir(folder);
        process::with_path(&mut command, restore.along.path);
        if let Some(tmp) = restore.along.tmp {
            command.env("TMPDIR", tmp);
        }
        command.env("CI", "true");
        // SAFETY: the closure runs in the child between fork and exec and only resets the umask, signal dispositions and
        // the mask with async-signal-safe calls ([`signals_reset`]), as the command's does.
        unsafe {
            command.pre_exec(signals_reset);
        }
        // Set once the install's readers started: a failure before it, its group record not readied included, means
        // the install never ran, which refuses as a start that failed does.
        let ran_at_all = Cell::new(false);
        let on_started = |_group: i32| ran_at_all.set(true);
        let watch = Watch {
            cap: 0,
            deadline: None,
            ending: Ending::TermThenKill(END_GRACE),
            output: Some(Arc::clone(output) as Arc<dyn Forwarding>),
            forward: Forward::Both(GAT, GAT),
            ends_on_gone: true,
            record: Some(&record),
            check: None,
            on_started: Some(&on_started),
            on_leader_exit: None,
            processes: None,
        };
        let started = Instant::now();
        let unseen = |why: String| format!("gate: remote could not see the {} install to its end: {why}", manager.name);
        let ran = match run_watched(command, &watch) {
            Ok(ran) => ran,
            Err(NotRan::Start(error)) => return Installed::NotStarted(error.to_string()),
            Err(NotRan::Failed(why)) if !ran_at_all.get() && !output.client_gone() => return Installed::NotStarted(why),
            Err(not) if output.client_gone() => {
                drop(not);
                return Installed::Ended(CLOSED_STATUS);
            }
            Err(not) => return Installed::Ended(said(output, &[&unseen(unran(not))], FAILED_STATUS)),
        };
        if output.client_gone() || ran.ended_by == Some(EndedBy::Gone) {
            return Installed::Ended(CLOSED_STATUS);
        }
        // Said after the install's own output and before any line saying it failed.
        let shown = if place == "." {
            String::new()
        } else {
            format!(" in {}", escaped(place))
        };
        let took = format!("gate: {} install took {:.1} s{shown}", manager.name, started.elapsed().as_secs_f64());
        let _ = output.say(Stream::Err, GAT, &took);
        match (ran.status.code(), ran.status.signal()) {
            (Some(code), _) => Installed::Exited(code),
            (None, Some(signal)) => Installed::Signalled(signal),
            (None, None) => Installed::Ended(said(output, &[&unseen("it gave no status".to_owned())], FAILED_STATUS)),
        }
    }

    /// Why the runner gave no exit for the install, `not`.
    fn unran(not: NotRan) -> String {
        match not {
            NotRan::Failed(why) => why,
            NotRan::Start(error) => error.to_string(),
            NotRan::TimedOut { group } => format!("its process group {group} ran past its deadline"),
            NotRan::OverCap => "it printed more than the box keeps".to_owned(),
        }
    }

    /// Says `lines` on stderr together, stamped `gat`, unless the client is gone; `status`.
    fn said<S: LineSink>(output: &Output<S>, lines: &[&str], status: u8) -> u8 {
        let lines: Vec<(Stamp, &str)> = lines.iter().map(|line| (GAT, *line)).collect();
        said_as(output, &lines, status)
    }

    /// Says `lines` on stderr together, each as its stamp says, unless the client is gone; `status`.
    fn said_as<S: LineSink>(output: &Output<S>, lines: &[(Stamp, &str)], status: u8) -> u8 {
        if !output.client_gone() {
            let lines: Vec<(Stream, Stamp, &str)> = lines.iter().map(|(stamp, line)| (Stream::Err, *stamp, *line)).collect();
            let _ = output.say_together(&lines);
        }
        status
    }

    #[cfg(test)]
    #[expect(clippy::expect_used, reason = "a test that cannot make its folders, files or repository must fail loudly")]
    mod tests {
        use std::fs;
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};
        use std::sync::Arc;

        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;
        use gate_wire::{PathForm, Payload, Refusal, refused_line};
        use serde_json::json;

        use super::{lockfile_for, packages_group_record, real_folder, update};
        use crate::console::{Output, RecordingSink};
        use crate::files::{effective_uid, held_for_test};
        use crate::git::test_program;
        use crate::lines::{Along, Godot};
        use crate::packages::packages_line;
        use crate::run::{REFUSED_STATUS, Request, Unready, run_git};

        const KEY: &str = "repo-0123abcd";
        const NONCE: &str = "0123456789abcdef0123456789abcdef";
        /// A pnpm lockfile's text.
        const LOCKFILE: &str = "lockfileVersion: '9.0'\n";

        /// A work root in a temp folder of the case `name`, private to this process and removed when the case ends:
        /// its `<root>/work`, the work folder a git repository ignoring `node_modules`, and a folder for the manager
        /// shims.
        struct Case {
            base: PathBuf,
        }

        impl Case {
            fn new(name: &str) -> Case {
                let base = std::env::temp_dir().join(format!("gate-box-unix-packages-{}-{name}", std::process::id()));
                let _ = fs::remove_dir_all(&base);
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(base.join("root").join("work"))
                    .expect("the work folders' parent is made");
                let case = Case {
                    base: fs::canonicalize(&base).expect("the case folder resolves"),
                };
                fs::create_dir_all(case.shims()).expect("the shim folder is made");
                git_init(&case.work());
                case.put(".gitignore", "node_modules/\n");
                case
            }

            /// The work folder, `<root>/work/<key>`: the run's main tree.
            fn work(&self) -> PathBuf {
                self.base.join("root").join("work").join(KEY)
            }

            fn shims(&self) -> PathBuf {
                self.base.join("shims")
            }

            fn log(&self) -> PathBuf {
                self.base.join("installs.log")
            }

            /// The work folder's packages record, `<work>.packages`.
            fn record(&self) -> PathBuf {
                PathBuf::from(format!("{}.packages", self.work().display()))
            }

            /// Writes `text` at `name` in the work folder, making its folder.
            fn put(&self, name: &str, text: &str) {
                let path = self.work().join(name);
                fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the file's folder is made");
                fs::write(path, text).expect("the file is written");
            }

            /// A `program` in the shim folder that notes the folder it ran in, its words and `CI` in the log, says a
            /// line on stderr, then runs `then` in that folder.
            fn shim(&self, program: &str, then: &str) {
                let script = format!(
                    "#!/bin/sh\necho \"$(pwd -P) $* CI=$CI\" >> '{log}'\necho installing >&2\n{then}\n",
                    log = self.log().display()
                );
                let path = self.shims().join(program);
                fs::write(&path, script).expect("the shim is written");
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("the shim is made runnable");
            }

            /// The installs the shims noted, each its folder, words and `CI`; none when none ran.
            fn installs(&self) -> Vec<String> {
                fs::read_to_string(self.log())
                    .map(|text| text.lines().map(str::to_owned).collect())
                    .unwrap_or_default()
            }

            /// The shim folder, then the test's own `PATH`.
            fn shim_path(&self) -> String {
                format!("{}:{}", self.shims().display(), std::env::var("PATH").unwrap_or_default())
            }

            /// Restores the packages of a run of `command` in the work folder's `subfolder`, the install found along
            /// `path`; the status, 0 when the run goes on, and the lines said.
            fn restored(&self, command: &[&str], subfolder: &str, path: &str) -> (u8, Vec<String>) {
                let work = self.work().display().to_string();
                let root = self.base.join("root").display().to_string();
                let json = json!({
                    "Key": KEY,
                    "Bare": format!("{root}/repo.git"),
                    "Work": work,
                    "Lock": format!("{work}.lock"),
                    "Index": format!("{work}.index"),
                    "OutputFolder": format!("{root}/outputs"),
                    "Job": format!("gate-remote-{KEY}-0123456789ab"),
                    "Nonce": NONCE,
                    "Snapshot": "a".repeat(40),
                    "Command": command,
                });
                let payload = Payload::decode(&STANDARD.encode(json.to_string()), PathForm::Unix).expect("the payload decodes");
                let request = Request::read(&payload).expect("the request reads");
                let lock = held_for_test(Path::new(request.lock));
                let output = Arc::new(Output::new(RecordingSink::default()));
                let own_path = self.shim_path();
                // Git's own PATH is the test's; only the install's is the case's.
                let git = run_git(&request, &lock, Some(&own_path), &output);
                let along = Along {
                    path: Some(path),
                    tmp: None,
                    godot: Godot::Unchosen,
                };
                let place = self.work().join(subfolder);
                let status = match update(&request, &place, &lock, along, &git, &output) {
                    Ok(()) => 0,
                    Err(Unready::Refused) => REFUSED_STATUS,
                    Err(Unready::Ended(status)) => status,
                };
                let said = output.sink().said().into_iter().map(|(_, line)| line).collect();
                (status, said)
            }

            /// [`Case::restored`] of `command` in the work folder, along the shim folder.
            fn restored_along_shims(&self, command: &[&str]) -> (u8, Vec<String>) {
                self.restored(command, "", &self.shim_path())
            }
        }

        impl Drop for Case {
            fn drop(&mut self) {
                // Best effort: a folder left behind is in the temp folder and named for this process.
                let _ = fs::remove_dir_all(&self.base);
            }
        }

        /// Makes `work` a git repository.
        #[expect(
            clippy::disallowed_methods,
            reason = "the spawn lock is the Windows gate's; this sets a case's repository up"
        )]
        fn git_init(work: &Path) {
            let ran = Command::new("git")
                .args(["init", "-q"])
                .arg(work)
                .stdin(Stdio::null())
                .output()
                .expect("git starts");
            assert!(ran.status.success(), "git init {}", work.display());
        }

        /// Whether `said` holds the line of an install's time.
        fn said_took(said: &[String]) -> bool {
            said.iter().any(|line| line.starts_with("gate: pnpm install took "))
        }

        /// The one install of pnpm in `folder` the shim notes.
        fn pnpm_install(folder: &Path) -> String {
            format!("{} install --frozen-lockfile CI=true", folder.display())
        }

        #[test]
        fn a_command_that_is_no_package_manager_s_restores_nothing() {
            let case = Case::new("none");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            let (status, said) = case.restored_along_shims(&["true"]);
            assert_eq!(status, 0, "{said:?}");
            assert!(said.is_empty(), "{said:?}");
            assert!(case.installs().is_empty(), "no install ran");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn a_changed_lockfile_runs_the_install_once_and_records_its_hash() {
            let case = Case::new("changed");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            case.put("package.json", "{}\n");
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(
                case.installs(),
                [pnpm_install(&case.work())],
                "one install in the work folder, with CI set"
            );
            assert!(said_took(&said), "{said:?}");
            assert!(
                said.iter().any(|line| line == "installing"),
                "the install's stderr is forwarded: {said:?}"
            );
            let recorded = fs::read_to_string(case.record()).expect("the packages record is written");
            let digest = recorded.strip_prefix(".\t").and_then(|rest| rest.strip_suffix('\n'));
            assert!(
                digest.is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())),
                "the work folder's line, a SHA-256 in hex: {recorded:?}"
            );

            case.put("pnpm-lock.yaml", "lockfileVersion: '9.1'\n");
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(case.installs().len(), 2, "one more install for the changed lockfile: {said:?}");
            let rerecorded = fs::read_to_string(case.record()).expect("the packages record is written");
            assert_ne!(rerecorded, recorded, "the changed lockfile's hash is recorded");
        }

        #[test]
        fn an_unchanged_lockfile_skips_the_install() {
            let case = Case::new("unchanged");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(case.installs().len(), 1, "the first run installs: {said:?}");

            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(case.installs().len(), 1, "no install for the unchanged lockfile: {said:?}");
            assert!(said.is_empty(), "{said:?}");
        }

        #[test]
        fn a_missing_node_modules_runs_the_install_again() {
            let case = Case::new("no-node-modules");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            fs::remove_dir(case.work().join("node_modules")).expect("the install's node_modules is removed");

            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(case.installs().len(), 2, "the record alone does not skip the install: {said:?}");
            assert!(said_took(&said), "{said:?}");
        }

        #[test]
        fn a_node_modules_git_does_not_ignore_skips_the_install_and_the_run_goes_on() {
            let case = Case::new("not-ignored");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put(".gitignore", "");
            case.put("pnpm-lock.yaml", LOCKFILE);
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(said, ["gate: remote pnpm install skipped: node_modules is not ignored in ."]);
            assert!(case.installs().is_empty(), "no install ran");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn an_install_that_fails_refuses_the_run_75_with_the_tools_tag() {
            let case = Case::new("fails");
            case.shim("npm", "exit 3");
            case.put("web/package-lock.json", "{}\n");
            let (status, said) = case.restored(&["npm", "test"], "web", &case.shim_path());
            assert_eq!(status, REFUSED_STATUS, "{said:?}");
            assert_eq!(
                case.installs(),
                [format!(
                    "{} ci --ignore-scripts --no-audit --no-fund CI=true",
                    case.work().join("web").display()
                )],
                "npm's install in the lockfile's folder"
            );
            assert!(
                said.iter()
                    .any(|line| line.starts_with("gate: npm install took ") && line.ends_with(" s in web")),
                "{said:?}"
            );
            let failed = said.iter().position(|line| line == "gate: remote package restore failed (status 3)");
            assert!(
                failed.is_some_and(|at| said[at + 1..] == [refused_line(NONCE, Refusal::Tools), packages_line("npm", KEY)]),
                "the failed line, then the tools tag and line, last: {said:?}"
            );
            assert!(!case.record().exists(), "no packages record");
        }

        /// The lines after the could-not-start line of a pnpm install: the failed line, the tools tag and line.
        fn not_started_after() -> [String; 3] {
            [
                "gate: remote package restore failed (status 1)".to_owned(),
                refused_line(NONCE, Refusal::Tools),
                packages_line("pnpm", KEY),
            ]
        }

        #[test]
        fn a_missing_package_manager_refuses_the_run_75_with_the_tools_tag() {
            let case = Case::new("missing");
            case.put("pnpm-lock.yaml", LOCKFILE);
            let empty = case.base.join("no-pnpm");
            fs::create_dir_all(&empty).expect("the empty folder is made");
            let (status, said) = case.restored(&["pnpm", "test"], "", &empty.display().to_string());
            assert_eq!(status, REFUSED_STATUS, "{said:?}");
            let start = said
                .iter()
                .position(|line| line.starts_with("gate: remote could not start the pnpm install: "));
            assert!(
                start.is_some_and(|at| said[at + 1..] == not_started_after()),
                "the could-not-start line, the failed line, then the tools tag and line, last: {said:?}"
            );
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn an_install_whose_group_record_cannot_be_written_refuses_the_run_75() {
            let case = Case::new("record-unwritable");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            // A folder at every temp name the install's group record can be staged under in this process, which the
            // staging cannot remove: the record is never readied, so the install never starts.
            let record = packages_group_record(&case.work().display().to_string());
            for number in 0..1000 {
                let temp = format!("{record}.gate-new-{}-{number}", std::process::id());
                fs::create_dir_all(Path::new(&temp).join("kept")).expect("a folder takes the temp name");
            }
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, REFUSED_STATUS, "{said:?}");
            let start = said
                .iter()
                .position(|line| line.starts_with("gate: remote could not start the pnpm install: "));
            assert!(
                start.is_some_and(|at| said[at + 1..] == not_started_after()),
                "the could-not-start line, the failed line, then the tools tag and line, last: {said:?}"
            );
            assert!(case.installs().is_empty(), "the install never ran");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn a_lockfile_that_cannot_be_looked_at_fails_the_run() {
            let case = Case::new("present-fails");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("web/pnpm-lock.yaml", LOCKFILE);
            let web = case.work().join("web");
            // A folder its own user may not search: the lockfile in it cannot be looked at.
            fs::set_permissions(&web, fs::Permissions::from_mode(0o000)).expect("the folder is closed");
            let (status, said) = case.restored(&["pnpm", "test"], "web", &case.shim_path());
            fs::set_permissions(&web, fs::Permissions::from_mode(0o700)).expect("the folder is opened again");
            assert_eq!(status, 1, "{said:?}");
            let start = format!("gate: remote could not read the lockfile {}: ", web.join("pnpm-lock.yaml").display());
            assert!(said.len() == 1 && said[0].starts_with(&start), "{said:?}");
            assert!(case.installs().is_empty(), "no install ran");
        }

        #[test]
        fn a_package_file_that_cannot_be_read_fails_the_run() {
            let case = Case::new("read-fails");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            // A folder where the package file goes: never a plain file.
            fs::create_dir(case.work().join("package.json")).expect("a folder takes the package file's name");
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 1, "{said:?}");
            let start = format!(
                "gate: remote could not read the package file {}: ",
                case.work().join("package.json").display()
            );
            assert!(said.len() == 1 && said[0].starts_with(&start), "{said:?}");
            assert!(case.installs().is_empty(), "no install ran");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn a_lockfile_folder_reached_through_a_link_skips_the_install() {
            let case = Case::new("linked");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("real/pnpm-lock.yaml", LOCKFILE);
            symlink(case.work().join("real"), case.work().join("web")).expect("the link is made");
            let (status, said) = case.restored(&["pnpm", "test"], "web", &case.shim_path());
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(said, ["gate: remote pnpm install skipped: web is reached through a link"]);
            assert!(case.installs().is_empty(), "no install ran");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn a_node_modules_that_is_a_link_is_never_installed_through() {
            let case = Case::new("modules-linked");
            case.shim("pnpm", "mkdir -p node_modules/x");
            case.put("pnpm-lock.yaml", LOCKFILE);
            let outside = case.base.join("elsewhere");
            fs::create_dir_all(&outside).expect("the link's target is made");
            symlink(&outside, case.work().join("node_modules")).expect("the link is made");
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(said, ["gate: remote pnpm install skipped: node_modules is reached through a link"]);
            assert!(case.installs().is_empty(), "no install ran");
            let held = fs::read_dir(&outside).expect("the link's target reads").count();
            assert_eq!(held, 0, "the link's target is untouched");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn a_package_file_with_another_link_fails_the_run() {
            let case = Case::new("hard-linked");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            case.put("other.json", "{}\n");
            fs::hard_link(case.work().join("other.json"), case.work().join("package.json")).expect("the hard link is made");
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 1, "{said:?}");
            let start = format!(
                "gate: remote could not read the package file {}: ",
                case.work().join("package.json").display()
            );
            assert!(said.len() == 1 && said[0].starts_with(&start), "{said:?}");
            assert!(case.installs().is_empty(), "no install ran");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn a_folder_on_the_walk_that_cannot_be_looked_at_fails_it() {
            if effective_uid() == 0 {
                // Root looks into any folder, whatever its mode.
                return;
            }
            let case = Case::new("folder-closed");
            let closed = case.work().join("closed");
            let web = closed.join("web");
            fs::create_dir_all(&web).expect("the folders are made");
            // A folder its own user may not search: the folder in it cannot be looked at.
            fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).expect("the folder is closed");
            let found = lockfile_for(&[String::from("pnpm")], &case.work(), &web, &|_| Ok(true), &real_folder);
            fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).expect("the folder is opened again");
            let start = format!("gate: remote could not read the folder {}: ", web.display());
            let found = found.map(|found| found.map(|(manager, found)| (manager.name, found)));
            assert!(
                found.as_ref().is_err_and(|line| line.starts_with(&start)),
                "an error, not a link: {found:?}"
            );
        }

        #[test]
        fn a_check_ignore_ended_by_a_signal_ends_the_run() {
            let case = Case::new("check-ignore-killed");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            let shim = case.base.join("git-shim");
            let script = format!(
                "#!/bin/sh\ncase \" $* \" in\n*\" check-ignore \"*) kill -TERM $$;;\nesac\nexec '{}' \"$@\"\n",
                test_program::real_git().display()
            );
            fs::write(&shim, script).expect("the git shim is written");
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).expect("the git shim is made runnable");
            test_program::set(&shim);
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 1, "{said:?}");
            assert_eq!(said, ["gate: remote check-ignore failed: git ended with signal 15"]);
            assert!(case.installs().is_empty(), "no install ran");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn an_install_that_fails_leaves_its_folder_unrecorded_so_the_next_run_installs_again() {
            let case = Case::new("fails-unrecorded");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            let recorded = fs::read_to_string(case.record()).expect("the packages record is written");

            // The lockfile changes and its install fails partway, leaving its node_modules.
            case.put("pnpm-lock.yaml", "lockfileVersion: '9.1'\n");
            case.shim("pnpm", "mkdir -p node_modules\nexit 3");
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, REFUSED_STATUS, "{said:?}");
            let left = fs::read_to_string(case.record()).expect("the packages record is there");
            assert_eq!(left, "", "the folder's line is dropped before the install");

            // Back to the recorded lockfile: the node_modules is the failed install's, so the install runs again.
            case.put("pnpm-lock.yaml", LOCKFILE);
            case.shim("pnpm", "mkdir -p node_modules");
            let (status, said) = case.restored_along_shims(&["pnpm", "test"]);
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(case.installs().len(), 3, "the third run installs: {said:?}");
            assert_eq!(fs::read_to_string(case.record()).expect("the packages record is written"), recorded);
        }

        #[test]
        fn a_lockfile_that_is_a_link_fails_the_run_and_installs_nowhere() {
            let case = Case::new("lockfile-linked");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("pnpm-lock.yaml", LOCKFILE);
            case.put("web/real-lock.yaml", LOCKFILE);
            let web = case.work().join("web");
            symlink(web.join("real-lock.yaml"), web.join("pnpm-lock.yaml")).expect("the link is made");
            let (status, said) = case.restored(&["pnpm", "test"], "web", &case.shim_path());
            assert_eq!(status, 1, "{said:?}");
            let start = format!("gate: remote could not read the lockfile {}: ", web.join("pnpm-lock.yaml").display());
            assert!(said.len() == 1 && said[0].starts_with(&start), "{said:?}");
            assert!(case.installs().is_empty(), "no install ran");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn a_start_folder_that_is_a_file_restores_nothing_and_runs_the_command() {
            let case = Case::new("start-file");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("web", "not a folder\n");
            let (status, said) = case.restored(&["pnpm", "test"], "web", &case.shim_path());
            assert_eq!(status, 0, "{said:?}");
            assert!(said.is_empty(), "{said:?}");
            assert!(case.installs().is_empty(), "no install ran");
            assert!(!case.record().exists(), "no packages record");
        }

        #[test]
        fn an_install_drops_the_other_managers_lines_for_its_folder() {
            let case = Case::new("drops-others");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put("web/pnpm-lock.yaml", LOCKFILE);
            let (a, b) = ("a".repeat(64), "b".repeat(64));
            fs::write(case.record(), format!("bun:web\t{a}\nnpm:web\t{b}\n")).expect("the packages record is written");
            let (status, said) = case.restored(&["pnpm", "test"], "web", &case.shim_path());
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(case.installs(), [pnpm_install(&case.work().join("web"))]);
            let recorded = fs::read_to_string(case.record()).expect("the packages record is written");
            let digest = recorded.strip_prefix("web\t").and_then(|rest| rest.strip_suffix('\n'));
            assert!(
                digest.is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())),
                "pnpm's line for web alone: {recorded:?}"
            );
        }

        #[test]
        fn a_subfolder_s_node_modules_is_asked_with_its_folder_in_the_pathspec() {
            let case = Case::new("pathspec-web");
            case.shim("pnpm", "mkdir -p node_modules");
            case.put(".gitignore", "/web/node_modules/\n");
            case.put("web/pnpm-lock.yaml", LOCKFILE);
            let (status, said) = case.restored(&["pnpm", "test"], "web", &case.shim_path());
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(case.installs(), [pnpm_install(&case.work().join("web"))], "{said:?}");

            let root = Case::new("pathspec-root");
            root.shim("pnpm", "mkdir -p node_modules");
            root.put(".gitignore", "/node_modules/\n");
            root.put("web/pnpm-lock.yaml", LOCKFILE);
            let (status, said) = root.restored(&["pnpm", "test"], "web", &root.shim_path());
            assert_eq!(status, 0, "{said:?}");
            assert_eq!(said, ["gate: remote pnpm install skipped: node_modules is not ignored in web"]);
            assert!(root.installs().is_empty(), "no install ran");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io;
    use std::path::{Path, PathBuf};

    use super::{
        BUN, Found, NPM, PNPM, Record, hash, holds_other_lockfile, is_command, key_holdable, lockfile_folder, lockfile_for, record_key, relative,
        start_folder, start_named, start_word,
    };

    /// The files of a test, by path: the disk the readers read.
    type Files = BTreeMap<PathBuf, Vec<u8>>;

    /// Whether `files` holds a file at a path.
    fn present(files: &Files) -> impl Fn(&Path) -> io::Result<bool> + '_ {
        move |path| Ok(files.contains_key(path))
    }

    /// The bytes `files` holds at a path, `None` when it holds none.
    fn read(files: &Files) -> impl Fn(&Path) -> io::Result<Option<Vec<u8>>> + '_ {
        move |path| Ok(files.get(path).cloned())
    }

    /// Says every folder is a folder itself.
    fn all_real() -> impl Fn(&Path) -> io::Result<bool> {
        |_| Ok(true)
    }

    /// Puts the file `name` holding `content` in `folder`.
    fn put(files: &mut Files, folder: &Path, name: &str, content: &[u8]) {
        files.insert(folder.join(name), content.to_vec());
    }

    /// `words` as the owned words a request holds.
    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|&word| word.to_owned()).collect()
    }

    /// Puts a pnpm lockfile in `folder`.
    fn lockfile(files: &mut Files, folder: &Path) {
        put(files, folder, "pnpm-lock.yaml", b"lockfileVersion: '9.0'\n");
    }

    /// The work root and place folders of the start-folder cases.
    fn root_and_place() -> (PathBuf, PathBuf) {
        (PathBuf::from("/work/app"), PathBuf::from("/work/app/web"))
    }

    #[test]
    fn a_pnpm_word_triggers_in_any_case_and_form() {
        for word in ["pnpm", "PNPM.CMD", "pnpm.ps1", "/usr/bin/pnpm", "/opt/homebrew/bin/pnpm"] {
            assert!(is_command(&PNPM, word), "{word} names pnpm");
        }
        for word in ["pnpmx", "npx", "node", "corepack", "", "/pnpm/node", "xpnpm.cmd"] {
            assert!(!is_command(&PNPM, word), "{word} does not name pnpm");
        }
    }

    #[test]
    fn a_bun_or_bunx_word_triggers_in_any_case_and_form() {
        for word in ["bun", "bunx", "BUNX.EXE", "/usr/bin/bun", "/x/bun.exe"] {
            assert!(is_command(&BUN, word), "{word} names bun");
            assert!(!is_command(&PNPM, word), "{word} does not name pnpm");
        }
        for word in ["bunyan", "bunk", "xbun.exe", "pnpm", "", "/bun/node"] {
            assert!(!is_command(&BUN, word), "{word} does not name bun");
        }
    }

    #[test]
    fn bun_reads_cwd_before_its_subcommand_and_after_run_or_test() {
        let start = |first: &str, rest: &[&str]| start_named(&BUN, first, &words(rest)).map(str::to_owned);
        assert_eq!(start("bun", &["--cwd=web", "test"]), Some("web".to_owned()));
        assert_eq!(
            start("bun", &["test", "-t", "smoke", "--cwd", "web"]),
            None,
            "another option's value ends the scan"
        );
        assert_eq!(start("bun", &["--cwd=web", "--version"]), Some("web".to_owned()), "no subcommand");
        assert_eq!(start("bun", &["run", "--cwd", "web", "x"]), Some("web".to_owned()));
        assert_eq!(start("bun", &["run", "--cwd=web", "x"]), Some("web".to_owned()));
        assert_eq!(start("bun", &["test", "--cwd", "web"]), Some("web".to_owned()));
        assert_eq!(
            start("bun", &["--cwd=a", "run", "--cwd", "b", "x"]),
            Some("b".to_owned()),
            "the last wins"
        );
        assert_eq!(
            start("bun", &["--cwd", "web", "test"]),
            None,
            "a bare `--cwd` before the subcommand ends the scan"
        );
        assert_eq!(
            start("bun", &["--cwd=a", "--cwd", "b", "run"]),
            Some("a".to_owned()),
            "a bare `--cwd` changes nothing"
        );
        assert_eq!(start("bun", &["run", "x", "--cwd", "web"]), None, "after the script is the script's");
        assert_eq!(start("bun", &["test", "src", "--cwd=web"]), None, "after the test path is the tests'");
        assert_eq!(start("bun", &["install", "--cwd", "web"]), None, "another subcommand's words are its own");
        assert_eq!(start("bun", &["run", "--cwd", "--", "x"]), None, "a lone `--` is never a value");
        assert_eq!(start("bun", &["run", "--cwd"]), None, "a trailing `--cwd` has no value");
        assert_eq!(start("bun", &["--cwd=", "test"]), None, "an empty value is none");
        assert_eq!(start("bun", &[]), None);
        assert_eq!(start("bunx", &["--cwd=p", "hello"]), None, "bunx looks its bin up from the place");
        assert_eq!(start("bun", &["--cwd=p", "x", "hello"]), None, "so does `bun x`");
        assert_eq!(start("bun", &["x", "--cwd", "p", "hello"]), None, "after `x` too");
        assert_eq!(start("BUNX.EXE", &["--cwd", "p", "hello"]), None);
    }

    #[test]
    fn a_bun_start_folder_is_joined_to_the_place() {
        let (work, place) = root_and_place();
        assert_eq!(
            start_folder(&work, &place, start_named(&BUN, "bun", &words(&["--cwd=..", "test"]))),
            Ok(work.clone()),
            "relative to the place"
        );
        assert_eq!(
            start_folder(&work, &place, start_named(&BUN, "bunx", &words(&["--cwd=..", "vitest"]))),
            Ok(place.clone()),
            "bunx starts at the place"
        );
    }

    #[test]
    fn the_last_dir_or_c_before_a_lone_dash_dash_names_the_start_folder() {
        assert_eq!(start_word(&words(&["--dir", "a", "run", "x"])), Some("a"));
        assert_eq!(start_word(&words(&["--dir=a", "install"])), Some("a"));
        assert_eq!(start_word(&words(&["-C", "a", "test"])), Some("a"));
        assert_eq!(start_word(&words(&["-Ca", "test"])), Some("a"));
        assert_eq!(start_word(&words(&["--dir", "a", "-C", "b", "run", "x"])), Some("b"), "the last wins");
        assert_eq!(
            start_word(&words(&["-Cb", "run", "--dir=c"])),
            Some("b"),
            "after the script is the script's"
        );
        assert_eq!(start_word(&words(&["vitest", "run", "--dir", "src"])), None, "a bin's own `--dir`");
        assert_eq!(
            start_word(&words(&["exec", "git", "-C", "../x", "status"])),
            None,
            "exec's program's `-C`"
        );
        assert_eq!(
            start_word(&words(&["--filter", "x", "--dir", "a"])),
            None,
            "another option's value ends the scan"
        );
        assert_eq!(
            start_word(&words(&["--dir", "a", "exec", "--", "--dir", "b"])),
            Some("a"),
            "after `--` is the program's"
        );
        assert_eq!(start_word(&words(&["exec", "--", "-Cb"])), None);
        assert_eq!(start_word(&words(&["run", "x", "--dir"])), None, "a trailing `--dir` has no value");
        assert_eq!(start_word(&words(&["-C", "a", "--dir"])), Some("a"), "a trailing `--dir` changes nothing");
        assert_eq!(start_word(&words(&["--dir", "--", "x"])), None, "a lone `--` is never a value");
        assert_eq!(start_word(&words(&["--dir=", "-C"])), None, "an empty value is none");
        assert_eq!(start_word(&words(&["run", "build"])), None);
        assert_eq!(start_word(&[]), None);
    }

    #[test]
    fn a_start_folder_outside_the_work_root_is_refused() {
        let (work, place) = root_and_place();
        let start = |named: &[&str]| start_folder(&work, &place, start_word(&words(named)));
        assert_eq!(start(&["run", "x"]), Ok(place.clone()), "no folder named: the place");
        assert_eq!(start(&["--dir", "../api"]), Ok(PathBuf::from("/work/app/api")), "relative to the place");
        assert_eq!(start(&["-C", ".."]), Ok(work.clone()), "the work root itself");
        assert_eq!(start(&["--dir", "./lib/."]), Ok(PathBuf::from("/work/app/web/lib")), "a `.` is dropped");
        assert_eq!(
            start(&["--dir", "/work/app/lib"]),
            Ok(PathBuf::from("/work/app/lib")),
            "an absolute folder inside"
        );
        assert_eq!(
            start(&["--dir", "/WORK/APP/lib"]),
            Err(PathBuf::from("/WORK/APP/lib")),
            "a folder in another case is another folder"
        );
        assert_eq!(
            start(&["--dir", "../../x"]),
            Err(PathBuf::from("/work/x")),
            "climbing out of the work root"
        );
        assert_eq!(
            start(&["--dir", "../../../../.."]),
            Err(PathBuf::from("/")),
            "climbing above the filesystem root stays at the root"
        );
        assert_eq!(
            start(&["--dir", "/work/app/../x"]),
            Err(PathBuf::from("/work/x")),
            "an absolute folder climbing out of the work root"
        );
        assert_eq!(
            start(&["--dir", "/elsewhere"]),
            Err(PathBuf::from("/elsewhere")),
            "an absolute folder elsewhere"
        );
        assert_eq!(
            start(&["--dir=/work/application"]),
            Err(PathBuf::from("/work/application")),
            "a sibling whose name starts with the root's is outside"
        );
    }

    #[test]
    fn the_walk_finds_the_nearest_lockfile_up_to_the_work_root() {
        let scratch = PathBuf::from("/scratch");
        let work = scratch.join("work");
        let place = work.join("web").join("app");
        let mut files = Files::new();
        lockfile(&mut files, &scratch);
        assert_eq!(
            lockfile_folder(&PNPM, &work, &place, &present(&files)),
            Ok(None),
            "a lockfile above the work root is never read"
        );

        lockfile(&mut files, &work);
        assert_eq!(
            lockfile_folder(&PNPM, &work, &place, &present(&files)),
            Ok(Some(work.clone())),
            "the work root's, reached from below"
        );

        lockfile(&mut files, &work.join("web"));
        assert_eq!(
            lockfile_folder(&PNPM, &work, &place, &present(&files)),
            Ok(Some(work.join("web"))),
            "the nearest parent's"
        );

        let leaf = work.join("leaf");
        lockfile(&mut files, &leaf);
        assert_eq!(
            lockfile_folder(&PNPM, &work, &leaf, &present(&files)),
            Ok(Some(leaf.clone())),
            "the start folder's own"
        );
    }

    #[test]
    fn the_bun_walk_finds_either_lockfile_and_no_other() {
        let work = PathBuf::from("/scratch/work");
        let place = work.join("web").join("app");
        let mut files = Files::new();
        lockfile(&mut files, &place);
        assert_eq!(
            lockfile_folder(&BUN, &work, &place, &present(&files)),
            Ok(None),
            "a pnpm lockfile is no bun lockfile"
        );

        put(&mut files, &work, "bun.lock", b"{}\n");
        assert_eq!(
            lockfile_folder(&BUN, &work, &place, &present(&files)),
            Ok(Some(work.clone())),
            "a bun.lock"
        );

        put(&mut files, &work.join("web"), "bun.lockb", &[0, 1]);
        assert_eq!(
            lockfile_folder(&BUN, &work, &place, &present(&files)),
            Ok(Some(work.join("web"))),
            "the nearest folder with a bun.lockb"
        );
    }

    /// The SHA-256 of the five bun package files [`the_bun_hash_reads_its_files_in_order`] writes, laid out as
    /// [`FOUR_FILES_HASH`] is; computed apart from this code.
    const BUN_FIVE_FILES_HASH: &str = "21768ac03a3f80b0670adcdabd467ee5f7baedb4b686844010048f7775ef98d6";
    /// The same of `bun.lock` and `.npmrc` alone, the three others absent.
    const BUN_TWO_FILES_HASH: &str = "c033598b4fa507d3580e82ea57aa57dced7d193fa883259f67db8e05a62fc40c";

    #[test]
    fn the_bun_hash_reads_its_files_in_order() {
        let folder = PathBuf::from("/scratch/web");
        let mut files = Files::new();
        put(&mut files, &folder, ".npmrc", b"a=1\n");
        put(&mut files, &folder, "bun.lock", b"lock\n");
        put(&mut files, &folder, "pnpm-lock.yaml", b"not bun's\n");
        assert_eq!(
            hash(&BUN, &folder, &read(&files)),
            Ok(BUN_TWO_FILES_HASH.to_owned()),
            "an absent file is skipped"
        );

        put(&mut files, &folder, "bunfig.toml", b"[install]\n");
        put(&mut files, &folder, "package.json", b"{}\n");
        put(&mut files, &folder, "bun.lockb", &[0, 1, b'l', b'o', b'c', b'k', b'b']);
        assert_eq!(
            hash(&BUN, &folder, &read(&files)),
            Ok(BUN_FIVE_FILES_HASH.to_owned()),
            "bun.lock, bun.lockb, package.json, bunfig.toml, .npmrc"
        );
    }

    #[test]
    fn an_npm_or_npx_word_triggers_in_any_case_and_form() {
        for word in ["npm", "npx", "NPM.cmd", "NPX.EXE", "/opt/homebrew/bin/npm", "npx.ps1", "/usr/bin/npm"] {
            assert!(is_command(&NPM, word), "{word} names npm");
            assert!(!is_command(&PNPM, word), "{word} does not name pnpm");
            assert!(!is_command(&BUN, word), "{word} does not name bun");
        }
        for word in ["npmx", "npxx", "node", "pnpm", "bun", "", "/npm/node", "xnpm.cmd"] {
            assert!(!is_command(&NPM, word), "{word} does not name npm");
        }
    }

    #[test]
    fn the_npm_walk_finds_either_lockfile_and_no_other() {
        let work = PathBuf::from("/scratch/work");
        let place = work.join("web").join("app");
        let mut files = Files::new();
        lockfile(&mut files, &place);
        assert_eq!(
            lockfile_folder(&NPM, &work, &place, &present(&files)),
            Ok(None),
            "a pnpm lockfile is no npm lockfile"
        );

        put(&mut files, &work, "npm-shrinkwrap.json", b"{}\n");
        assert_eq!(
            lockfile_folder(&NPM, &work, &place, &present(&files)),
            Ok(Some(work.clone())),
            "an npm-shrinkwrap.json"
        );

        put(&mut files, &work.join("web"), "package-lock.json", b"{}\n");
        assert_eq!(
            lockfile_folder(&NPM, &work, &place, &present(&files)),
            Ok(Some(work.join("web"))),
            "the nearest folder with a package-lock.json, the package lock read before the shrinkwrap"
        );
        assert_eq!(
            NPM.lockfiles.to_vec(),
            vec!["package-lock.json", "npm-shrinkwrap.json"],
            "package-lock.json is the lockfile the walk reads first"
        );
    }

    /// The SHA-256 [`hash`] gives the package files `files`, each its name, a NUL byte, its bytes and a NUL byte in
    /// order, computed here from the bytes rather than by the code under test.
    fn digest(files: &[(&str, &[u8])]) -> String {
        use std::fmt::Write as _;

        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for (name, content) in files {
            hasher.update(name.as_bytes());
            hasher.update([0_u8]);
            hasher.update(content);
            hasher.update([0_u8]);
        }
        hasher.finalize().iter().fold(String::with_capacity(64), |mut hex, byte| {
            // Writing to a String cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }

    #[test]
    fn the_npm_hash_reads_its_files_in_order() {
        let folder = PathBuf::from("/scratch/web");
        let mut files = Files::new();
        put(&mut files, &folder, ".npmrc", b"a=1\n");
        put(&mut files, &folder, "package-lock.json", b"lock\n");
        put(&mut files, &folder, "bun.lock", b"not npm's\n");
        assert_eq!(
            hash(&NPM, &folder, &read(&files)),
            Ok(digest(&[("package-lock.json", b"lock\n"), (".npmrc", b"a=1\n")])),
            "an absent file is skipped"
        );

        put(&mut files, &folder, "package.json", b"{}\n");
        put(&mut files, &folder, "npm-shrinkwrap.json", b"{\"x\":1}\n");
        assert_eq!(
            hash(&NPM, &folder, &read(&files)),
            Ok(digest(&[
                ("package-lock.json", b"lock\n"),
                ("npm-shrinkwrap.json", b"{\"x\":1}\n"),
                ("package.json", b"{}\n"),
                (".npmrc", b"a=1\n"),
            ])),
            "package-lock.json, npm-shrinkwrap.json, package.json, .npmrc"
        );
    }

    #[test]
    fn an_npm_command_restores_nothing_in_a_folder_with_a_bun_lockfile() {
        let work = PathBuf::from("/scratch/work");
        let mut files = Files::new();
        put(&mut files, &work, "package-lock.json", b"{}\n");
        put(&mut files, &work, "bun.lock", b"{}\n");

        assert_eq!(
            holds_other_lockfile(&work, &present(&files)),
            Ok(true),
            "a bun.lock beside the npm lockfile is another manager's"
        );
        let found = lockfile_for(&words(&["npm", "test"]), &work, &work, &present(&files), &all_real());
        let skipped = matches!(found, Ok(Some((manager, Found::OtherManager(ref folder)))) if manager.name == "npm" && *folder == work);
        assert!(
            skipped,
            "the npm command restores nothing, saying the folder holds a pnpm or bun lockfile"
        );
    }

    #[test]
    fn a_folder_key_is_relative_to_the_work_root_with_slashes() {
        let work = Path::new("/scratch/work");
        assert_eq!(relative(work, work), Some(".".to_owned()));
        assert_eq!(relative(work, &work.join("web").join("app")), Some("web/app".to_owned()));
        assert_eq!(relative(work, Path::new("/scratch/other")), None, "a folder outside the work root");
    }

    #[test]
    fn a_folder_key_holding_a_tab_or_newline_is_refused() {
        let work = Path::new("/scratch/work");
        assert!(key_holdable(work, work), "the work root's own key");
        assert!(key_holdable(work, &work.join("web").join("app")));
        for name in ["we\tb", "we\nb", "we\rb", "we\u{7f}b"] {
            assert!(!key_holdable(work, &work.join(name).join("app")), "{name:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_key_that_is_not_utf8_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let work = Path::new("/scratch/work");
        assert!(!key_holdable(work, &work.join(OsStr::from_bytes(b"we\xffb"))));
    }

    #[test]
    fn an_npm_folder_key_starts_with_npm() {
        assert_eq!(record_key(&NPM, "web"), "npm:web");
        assert_eq!(record_key(&NPM, "."), "npm:.");
        let mut record = Record::default();
        let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        record.set(&record_key(&PNPM, "web"), &a);
        record.set(&record_key(&BUN, "web"), &b);
        record.set(&record_key(&NPM, "web"), &c);
        assert_eq!(
            record.text(),
            format!("bun:web\t{b}\nnpm:web\t{c}\nweb\t{a}\n"),
            "one line for each manager, sorted"
        );
    }

    #[test]
    fn a_bun_folder_key_is_prefixed_and_a_pnpm_one_is_not() {
        assert_eq!(record_key(&BUN, "web"), "bun:web");
        assert_eq!(record_key(&BUN, "."), "bun:.");
        assert_eq!(record_key(&PNPM, "web"), "web");
        let mut record = Record::default();
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        record.set(&record_key(&PNPM, "web"), &a);
        record.set(&record_key(&BUN, "web"), &b);
        assert_eq!(record.text(), format!("bun:web\t{b}\nweb\t{a}\n"), "one line for each manager");
    }

    /// The SHA-256 of the four package files [`hash_layout_is_name_nul_bytes_nul`] writes, in hash order, each its
    /// name, a NUL byte, its bytes and a NUL byte, as lower-case hex; computed apart from this code, so a change to the
    /// layout fails the test.
    const FOUR_FILES_HASH: &str = "803501eb14e70316e17f89138a20bf9d4eae7d8008fce25f332918b8974ea19d";
    /// The same of the lockfile and `.npmrc` alone, the two others absent.
    const TWO_FILES_HASH: &str = "aba0ef6ce5e9c3c599a0e90e593c255d130d10113869fa0ab30fd5dd8a891675";

    #[test]
    fn hash_layout_is_name_nul_bytes_nul() {
        let folder = PathBuf::from("/scratch/web");
        let mut files = Files::new();
        put(&mut files, &folder, ".npmrc", b"a=1\n");
        put(&mut files, &folder, "pnpm-lock.yaml", b"lock\n");
        assert_eq!(
            hash(&PNPM, &folder, &read(&files)),
            Ok(TWO_FILES_HASH.to_owned()),
            "an absent file is skipped"
        );

        put(&mut files, &folder, "pnpm-workspace.yaml", b"packages: []\n");
        put(&mut files, &folder, "package.json", b"{}\n");
        assert_eq!(
            hash(&PNPM, &folder, &read(&files)),
            Ok(FOUR_FILES_HASH.to_owned()),
            "the name, a NUL, the bytes, a NUL"
        );
    }

    #[test]
    fn record_keys_differ_by_letter_case() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let mut record = Record::default();
        record.set("web", &a);
        record.set("Web", &b);
        assert!(record.holds("web", &a), "the lower-case key keeps its line");
        assert!(record.holds("Web", &b), "the other case is another key");
        assert!(!record.holds("WEB", &a), "a third case finds nothing");
        assert_eq!(record.text(), format!("Web\t{b}\nweb\t{a}\n"), "two lines, sorted as bytes");
        assert!(!record.remove("WEB"), "removing a third case removes nothing");
        assert!(record.remove("Web"));
        assert_eq!(record.text(), format!("web\t{a}\n"), "the other case's line kept");
    }

    #[test]
    fn the_record_keeps_other_folders_lines_sorted() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        let text = format!(
            "web\t{a}\nnot a line\n.\t{b}\nshort\tabc\nUPPER\t{}\napi\t{b}\textra\n\t{a}\n",
            "A".repeat(64)
        );
        let mut record = Record::parse(&text);

        assert!(record.holds("web", &a));
        assert!(!record.holds("WEB", &a), "keys compare as bytes");
        assert!(record.holds(".", &b));
        assert!(!record.holds("web", &b), "another hash");
        assert!(!record.holds("short", "abc"), "a malformed hash is dropped");
        assert!(!record.holds("UPPER", &"A".repeat(64)), "an upper-case hash is dropped");
        assert!(!record.holds("api", &b), "a line with a third field is dropped");
        assert_eq!(record.text(), format!(".\t{b}\nweb\t{a}\n"), "the valid lines, sorted");

        record.set("web", &c);
        record.set("tests/browser", &a);
        assert_eq!(
            record.text(),
            format!(".\t{b}\ntests/browser\t{a}\nweb\t{c}\n"),
            "a folder's line replaced, a new one added, the others kept, sorted"
        );
        assert_eq!(Record::parse(""), Record::default());
        assert_eq!(Record::parse(&record.text()), record, "the text reads back");
    }
}
