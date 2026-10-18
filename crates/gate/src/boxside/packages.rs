//! The repo's pnpm, bun and npm packages, installed before a routed command of their package manager, after the dotnet
//! tool restore: a `pnpm` command's `pnpm-lock.yaml`, a `bun` or `bunx` command's `bun.lock` or `bun.lockb`, or an
//! `npm` or `npx` command's `package-lock.json` or `npm-shrinkwrap.json`, nearest the folder the command starts in,
//! never above the main repo's tree, has its folder's package files hashed and compared with that folder's line for the
//! manager in the work folder's `<Work>.packages` record, so `pnpm install --frozen-lockfile`, `bun install
//! --frozen-lockfile` or `npm ci --ignore-scripts --no-audit --no-fund` runs there only when a package file changed or
//! `node_modules` is gone, and only when git ignores `node_modules` and no folder from the work root down to the
//! lockfile's is a link or a junction. Every manager installs into the folder's one `node_modules`, so a passed install
//! drops the other managers' lines for its folder, and a run whose commands need two managers in one folder is refused.
//! An npm or npx command restores nothing in a folder holding a pnpm or bun lockfile, whose manager owns the folder's
//! `node_modules` instead. The install runs with the command's restricted token, its `safe.directory` and `CI=true`,
//! in a job of its own that ends every process in it when the box's handle to it closes; its two streams are forwarded
//! like the command's. It ends when it ends, when a stop sets the run's stop signal, or when a write to the box's own
//! output fails. An install that fails or cannot start refuses the run with status 75, as a failed tool restore does, so
//! the gate falls back to a local run.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::PoisonError;
use std::time::Instant;

use sha2::{Digest, Sha256};

use gate_win32::{Job, PipedConsole, PipedLaunch, PipedSuspended, RestrictedToken, Running};

use crate::boxside::forward::{DRAIN, Forwarders, OutputLock};
use crate::boxside::payload::{escaped, ordinal_upper};
use crate::boxside::preflight::{is_real_folder, locate};
use crate::boxside::run::{
    Beat, FAILED_STATUS, FILE_FLAG_OPEN_REPARSE_POINT, Folders, Look, REFUSED_STATUS, Request, Stopper, WaitEnd, Watched, Watching, WorkGit,
    before_the_job, box_variables, end_job, full_path, said_stopped, with_safe_directory,
};
use crate::boxside::stop::{packages_job_name, terminate_and_wait};
use crate::boxside::tools::{read_record, write_record_file};
use crate::console::Console;
use crate::remote::{Refusal, refused_line};
use crate::run::{ERROR_FILE_NOT_FOUND, Written, written};

/// A package manager whose packages the box installs before a routed command of it.
struct Manager {
    /// Its name in the box's lines.
    name: &'static str,
    /// The program its install runs, found on the box's `PATH`.
    program: &'static str,
    /// The words its install runs, after its program.
    install_words: &'static [&'static str],
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

/// The package managers the box installs for, in the order a command is matched against them and two of them are named.
const MANAGERS: [&Manager; 3] = [&PNPM, &BUN, &NPM];

/// The hex digit of each hash byte's high nibble, the low nibble reading from the same table.
const HEX: &[u8; 16] = b"0123456789abcdef";
/// The path git is asked about: with its trailing separator, so a folder pattern such as `node_modules/` matches it
/// before the folder exists.
const NODE_MODULES: &str = "node_modules/";

/// The line the box says when the work folder `key` could not restore the packages of the manager `name`, just after
/// its refusal tag.
pub(crate) fn packages_line(name: &str, key: &str) -> String {
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

/// Whether `path` is the full path `work` or a folder below it, names compared as `OrdinalIgnoreCase` compares them.
fn is_within(work: &Path, path: &Path) -> bool {
    let (work_text, path_text) = (ordinal_upper(&work.to_string_lossy()), ordinal_upper(&path.to_string_lossy()));
    path_text == work_text || path_text.starts_with(&format!("{work_text}\\"))
}

/// The folder the lockfile walk starts in: the folder `named` by the command's start-folder option, joined to `place`
/// when relative and folded, else `place`; `Err` with the folder when it lies outside the work root `work`.
fn start_folder(work: &Path, place: &Path, named: Option<&str>) -> Result<PathBuf, PathBuf> {
    let Some(named) = named else {
        return Ok(place.to_owned());
    };
    let joined = place.join(named);
    match full_path(&joined) {
        Some(full) if is_within(work, &full) => Ok(full),
        Some(full) => Err(full),
        None => Err(joined),
    }
}

/// The folder of the first of `manager`'s lockfiles that is a file, from `start` up to the work root `work` inclusive,
/// never above it; none when there is none. A lockfile whose metadata cannot be read for any reason but its absence is
/// an error, the line saying so.
fn lockfile_folder(manager: &Manager, work: &Path, start: &Path) -> Result<Option<PathBuf>, String> {
    let mut here = start.to_owned();
    while is_within(work, &here) {
        if holds_lockfile(manager, &here)? {
            return Ok(Some(here));
        }
        if !here.pop() {
            break;
        }
    }
    Ok(None)
}

/// Whether `folder` holds one of `manager`'s lockfiles as a file; `Err` with the line saying one's metadata cannot be
/// read for any reason but its absence.
fn holds_lockfile(manager: &Manager, folder: &Path) -> Result<bool, String> {
    for name in manager.lockfiles {
        let path = folder.join(name);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(unreadable(manager, &path, name, &error)),
        }
    }
    Ok(false)
}

/// Whether `folder` also holds a pnpm or a bun lockfile, whose manager owns its `node_modules`, so npm restores nothing
/// there; `Err` with the line saying one's metadata cannot be read.
fn holds_other_lockfile(folder: &Path) -> Result<bool, String> {
    for manager in [&PNPM, &BUN] {
        if holds_lockfile(manager, folder)? {
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

/// The first folder from the work root `work` down to `folder`, both included, that is a link or a junction rather
/// than a folder itself, relative to the work root; none when every one is a folder.
fn linked(work: &Path, folder: &Path) -> Option<String> {
    let mut here = work.to_owned();
    let below = relative(work, folder);
    let steps = if below == "." { Vec::new() } else { below.split('\\').collect() };
    if !is_real_folder(&here) {
        return Some(".".to_owned());
    }
    for step in steps {
        here.push(step);
        if !is_real_folder(&here) {
            return Some(relative(work, &here));
        }
    }
    None
}

/// The `<Work>.packages` record: for each folder whose install passed, its path relative to the work root and the hash
/// of its package files then, one `{folder}\t{hash}` line each, sorted by folder.
#[derive(Debug, Default, PartialEq, Eq)]
struct Record(BTreeMap<String, String>);

impl Record {
    /// The record of `text`, a line that is not a folder, a tab and 64 lower-case hex digits dropped.
    fn parse(text: &str) -> Record {
        let lines = text.lines().filter_map(|line| {
            let (folder, digest) = line.split_once('\t')?;
            let valid = !folder.is_empty() && digest.len() == 64 && digest.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
            valid.then(|| (folder.to_owned(), digest.to_owned()))
        });
        Record(lines.collect())
    }

    /// The line of `folder`, its name compared in any case.
    fn find(&self, folder: &str) -> Option<&String> {
        let upper = ordinal_upper(folder);
        self.0.iter().find(|(held, _)| ordinal_upper(held) == upper).map(|(_, digest)| digest)
    }

    /// Whether the line of `folder` holds `digest`.
    fn holds(&self, folder: &str, digest: &str) -> bool {
        self.find(folder).is_some_and(|held| held == digest)
    }

    /// Sets the line of `folder` to `digest`, replacing the one it had in any case, the others kept.
    fn set(&mut self, folder: &str, digest: &str) {
        self.remove(folder);
        self.0.insert(folder.to_owned(), digest.to_owned());
    }

    /// Drops the line of `folder`, its name compared in any case, the others kept; whether there was one.
    fn remove(&mut self, folder: &str) -> bool {
        let upper = ordinal_upper(folder);
        let before = self.0.len();
        self.0.retain(|held, _| ordinal_upper(held) != upper);
        self.0.len() != before
    }

    /// The record's text, a line for each folder, each ending in LF.
    fn text(&self) -> String {
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
#[derive(PartialEq, Eq)]
enum Found {
    /// No install: no lockfile is found up to the work root.
    Nothing,
    /// No install: the folder the command starts in lies outside the work root.
    Outside(PathBuf),
    /// No install: this folder from the work root down to the lockfile's is a link or a junction.
    Linked(String),
    /// No install: this npm lockfile folder also holds a pnpm or a bun lockfile, whose manager owns its `node_modules`.
    OtherManager(PathBuf),
    /// The folder of the lockfile to install from.
    Lockfile(PathBuf),
}

/// The package manager of the command of `words` and where it leads its install ([`Found`]), nothing said; none when
/// the command is no manager's. `Err` with the line saying a lockfile's metadata cannot be read.
fn lockfile_for(words: &[String], folders: &Folders<'_>) -> Result<Option<(&'static Manager, Found)>, String> {
    let Some((word, rest)) = words.split_first() else {
        return Ok(None);
    };
    let Some(manager) = MANAGERS.into_iter().find(|manager| is_command(manager, word)) else {
        return Ok(None);
    };
    let start = match start_folder(folders.work, folders.place, start_named(manager, word, rest)) {
        Ok(start) => start,
        Err(outside) => return Ok(Some((manager, Found::Outside(outside)))),
    };
    let found = match lockfile_folder(manager, folders.work, &start)? {
        None => Found::Nothing,
        Some(folder) => match linked(folders.work, &folder) {
            Some(link) => Found::Linked(link),
            None if manager.name == NPM.name && holds_other_lockfile(&folder)? => Found::OtherManager(folder),
            None => Found::Lockfile(folder),
        },
    };
    Ok(Some((manager, found)))
}

/// The hash of `folder`'s package files: for each of `manager`'s hashed files that exists, its name, a NUL byte, its
/// bytes and a NUL byte, hashed with SHA-256 and written as lower-case hex; else the line saying which file could not
/// be read.
fn hash(manager: &Manager, folder: &Path) -> Result<String, String> {
    let mut hasher = Sha256::new();
    for name in manager.hashed {
        let path = folder.join(name);
        let content = match read_plain(&path) {
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

/// The bytes of the file at `path`, `Ok(None)` when there is none: opened without following a link there and checked a
/// plain file, as the tool manifests' record is, so a link, a folder or a file with more than one name is not read.
fn read_plain(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = match OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    gate_win32::check_plain_file(&file)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;
    Ok(Some(content))
}

/// `folder`'s path relative to the work root `work`, at or below which it lies, `.` for the root itself.
fn relative(work: &Path, folder: &Path) -> String {
    let skipped = work.to_string_lossy().chars().count();
    let rest: String = folder.to_string_lossy().chars().skip(skipped).collect();
    match rest.trim_start_matches('\\') {
        "" => ".".to_owned(),
        rest => rest.to_owned(),
    }
}

/// The key of `manager`'s line for the folder `relative` (as [`relative`] names it) in the record: the folder after
/// the manager's prefix, so pnpm's keys are bare folders and bun's start `bun:` and npm's `npm:`.
fn record_key(manager: &Manager, relative: &str) -> String {
    format!("{}{relative}", manager.key_prefix)
}

/// The record at `path`, empty when there is none; one that cannot be read is said, and read as empty.
fn read(path: &str, console: &mut Console) -> Record {
    match read_record(path) {
        Ok(Some(text)) => Record::parse(&text),
        Ok(None) => Record::default(),
        Err(error) => {
            console.err(&format!(
                "gate: remote could not read {}: {}; installing the packages",
                escaped(path),
                escaped(&error.to_string())
            ));
            Record::default()
        }
    }
}

/// Whether the install in `folder`, named `key` in `record`, can be skipped: the record holds `digest` for it and its
/// `node_modules` is a folder itself, not a link or a junction.
fn installed(record: &Record, key: &str, digest: &str, folder: &Path) -> bool {
    record.holds(key, digest) && is_real_folder(&folder.join("node_modules"))
}

/// Whether git ignores `node_modules` in the folder `key` below the work root `work`, asked with the box's own git as
/// it runs in the work tree ([`WorkGit::args`]), its repository and work tree named, never found from the folder;
/// when it does not, or cannot say, the line saying why `manager`'s install is skipped is said.
fn ignored(manager: &Manager, git: &WorkGit, work: &Path, key: &str, console: &mut Console) -> bool {
    let work_text = work.to_string_lossy();
    let git_dir = format!(r"--git-dir={work_text}\.git");
    let work_tree = format!("--work-tree={work_text}");
    // `:(top)` reads the path from the work tree's top, wherever the box's own working folder is.
    let path = if key == "." {
        format!(":(top){NODE_MODULES}")
    } else {
        format!(":(top){}/{NODE_MODULES}", key.replace('\\', "/"))
    };
    let args = git.args(&[&git_dir, &work_tree, "check-ignore", "-q", &path]);
    let why = match git.git.code(&args, Stdio::null(), Stdio::null()) {
        Ok(0) => return true,
        Ok(1) => {
            console.err(&format!(
                "gate: remote {} install skipped: node_modules is not ignored in {}",
                manager.name,
                escaped(key)
            ));
            return false;
        }
        Ok(status) => format!("status {status}"),
        Err(message) => escaped(&message),
    };
    console.err(&format!(
        "gate: remote could not ask git whether node_modules is ignored in {}: {why}",
        escaped(key)
    ));
    false
}

/// How running the install ended.
enum Installed {
    /// The install ran to its end with this status.
    Status(i32),
    /// The run ended while the install ran, or before it started, with the status to answer, its line already said.
    Ended(u32),
}

/// The launch of `manager`'s install in `folder`: its program and install words, the program found on the box's
/// `PATH`, the box's own environment as a run's processes get it ([`box_variables`]) with git's `safe.directory` set to
/// `*` over it as the command's is ([`with_safe_directory`]) and `CI=true` over that, and the restricted token the
/// command gets; else why it cannot be launched.
fn install_launch(manager: &Manager, folder: &Path) -> Result<PipedLaunch, String> {
    let found = locate(manager.program)?.ok_or_else(|| gate_win32::win32_message(ERROR_FILE_NOT_FOUND))?;
    let token = RestrictedToken::of_this_process().map_err(|error| format!("cannot make its restricted token: {error}"))?;
    let variables = with_safe_directory(box_variables()).with("CI", "true");
    let environment = variables.block().map_err(|error| format!("cannot build its environment: {error}"))?;
    let words: Vec<OsString> = manager.install_words.iter().copied().map(OsString::from).collect();
    match written(&found, &words, &variables) {
        Written::Line(application, command_line) => Ok(PipedLaunch {
            application,
            command_line,
            directory: folder.to_owned(),
            environment,
            console: PipedConsole::Hidden,
            token: Some(token),
        }),
        Written::Refused(line) | Written::NoShell(line) => Err(line),
    }
}

/// Runs the install of `lockfile`'s folder, named `key`, in a job of its own named after the run's job
/// ([`packages_job_name`]), its two streams forwarded through the box's output lock, and sees it to its end:
/// [`Installed::Status`] with its status, after a line saying how long it ran, or [`Installed::Ended`] when a stop set
/// the run's signal before or while it ran, when the box could not wait for it, or when the client closed the
/// connection. `Err` carries the message of an install that could not be started. Its job kills on close and is
/// terminated on every way out of here.
fn install(request: &Request<'_>, lockfile: &Lockfile<'_>, key: &str, beat: &Beat<'_>, console: &mut Console) -> Result<Installed, String> {
    let manager = lockfile.manager;
    let starting = format!("starting the {} install", manager.name);
    if let Err(status) = before_the_job(beat.stop.look(&starting), request.job, beat.lock, console) {
        return Ok(Installed::Ended(status));
    }
    let launch = install_launch(manager, &lockfile.folder)?;
    let name = packages_job_name(request.job);
    let job = Job::create_kill_on_close(&name).map_err(|error| format!("cannot create its job {name}: {error}"))?;
    let _stopper = Stopper { job: &job, armed: true };
    let started = Instant::now();
    let (running, forwarders) = match start(&launch, &job, &starting, beat, console)? {
        Ok(started) => started,
        Err(status) => return Ok(Installed::Ended(status)),
    };
    let watched = Watched {
        job: &job,
        running: &running,
        forwarders,
        watching: Watching::Packages,
    };
    // No ceiling watches the install, so no kill ends its wait.
    Ok(match watched.wait(Some(&beat.stop), |_| None::<Infallible>, console) {
        WaitEnd::Failed(status) => Installed::Ended(status),
        WaitEnd::ClientGone => Installed::Ended(watched.client_gone(console)),
        WaitEnd::Exited { stopped: true, .. } => Installed::Ended(stopped(watched, beat.lock, console)),
        WaitEnd::Exited { code, stopped: false } => {
            let took = started.elapsed();
            let drained = watched.drained(console);
            end_job(&job, console);
            match drained {
                Ok(()) => {
                    let place = if key == "." { String::new() } else { format!(" in {}", escaped(key)) };
                    console.err(&format!("gate: {} install took {:.1} s{place}", manager.name, took.as_secs_f64()));
                    Installed::Status(code.cast_signed())
                }
                Err(status) => Installed::Ended(status),
            }
        }
    })
}

/// Starts `launch` suspended in `job`, then, unless a stop came meanwhile, resumes it and starts forwarding its two
/// streams through the box's output lock: the install running and its readers, or `Ok(Err)` with status 1 for a stop
/// that came first, its job terminated and its line said; `Err` with why it could not be started. `starting` is what
/// the box does instead when it cannot read the stop signal.
fn start(
    launch: &PipedLaunch,
    job: &Job,
    starting: &str,
    beat: &Beat<'_>,
    console: &mut Console,
) -> Result<Result<(Running, Forwarders), u32>, String> {
    let PipedSuspended { suspended, stdout, stderr } = launch.start_suspended(job).map_err(|error| error.to_string())?;
    let assigned = suspended.assign(job);
    // A stop that came while the install was created ends it before it runs, as the command's launch looks: whether
    // the install went in the job or the stopped job refused it, it never runs, and is ended as it is dropped.
    match beat.stop.look(starting) {
        Look::Stopped => return Ok(Err(said_stopped(job, terminate_and_wait(job), beat.lock, console))),
        Look::Go(Some(unread)) => beat.say(console, &unread),
        Look::Go(None) => {}
    }
    let running = assigned.and_then(|()| suspended.resume()).map_err(|error| error.to_string())?;
    match Forwarders::start(stdout, stderr, beat.lock) {
        Ok(forwarders) => Ok(Ok((running, forwarders))),
        Err(error) => {
            end_job(job, console);
            Err(format!("cannot read its output: {error}"))
        }
    }
}

/// Ends an install a stop came during: its job terminated and waited for, then its readers drained, so the stop line,
/// said last, follows every line of the install; status 1.
fn stopped(watched: Watched<'_>, lock: &OutputLock, console: &mut Console) -> u32 {
    let ending = terminate_and_wait(watched.job);
    watched.forwarders.drain(DRAIN);
    said_stopped(watched.job, ending, lock, console)
}

/// Writes `record` at `path`; a failure is said and fails the run with status 1.
fn write(path: &str, record: &Record, console: &mut Console) -> Result<(), u32> {
    if let Err(error) = write_record_file(path, &record.text()) {
        console.err(&format!(
            "gate: remote could not write {}: {}",
            escaped(path),
            escaped(&error.to_string())
        ));
        return Err(FAILED_STATUS);
    }
    Ok(())
}

/// Refuses the run for `manager`'s install that ended with `status`: the status line, then the `tools` tag and the
/// packages line under the heartbeat's lock; status 75.
fn refused(request: &Request<'_>, manager: &Manager, status: i32, beat: &Beat<'_>, console: &mut Console) -> u32 {
    console.err(&format!("gate: remote package restore failed (status {status})"));
    // Held across both lines, so no heartbeat lands between the tag and the line it tags.
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    console.err(&refused_line(request.nonce, Refusal::Tools));
    console.err(&packages_line(manager.name, request.key));
    REFUSED_STATUS
}

/// Installs the packages of each package manager's command of the run before its first command runs
/// ([`update_folder`]), once for each manager and lockfile folder the commands need, in their order: nothing for
/// another command or one without a lockfile. Where each command leads is found first and the same place of the same
/// manager kept once, so a skip's line is said once however many commands lead to it: the install skipped when the
/// folder the command starts in lies outside the work root, or when a folder from the work root down to the lockfile's
/// is a link or a junction. A run whose commands lead two managers to one lockfile folder installs nothing and is
/// refused ([`refused_shared`]), since both would install into its one `node_modules`. `Ok` when nothing ran or every
/// install passed; else `Err` with the status of the first that did not, as [`update_folder`] gives it, its lines
/// said, and no later install run; 1 when a lockfile's metadata cannot be read, its line said.
pub(super) fn update(request: &Request<'_>, git: &WorkGit, folders: &Folders<'_>, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
    let mut places: Vec<(&'static Manager, Found)> = Vec::new();
    for words in request.commands() {
        let place = lockfile_for(words, folders).map_err(|line| {
            console.err(&line);
            FAILED_STATUS
        })?;
        let Some((manager, found)) = place else {
            continue;
        };
        if !places
            .iter()
            .any(|(known_manager, known)| known_manager.name == manager.name && same_place(known, &found))
        {
            places.push((manager, found));
        }
    }
    if let Some(shared) = shared_folder(&places, folders.work) {
        return Err(refused_shared(request, &shared, beat, console));
    }
    for (manager, found) in places {
        match found {
            Found::Nothing => {}
            Found::Outside(outside) => console.err(&format!(
                "gate: remote {} install skipped: {} is outside the work root",
                manager.name,
                escaped(&outside.to_string_lossy())
            )),
            Found::Linked(link) => console.err(&format!(
                "gate: remote {} install skipped: {} is reached through a link",
                manager.name,
                escaped(&link)
            )),
            Found::OtherManager(folder) => console.err(&format!(
                "gate: remote {} install skipped: {} holds a pnpm or bun lockfile",
                manager.name,
                escaped(&relative(folders.work, &folder))
            )),
            Found::Lockfile(folder) => {
                let lockfile = Lockfile {
                    manager,
                    work: folders.work,
                    folder,
                };
                update_folder(request, git, &lockfile, beat, console)?;
            }
        }
    }
    Ok(())
}

/// A lockfile folder that two managers lead to, and the two earliest of them in [`MANAGERS`] order.
struct Shared {
    folder: String,
    first: &'static Manager,
    second: &'static Manager,
}

/// The first lockfile folder of `places` that two managers lead to, relative to the work root `work`, with the two
/// earliest of the managers that lead there in [`MANAGERS`] order; none when each lockfile folder is one manager's.
fn shared_folder(places: &[(&'static Manager, Found)], work: &Path) -> Option<Shared> {
    places.iter().find_map(|(_, found)| {
        let Found::Lockfile(folder) = found else {
            return None;
        };
        let mut managers = MANAGERS
            .iter()
            .copied()
            .filter(|manager| places.iter().any(|(known, lead)| known.name == manager.name && same_place(lead, found)));
        let first = managers.next()?;
        let second = managers.next()?;
        Some(Shared {
            folder: relative(work, folder),
            first,
            second,
        })
    })
}

/// Refuses a run whose commands need the two managers of `shared` in its lockfile folder: the `tools` tag and the
/// line naming the folder and the two managers under the heartbeat's lock; status 75.
fn refused_shared(request: &Request<'_>, shared: &Shared, beat: &Beat<'_>, console: &mut Console) -> u32 {
    // Held across both lines, so no heartbeat lands between the tag and the line it tags.
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    console.err(&refused_line(request.nonce, Refusal::Tools));
    console.err(&format!(
        "gate: remote work folder {} uses both {} and {} in {}",
        request.key,
        shared.first.name,
        shared.second.name,
        escaped(&shared.folder)
    ));
    REFUSED_STATUS
}

/// Whether `one` and `other` are the same place: two lockfile folders compared as Windows names them, without regard
/// to letter case ([`ordinal_upper`]), anything else exactly.
fn same_place(one: &Found, other: &Found) -> bool {
    match (one, other) {
        (Found::Lockfile(one), Found::Lockfile(other)) => ordinal_upper(&one.to_string_lossy()) == ordinal_upper(&other.to_string_lossy()),
        _ => one == other,
    }
}

/// A folder holding a lockfile of a package manager a command needs, and the work folder it lies under.
struct Lockfile<'a> {
    manager: &'static Manager,
    work: &'a Path,
    folder: PathBuf,
}

/// Installs the packages of `lockfile`'s folder with its manager: nothing when the record holds the folder's hash in
/// the manager's line ([`record_key`]) and its `node_modules` is there, or when git does not ignore `node_modules`
/// there (said); else the manager's install in the folder, the manager's line for the folder in `<Work>.packages`
/// rewritten when it passes. The other managers' lines for the folder are dropped, and the record written, before the
/// install starts, since whatever its end it rewrites the `node_modules` they were for; a record
/// without such a line is not written then. `Ok` when nothing ran or the install passed; else `Err` with the status the run
/// ends on, its lines said: 75 when the install failed or could not start; 1 when a package file could not be read,
/// the record could not be written, a stop came before or during the install, or the box could not wait for it; 255
/// when the client closed the connection during it.
fn update_folder(request: &Request<'_>, git: &WorkGit, lockfile: &Lockfile<'_>, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
    let (manager, folder) = (lockfile.manager, &lockfile.folder);
    let digest = hash(manager, folder).map_err(|line| {
        console.err(&line);
        FAILED_STATUS
    })?;
    let place = relative(lockfile.work, folder);
    let key = record_key(manager, &place);
    let path = format!("{}.packages", request.work);
    let mut record = read(&path, console);
    if installed(&record, &key, &digest, folder) || !ignored(manager, git, lockfile.work, &place, console) {
        return Ok(());
    }
    let mut dropped = false;
    for other in MANAGERS.into_iter().filter(|other| other.name != manager.name) {
        dropped |= record.remove(&record_key(other, &place));
    }
    if dropped {
        write(&path, &record, console)?;
    }
    let status = match install(request, lockfile, &place, beat, console) {
        Ok(Installed::Status(status)) => status,
        Ok(Installed::Ended(status)) => return Err(status),
        Err(message) => {
            console.err(&format!(
                "gate: remote could not start the {} install: {}",
                manager.name,
                escaped(&message)
            ));
            FAILED_STATUS.cast_signed()
        }
    };
    if status != 0 {
        return Err(refused(request, manager, status, beat, console));
    }
    record.set(&key, &digest);
    write(&path, &record, console)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders or files must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::boxside::run::Folders;

    use super::{
        BUN, Found, NPM, PNPM, Record, hash, holds_other_lockfile, is_command, lockfile_folder, lockfile_for, record_key, same_place, start_folder,
        start_named, start_word,
    };

    #[test]
    fn lockfile_folders_are_one_place_whatever_their_letter_case() {
        let folder = |path: &str| Found::Lockfile(PathBuf::from(path));
        assert!(same_place(&folder(r"C:\w\App"), &folder(r"c:\W\app")), "one folder in two cases");
        assert!(!same_place(&folder(r"C:\w\App"), &folder(r"C:\w\Lib")), "two folders");
        assert!(!same_place(&folder(r"C:\w\App"), &Found::Nothing), "a folder and no install");
        assert!(same_place(&Found::Nothing, &Found::Nothing));
    }

    /// A folder for one test, removed when it ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(test: &str) -> Scratch {
            let folder = crate::test_support::temp_dir().join(format!("gate-packages-{}-{test}", std::process::id()));
            let _ = fs::remove_dir_all(&folder);
            fs::create_dir_all(&folder).expect("the scratch folder can be made");
            Scratch(folder)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// `words` as the owned words a request holds.
    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|&word| word.to_owned()).collect()
    }

    /// Writes an empty lockfile in `folder`, making it.
    fn lockfile(folder: &Path) {
        fs::create_dir_all(folder).expect("the lockfile's folder can be made");
        fs::write(folder.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").expect("the lockfile can be written");
    }

    /// The work root and place folders of the start-folder cases, under a drive that need not exist.
    fn root_and_place() -> (PathBuf, PathBuf) {
        (PathBuf::from(r"C:\work\app"), PathBuf::from(r"C:\work\app\web"))
    }

    #[test]
    fn a_pnpm_word_triggers_in_any_case_and_form() {
        for word in ["pnpm", "PNPM.CMD", r"C:\tools\pnpm.exe", "pnpm.ps1", "/usr/bin/pnpm", "C:pnpm.cmd"] {
            assert!(is_command(&PNPM, word), "{word} names pnpm");
        }
        for word in ["pnpmx", "npx", "node", "corepack", "", r"C:\pnpm\node.exe", "xpnpm.cmd"] {
            assert!(!is_command(&PNPM, word), "{word} does not name pnpm");
        }
    }

    #[test]
    fn a_bun_or_bunx_word_triggers_in_any_case_and_form() {
        for word in ["bun", "bunx", r"C:\x\bun.exe", "BUNX.EXE", "/usr/bin/bun", "C:bunx.cmd"] {
            assert!(is_command(&BUN, word), "{word} names bun");
            assert!(!is_command(&PNPM, word), "{word} does not name pnpm");
        }
        for word in ["bunyan", "bunk", "xbun.exe", "pnpm", "", r"C:\bun\node.exe"] {
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
            start_word(&words(&["exec", "git", "-C", r"..\x", "status"])),
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
        assert_eq!(
            start_folder(&work, &place, start_word(&words(&["run", "x"]))),
            Ok(place.clone()),
            "no folder named: the place"
        );
        assert_eq!(
            start_folder(&work, &place, start_word(&words(&["--dir", r"..\api"]))),
            Ok(PathBuf::from(r"C:\work\app\api")),
            "relative to the place"
        );
        assert_eq!(
            start_folder(&work, &place, start_word(&words(&["-C", ".."]))),
            Ok(work.clone()),
            "the work root itself"
        );
        assert_eq!(
            start_folder(&work, &place, start_word(&words(&["--dir", r"C:\WORK\APP\lib"]))),
            Ok(PathBuf::from(r"C:\WORK\APP\lib")),
            "an absolute folder inside, in another case"
        );
        assert_eq!(
            start_folder(&work, &place, start_word(&words(&["--dir", r"..\..\x"]))),
            Err(PathBuf::from(r"C:\work\x")),
            "climbing out of the work root"
        );
        assert_eq!(
            start_folder(&work, &place, start_word(&words(&["--dir", r"D:\elsewhere"]))),
            Err(PathBuf::from(r"D:\elsewhere")),
            "an absolute folder elsewhere"
        );
        assert_eq!(
            start_folder(&work, &place, start_word(&words(&[r"--dir=C:\work\application"]))),
            Err(PathBuf::from(r"C:\work\application")),
            "a sibling whose name starts with the root's is outside"
        );
    }

    #[test]
    fn the_walk_finds_the_nearest_lockfile_up_to_the_work_root() {
        let scratch = Scratch::new("the_walk_finds_the_nearest_lockfile_up_to_the_work_root");
        let work = scratch.0.join("work");
        let place = work.join("web").join("app");
        fs::create_dir_all(&place).expect("the place can be made");
        lockfile(&scratch.0);
        assert_eq!(
            lockfile_folder(&PNPM, &work, &place),
            Ok(None),
            "a lockfile above the work root is never read"
        );

        lockfile(&work);
        assert_eq!(
            lockfile_folder(&PNPM, &work, &place),
            Ok(Some(work.clone())),
            "the work root's, reached from below"
        );

        lockfile(&work.join("web"));
        assert_eq!(lockfile_folder(&PNPM, &work, &place), Ok(Some(work.join("web"))), "the nearest parent's");

        fs::create_dir_all(place.join("pnpm-lock.yaml")).expect("a folder named as the lockfile can be made");
        assert_eq!(
            lockfile_folder(&PNPM, &work, &place),
            Ok(Some(work.join("web"))),
            "a folder named as the lockfile is skipped"
        );

        let leaf = work.join("leaf");
        lockfile(&leaf);
        assert_eq!(lockfile_folder(&PNPM, &work, &leaf), Ok(Some(leaf.clone())), "the start folder's own");
    }

    #[test]
    fn the_bun_walk_finds_either_lockfile_and_no_other() {
        let scratch = Scratch::new("the_bun_walk_finds_either_lockfile_and_no_other");
        let work = scratch.0.join("work");
        let place = work.join("web").join("app");
        fs::create_dir_all(&place).expect("the place can be made");
        lockfile(&place);
        assert_eq!(lockfile_folder(&BUN, &work, &place), Ok(None), "a pnpm lockfile is no bun lockfile");

        fs::write(work.join("bun.lock"), "{}\n").expect("the lockfile can be written");
        assert_eq!(lockfile_folder(&BUN, &work, &place), Ok(Some(work.clone())), "a bun.lock");

        fs::write(work.join("web").join("bun.lockb"), [0_u8, 1]).expect("the lockfile can be written");
        assert_eq!(
            lockfile_folder(&BUN, &work, &place),
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
        let scratch = Scratch::new("the_bun_hash_reads_its_files_in_order");
        let folder = scratch.0.join("web");
        fs::create_dir_all(&folder).expect("the folder can be made");
        fs::write(folder.join(".npmrc"), "a=1\n").expect("the file can be written");
        fs::write(folder.join("bun.lock"), "lock\n").expect("the file can be written");
        fs::write(folder.join("pnpm-lock.yaml"), "not bun's\n").expect("the file can be written");
        assert_eq!(hash(&BUN, &folder), Ok(BUN_TWO_FILES_HASH.to_owned()), "an absent file is skipped");

        fs::write(folder.join("bunfig.toml"), "[install]\n").expect("the file can be written");
        fs::write(folder.join("package.json"), "{}\n").expect("the file can be written");
        fs::write(folder.join("bun.lockb"), [0_u8, 1, b'l', b'o', b'c', b'k', b'b']).expect("the file can be written");
        assert_eq!(
            hash(&BUN, &folder),
            Ok(BUN_FIVE_FILES_HASH.to_owned()),
            "bun.lock, bun.lockb, package.json, bunfig.toml, .npmrc"
        );
    }

    #[test]
    fn an_npm_or_npx_word_triggers_in_any_case_and_form() {
        for word in [
            "npm",
            "npx",
            "NPM.cmd",
            "NPX.EXE",
            r"C:\tools\npm.cmd",
            "npx.ps1",
            "/usr/bin/npm",
            "C:npx",
        ] {
            assert!(is_command(&NPM, word), "{word} names npm");
            assert!(!is_command(&PNPM, word), "{word} does not name pnpm");
            assert!(!is_command(&BUN, word), "{word} does not name bun");
        }
        for word in ["npmx", "npxx", "node", "pnpm", "bun", "", r"C:\npm\node.exe", "xnpm.cmd"] {
            assert!(!is_command(&NPM, word), "{word} does not name npm");
        }
    }

    #[test]
    fn the_npm_walk_finds_either_lockfile_and_no_other() {
        let scratch = Scratch::new("the_npm_walk_finds_either_lockfile_and_no_other");
        let work = scratch.0.join("work");
        let place = work.join("web").join("app");
        fs::create_dir_all(&place).expect("the place can be made");
        lockfile(&place);
        assert_eq!(lockfile_folder(&NPM, &work, &place), Ok(None), "a pnpm lockfile is no npm lockfile");

        fs::write(work.join("npm-shrinkwrap.json"), "{}\n").expect("the lockfile can be written");
        assert_eq!(lockfile_folder(&NPM, &work, &place), Ok(Some(work.clone())), "an npm-shrinkwrap.json");

        fs::write(work.join("web").join("package-lock.json"), "{}\n").expect("the lockfile can be written");
        assert_eq!(
            lockfile_folder(&NPM, &work, &place),
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
        let mut hex = String::new();
        for byte in hasher.finalize() {
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
        }
        hex
    }

    #[test]
    fn the_npm_hash_reads_its_files_in_order() {
        let scratch = Scratch::new("the_npm_hash_reads_its_files_in_order");
        let folder = scratch.0.join("web");
        fs::create_dir_all(&folder).expect("the folder can be made");
        // Written in another order than the hash's, which the hash ignores.
        fs::write(folder.join(".npmrc"), "a=1\n").expect("the file can be written");
        fs::write(folder.join("package-lock.json"), "lock\n").expect("the file can be written");
        fs::write(folder.join("bun.lock"), "not npm's\n").expect("the file can be written");
        assert_eq!(
            hash(&NPM, &folder),
            Ok(digest(&[("package-lock.json", b"lock\n"), (".npmrc", b"a=1\n")])),
            "an absent file is skipped"
        );

        fs::write(folder.join("package.json"), "{}\n").expect("the file can be written");
        fs::write(folder.join("npm-shrinkwrap.json"), "{\"x\":1}\n").expect("the file can be written");
        assert_eq!(
            hash(&NPM, &folder),
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
        let scratch = Scratch::new("an_npm_command_restores_nothing_in_a_folder_with_a_bun_lockfile");
        let work = scratch.0.join("work");
        fs::create_dir_all(&work).expect("the work root can be made");
        fs::write(work.join("package-lock.json"), "{}\n").expect("the lockfile can be written");
        fs::write(work.join("bun.lock"), "{}\n").expect("the lockfile can be written");
        let folders = Folders { work: &work, place: &work };

        assert_eq!(
            holds_other_lockfile(&work),
            Ok(true),
            "a bun.lock beside the npm lockfile is another manager's"
        );
        let found = lockfile_for(&words(&["npm", "test"]), &folders).expect("no line is said");
        let skipped = matches!(found, Some((manager, Found::OtherManager(ref folder))) if manager.name == "npm" && *folder == work);
        assert!(
            skipped,
            "the npm command restores nothing, saying the folder holds a pnpm or bun lockfile"
        );
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
        let scratch = Scratch::new("hash_layout_is_name_nul_bytes_nul");
        let folder = scratch.0.join("web");
        fs::create_dir_all(&folder).expect("the folder can be made");
        // Written in another order than the hash's, which the hash ignores.
        fs::write(folder.join(".npmrc"), "a=1\n").expect("the file can be written");
        fs::write(folder.join("pnpm-lock.yaml"), "lock\n").expect("the file can be written");
        assert_eq!(hash(&PNPM, &folder), Ok(TWO_FILES_HASH.to_owned()), "an absent file is skipped");

        fs::write(folder.join("pnpm-workspace.yaml"), "packages: []\n").expect("the file can be written");
        fs::write(folder.join("package.json"), "{}\n").expect("the file can be written");
        assert_eq!(hash(&PNPM, &folder), Ok(FOUR_FILES_HASH.to_owned()), "the name, a NUL, the bytes, a NUL");
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
        assert!(record.holds("WEB", &a), "folders compare in any case");
        assert!(record.holds(".", &b));
        assert!(!record.holds("web", &b), "another hash");
        assert!(!record.holds("short", "abc"), "a malformed hash is dropped");
        assert!(!record.holds("UPPER", &"A".repeat(64)), "an upper-case hash is dropped");
        assert!(!record.holds("api", &b), "a line with a third field is dropped");
        assert_eq!(record.text(), format!(".\t{b}\nweb\t{a}\n"), "the valid lines, sorted");

        record.set("Web", &c);
        record.set(r"tests\browser", &a);
        assert_eq!(
            record.text(),
            format!(".\t{b}\nWeb\t{c}\ntests\\browser\t{a}\n"),
            "a folder's line replaced, a new one added, the others kept, sorted"
        );
        assert_eq!(Record::parse(""), Record::default());
        assert_eq!(Record::parse(&record.text()), record, "the text reads back");
    }
}
