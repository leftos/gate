//! The builder's tool probes: for each tool a routed run needs, what the builder has, read by running the tool's own
//! version command from this module's table, never a program the caller names. The tool the box may have several
//! installs of is godot: the folder the batch's environment names as its root holds one install per subfolder, and each
//! is asked for its version, so the line carries every version the box has. Every probe of a batch that runs a program
//! runs at once, under one deadline the batch shares, in the system folder and on a hidden console of its own,
//! as [`git::program_output`] runs a program; a probe still running at the deadline is ended with its job. Nothing here
//! writes to the console or reads a payload: the caller hands over the names and says the lines.

use std::any::Any;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread::{self, Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use gate_win32::PipedConsole;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

use crate::boxside::payload::{escaped, is_drive_path};
use crate::environment::Variables;
use crate::remote::git::{self, GitError, ProgramCall, ProgramOutput, ProgramPlace};

/// How long a batch of probes may take together, unless a test seam says otherwise.
pub(crate) const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// The most digits a part of a version may have: a longer one is no version the client could compare.
pub(crate) const MAX_PART_DIGITS: usize = 10;

/// The console program of a Godot install.
pub(crate) const GODOT: &str = "Godot_console.exe";

/// The word a Godot install is asked for its version with.
const GODOT_VERSION: &[&str] = &["--version"];

/// The most entries of the folder `GATE_GODOT_ROOT` names that are looked at: a folder holding more is no Godot
/// install root.
const GODOT_ENTRIES: usize = 64;

/// The most characters of a Godot install's answer the no-version reason carries before it is escaped.
const GODOT_ANSWER_MAX: usize = 200;

/// What a probe found of one tool.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Probed {
    /// It ran, and its version reads as this text: a tool the box has several installs of names every version found,
    /// `,`-joined.
    Version(String),
    /// It ran, and no version reads in what it wrote.
    Present,
    /// It is not found.
    Missing,
    /// It was not probed to an answer, for this reason.
    Skipped(String),
}

/// How the table probes a tool it knows.
enum ToolProbe {
    /// Runs `program` with `args`.
    Runs { program: &'static str, args: &'static [&'static str] },
    /// Runs the `--version` of every Godot install the batch's environment names.
    Godot,
}

/// The table's probe of the tool `name`; `None` for a name it does not know. cargo-nextest is run as the plugin itself,
/// never through cargo, which would read a `.cargo\config.toml` in every folder above its working folder and run the
/// alias one defines.
fn probe_of(name: &str) -> Option<ToolProbe> {
    let runs = |program, args| Some(ToolProbe::Runs { program, args });
    match name {
        "rust" => runs("rustc", &["--version"]),
        "cargo-nextest" => runs("cargo-nextest", &["nextest", "--version"]),
        "node" => runs("node", &["--version"]),
        "pnpm" => runs("pnpm", &["--version"]),
        "pwsh" => runs("pwsh", &["--version"]),
        "ffmpeg" => runs("ffmpeg", &["-version"]),
        "godot" => Some(ToolProbe::Godot),
        _ => None,
    }
}

/// A probe of the batch: found without running anything, or running on a thread of its own.
enum Pending<'scope> {
    Found(Probed),
    Running(ScopedJoinHandle<'scope, Probed>),
}

/// What one batch of probes shares beyond the place a program is looked for in: the folders no program may come from,
/// the folder the batch's environment names as `GATE_GODOT_ROOT`, and the batch's deadline and its limit.
#[derive(Clone, Copy)]
struct Batch<'a> {
    worktrees: &'a [&'a Path],
    godot_root: Option<&'a OsStr>,
    deadline: Instant,
    limit: Duration,
}

/// Probes each tool of `names`, all within `limit` from now, and gives back each name with what its probe found, in
/// the order of `names`. The probes that run a program run at once, each on a thread of its own, sharing the system
/// folder and the environment `variables` for the batch, whose programs are looked for along its `PATH`, and none of
/// them from a folder of `worktrees`: a program found that still lies inside any of them is refused
/// ([`git::program_output`]). Environment `variables` that could not be made, for the reason given, skip every probe
/// that runs a program. The godot probe reads the folder `variables` names as `GATE_GODOT_ROOT` to find the installs
/// it runs.
pub(crate) fn probe(names: &[String], limit: Duration, variables: Result<Variables, String>, worktrees: &[&Path]) -> Vec<(String, Probed)> {
    let deadline = Instant::now() + limit;
    let godot_root = variables
        .as_ref()
        .ok()
        .and_then(|variables| variables.get("GATE_GODOT_ROOT"))
        .map(OsStr::to_os_string);
    let place = variables.and_then(ProgramPlace::new);
    let batch = Batch {
        worktrees,
        godot_root: godot_root.as_deref(),
        deadline,
        limit,
    };
    thread::scope(|scope| {
        let pending: Vec<Pending<'_>> = names.iter().map(|name| started(scope, name, &place, batch)).collect();
        names
            .iter()
            .zip(pending)
            .map(|(name, pending)| {
                let found = match pending {
                    Pending::Found(found) => found,
                    Pending::Running(probe) => probe.join().unwrap_or_else(|panic| Probed::Skipped(panic_reason(&*panic))),
                };
                (name.clone(), found)
            })
            .collect()
    })
}

/// The probe of the tool `name`: what is found without running anything, or the thread on `scope` that runs it in
/// `place`, never a program inside a folder of the batch's worktrees, by the batch's deadline.
fn started<'scope, 'env>(
    scope: &'scope Scope<'scope, 'env>,
    name: &str,
    place: &'env Result<ProgramPlace, String>,
    batch: Batch<'env>,
) -> Pending<'scope> {
    match probe_of(name) {
        None => Pending::Found(Probed::Skipped("unknown tool".to_owned())),
        Some(ToolProbe::Runs { program, args }) => match place {
            Err(why) => Pending::Found(Probed::Skipped(format!("cannot run {program}: {why}"))),
            Ok(place) => Pending::Running(scope.spawn(move || ran(program, args, place, batch, read))),
        },
        Some(ToolProbe::Godot) => match place {
            Err(why) => Pending::Found(Probed::Skipped(format!("cannot run {GODOT}: {why}"))),
            Ok(place) => Pending::Running(scope.spawn(move || godot(place, batch))),
        },
    }
}

/// Why a probe did not answer: the batch had no time left for it.
fn no_answer(limit: Duration) -> String {
    format!("no answer in {}", git::spelled_limit(limit))
}

/// What running `program` with `args` in `place` finds by the batch's deadline, `read` turning what it wrote into what
/// the probe found; nothing is run once the deadline has passed, and a program that lies inside one of the batch's
/// worktrees is refused.
fn ran(program: &str, args: &[&str], place: &ProgramPlace, batch: Batch<'_>, read: fn(&ProgramOutput) -> Probed) -> Probed {
    let remaining = batch.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Probed::Skipped(no_answer(batch.limit));
    }
    let call = ProgramCall {
        program,
        args,
        limit: remaining,
        interrupt: None,
        worktrees: batch.worktrees,
        console: PipedConsole::Hidden,
        stdin: None,
    };
    match git::program_output(&call, place) {
        Ok(None) => Probed::Missing,
        Ok(Some(output)) => read(&output),
        Err(GitError::Failed(why)) => Probed::Skipped(late_reason(&why, &call.late_line(), batch.limit).unwrap_or(why)),
        Err(error) => Probed::Skipped(String::from(error)),
    }
}

/// The reason a probe that failed with `why` is skipped for, when `why` is its `late` line, alone or followed by why
/// it could not be stopped: `no answer in <limit>` with the batch's `limit`, and what followed. `None` for another
/// failure.
fn late_reason(why: &str, late: &str, limit: Duration) -> Option<String> {
    why.strip_prefix(late)
        .map(|rest| format!("no answer in {}{rest}", git::spelled_limit(limit)))
}

/// What the godot probe finds in `place`: the version of every install the batch's environment names, `,`-joined,
/// `missing` when the root names no install at all, and `skipped` with the first reason its entries gave when installs
/// are there and none of them answers with a version. An install of the root whose folder is not named the version it
/// answers with is left out, as a run finds an install by its folder's name ([`installed_godot`]).
fn godot(place: &ProgramPlace, batch: Batch<'_>) -> Probed {
    let candidates = match godot_candidates(batch.godot_root, batch.deadline, batch.limit) {
        Ok(candidates) => candidates,
        Err(why) => return Probed::Skipped(why),
    };
    let mut versions = Vec::new();
    let mut reason: Option<String> = None;
    for candidate in &candidates {
        let found = match candidate {
            Candidate::Install(install) => {
                let found = ran(install, GODOT_VERSION, place, batch, read_godot);
                if batch.godot_root.is_some() {
                    named_as_found(install, found)
                } else {
                    found
                }
            }
            Candidate::Skipped(why) => Probed::Skipped(why.clone()),
        };
        match found {
            Probed::Version(version) => versions.push(version),
            Probed::Present | Probed::Missing => {}
            Probed::Skipped(why) => {
                if reason.is_none() {
                    reason = Some(why);
                }
            }
        }
    }
    if versions.is_empty() {
        return reason.map_or(Probed::Missing, Probed::Skipped);
    }
    Probed::Version(version_list(&versions))
}

/// What the probe of the root's install `install` `found`, kept only when it is a version the install's folder is named:
/// else skipped with the reason `named <folder> but reports <version>`.
fn named_as_found(install: &str, found: Probed) -> Probed {
    let Probed::Version(version) = found else {
        return found;
    };
    let folder = Path::new(install)
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if folder == version {
        Probed::Version(version)
    } else {
        Probed::Skipped(format!("named {} but reports {version}", escaped(&folder)))
    }
}

/// The `Godot_console.exe` of the install of `version` under the root `root`, the box's `GATE_GODOT_ROOT`, as a run
/// finds it by name: `<root>\<version>\Godot_console.exe`, when the root is an absolute path on a drive and that file is
/// a plain one in a plain folder, neither of them a reparse point ([`godot_install`]); else `None`.
pub(crate) fn installed_godot(root: &OsStr, version: &str) -> Option<PathBuf> {
    let root = root.to_str().filter(|root| is_drive_path(root))?;
    match godot_install(&Path::new(root).join(version))? {
        Candidate::Install(program) => Some(PathBuf::from(program)),
        Candidate::Skipped(_) => None,
    }
}

/// One direct subfolder of `GATE_GODOT_ROOT` as the godot probe reads it.
#[derive(Debug, PartialEq, Eq)]
enum Candidate {
    /// The `Godot_console.exe` to run, by path.
    Install(String),
    /// A folder, or the program in it, that is a link or a junction: left out for this reason.
    Skipped(String),
}

/// The subfolders the godot probe reads, in name order: those of the folder the batch's environment names as
/// `GATE_GODOT_ROOT`, or `Godot_console.exe` itself, looked for along the `PATH`, when it names none. Else why that
/// folder is refused. The folder must be an absolute path on a drive, and its subfolders are read as they are: a
/// subfolder whose name is no Unicode, one past the first [`GODOT_ENTRIES`] and an entry that cannot be read are left
/// out, each subfolder the rest come to is judged by [`godot_install`], and the listing ends at `deadline`, which with
/// nothing read at all is the batch's own `no answer` reason.
fn godot_candidates(root: Option<&OsStr>, deadline: Instant, limit: Duration) -> Result<Vec<Candidate>, String> {
    let Some(root) = root else {
        return Ok(vec![Candidate::Install(GODOT.to_owned())]);
    };
    if !root.to_str().is_some_and(is_drive_path) {
        return Err(format!(
            "GATE_GODOT_ROOT {} is not an absolute path on a drive",
            escaped(&root.to_string_lossy())
        ));
    }
    let folder = Path::new(root);
    let unreadable = |error: &io::Error| format!("cannot read GATE_GODOT_ROOT {}: {error}", escaped(&folder.to_string_lossy()));
    let entries = fs::read_dir(folder).map_err(|error| unreadable(&error))?;
    let mut folders = Vec::new();
    let mut cut = false;
    for entry in entries.take(GODOT_ENTRIES) {
        if Instant::now() >= deadline {
            cut = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if entry.file_name().to_str().is_none() {
            continue;
        }
        folders.push(entry.path());
    }
    if cut && folders.is_empty() {
        return Err(no_answer(limit));
    }
    folders.sort();
    Ok(folders.iter().filter_map(|folder| godot_install(folder)).collect())
}

/// What `folder`, one direct subfolder of `GATE_GODOT_ROOT`, comes to: the `Godot_console.exe` in it to run, why a
/// link or a junction leaves it out, or nothing when it holds no program to run.
fn godot_install(folder: &Path) -> Option<Candidate> {
    let program = folder.join(GODOT);
    if linked(folder) {
        return Some(Candidate::Skipped(linked_reason(&program, folder)));
    }
    if !plain_folder(folder) {
        return None;
    }
    if linked(&program) {
        return Some(Candidate::Skipped(linked_reason(&program, &program)));
    }
    plain_file(&program).then(|| Candidate::Install(program.to_string_lossy().into_owned()))
}

/// Whether `path` is a plain folder, read as it is and no reparse point ([`linked`]).
fn plain_folder(path: &Path) -> bool {
    plain_type(path).is_some_and(|(kind, reparse)| kind.is_dir() && !reparse)
}

/// Whether `path` is a plain file, read as it is and no reparse point ([`linked`]).
fn plain_file(path: &Path) -> bool {
    plain_type(path).is_some_and(|(kind, reparse)| kind.is_file() && !reparse)
}

/// Whether `path` is a link, a junction or any other reparse point (an app execution alias, a cloud placeholder), read
/// as it is, so what one leads to is never taken for it.
fn linked(path: &Path) -> bool {
    plain_type(path).is_some_and(|(_, reparse)| reparse)
}

/// The kind of file `path` is, read as it is ([`fs::symlink_metadata`]), and whether it is a reparse point
/// ([`is_reparse_point`]); `None` when it cannot be read at all.
fn plain_type(path: &Path) -> Option<(fs::FileType, bool)> {
    fs::symlink_metadata(path).ok().map(|metadata| {
        let kind = metadata.file_type();
        (kind, is_reparse_point(kind.is_symlink(), metadata.file_attributes()))
    })
}

/// Whether a file read as itself is a reparse point: a link or a junction as `is_symlink` says, or any file whose
/// `attributes` carry `FILE_ATTRIBUTE_REPARSE_POINT`, as the box judges a folder itself ([`is_folder_itself`]).
///
/// [`is_folder_itself`]: crate::boxside::preflight::is_folder_itself
fn is_reparse_point(is_symlink: bool, attributes: u32) -> bool {
    is_symlink || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// The reason a link or a junction at `path`, found where the program `exe` would be run from, is left out, in the
/// shape the probe's other reasons take.
fn linked_reason(exe: &Path, path: &Path) -> String {
    format!(
        "cannot run {}: {} is a link or a junction",
        escaped(&exe.to_string_lossy()),
        escaped(&path.to_string_lossy())
    )
}

/// What a Godot install's `--version` output found: the version the first line of its standard output leads with, or
/// why it gave none.
fn read_godot(output: &ProgramOutput) -> Probed {
    let stdout = String::from_utf8_lossy(&output.stdout);
    godot_version(&stdout).map_or_else(|| Probed::Skipped(godot_gave_no_version(&stdout)), Probed::Version)
}

/// The version a Godot install's standard output `stdout` gives: the leading dot-separated runs of ASCII digits of its
/// first line, trimmed, as Godot writes it (`4.7.2.stable.mono.official.abc`), each spelled with no leading zero, the
/// canonical spelling an install's folder and a run's `GodotVersion` have ([`is_canonical_version`]). Three runs are
/// taken, and two are given a `.0`, which Godot leaves out of a zero patch (`4.3.stable.mono.official.77dcf97d8` is
/// `4.3.0`). `None` for any other shape, an output with no line among them.
///
/// [`is_canonical_version`]: crate::boxside::payload::is_canonical_version
fn godot_version(stdout: &str) -> Option<String> {
    let line = stdout.lines().next().unwrap_or("").trim();
    let canonical = |run: &&str| match run.trim_start_matches('0') {
        "" => "0".to_owned(),
        digits => digits.to_owned(),
    };
    let runs: Vec<String> = godot_leading(line)?.iter().map(canonical).collect();
    match runs.as_slice() {
        [major, minor] => Some(format!("{major}.{minor}.0")),
        [major, minor, patch] => Some(format!("{major}.{minor}.{patch}")),
        _ => None,
    }
}

/// The leading dot-separated runs of ASCII digits `line` spells, with a trailing `.` dropped: `None` when it does not
/// start with a digit, one of the runs is empty, or one has more than [`MAX_PART_DIGITS`] digits.
fn godot_leading(line: &str) -> Option<Vec<&str>> {
    let end = line
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(line.len());
    let runs: Vec<&str> = line[..end].trim_end_matches('.').split('.').collect();
    let part = |run: &&str| !run.is_empty() && run.len() <= MAX_PART_DIGITS && run.bytes().all(|byte| byte.is_ascii_digit());
    runs.iter().all(part).then_some(runs)
}

/// Why a Godot install whose standard output was `stdout` gave no version: the first line it wrote, cut to
/// [`GODOT_ANSWER_MAX`] characters with `…` after when it is longer, escaped.
fn godot_gave_no_version(stdout: &str) -> String {
    format!("{GODOT} --version gave no version: {}", escaped(&godot_answered(stdout)))
}

/// The first line of a Godot install's standard output, trimmed and cut to [`GODOT_ANSWER_MAX`] characters, with `…`
/// after when it is longer.
fn godot_answered(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("").trim();
    let mut cut: String = line.chars().take(GODOT_ANSWER_MAX).collect();
    if cut.len() < line.len() {
        cut.push('…');
    }
    cut
}

/// The reason a probe whose thread panicked with `panic` is skipped for, its message when it has one.
pub(crate) fn panic_reason(panic: &(dyn Any + Send)) -> String {
    let message = panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned());
    message.map_or_else(|| "the probe failed".to_owned(), |message| format!("the probe failed: {message}"))
}

/// What a probe that ran to its exit with `output` found: present when it exited non-zero or its first line holds no
/// version; else that version.
fn read(output: &ProgramOutput) -> Probed {
    if !output.status.success() {
        return Probed::Present;
    }
    version_of(&String::from_utf8_lossy(&output.stdout), &String::from_utf8_lossy(&output.stderr)).map_or(Probed::Present, Probed::Version)
}

/// The version a probe wrote: the first version of the first non-blank line of its standard output, or of its standard
/// error when its standard output holds nothing but blanks. Only that line is read: a tool's later lines may name
/// another program's version, as ffmpeg's `built with gcc 14.2.0` does.
fn version_of(stdout: &str, stderr: &str) -> Option<String> {
    let chosen = if stdout.trim().is_empty() { stderr } else { stdout };
    chosen.lines().find(|line| !line.trim().is_empty()).and_then(first_version)
}

/// The first version `text` spells: the first run of two or more dot-separated runs of ASCII digits, after an optional
/// `v`, that starts no word or number already begun (so not after a letter, a digit or a `.`), cut to its first three
/// numbers; what follows it, such as a `-` build label, is no part of it. `None` when there is no such run, or when a
/// number of the first one has more than [`MAX_PART_DIGITS`] digits.
pub(crate) fn first_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let parts = (0..bytes.len())
        .filter(|&at| starts_number(bytes, at))
        .find_map(|at| dotted(&text[at..]))?;
    let readable = parts.iter().all(|part| part.len() <= MAX_PART_DIGITS);
    readable.then(|| parts[..parts.len().min(3)].join("."))
}

/// Whether a version may start at `at` in `bytes`: a digit not after a letter, a digit or a `.`, or after a `v` that
/// is itself not after one of them.
fn starts_number(bytes: &[u8], at: usize) -> bool {
    let free = |before: Option<&u8>| before.is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'.');
    let before = at.checked_sub(1).and_then(|index| bytes.get(index));
    bytes[at].is_ascii_digit()
        && match before {
            Some(b'v' | b'V') => free(at.checked_sub(2).and_then(|index| bytes.get(index))),
            _ => free(before),
        }
}

/// The dot-separated runs of digits `text` starts with, up to the first empty one, when there are two or more; `None`
/// when there are fewer.
fn dotted(text: &str) -> Option<Vec<&str>> {
    let end = text
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(text.len());
    let parts: Vec<&str> = text[..end].split('.').take_while(|part| !part.is_empty()).collect();
    (parts.len() >= 2).then_some(parts)
}

/// The versions `versions` names, deduplicated and ordered by number, joined by `,`.
fn version_list(versions: &[String]) -> String {
    let mut ordered: Vec<&String> = versions.iter().collect();
    ordered.sort_by(|left, right| by_number(left, right));
    ordered.dedup();
    ordered.iter().map(|version| version.as_str()).collect::<Vec<&str>>().join(",")
}

/// How the versions `a` and `b` order by number: part by part, a run of digits compared as the number it spells, and
/// a version with more parts the larger one.
fn by_number(a: &str, b: &str) -> Ordering {
    a.split('.').map(number).cmp(b.split('.').map(number))
}

/// A run of digits as it orders: its digits with the leading zeros dropped, so a longer run is the larger number.
fn number(run: &str) -> (usize, &str) {
    let held = run.trim_start_matches('0');
    (held.len(), held)
}

/// The line the preflight says for the tool `name`, as its probe found it: `gate: remote tool <name> <version>`,
/// `present`, `missing` or `skipped: <why>`, the name and the reason escaped, as either may come from outside.
pub(crate) fn tool_line(name: &str, probed: &Probed) -> String {
    let name = escaped(name);
    match probed {
        Probed::Version(version) => format!("gate: remote tool {name} {version}"),
        Probed::Present => format!("gate: remote tool {name} present"),
        Probed::Missing => format!("gate: remote tool {name} missing"),
        Probed::Skipped(why) => format!("gate: remote tool {name} skipped: {}", escaped(why)),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::GODOT;
    use super::{
        Candidate, Probed, first_version, godot_candidates, godot_gave_no_version, godot_version, is_reparse_point, late_reason, named_as_found,
        panic_reason, probe, read_godot, tool_line, version_list, version_of,
    };
    use crate::environment::Variables;
    use crate::remote::git::ProgramOutput;

    /// `FILE_ATTRIBUTE_DIRECTORY`, `FILE_ATTRIBUTE_ARCHIVE` and `FILE_ATTRIBUTE_REPARSE_POINT`.
    const DIRECTORY: u32 = 0x10;
    const ARCHIVE: u32 = 0x20;
    const REPARSE_POINT: u32 = 0x400;

    /// An install's version is spelled with no leading zero in a part, so the one folder name it may have is the
    /// canonical `x.y.z` a run's `GodotVersion` names: a folder spelled otherwise is left out with its reason.
    #[test]
    fn an_install_s_folder_is_named_its_canonical_version() {
        assert_eq!(godot_version("04.07.2.stable.mono.official.x\n"), Some("4.7.2".to_owned()));
        assert_eq!(godot_version("4.0.10.stable\n"), Some("4.0.10".to_owned()));
        assert_eq!(godot_version("4.3.00.stable\n"), Some("4.3.0".to_owned()));
        let install = |folder: &str| format!(r"C:\godot\{folder}\Godot_console.exe");
        assert_eq!(
            named_as_found(&install("4.7.2"), Probed::Version("4.7.2".to_owned())),
            Probed::Version("4.7.2".to_owned())
        );
        for (folder, version) in [("04.7.2", "4.7.2"), ("4.3", "4.3.0"), ("4.7.2", "4.7.10")] {
            assert_eq!(
                named_as_found(&install(folder), Probed::Version(version.to_owned())),
                Probed::Skipped(format!("named {folder} but reports {version}")),
                "{folder}"
            );
        }
        assert_eq!(
            named_as_found(&install("4.7.2"), Probed::Missing),
            Probed::Missing,
            "no version, nothing to judge"
        );
    }

    /// A reparse point that is no link, as an app execution alias or a cloud placeholder is, is refused as a link is.
    #[test]
    fn any_reparse_point_is_refused_not_only_a_link() {
        assert!(is_reparse_point(false, REPARSE_POINT | ARCHIVE), "an app execution alias");
        assert!(is_reparse_point(false, REPARSE_POINT | DIRECTORY), "a reparse folder that is no junction");
        assert!(is_reparse_point(true, REPARSE_POINT), "a link");
        assert!(is_reparse_point(true, 0), "a link as the file type says");
        assert!(!is_reparse_point(false, ARCHIVE), "a plain file");
        assert!(!is_reparse_point(false, DIRECTORY), "a plain folder");
    }

    #[test]
    fn the_version_is_the_first_dotted_number_after_an_optional_v() {
        for (text, version) in [
            ("rustc 1.99.0 (b940084d7 2026-09-28)", "1.99.0"),
            ("v24.19.0", "24.19.0"),
            ("12.5.1", "12.5.1"),
            ("PowerShell 7.6.6", "7.6.6"),
            ("ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright (c) 2000-2026", "9.0.2"),
            ("cargo-nextest 0.9.100 (abc 2026-01-01)", "0.9.100"),
            ("cargo-nextest 0.9.148 (cd1d6d546 2026-10-07)", "0.9.148"),
            ("10.12", "10.12"),
            ("tool 1.2.3.4", "1.2.3"),
            ("tool 1.2.", "1.2"),
            ("tool 1234567890.1", "1234567890.1"),
        ] {
            assert_eq!(first_version(text).as_deref(), Some(version), "{text:?}");
        }
    }

    #[test]
    fn text_with_no_dotted_number_has_no_version() {
        for text in [
            "ffmpeg version N-112345-gabcdef",
            "",
            "24",
            "x1.2 rev2.3",
            "abc1.2.3",
            "1..2",
            "release 7.",
        ] {
            assert_eq!(first_version(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_number_of_more_than_ten_digits_is_no_readable_version() {
        for text in [
            "tool 12345678901.2",
            "tool 1.12345678901",
            "tool 1.2.3.12345678901",
            "v99999999999.0 then 1.2.3",
        ] {
            assert_eq!(first_version(text), None, "{text:?}");
        }
    }

    #[test]
    fn only_the_first_line_of_the_chosen_stream_is_read() {
        let gyan = "ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright (c) 2000-2026\r\nbuilt with gcc 14.2.0\r\n";
        assert_eq!(version_of(gyan, "").as_deref(), Some("9.0.2"));
        let git_build = "ffmpeg version N-117456-gabc Copyright (c) 2000-2026\r\nbuilt with gcc 14.2.0\r\n";
        assert_eq!(version_of(git_build, ""), None, "the compiler's version is not the tool's");
        assert_eq!(version_of("\r\n  \r\nv24.19.0\r\n", "").as_deref(), Some("24.19.0"), "blank lines first");
        assert_eq!(
            version_of(" \r\n", "PowerShell 7.6.6\r\n").as_deref(),
            Some("7.6.6"),
            "stderr when stdout is blank"
        );
        assert_eq!(
            version_of("no version here\r\n", "7.6.6\r\n"),
            None,
            "stderr is not read when stdout has text"
        );
    }

    #[test]
    fn a_late_probe_is_said_with_the_batch_limit_and_why_it_could_not_be_stopped() {
        let late = "node --version did not answer in 9873 ms";
        let limit = Duration::from_secs(10);
        assert_eq!(late_reason(late, late, limit).as_deref(), Some("no answer in 10 s"));
        assert_eq!(
            late_reason(&format!("{late}, and could not be stopped: access denied"), late, limit).as_deref(),
            Some("no answer in 10 s, and could not be stopped: access denied")
        );
        assert_eq!(
            late_reason(late, late, Duration::from_millis(200)).as_deref(),
            Some("no answer in 200 ms")
        );
        assert_eq!(late_reason("cannot run node: access denied", late, limit), None);
    }

    #[test]
    fn a_panicking_probe_says_its_message() {
        assert_eq!(panic_reason(&"boom"), "the probe failed: boom");
        assert_eq!(panic_reason(&"bang".to_owned()), "the probe failed: bang");
        assert_eq!(panic_reason(&7_u8), "the probe failed");
    }

    #[test]
    fn each_finding_says_its_line() {
        assert_eq!(
            tool_line("node", &Probed::Version("24.19.0".to_owned())),
            "gate: remote tool node 24.19.0"
        );
        assert_eq!(tool_line("ffmpeg", &Probed::Present), "gate: remote tool ffmpeg present");
        assert_eq!(tool_line("pnpm", &Probed::Missing), "gate: remote tool pnpm missing");
        assert_eq!(
            tool_line("node", &Probed::Skipped("no answer in 10 s".to_owned())),
            "gate: remote tool node skipped: no answer in 10 s"
        );
        assert_eq!(
            tool_line("a\nb", &Probed::Skipped("why\r".to_owned())),
            r"gate: remote tool a\nb skipped: why\r",
            "a name and a reason from outside are escaped"
        );
    }

    #[test]
    fn a_name_the_table_does_not_know_runs_nothing() {
        let names = ["nodejs".to_owned(), "Node".to_owned()];
        assert_eq!(
            probe(&names, Duration::from_secs(1), Ok(Variables::from_process()), &[]),
            [
                ("nodejs".to_owned(), Probed::Skipped("unknown tool".to_owned())),
                ("Node".to_owned(), Probed::Skipped("unknown tool".to_owned())),
            ]
        );
    }

    /// The godot probe is given a root of its own here, holding one entry: a listing its first entry comes to with no
    /// time left says the batch's own reason, as a probe that runs a program does.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose root cannot be made must fail loudly")]
    fn a_batch_with_no_time_left_runs_nothing_and_says_so() {
        let names = ["node".to_owned(), "rust".to_owned(), "godot".to_owned()];
        let root = unit_root("no-time");
        fs::create_dir_all(root.join("4.7.2")).expect("the root's one entry can be made");
        let root = root.to_string_lossy().into_owned();
        assert_eq!(
            probe(&names, Duration::ZERO, Ok(Variables::from_process().with("GATE_GODOT_ROOT", &root)), &[]),
            [
                ("node".to_owned(), Probed::Skipped("no answer in 0 s".to_owned())),
                ("rust".to_owned(), Probed::Skipped("no answer in 0 s".to_owned())),
                ("godot".to_owned(), Probed::Skipped("no answer in 0 s".to_owned())),
            ]
        );
        fs::remove_dir_all(&root).expect("the test's root can be removed");
    }

    #[test]
    fn the_version_a_godot_install_answers_with_is_the_number_its_line_leads_with() {
        for (output, version) in [
            ("4.7.2.stable.mono.official.abc\n", "4.7.2"),
            ("4.3.stable.mono.official.77dcf97d8\n", "4.3.0"),
            ("  4.7.2.stable.mono.official.abc  \r\nsecond line\r\n", "4.7.2"),
            ("4.7.2\n", "4.7.2"),
            ("4.7\n", "4.7.0"),
        ] {
            assert_eq!(godot_version(output).as_deref(), Some(version), "{output:?}");
        }
    }

    #[test]
    fn a_godot_output_whose_first_line_leads_with_no_version_has_none() {
        for output in [
            "",
            "\n4.7.2.stable.mono.official.abc\n",
            "hello\n",
            "Godot 4.7.2.stable.mono.official.abc\n",
            "4\n",
            "4.7.2.1.stable\n",
            "4.dev.stable\n",
        ] {
            assert_eq!(godot_version(output), None, "{output:?}");
        }
    }

    #[test]
    fn the_godot_list_is_deduplicated_and_ordered_by_number() {
        assert_eq!(version_list(&[]), "");
        assert_eq!(version_list(&["4.7.2".to_owned()]), "4.7.2");
        assert_eq!(version_list(&["4.7.2".to_owned(), "4.10.0".to_owned()]), "4.7.2,4.10.0");
        assert_eq!(version_list(&["4.7.2".to_owned(), "4.3.0".to_owned(), "4.7.2".to_owned()]), "4.3.0,4.7.2");
    }

    #[test]
    fn a_godot_version_part_of_more_than_ten_digits_is_no_version() {
        for output in ["12345678901.2.stable\n", "4.12345678901.stable\n", "4.7.12345678901.stable\n"] {
            assert_eq!(godot_version(output), None, "{output:?}");
        }
    }

    #[test]
    fn a_long_answer_is_cut_for_the_no_version_reason() {
        assert_eq!(
            godot_gave_no_version(&format!("{}\nsecond line\n", "x".repeat(300))),
            format!("Godot_console.exe --version gave no version: {}…", "x".repeat(200))
        );
        assert_eq!(
            godot_gave_no_version(&"y".repeat(200)),
            format!("Godot_console.exe --version gave no version: {}", "y".repeat(200)),
            "a line of exactly 200 characters is not cut"
        );
    }

    #[test]
    fn a_root_that_is_no_drive_path_is_skipped_with_its_path() {
        let deadline = Instant::now() + Duration::from_secs(1);
        for root in ["godot", r"\\server\share\godot", r"\\?\C:\godot", "/godot"] {
            assert_eq!(
                godot_candidates(Some(OsStr::new(root)), deadline, Duration::from_secs(10)),
                Err(format!("GATE_GODOT_ROOT {root} is not an absolute path on a drive")),
                "{root}"
            );
        }
    }

    #[test]
    fn an_install_answering_with_a_version_is_read_whatever_its_exit_status() {
        let output = ProgramOutput {
            status: ExitStatusExt::from_raw(3),
            stdout: b"4.7.2.stable.mono.official.abc\n".to_vec(),
            stderr: Vec::new(),
        };

        assert_eq!(read_godot(&output), Probed::Version("4.7.2".to_owned()));
    }

    /// The reason a root that is no absolute path on a drive is refused for escapes the root it names.
    #[test]
    fn the_drive_reason_escapes_the_root_it_names() {
        assert_eq!(
            godot_candidates(
                Some(OsStr::new("C:\\godot\u{7}")),
                Instant::now() + Duration::from_secs(1),
                Duration::from_secs(10)
            ),
            Err(r"GATE_GODOT_ROOT C:\godot\u{7} is not an absolute path on a drive".to_owned())
        );
    }

    /// A root of a test's own in the tests' temp folder, made and empty.
    #[expect(clippy::expect_used, reason = "a test whose root cannot be made must fail loudly")]
    fn unit_root(name: &str) -> PathBuf {
        let root = crate::test_support::temp_dir().join(format!("gate-godot-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).expect("the test's root can be made");
        root
    }

    /// Only the first sixty-four subfolders are read: which one the cap leaves out is the folder's own order, so the
    /// count is what a case can hold.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose install folders cannot be made must fail loudly")]
    fn only_the_first_sixty_four_godot_subfolders_are_read() {
        let root = unit_root("cap");
        for at in 0..65 {
            let folder = root.join(format!("s{at:02}"));
            fs::create_dir_all(&folder).expect("an install folder can be made");
            fs::write(folder.join(GODOT), b"").expect("a file named as the program can be written");
        }

        let candidates =
            godot_candidates(Some(root.as_os_str()), Instant::now() + Duration::from_secs(60), Duration::from_secs(10)).expect("the root reads");
        let installed: Vec<&str> = candidates
            .iter()
            .filter_map(|candidate| match candidate {
                Candidate::Install(install) => Some(install.as_str()),
                Candidate::Skipped(_) => None,
            })
            .collect();

        assert_eq!(installed.len(), 64, "only the first sixty-four are read: {installed:?}");
        fs::remove_dir_all(&root).expect("the test's root can be removed");
    }

    /// A subfolder whose name is no Unicode is left out, and the plain one beside it still lists.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose folders cannot be made must fail loudly")]
    fn a_godot_subfolder_whose_name_is_no_unicode_is_left_out() {
        let root = unit_root("unicode");
        let odd = root.join(OsString::from_wide(&[0xD800]));
        fs::create_dir_all(&odd).expect("a folder with a lone surrogate in its name can be made");
        fs::write(odd.join(GODOT), b"").expect("a file named as the program can be written");
        let plain = root.join("4.7.2");
        fs::create_dir_all(&plain).expect("an install folder can be made");
        fs::write(plain.join(GODOT), b"").expect("a file named as the program can be written");

        let candidates =
            godot_candidates(Some(root.as_os_str()), Instant::now() + Duration::from_secs(60), Duration::from_secs(10)).expect("the root reads");

        assert_eq!(candidates, [Candidate::Install(plain.join(GODOT).to_string_lossy().into_owned())]);
        fs::remove_dir_all(&root).expect("the test's root can be removed");
    }
}
