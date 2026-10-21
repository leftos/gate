//! The builder's tool probes: for each tool a routed run needs, what the builder has, read by running the tool's own
//! version command from this module's table, never a program the caller names. The tool the box may have several
//! installs of is godot: the folder the batch's environment names as its root holds one install per subfolder, and each
//! is asked for its version, so the line carries every version the box has. The unreal probe reads the folder the
//! batch's environment names as `GATE_UNREAL_ROOT` likewise, one Unreal Engine install per subfolder, each one's version
//! read from its `Engine\Build\Build.version` file without running anything. The netfx-sdk probe reads the .NET
//! Framework SDKs the registry lists, as `UnrealBuildTool` looks for one, also running nothing. Every probe of a batch
//! that runs a program runs at once, under one deadline the batch shares, in the system folder and on a hidden console
//! of its own, as [`git::program_output`] runs a program; a probe still running at the deadline is ended with its job.
//! Nothing here writes to the console or reads a payload: the caller hands over the names and says the lines.

use std::any::Any;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::thread::{self, Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use gate_router::escaped;
use gate_win32::{Hive, PipedConsole, Subkey};
use gate_wire::{MAX_PART_DIGITS, is_canonical_version, is_drive_path, version_of};
use serde_json::Value;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT};

use crate::environment::Variables;
use crate::remote::git::{self, GitError, ProgramCall, ProgramOutput, ProgramPlace};
use crate::remote::launch;
use crate::run::{is_within, resolved};

/// How long a batch of probes may take together, unless a test seam says otherwise.
pub(crate) const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// The time windows of one batch of probes: how long the batch may take together, and how long the readers of each
/// program a probe runs get to end once it has exited.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Windows {
    pub limit: Duration,
    pub drain: Duration,
}

impl Windows {
    /// The windows test seams give, in milliseconds: `limit_ms` (`--test-probe-limit-ms`), else [`PROBE_LIMIT`], and
    /// `drain_ms` (`--test-probe-drain-ms`), else [`launch::DRAIN`].
    pub(crate) fn of(limit_ms: Option<u64>, drain_ms: Option<u64>) -> Windows {
        Windows {
            limit: limit_ms.map_or(PROBE_LIMIT, Duration::from_millis),
            drain: drain_ms.map_or(launch::DRAIN, Duration::from_millis),
        }
    }
}

/// The console program of a Godot install.
pub(crate) const GODOT: &str = "Godot_console.exe";

/// The word a Godot install is asked for its version with.
const GODOT_VERSION: &[&str] = &["--version"];

/// The variable naming the folder of the box's Godot installs.
const GODOT_ROOT: &str = "GATE_GODOT_ROOT";

/// The variable naming the folder of the box's Unreal Engine installs.
pub(crate) const UNREAL_ROOT: &str = "GATE_UNREAL_ROOT";

/// The most entries of the folder `GATE_GODOT_ROOT` or `GATE_UNREAL_ROOT` names that are looked at: a folder holding
/// more is no install root.
const ROOT_ENTRIES: usize = 64;

/// The most bytes of an Unreal Engine install's `Build.version` that are read: a longer one is no version file.
const UNREAL_BUILD_VERSION_MAX: u64 = 64 * 1024;

/// The most characters of a Godot install's answer the no-version reason carries before it is escaped.
const GODOT_ANSWER_MAX: usize = 200;

/// The key, in the 32-bit view of `HKEY_LOCAL_MACHINE` (`WOW6432Node`), whose subkeys are the installed .NET Framework
/// SDKs, one per version, where `UnrealBuildTool` looks for one.
const NETFX_SDK_KEY: &str = r"SOFTWARE\Microsoft\Microsoft SDKs\NETFXSDK";

/// The value of a .NET Framework SDK's subkey that names the folder it is installed in.
const NETFX_KITS_FOLDER: &str = "KitsInstallationFolder";

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
    /// Reads the `Build.version` of every Unreal Engine install the batch's environment names, running nothing.
    Unreal,
    /// Reads the .NET Framework SDKs the registry lists, running nothing.
    NetfxSdk,
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
        "git-lfs" => runs("git-lfs", &["version"]),
        "godot" => Some(ToolProbe::Godot),
        "unreal" => Some(ToolProbe::Unreal),
        "netfx-sdk" => Some(ToolProbe::NetfxSdk),
        _ => None,
    }
}

/// A probe of the batch: found without running anything, or running on a thread of its own.
enum Pending<'scope> {
    Found(Probed),
    Running(ScopedJoinHandle<'scope, Probed>),
}

/// What one batch of probes shares beyond the place a program is looked for in: the folders no program may come from,
/// the folders the batch's environment names as `GATE_GODOT_ROOT` and `GATE_UNREAL_ROOT` (the latter `Err` with why
/// the environment could not be made), the key under `HKEY_CURRENT_USER` a test seam has the netfx-sdk probe read in
/// place of the machine's own list, the batch's deadline and its limit, and how long the readers of each program it runs
/// get to end once it has exited.
#[derive(Clone, Copy)]
struct Batch<'a> {
    worktrees: &'a [&'a Path],
    godot_root: Option<&'a OsStr>,
    unreal_root: Result<Option<&'a OsStr>, &'a str>,
    netfx_key: Option<&'a str>,
    deadline: Instant,
    limit: Duration,
    drain: Duration,
}

/// Probes each tool of `names`, all within the limit of `windows` from now, the readers of each program run given its
/// drain once it has exited, and gives back each name with what its probe found, in the order of `names`. The probes that
/// run a program run at once, each on a thread of its own, sharing the system folder and the environment `variables` for
/// the batch, whose programs are looked for along its `PATH`, and none of them from a folder of `worktrees`: a program
/// found that still lies inside any of them is refused ([`git::program_output`]). Environment `variables` that could not
/// be made, for the reason given, skip every probe that runs a program. The godot probe reads the folder `variables`
/// names as `GATE_GODOT_ROOT` to find the installs it runs, and the unreal probe the folder they name as
/// `GATE_UNREAL_ROOT`, refused when it lies inside any of `worktrees`; it runs no program, so a place no program can be
/// looked for in never skips it. The netfx-sdk probe reads the machine's list of .NET Framework SDKs, or the key
/// `netfx_key` names under `HKEY_CURRENT_USER` when a test seam gives one ([`netfx_sdk`]).
pub(crate) fn probe(
    names: &[String],
    windows: Windows,
    variables: Result<Variables, String>,
    worktrees: &[&Path],
    netfx_key: Option<&str>,
) -> Vec<(String, Probed)> {
    let deadline = Instant::now() + windows.limit;
    let root_of = |name: &str| {
        variables
            .as_ref()
            .map(|variables| variables.get(name).map(OsStr::to_os_string))
            .map_err(String::clone)
    };
    let godot_root = root_of(GODOT_ROOT).ok().flatten();
    let unreal_root = root_of(UNREAL_ROOT);
    let place = variables.and_then(ProgramPlace::new);
    let batch = Batch {
        worktrees,
        godot_root: godot_root.as_deref(),
        unreal_root: unreal_root.as_ref().map(Option::as_deref).map_err(String::as_str),
        netfx_key,
        deadline,
        limit: windows.limit,
        drain: windows.drain,
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
        Some(ToolProbe::Unreal) => Pending::Running(scope.spawn(move || unreal(batch))),
        Some(ToolProbe::NetfxSdk) => Pending::Found(netfx_sdk(batch.netfx_key)),
    }
}

/// What the netfx-sdk probe finds: the version of every .NET Framework SDK the registry lists, `,`-joined
/// ([`version_list`]), and `missing` when it lists none. The list is the subkeys of [`NETFX_SDK_KEY`] in the 32-bit view
/// of `HKEY_LOCAL_MACHINE`, or of the key `user_key` names under `HKEY_CURRENT_USER` when a test seam gives one; a
/// subkey counts when it is named a version and its [`NETFX_KITS_FOLDER`] names a folder that exists
/// ([`netfx_installed`]), and at most [`ROOT_ENTRIES`] are looked at. A key that is not there, or cannot be read, lists
/// none, so the SDK reads as missing, never skipped: a run that needs it must not go where it cannot be found.
fn netfx_sdk(user_key: Option<&str>) -> Probed {
    let read = match user_key {
        Some(key) => gate_win32::registry_subkeys(Hive::CurrentUser, key, NETFX_KITS_FOLDER, ROOT_ENTRIES),
        None => gate_win32::registry_subkeys(Hive::LocalMachine32, NETFX_SDK_KEY, NETFX_KITS_FOLDER, ROOT_ENTRIES),
    };
    let Ok(Some(subkeys)) = read else {
        return Probed::Missing;
    };
    let versions: Vec<String> = subkeys.into_iter().filter(netfx_installed).map(|subkey| subkey.name).collect();
    if versions.is_empty() {
        return Probed::Missing;
    }
    Probed::Version(version_list(&versions))
}

/// Whether the subkey `subkey` of the .NET Framework SDK list is an installed SDK: its name is a version
/// ([`is_version_name`]) and its [`NETFX_KITS_FOLDER`] names, as an absolute path, a folder that exists.
fn netfx_installed(subkey: &Subkey) -> bool {
    let folder = subkey.value.as_deref().map(Path::new);
    is_version_name(&subkey.name) && folder.is_some_and(|folder| folder.is_absolute() && folder.is_dir())
}

/// Whether `name` is a version as the .NET Framework SDK list names one: one to three dot-separated runs of ASCII
/// digits, each of 1 to [`MAX_PART_DIGITS`] digits, such as `4.8` or `4.7.2`.
fn is_version_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    let part = |part: &&str| (1..=MAX_PART_DIGITS).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit());
    (1..=3).contains(&parts.len()) && parts.iter().all(part)
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
        drain: batch.drain,
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
    gathered(candidates.iter().map(|candidate| match candidate {
        Candidate::Install(install) => {
            let found = ran(install, GODOT_VERSION, place, batch, read_godot);
            if batch.godot_root.is_some() {
                named_as_found(install, found)
            } else {
                found
            }
        }
        Candidate::Skipped(why) => Probed::Skipped(why.clone()),
    }))
}

/// What a probe of several installs comes to from what each install's probe `found`, in order: the version of every
/// one that gave one, `,`-joined ([`version_list`]), `missing` when there is none at all, and `skipped` with the first
/// reason one gave when none gave a version.
fn gathered(found: impl Iterator<Item = Probed>) -> Probed {
    let mut versions = Vec::new();
    let mut reason: Option<String> = None;
    for found in found {
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

/// What the probe of the root's install `install` `found`, kept only when it is a version the install's folder is named
/// ([`named_as`]).
fn named_as_found(install: &str, found: Probed) -> Probed {
    let Probed::Version(version) = found else {
        return found;
    };
    let folder = Path::new(install)
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    named_as(&folder, version)
}

/// The `version` an install whose folder is named `folder` reports, kept only when it is that name: else skipped with
/// the reason `named <folder> but reports <version>`.
fn named_as(folder: &str, version: String) -> Probed {
    if folder == version {
        Probed::Version(version)
    } else {
        Probed::Skipped(format!("named {} but reports {version}", escaped(folder)))
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
/// `GATE_GODOT_ROOT` ([`listed`], each judged by [`godot_install`]), or `Godot_console.exe` itself, looked for along the
/// `PATH`, when it names none. Else why that folder is refused.
fn godot_candidates(root: Option<&OsStr>, deadline: Instant, limit: Duration) -> Result<Vec<Candidate>, String> {
    let Some(root) = root else {
        return Ok(vec![Candidate::Install(GODOT.to_owned())]);
    };
    listed(GODOT_ROOT, drive_root(GODOT_ROOT, root)?, deadline, limit, godot_install)
}

/// The root `root` the variable `variable` names as a path, when it is an absolute path on a drive; else why it is
/// refused.
fn drive_root<'r>(variable: &str, root: &'r OsStr) -> Result<&'r Path, String> {
    match root.to_str().filter(|root| is_drive_path(root)) {
        Some(root) => Ok(Path::new(root)),
        None => Err(format!(
            "{variable} {} is not an absolute path on a drive",
            escaped(&root.to_string_lossy())
        )),
    }
}

/// What `judged` makes of each subfolder of `folder`, the install root the variable `variable` names, in name order;
/// else why the folder cannot be read. Its subfolders are read as they are: a subfolder whose name is no Unicode, one
/// past the first [`ROOT_ENTRIES`] and an entry that cannot be read are left out, as is one `judged` gives nothing for,
/// and the listing ends at `deadline`, which with nothing read at all is the batch's own `no answer` reason.
fn listed<T>(variable: &str, folder: &Path, deadline: Instant, limit: Duration, judged: impl Fn(&Path) -> Option<T>) -> Result<Vec<T>, String> {
    let unreadable = |error: &io::Error| format!("cannot read {variable} {}: {error}", escaped(&folder.to_string_lossy()));
    let entries = fs::read_dir(folder).map_err(|error| unreadable(&error))?;
    let mut folders = Vec::new();
    let mut cut = false;
    for entry in entries.take(ROOT_ENTRIES) {
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
    Ok(folders.iter().filter_map(|folder| judged(folder)).collect())
}

/// What the unreal probe finds: the version of every install the folder the batch's environment names as
/// `GATE_UNREAL_ROOT` holds, `,`-joined, as [`gathered`] comes to it; `missing` with no root, as nothing is looked for
/// along the `PATH`; `skipped` when the root is refused ([`unreal_installs`]) or the environment could not be read.
fn unreal(batch: Batch<'_>) -> Probed {
    let root = match batch.unreal_root {
        Err(why) => return Probed::Skipped(format!("cannot read {UNREAL_ROOT}: {why}")),
        Ok(None) => return Probed::Missing,
        Ok(Some(root)) => root,
    };
    match unreal_installs(root, batch.worktrees, batch.deadline, batch.limit) {
        Ok(found) => gathered(found.into_iter()),
        Err(why) => Probed::Skipped(why),
    }
}

/// What each subfolder of the Unreal Engine root `root` comes to, in name order ([`listed`], then [`unreal_read`]):
/// else why the root is refused ([`checked_unreal_root`]) or cannot be read.
fn unreal_installs(root: &OsStr, worktrees: &[&Path], deadline: Instant, limit: Duration) -> Result<Vec<Probed>, String> {
    let folder = checked_unreal_root(root, worktrees)?;
    let folders = listed(UNREAL_ROOT, folder, deadline, limit, |folder| Some(folder.to_path_buf()))?;
    // A listing the deadline cut short leaves the deadline passed, so the reading says `no answer` before its first
    // install: a partial list is never taken for the whole.
    unreal_read(&folders, limit, || Instant::now() >= deadline)
}

/// What each install folder of `folders` comes to, in order ([`unreal_install`]), each read only once `passed` says
/// the batch's deadline has not passed; else the batch's own `no answer` reason, with its `limit`, whatever was read
/// before, so a reading cut short never passes for the root's whole list.
fn unreal_read(folders: &[PathBuf], limit: Duration, mut passed: impl FnMut() -> bool) -> Result<Vec<Probed>, String> {
    let mut found = Vec::new();
    for folder in folders {
        if passed() {
            return Err(no_answer(limit));
        }
        found.extend(unreal_install(folder));
    }
    Ok(found)
}

/// The folder the Unreal Engine root `root`, the box's `GATE_UNREAL_ROOT`, names, as the probe and a run both take
/// it: an absolute path on a drive ([`drive_root`]), neither it nor any folder above it a link or a junction
/// ([`linked`]), and overlapping none of `worktrees`, the folders a run writes, followed through links: neither inside
/// one nor holding one. Else why it is refused, naming the first folder it overlaps.
pub(crate) fn checked_unreal_root<'r>(root: &'r OsStr, worktrees: &[&Path]) -> Result<&'r Path, String> {
    let folder = drive_root(UNREAL_ROOT, root)?;
    if let Some(link) = folder.ancestors().find(|path| linked(path)) {
        return Err(format!(
            "{UNREAL_ROOT} {}: {} is a link or a junction",
            escaped(&folder.to_string_lossy()),
            escaped(&link.to_string_lossy())
        ));
    }
    let held = resolved(folder);
    let overlaps = |worktree: &&&Path| {
        let written = resolved(worktree);
        is_within(&held, &written) || is_within(&written, &held)
    };
    if let Some(written) = worktrees.iter().find(overlaps) {
        return Err(format!(
            "{UNREAL_ROOT} {} overlaps {}, which a run writes",
            escaped(&folder.to_string_lossy()),
            escaped(&written.to_string_lossy())
        ));
    }
    Ok(folder)
}

/// The engine folder of the install of `version` under the root `root`, the box's `GATE_UNREAL_ROOT`, as a run finds
/// it by name: `<root>\<version>`, when the root passes the probe's checks against the run's folders `worktrees`
/// ([`checked_unreal_root`]) and that folder is an install that reports that version ([`unreal_install`]); else why
/// not.
pub(crate) fn installed_unreal(root: &OsStr, version: &str, worktrees: &[&Path]) -> Result<PathBuf, String> {
    let folder = checked_unreal_root(root, worktrees)?.join(version);
    match unreal_install(&folder) {
        Some(Probed::Version(_)) => Ok(folder),
        Some(Probed::Skipped(why)) => Err(why),
        Some(Probed::Present | Probed::Missing) | None => Err(format!("{} is not a file", escaped(&unreal_build_version(&folder).to_string_lossy()))),
    }
}

/// The file an Unreal Engine install in `folder` says its version in: `<folder>\Engine\Build\Build.version`.
fn unreal_build_version(folder: &Path) -> PathBuf {
    folder.join("Engine").join("Build").join("Build.version")
}

/// What `folder`, one direct subfolder of `GATE_UNREAL_ROOT`, comes to: the version its `Build.version` reports, kept
/// only when the folder is named it ([`named_as`]); skipped with why when the folder, its `Engine` or `Build` folder or
/// the file is a link or a junction, or the file gives no version ([`build_version`]); nothing when it holds no such
/// plain file.
fn unreal_install(folder: &Path) -> Option<Probed> {
    let file = unreal_build_version(folder);
    let engine = folder.join("Engine");
    let build = engine.join("Build");
    if let Some(link) = [folder, engine.as_path(), build.as_path(), file.as_path()]
        .into_iter()
        .find(|path| linked(path))
    {
        return Some(Probed::Skipped(format!(
            "cannot read {}: {} is a link or a junction",
            escaped(&file.to_string_lossy()),
            escaped(&link.to_string_lossy())
        )));
    }
    if !plain_file(&file) {
        return None;
    }
    let name = folder.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    Some(build_version(&file).map_or_else(Probed::Skipped, |version| named_as(&name, version)))
}

/// The version the `Build.version` file `file` reports ([`unreal_version`]); else why it gives none: it cannot be
/// read, it is over [`UNREAL_BUILD_VERSION_MAX`] bytes, it is not UTF-8, or it is not that JSON. The file is opened as
/// itself, never through a link, and read only when the handle is a plain disk file ([`gate_win32::check_plain_file`]),
/// so a link or a pipe put there after the probe looked is never read.
fn build_version(file: &Path) -> Result<String, String> {
    let shown = escaped(&file.to_string_lossy());
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(file)
        .and_then(|opened| gate_win32::check_plain_file(&opened).map(|()| opened))
        .and_then(|opened| opened.take(UNREAL_BUILD_VERSION_MAX + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("cannot read {shown}: {error}"))?;
    // A length past what u64 holds is past the cap too.
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > UNREAL_BUILD_VERSION_MAX {
        return Err(format!("{shown} is over 64 KiB"));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8"))?;
    unreal_version(&text).ok_or_else(|| format!("{shown} gives no integer MajorVersion, MinorVersion and PatchVersion"))
}

/// The version a `Build.version` text `text` reports: a JSON object, after an optional byte order mark, whose
/// `MajorVersion`, `MinorVersion` and `PatchVersion` are non-negative integers, as `major.minor.patch` in its canonical
/// spelling ([`is_canonical_version`]); its other keys are not read. `None` for any other shape.
fn unreal_version(text: &str) -> Option<String> {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(text)) else {
        return None;
    };
    let part = |key: &str| fields.get(key).and_then(Value::as_u64);
    let version = format!("{}.{}.{}", part("MajorVersion")?, part("MinorVersion")?, part("PatchVersion")?);
    is_canonical_version(&version).then_some(version)
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
/// [`is_canonical_version`]: gate_wire::is_canonical_version
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
/// version ([`version_of`]); else that version.
fn read(output: &ProgramOutput) -> Probed {
    if !output.status.success() {
        return Probed::Present;
    }
    version_of(&String::from_utf8_lossy(&output.stdout), &String::from_utf8_lossy(&output.stderr)).map_or(Probed::Present, Probed::Version)
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
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::GODOT;
    use super::{
        Batch, Candidate, Pending, Probed, Windows, build_version, godot_candidates, godot_gave_no_version, godot_version, is_reparse_point,
        is_version_name, late_reason, named_as_found, netfx_sdk, panic_reason, probe, read_godot, started, tool_line, unreal_installs, unreal_read,
        unreal_version, version_list,
    };
    use crate::environment::Variables;
    use crate::remote::git::{ProgramOutput, ProgramPlace};

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

    /// `git-lfs` is probed by `git-lfs version`, whose `git-lfs/<x.y.z>` line `gate_wire::version_of` reads.
    #[test]
    fn git_lfs_is_probed_by_its_version_verb() {
        assert!(matches!(
            super::probe_of("git-lfs"),
            Some(super::ToolProbe::Runs {
                program: "git-lfs",
                args: &["version"]
            })
        ));
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
            probe(&names, Windows::of(Some(1000), None), Ok(Variables::from_process()), &[], None),
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
            probe(
                &names,
                Windows::of(Some(0), None),
                Ok(Variables::from_process().with("GATE_GODOT_ROOT", &root)),
                &[],
                None
            ),
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

    /// Makes `folder` an Unreal Engine install whose `Build.version` holds `text`.
    #[expect(clippy::expect_used, reason = "a test whose install cannot be made must fail loudly")]
    fn unit_unreal(folder: &Path, text: &str) {
        let build = folder.join("Engine").join("Build");
        fs::create_dir_all(&build).expect("an engine's folders can be made");
        fs::write(build.join("Build.version"), text).expect("an engine's Build.version can be written");
    }

    /// The `Build.version` text of an engine reporting `major.minor.patch`.
    fn build_version_text(major: u32, minor: u32, patch: u32) -> String {
        format!(r#"{{"MajorVersion": {major}, "MinorVersion": {minor}, "PatchVersion": {patch}, "BranchName": "++UE5"}}"#)
    }

    /// Only the first sixty-four subfolders of the unreal root are read, and what they come to is in name order.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose install folders cannot be made must fail loudly")]
    fn only_the_first_sixty_four_unreal_subfolders_are_read_in_name_order() {
        let root = unit_root("unreal-cap");
        for patch in 10..75 {
            unit_unreal(&root.join(format!("1.0.{patch}")), &build_version_text(1, 0, patch));
        }

        let found =
            unreal_installs(root.as_os_str(), &[], Instant::now() + Duration::from_secs(60), Duration::from_secs(10)).expect("the root reads");

        let expected: Vec<Probed> = (10..74).map(|patch| Probed::Version(format!("1.0.{patch}"))).collect();
        assert_eq!(found, expected);
        fs::remove_dir_all(&root).expect("the test's root can be removed");
    }

    /// The unreal line orders its versions by number, not by the folders' names; an environment that could not be made
    /// skips it with why.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose install folders cannot be made must fail loudly")]
    fn the_unreal_line_orders_its_versions_by_number() {
        let root = unit_root("unreal-order");
        unit_unreal(&root.join("5.10.0"), &build_version_text(5, 10, 0));
        unit_unreal(&root.join("5.8.0"), &build_version_text(5, 8, 0));
        let names = ["unreal".to_owned()];
        let variables = Variables::from_process().with("GATE_UNREAL_ROOT", &root.to_string_lossy());

        assert_eq!(
            probe(&names, Windows::of(Some(10_000), None), Ok(variables), &[], None),
            [("unreal".to_owned(), Probed::Version("5.8.0,5.10.0".to_owned()))]
        );
        assert_eq!(
            probe(&names, Windows::of(Some(10_000), None), Err("no environment".to_owned()), &[], None),
            [(
                "unreal".to_owned(),
                Probed::Skipped("cannot read GATE_UNREAL_ROOT: no environment".to_owned())
            )]
        );
        fs::remove_dir_all(&root).expect("the test's root can be removed");
    }

    /// A `Build.version` reports a version only as a JSON object of three non-negative integer parts, each of at most
    /// ten digits, a byte order mark before it allowed and its other keys ignored.
    #[test]
    fn an_unreal_build_version_is_an_object_of_three_integer_parts() {
        assert_eq!(unreal_version(&build_version_text(5, 8, 0)).as_deref(), Some("5.8.0"));
        assert_eq!(
            unreal_version(&format!("\u{feff}{}", build_version_text(5, 7, 3))).as_deref(),
            Some("5.7.3")
        );
        for text in [
            "",
            "not json",
            "[5, 8, 0]",
            r#"{"MajorVersion": 5, "MinorVersion": 8}"#,
            r#"{"MajorVersion": "5", "MinorVersion": 8, "PatchVersion": 0}"#,
            r#"{"MajorVersion": 5, "MinorVersion": -8, "PatchVersion": 0}"#,
            r#"{"MajorVersion": 5.0, "MinorVersion": 8, "PatchVersion": 0}"#,
            r#"{"MajorVersion": 12345678901, "MinorVersion": 8, "PatchVersion": 0}"#,
        ] {
            assert_eq!(unreal_version(text), None, "{text:?}");
        }
    }

    /// A `Build.version` over 64 KiB, or one that is not UTF-8, is no version file, said with the file's path.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose files cannot be made must fail loudly")]
    fn an_unreal_build_version_over_64_kib_or_not_utf8_gives_no_version() {
        let root = unit_root("unreal-file");
        let file = root.join("Build.version");
        let text = build_version_text(5, 8, 0);
        let padded = |length: usize| format!("{text}{}", " ".repeat(length - text.len()));
        let shown = file.to_string_lossy().into_owned();

        fs::write(&file, padded(64 * 1024)).expect("the file can be written");
        assert_eq!(build_version(&file), Ok("5.8.0".to_owned()), "exactly 64 KiB is read");

        fs::write(&file, padded(64 * 1024 + 1)).expect("the file can be written");
        assert_eq!(build_version(&file), Err(format!("{shown} is over 64 KiB")));

        fs::write(&file, b"{\"MajorVersion\": 5, \xff}").expect("the file can be written");
        assert_eq!(build_version(&file), Err(format!("{shown} is not UTF-8")));
        fs::remove_dir_all(&root).expect("the test's root can be removed");
    }

    /// The unreal probe runs on a thread of its own, as the godot probe does, so a slow root never holds up the probes
    /// after it in the batch.
    #[test]
    fn the_unreal_probe_runs_on_a_thread_of_its_own() {
        let place: Result<ProgramPlace, String> = Err("no place".to_owned());
        let batch = Batch {
            worktrees: &[],
            godot_root: None,
            unreal_root: Ok(None),
            netfx_key: None,
            deadline: Instant::now(),
            limit: Duration::ZERO,
            drain: Duration::ZERO,
        };

        thread::scope(|scope| {
            let pending = started(scope, "unreal", &place, batch);
            assert!(matches!(pending, Pending::Running(_)), "the unreal probe runs inline");
        });
    }

    /// A key under `HKEY_CURRENT_USER` of a test's own that stands in for the machine's .NET Framework SDK list,
    /// removed when dropped; it does not exist until an SDK is listed in it.
    struct NetfxKey(String);

    impl NetfxKey {
        /// The key of the test `name` in this process, emptied of what an earlier process of the same id left.
        fn new(name: &str) -> NetfxKey {
            let key = NetfxKey(format!(r"Software\gate-tests\unit-netfx-{name}-{}", std::process::id()));
            key.removed();
            key
        }

        /// Lists the subkey `version`, with `folder` as its `KitsInstallationFolder` when given.
        #[expect(clippy::expect_used, reason = "a test whose key cannot be written must fail loudly")]
        fn with(self, version: &str, folder: Option<&Path>) -> NetfxKey {
            let subkey = format!(r"HKCU\{}\{version}", self.0);
            let mut args = vec!["add".to_owned(), subkey, "/f".to_owned()];
            if let Some(folder) = folder {
                args.extend(["/v", "KitsInstallationFolder", "/t", "REG_SZ", "/d"].map(str::to_owned));
                args.push(folder.display().to_string());
            }
            let status = reg(&args).expect("reg.exe runs");
            assert!(status.success(), "reg.exe {args:?} failed: {status:?}");
            self
        }

        /// Removes the key and all it holds; a key that is not there is no failure.
        fn removed(&self) {
            // A key that is not there makes reg.exe fail, which is what removing it leaves.
            let _ = reg(&["delete".to_owned(), format!(r"HKCU\{}", self.0), "/f".to_owned()]);
        }
    }

    impl Drop for NetfxKey {
        fn drop(&mut self) {
            self.removed();
        }
    }

    /// The exit status of the system's `reg.exe` run with `args`, its output dropped.
    fn reg(args: &[String]) -> std::io::Result<std::process::ExitStatus> {
        let system = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
        let mut command = std::process::Command::new(Path::new(&system).join("System32").join("reg.exe"));
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        gate_win32::spawn(&mut command)?.wait()
    }

    /// Every subkey the key lists that is named a version and whose folder exists is an SDK, said by number, through
    /// the batch's own probe.
    #[test]
    fn the_netfx_sdk_probe_says_every_sdk_the_key_lists() {
        let sdks = unit_root("netfx-several");
        let key = NetfxKey::new("several")
            .with("4.8", Some(&sdks))
            .with("4.6.2", Some(&sdks))
            .with("4.7.1", Some(&sdks));

        assert_eq!(
            probe(
                &["netfx-sdk".to_owned()],
                Windows::of(Some(10_000), None),
                Err("no environment".to_owned()),
                &[],
                Some(&key.0)
            ),
            [("netfx-sdk".to_owned(), Probed::Version("4.6.2,4.7.1,4.8".to_owned()))]
        );
    }

    /// A subkey with no `KitsInstallationFolder`, one naming a folder that does not exist, one naming a relative path
    /// and one not named a version are no SDK; with nothing else listed, the SDK is missing.
    #[test]
    fn the_netfx_sdk_probe_leaves_out_a_subkey_without_an_existing_folder() {
        let sdks = unit_root("netfx-left-out");
        let key = NetfxKey::new("left-out")
            .with("4.8", None)
            .with("4.7.2", Some(&sdks.join("not-there")))
            .with("4.6.2", Some(Path::new("relative")))
            .with("WinSDK-NetFx40Tools", Some(&sdks));

        assert_eq!(netfx_sdk(Some(&key.0)), Probed::Missing);

        let key = key.with("4.6.1", Some(&sdks));
        assert_eq!(netfx_sdk(Some(&key.0)), Probed::Version("4.6.1".to_owned()), "the one SDK that counts");
    }

    /// A key that is not there lists no SDK: the SDK is missing, not skipped.
    #[test]
    fn the_netfx_sdk_probe_says_missing_with_no_key() {
        let key = NetfxKey::new("absent");

        assert_eq!(netfx_sdk(Some(&key.0)), Probed::Missing);
    }

    /// A version as the SDK list names one: one to three runs of digits, none empty or past the digits a part may have.
    #[test]
    fn a_netfx_sdk_subkey_is_named_a_version() {
        for name in ["4", "4.8", "4.7.2", "10.0.1"] {
            assert!(is_version_name(name), "{name}");
        }
        for name in ["", "4.", ".8", "4.8.1.0", "v4.8", "4.8-preview", "WinSDK-NetFx40Tools", "1.12345678901"] {
            assert!(!is_version_name(name), "{name}");
        }
    }

    /// A `Build.version` that is a symbolic link is refused when it is opened, as the link itself, so a link put there
    /// after the probe looked is never followed to the file it names.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose files cannot be made must fail loudly")]
    #[expect(clippy::print_stderr, reason = "a skipped case says why it skipped")]
    fn an_unreal_build_version_that_is_a_symlink_is_refused_at_open() {
        let root = unit_root("unreal-symlink");
        let target = root.join("target.version");
        fs::write(&target, build_version_text(5, 8, 0)).expect("the target can be written");
        let link = root.join("Build.version");
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &link) {
            eprintln!("skipped: a file symlink cannot be made here (no privilege, or developer mode off): {error}");
            fs::remove_dir_all(&root).expect("the test's root can be removed");
            return;
        }

        let read = build_version(&link);

        let prefix = format!("cannot read {}: ", link.display());
        assert!(read.as_ref().is_err_and(|why| why.starts_with(&prefix)), "{read:?}");
        fs::remove_dir_all(&root).expect("the test's root can be removed");
    }

    /// A reading the deadline cuts short is the batch's own `no answer`, never the versions read before it, so a pin is
    /// judged not probed rather than missing from a partial list; a whole probe given no time says it too.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose install folders cannot be made must fail loudly")]
    fn an_unreal_listing_cut_by_the_deadline_is_skipped_even_with_versions_read() {
        let root = unit_root("unreal-deadline");
        let folders = [root.join("5.7.3"), root.join("5.8.0")];
        unit_unreal(&folders[0], &build_version_text(5, 7, 3));
        unit_unreal(&folders[1], &build_version_text(5, 8, 0));

        let mut looks = 0;
        let cut = unreal_read(&folders, Duration::ZERO, || {
            looks += 1;
            looks > 1
        });
        assert_eq!(cut, Err("no answer in 0 s".to_owned()), "the first install was read before the cut");
        assert_eq!(
            unreal_read(&folders, Duration::from_secs(60), || false),
            Ok(vec![Probed::Version("5.7.3".to_owned()), Probed::Version("5.8.0".to_owned())])
        );
        let variables = Variables::from_process().with("GATE_UNREAL_ROOT", &root.to_string_lossy());
        assert_eq!(
            probe(&["unreal".to_owned()], Windows::of(Some(0), None), Ok(variables), &[], None),
            [("unreal".to_owned(), Probed::Skipped("no answer in 0 s".to_owned()))]
        );
        fs::remove_dir_all(&root).expect("the test's root can be removed");
    }
}
