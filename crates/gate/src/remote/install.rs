//! A routed run's fetched outputs archive put into the worktree: unpacked into a stage folder beside it, which checks
//! every member's path and hash; each member then checked against the run's output globs, against the PC's manifest
//! (a file it held must still have its size and last write, one it lacked must still be missing), and one that counts as
//! tracked (its git index, a submodule or a nested repository holds it) unless a `--bring-tracked` glob matches it, or
//! whose PC file is in use or read-only, skipped and said;
//! the box's work folder rewritten to the worktree's in the files that record it; and each file, its last write clamped
//! to this PC's clock in the stage and checked unchanged once more, moved into place. A complete install also keeps the
//! hash of each file it put in place, at the stamp that file's handle reads after the move, in the worktree's hash
//! cache, so the next run takes it instead of reading the file again. The success line says what came back; every end
//! removes the archive and the stage this run made, and only a success that skipped nothing clears the pending marker.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use gate_state::v1;
use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES};

use super::client::{Attempt, End};
use super::git::{Git, GitError};
use super::outputs::{self, Archive, Polled};
use crate::boxside::payload::{escaped, ordinal_upper};
use crate::outputs::archive::{Entry, check_folder, unpack};
use crate::outputs::cache::{HashCache, file_time_now};
use crate::outputs::glob::Globs;
use crate::outputs::manifest::Manifest;
use crate::outputs::walk::{Stamp, in_context};

/// The name of the ASP.NET test host's file of content roots, which records the box's work folder.
const TEST_HOST_MANIFEST: &str = "MvcTestingAppManifest.json";
/// The start of the name of the coverage collector's file of source roots, which records the box's work folder.
const COVERAGE_ROOTS: &str = ".msCoverageSourceRootsMapping_";
/// The bytes a UTF-8 text file may start with to say it is UTF-8.
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
/// The largest file the install rewrites, 16 MiB: a larger one is refused before it is read.
const REWRITE_LIMIT: u64 = 16 * 1024 * 1024;
/// The bytes in a megabyte, as the success line counts them.
const MB: f64 = 1024.0 * 1024.0;
/// The seam's name for the install, to `--test-remote-interrupt-at`: the interrupt is raised before the second file's
/// move, never for an install of a single file.
const INSTALL: &str = "install";

/// What the install of a fetched archive needs: the archive, the stage folder beside it, the pending marker, the PC's
/// manifest and the run's output globs, the box's work folder, and when the fetch started.
pub(super) struct Fetched<'f> {
    pub(super) archive: &'f Archive,
    pub(super) stage: PathBuf,
    pub(super) marker: &'f Path,
    pub(super) manifest: &'f Manifest,
    pub(super) globs: &'f [String],
    pub(super) box_work: &'f str,
    pub(super) started: Instant,
}

/// What an install put into the worktree: its files, their bytes, and the archive's bytes; the paths of the files it
/// skipped, each with why; and the files it moved into place, for the hash cache it keeps ([`cached`]).
struct Installed {
    count: usize,
    bytes: u64,
    compressed: u64,
    skipped: Vec<(String, Skip)>,
    /// Each file the install moved into place, with the stamp its handle read after the move.
    moved: Vec<Moved>,
}

/// A file the install moved into place: the archive's entry for it, whose SHA-256 [`unpack`] verified against the
/// staged bytes, and the stamp the installed file's handle reads after the move; none when that read failed.
#[derive(Debug, PartialEq, Eq)]
struct Moved {
    entry: Entry,
    stamp: Option<Stamp>,
}

/// Why the install skipped a member, which then counts as not brought back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Skip {
    /// The worktree's git index holds its path, and no `--bring-tracked` glob matches it.
    Tracked,
    /// The PC's file is in use or read-only ([`writable`]).
    InUse,
}

impl Skip {
    /// The end of the line that says a member was skipped for this reason.
    fn said(self) -> &'static str {
        match self {
            Skip::Tracked => "is tracked by git",
            Skip::InUse => "is in use or read-only",
        }
    }
}

/// Why an install did not complete.
#[derive(Debug, PartialEq, Eq)]
enum Unmet {
    /// The console interrupt, once `done` of `total` files were moved; none when it came before the move.
    Interrupted { done: usize, total: usize },
    /// Why the outputs were not brought back.
    Refused(String),
}

/// Why the move of the files stopped.
#[derive(Debug, PartialEq, Eq)]
enum Unmoved {
    /// The console interrupt, once `done` files were moved.
    Interrupted { done: usize },
    /// Why a file failed, and how many were moved before it.
    Failed(String),
}

/// The end of a passing run whose outputs archive `fetched` holds was fetched whole: installed, it says the success
/// line and ends 0, clearing the pending marker unless it skipped a file git tracks or one in use or read-only, which it
/// says ([`said`]).
/// A refusal says why the outputs were not brought back and ends the run 1; a Ctrl+C ends it as an interrupt, saying how
/// many files were installed when any were ([`ending`]). Every end removes the archive and the stage folder this run
/// made, never one that was there before, saying a removal that fails; only a success that skipped nothing clears the
/// marker, and only one keeps the hashes of the files it put in place ([`cached`]), which no other end does.
/// With the end, whether the install was complete: a success that skipped nothing.
pub(super) fn installed(attempt: &mut Attempt<'_, '_>, fetched: &Fetched<'_>) -> (End, bool) {
    let staged = Cell::new(false);
    let (end, lines, complete) = match install(attempt, fetched, &staged) {
        Ok((done, started)) => {
            cached(&v1::outputs_hashes_path(attempt.call.state_dir, attempt.call.key), &done, started);
            let lines = said(&done, fetched.started.elapsed().as_secs_f64());
            (End::Status(0), lines, done.skipped.is_empty())
        }
        Err(unmet) => {
            let (end, line) = ending(unmet);
            (end, line.into_iter().collect(), false)
        }
    };
    for line in lines {
        attempt.say(line);
    }
    if staged.get()
        && let Err(error) = fs::remove_dir_all(&fetched.stage)
        && error.kind() != io::ErrorKind::NotFound
    {
        attempt.say(format!("gate: cannot remove the outputs stage {}: {error}", fetched.stage.display()));
    }
    outputs::removed(attempt, fetched.archive, outputs::FETCHED);
    if complete && let Err(why) = outputs::clear_pending(fetched.marker) {
        attempt.say(format!("gate: {why}"));
    }
    (end, complete)
}

/// The end of an install that did not complete as `unmet` says, and the line it says: none for a Ctrl+C before any file
/// was moved, which the run's own interrupt handling says.
fn ending(unmet: Unmet) -> (End, Option<String>) {
    match unmet {
        Unmet::Interrupted { done: 0, .. } => (End::Interrupted, None),
        Unmet::Interrupted { done, total } => {
            let reason = format!("{done} of {total} files were installed before the interrupt");
            (End::Interrupted, Some(outputs::not_brought_back(&reason)))
        }
        Unmet::Refused(reason) => (End::Status(1), Some(outputs::not_brought_back(&reason))),
    }
}

/// Installs the archive `fetched` holds into the attempt's worktree, as [`installed`] says, marking `staged` once the
/// unpack ended having made the stage folder ([`stage_made`]); what it installed, and the file time its moves began,
/// which the hash cache keeps their hashes against ([`cached`]).
fn install(attempt: &Attempt<'_, '_>, fetched: &Fetched<'_>, staged: &Cell<bool>) -> Result<(Installed, u64), Unmet> {
    let path = fetched.archive.path();
    let name = path
        .file_name()
        .map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
    let unopened = |error: &io::Error| Unmet::Refused(format!("the outputs archive {name} cannot be opened: {error}"));
    let file = File::open(path).map_err(|error| unopened(&error))?;
    let compressed = file.metadata().map_err(|error| unopened(&error))?.len();
    let stopped = Cell::new(false);
    let unpacking = unpacked(file, &fetched.stage, attempt.outputs_budget, &|| attempt.go_on().is_ok(), &stopped);
    staged.set(stage_made(&unpacking));
    let entries = unpacking.map_err(|error| {
        if stopped.get() {
            Unmet::Interrupted { done: 0, total: 0 }
        } else {
            unopened(&error)
        }
    })?;
    let root =
        std::path::absolute(attempt.call.root).map_err(|error| Unmet::Refused(in_context(Path::new(attempt.call.root), &error).to_string()))?;
    let globs = Globs::new(fetched.globs).map_err(Unmet::Refused)?;
    let in_use = checked(&root, &entries, &globs, fetched.manifest).map_err(Unmet::Refused)?;
    let skipped = skips(attempt, &root, &entries, &in_use)?;
    let skipped_paths: HashSet<&str> = skipped.iter().map(|(path, _)| path.as_str()).collect();
    let entries: Vec<Entry> = entries.into_iter().filter(|entry| !skipped_paths.contains(entry.path.as_str())).collect();
    let pc_root = root.display().to_string();
    rewritten(&fetched.stage, &entries, fetched.box_work, &pc_root).map_err(Unmet::Refused)?;
    let raise = attempt.call.interrupt.filter(|_| attempt.raises_at(INSTALL));
    let total = entries.len();
    // The file time the moves begin at, from which the racy rule keeps the hash of a file written long enough before.
    let started = file_time_now();
    let moved = moved(&fetched.stage, &root, &entries, fetched.manifest, |done| {
        if done == 1
            && let Some(interrupt) = raise
        {
            interrupt.raise();
        }
        attempt.go_on().is_ok()
    })
    .map_err(|unmoved| match unmoved {
        Unmoved::Interrupted { done } => Unmet::Interrupted { done, total },
        Unmoved::Failed(reason) => Unmet::Refused(reason),
    })?;
    Ok((
        Installed {
            count: total,
            bytes: entries.iter().map(|entry| entry.size).sum(),
            compressed,
            skipped,
            moved,
        },
        started,
    ))
}

/// What the install skips of `entries`, below the worktree `root`, `in_use` being the paths of those whose PC file is in
/// use or read-only: each member that counts as tracked and no `--bring-tracked` glob matches ([`tracked`]), its index
/// read with one `git ls-files -z -s` over the whole of it, and each in use ([`reasons`]). An archive with no members
/// asks git nothing. Else why the install is refused, or the interrupt ([`unmet_of`]).
fn skips(attempt: &Attempt<'_, '_>, root: &Path, entries: &[Entry], in_use: &[String]) -> Result<Vec<(String, Skip)>, Unmet> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let bring = Globs::bring_tracked(attempt.call.bring_tracked).map_err(Unmet::Refused)?;
    let git = Git::find(Path::new(attempt.call.root)).map_err(Unmet::Refused)?;
    let listing = attempt.git_bytes(&git, &["ls-files", "-z", "-s"]).map_err(unmet_of)?;
    let index = Index::of(&listing).map_err(Unmet::Refused)?;
    let in_use: HashSet<&str> = in_use.iter().map(String::as_str).collect();
    Ok(reasons(entries, &tracked(&index, root, entries, &bring), &in_use))
}

/// The end of an install whose read of the git index failed as `error`: the console interrupt is an interrupt before
/// any move; a failure, or a listing past the git runner's cap, refuses the install.
fn unmet_of(error: GitError) -> Unmet {
    match error {
        GitError::Interrupted => Unmet::Interrupted { done: 0, total: 0 },
        GitError::Failed(why) | GitError::OverCap(why) => Unmet::Refused(why),
    }
}

/// The mode `git ls-files -s` gives a gitlink, a submodule's entry in its superproject's index.
const GITLINK: &str = "160000";

/// What the worktree's git index holds, as `git ls-files -z -s` lists it, each path keyed by [`ordinal_upper`]: the
/// worktree's files match without regard to case.
#[derive(Debug, Default)]
struct Index {
    /// Each path it lists.
    paths: HashSet<String>,
    /// Each folder every member below which counts as tracked: a submodule's (a gitlink), whose files the index does not
    /// list, initialised or not; and the one holding a folder whose name is not UTF-8.
    below: HashSet<String>,
    /// Each folder every member directly in which counts as tracked: the one holding a file whose name is not UTF-8,
    /// which Git for Windows checks out under a decoded name the install cannot match exactly.
    within: HashSet<String>,
}

impl Index {
    /// The index `listing` lists, each record `<mode> <object> <stage>\t<path>` ended by a NUL; else why it cannot be
    /// read.
    fn of(listing: &[u8]) -> Result<Index, String> {
        let mut index = Index::default();
        for record in listing.split(|byte| *byte == 0).filter(|record| !record.is_empty()) {
            let Some((mode, path)) = record_parts(record) else {
                return Err(format!(
                    "git ls-files -z -s wrote a record that is not '<mode> <object> <stage>\\t<path>': {}",
                    escaped(&String::from_utf8_lossy(record))
                ));
            };
            match std::str::from_utf8(path) {
                Ok(path) if mode == GITLINK => {
                    index.below.insert(ordinal_upper(path));
                    index.paths.insert(ordinal_upper(path));
                }
                Ok(path) => {
                    index.paths.insert(ordinal_upper(path));
                }
                Err(_) => index.undecodable(path),
            }
        }
        Ok(index)
    }

    /// Records the listed `path`, which is not UTF-8, by the folder its first part that is not holds: the members
    /// directly in that folder count as tracked when the part is the file's name, every member below it when the part
    /// is a folder's.
    fn undecodable(&mut self, path: &[u8]) {
        let parts: Vec<&str> = path
            .split(|byte| *byte == b'/')
            .map_while(|part| std::str::from_utf8(part).ok())
            .collect();
        let folder = ordinal_upper(&parts.join("/"));
        // A `/` is never inside a UTF-8 sequence, so the parts that decode end at a part of its own that does not.
        let named_file = path.split(|byte| *byte == b'/').count() == parts.len() + 1;
        if named_file {
            self.within.insert(folder);
        } else {
            self.below.insert(folder);
        }
    }

    /// Whether the member at `path` counts as tracked: the index lists it, or its folder or one above it is one whose
    /// members all count ([`Index::below`], [`Index::within`]).
    fn holds(&self, path: &str) -> bool {
        let key = ordinal_upper(path);
        let parent = key.rfind('/').map_or("", |end| &key[..end]);
        self.paths.contains(&key)
            || self.within.contains(parent)
            || std::iter::once("")
                .chain(key.match_indices('/').map(|(end, _)| &key[..end]))
                .any(|folder| self.below.contains(folder))
    }
}

/// The mode and the path of a `git ls-files -z -s` record, `<mode> <object> <stage>\t<path>`; `None` for one of
/// another shape.
fn record_parts(record: &[u8]) -> Option<(&str, &[u8])> {
    let tab = record.iter().position(|byte| *byte == b'\t')?;
    let fields: Vec<&str> = std::str::from_utf8(&record[..tab]).ok()?.split(' ').collect();
    match fields.as_slice() {
        [mode, _object, _stage] => Some((mode, &record[tab + 1..])),
        _ => None,
    }
}

/// The paths of the `entries` below the worktree `root` that count as tracked, but for those a `bring` glob matches:
/// each `index` holds ([`Index::holds`]), and each below a folder holding a repository of its own ([`nested`]).
fn tracked<'e>(index: &Index, root: &Path, entries: &'e [Entry], bring: &Globs) -> HashSet<&'e str> {
    let mut seen = HashMap::new();
    entries
        .iter()
        .filter(|entry| !bring.matches(&entry.path) && (index.holds(&entry.path) || nested(root, &entry.path, &mut seen)))
        .map(|entry| entry.path.as_str())
        .collect()
}

/// Whether a folder of the member at the `/`-separated `path` below the worktree `root`, the root not counted, holds a
/// `.git` entry of its own: a repository the worktree's does not track, whose files no index the install reads lists.
/// `seen` keeps each folder's answer.
fn nested(root: &Path, path: &str, seen: &mut HashMap<String, bool>) -> bool {
    path.match_indices('/').any(|(end, _)| {
        let folder = &path[..end];
        *seen.entry(ordinal_upper(folder)).or_insert_with(|| holds_git(&below(root, folder)))
    })
}

/// Whether `folder` holds a `.git` file or folder. A missing folder, or a file in its place, holds none; one that
/// cannot be read counts as holding one, so a tracked file is never overwritten for want of a look.
fn holds_git(folder: &Path) -> bool {
    match fs::symlink_metadata(folder.join(".git")) {
        Ok(_) => true,
        Err(error) => !matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory),
    }
}

/// Each of `entries` the install skips and why, in order: one in `tracked` as tracked, whether or not it is also
/// `in_use`, then one in `in_use` as in use.
fn reasons(entries: &[Entry], tracked: &HashSet<&str>, in_use: &HashSet<&str>) -> Vec<(String, Skip)> {
    entries
        .iter()
        .filter_map(|entry| {
            let skip = if tracked.contains(entry.path.as_str()) {
                Skip::Tracked
            } else if in_use.contains(entry.path.as_str()) {
                Skip::InUse
            } else {
                return None;
            };
            Some((entry.path.clone(), skip))
        })
        .collect()
}

/// The lines an install that put `installed` into the worktree says, `seconds` after its fetch started: one for each
/// file it skipped, saying why, then the success line.
fn said(installed: &Installed, seconds: f64) -> Vec<String> {
    let mut lines: Vec<String> = installed
        .skipped
        .iter()
        .map(|(path, skip)| {
            format!(
                "gate: remote outputs not fully brought back: {}",
                escaped(&format!("{path} {}", skip.said()))
            )
        })
        .collect();
    lines.push(success_line(installed, seconds));
    lines
}

/// The line an install that put `installed` into the worktree says, `seconds` after its fetch started.
#[expect(clippy::cast_precision_loss, reason = "a size said to one decimal of a megabyte")]
fn success_line(installed: &Installed, seconds: f64) -> String {
    let bytes = one_decimal(installed.bytes as f64 / MB);
    let compressed = one_decimal(installed.compressed as f64 / MB);
    let seconds = one_decimal(seconds);
    format!(
        "gate: outputs: {} file(s), {bytes} MB ({compressed} MB compressed) in {seconds} s",
        installed.count
    )
}

/// `value` to one decimal, a half rounded away from zero as .NET's `{0:0.0}` rounds it; Rust's `{:.1}` rounds a half
/// to even.
fn one_decimal(value: f64) -> String {
    format!("{:.1}", (value * 10.0).round() / 10.0)
}

/// Whether an unpack into the stage folder that ended as `unpacking` made the folder, which is then this run's to
/// remove: not when the folder was there before, which the unpack refuses and leaves as it is.
fn stage_made(unpacking: &io::Result<Vec<Entry>>) -> bool {
    !matches!(unpacking, Err(error) if error.kind() == io::ErrorKind::AlreadyExists)
}

/// Unpacks the archive `source` into the folder `stage`, which must not exist yet, asking `go_on` before each read
/// and marking `stopped` once it says no, to at most `limit` bytes; the entries, else why not.
fn unpacked(source: impl Read, stage: &Path, limit: u64, go_on: &dyn Fn() -> bool, stopped: &Cell<bool>) -> io::Result<Vec<Entry>> {
    unpack(
        Polled {
            inner: source,
            go_on,
            stopped,
        },
        stage,
        limit,
    )
}

/// The full path of the `/`-separated `path` below `root`.
fn below(root: &Path, path: &str) -> PathBuf {
    path.split('/').fold(root.to_path_buf(), |full, part| full.join(part))
}

/// Whether each of `entries`, to be installed below the worktree `root`, may be: every one under `globs`, then every
/// one unchanged since `manifest`; else the first refusal. The paths, in order, of those whose PC file is not
/// replaceable ([`writable`]), which the install skips.
fn checked(root: &Path, entries: &[Entry], globs: &Globs, manifest: &Manifest) -> Result<Vec<String>, String> {
    if let Some(outside) = entries.iter().find(|entry| !globs.matches(&entry.path)) {
        return Err(format!("the archive member {} is not under the output globs", outside.path));
    }
    for entry in entries {
        unchanged(root, entry, manifest)?;
    }
    Ok(entries
        .iter()
        .filter(|entry| !writable(root, entry))
        .map(|entry| entry.path.clone())
        .collect())
}

/// Whether the PC's file of `entry` below `root` is as `manifest` holds it: one it holds still has its size and last
/// write, one it lacks is still missing; else the refusal, which names a folder in the file's place as not a file.
fn unchanged(root: &Path, entry: &Entry, manifest: &Manifest) -> Result<(), String> {
    let target = below(root, &entry.path);
    let now = match fs::metadata(&target) {
        Ok(metadata) if !metadata.is_file() => return Err(format!("{} is not a file", entry.path)),
        Ok(metadata) => Some(Stamp::of(&metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(in_context(&target, &error).to_string()),
    };
    match (manifest.get(&entry.path), now) {
        (Some(held), Some(stamp)) if held.stamp == stamp => Ok(()),
        (None, None) => Ok(()),
        _ => Err(format!("{} changed during the run", entry.path)),
    }
}

/// Whether the PC's file of `entry` below `root`, when there is one, opens for reading and writing with no sharing,
/// so a move can replace it.
fn writable(root: &Path, entry: &Entry) -> bool {
    let target = below(root, &entry.path);
    // Opened and closed at once: only whether it opens counts, and the file is then said as in use or read-only.
    !target.is_file() || OpenOptions::new().read(true).write(true).share_mode(0).open(&target).is_ok()
}

/// Rewrites `box_work` to `pc_root` in each of the staged `entries` below `stage` that records it ([`records_a_root`]),
/// as [`rewrite_file`] does; else why one cannot be.
fn rewritten(stage: &Path, entries: &[Entry], box_work: &str, pc_root: &str) -> Result<(), String> {
    let from = box_work.trim_end_matches('\\');
    let to = pc_root.trim_end_matches('\\');
    for entry in entries.iter().filter(|entry| records_a_root(&entry.path)) {
        rewrite_file(&below(stage, &entry.path), &entry.path, from, to)?;
    }
    Ok(())
}

/// Whether the file at the `/`-separated `path` records the folder it was built in: its name, in any case, is the test
/// host's [`TEST_HOST_MANIFEST`] or starts with the coverage collector's [`COVERAGE_ROOTS`].
fn records_a_root(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.eq_ignore_ascii_case(TEST_HOST_MANIFEST)
        || name
            .get(..COVERAGE_ROOTS.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(COVERAGE_ROOTS))
}

/// Rewrites `from` to `to` in the UTF-8 text file at `path`, shown as `shown`: its JSON-escaped form (each `\` doubled)
/// first, then its raw form, each without regard to case as .NET's `OrdinalIgnoreCase` compares. A file whose text
/// changes is written back with the byte order mark it had, and its last write kept. Else the refusal: a file larger
/// than [`REWRITE_LIMIT`], refused before it is read, one that is not UTF-8 after its byte order mark, or why it cannot
/// be read or written.
fn rewrite_file(path: &Path, shown: &str, from: &str, to: &str) -> Result<(), String> {
    let cannot = |error: io::Error| format!("cannot rewrite {shown}: {error}");
    let mut file = File::open(path).map_err(cannot)?;
    let metadata = file.metadata().map_err(cannot)?;
    if metadata.len() > REWRITE_LIMIT {
        return Err(format!("{shown} is too large to rewrite ({} bytes, over 16 MiB)", metadata.len()));
    }
    let modified = metadata.modified().map_err(cannot)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(cannot)?;
    drop(file);
    let (bom, body) = match bytes.strip_prefix(UTF8_BOM) {
        Some(body) => (UTF8_BOM, body),
        None => (&[][..], &bytes[..]),
    };
    // Which byte is not UTF-8 is no use to the reader; that the file is not is the one reason said.
    let text = std::str::from_utf8(body).map_err(|_| format!("cannot rewrite {shown}: it is not UTF-8"))?;
    let escaped = replaced(text, &from.replace('\\', r"\\"), &to.replace('\\', r"\\"));
    let new = replaced(&escaped, from, to);
    if new == text {
        return Ok(());
    }
    fs::write(path, [bom, new.as_bytes()].concat()).map_err(cannot)?;
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(modified))
        .map_err(cannot)
}

/// `text` with each run of characters equal to `from`, without regard to case, replaced by `to`.
fn replaced(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_owned();
    }
    // `ordinal_upper` maps each character to one, so the characters of `text` and of its uppercase line up.
    let wanted: Vec<char> = ordinal_upper(from).chars().collect();
    let upper: Vec<char> = ordinal_upper(text).chars().collect();
    let characters: Vec<char> = text.chars().collect();
    let mut new = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(&character) = characters.get(index) {
        if upper[index..].starts_with(&wanted) {
            new.push_str(to);
            index += wanted.len();
        } else {
            new.push(character);
            index += 1;
        }
    }
    new
}

/// Moves each of `entries` from `stage` into place below `root`, as [`move_one`] does against `manifest`, asking
/// `go_on`, given the files moved so far, before each one; else the interrupt once `go_on` says no, or why a file
/// failed, the files moved before it counted and left in place. Each file moved is returned with the stamp its handle
/// read after the move, none when that read failed.
fn moved(stage: &Path, root: &Path, entries: &[Entry], manifest: &Manifest, mut go_on: impl FnMut(usize) -> bool) -> Result<Vec<Moved>, Unmoved> {
    let now = SystemTime::now();
    let total = entries.len();
    let mut moved = Vec::with_capacity(total);
    for (done, entry) in entries.iter().enumerate() {
        if !go_on(done) {
            return Err(Unmoved::Interrupted { done });
        }
        let stamp = move_one(stage, root, entry, manifest, now)
            .map_err(|error| Unmoved::Failed(format!("{done} of {total} files were installed before {} failed: {error}", entry.path)))?;
        moved.push(Moved { entry: entry.clone(), stamp });
    }
    Ok(moved)
}

/// Moves the staged file of `entry` below `stage` to the same path below `root`, replacing a file there, each folder
/// between `root` and it made when missing and refused when it is a link, a junction or not a folder. Its last write,
/// when past `now`, is set to `now` in the stage, before the move; the PC's file is then checked unchanged since
/// `manifest` once more ([`unchanged`]), just before the move. The stamp read after the move through a handle opened on
/// the staged file before it, the clamp included, is returned: the moved file's own, never that of a file put at its
/// path after the move. None when the handle cannot be opened or read, which costs the file its hash cache entry only.
fn move_one(stage: &Path, root: &Path, entry: &Entry, manifest: &Manifest, now: SystemTime) -> io::Result<Option<Stamp>> {
    let path = entry.path.as_str();
    let source = below(stage, path);
    let (parents, _) = path.rsplit_once('/').unwrap_or(("", path));
    let mut folder = root.to_path_buf();
    for part in parents.split('/').filter(|part| !part.is_empty()) {
        folder.push(part);
        match fs::create_dir(&folder) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(in_context(&folder, &error)),
            _ => check_folder(&folder)?,
        }
    }
    let written = fs::metadata(&source)?.modified()?;
    if written > now {
        // Its attributes alone: a handle open on the staged file, sharing its deletion, then neither refuses nor is moved.
        OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).open(&source)?.set_modified(now)?;
    }
    unchanged(root, entry, manifest).map_err(io::Error::other)?;
    // Its attributes alone, every sharing given: the handle neither refuses the move nor any other opener.
    let handle = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&source);
    fs::rename(&source, below(root, path))?;
    // No stamp is the right answer for a handle that cannot be opened or read: the file is installed, its hash not kept.
    Ok(handle.and_then(|file| file.metadata()).ok().map(|metadata| Stamp::of(&metadata)))
}

/// Writes the hash cache at `path` for the files an install put in place: an entry for each of `installed.moved`, at
/// the stamp its handle read after the move and the hash [`unpack`] verified, kept against `started`, the file time the
/// moves began, so the next run reads no file whose size and last write still match. Nothing is written when the install
/// skipped a file ([`Skip`]): only an archive installed whole is one whose files the run after it may take as the PC's,
/// a partial one costing the next run a rehash. A file the install rewrote ([`records_a_root`]) is left out, its bytes
/// no longer the hash's, and so is one whose stamp could not be read after its move. The cache the file at `path` holds
/// is read first, so entries another run saved meanwhile are kept; one missing or corrupt starts empty. The cache is
/// saved only when it kept at least one entry.
fn cached(path: &Path, installed: &Installed, started: u64) {
    if !installed.skipped.is_empty() {
        return;
    }
    let mut cache = HashCache::load(path).unwrap_or_default();
    let mut kept = false;
    for moved in &installed.moved {
        let Some(stamp) = moved.stamp else { continue };
        if records_a_root(&moved.entry.path) {
            continue;
        }
        let (file, sha256) = (moved.entry.path.as_str(), moved.entry.sha256.as_str());
        cache.keep(file, stamp, sha256, started);
        kept |= cache.held(file).is_some_and(|held| held.stamp == stamp && held.sha256 == sha256);
    }
    if kept {
        cache.save(path);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::fs::{self, File};
    use std::io::{self, Read, Write};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;

    use super::{
        Index, Installed, Moved, Skip, Unmet, Unmoved, cached, checked, ending, moved, reasons, rewritten, said, stage_made, success_line, tracked,
        unmet_of, unpacked,
    };
    use crate::boxside::outputs::OUTPUTS_BUDGET;
    use crate::outputs::archive::{Entry, pack};
    use crate::outputs::cache::{HashCache, file_time_now};
    use crate::outputs::glob::Globs;
    use crate::outputs::manifest::{Held, Manifest};
    use crate::outputs::walk::{Stamp, walk};
    use crate::remote::client::End;
    use crate::remote::git::GitError;

    const BOX_WORK: &str = r"D:\gate-remote\work\app-1a2b3c4d";
    const PC_ROOT: &str = r"C:\src\app";

    /// A scratch folder for one test, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-install-{}-{name}", std::process::id()));
        // A folder left by an earlier run of the test may not be there.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
    }

    /// Writes `text` to `path` below `root`, its folders made.
    fn put(root: &Path, path: &str, text: &[u8]) -> PathBuf {
        let full = super::below(root, path);
        fs::create_dir_all(full.parent().expect("a file has a folder")).expect("the folder can be made");
        fs::write(&full, text).expect("the file can be written");
        full
    }

    /// An entry of the archive for `path`, its size and hash not checked here.
    fn entry(path: &str) -> Entry {
        Entry {
            path: path.to_owned(),
            size: 1,
            sha256: "0".repeat(64),
        }
    }

    /// The stamp of the file at `full`.
    fn stamp(full: &Path) -> Stamp {
        let metadata = fs::metadata(full).expect("the file is there");
        Stamp {
            size: metadata.file_size(),
            modified: metadata.last_write_time(),
        }
    }

    /// A manifest holding each of `paths` below `root` with its stamp now.
    fn manifest_of(root: &Path, paths: &[&str]) -> Manifest {
        let mut manifest = Manifest::default();
        for path in paths {
            let held = Held {
                stamp: stamp(&super::below(root, path)),
                sha256: "0".repeat(64),
            };
            manifest.insert((*path).to_owned(), held);
        }
        manifest
    }

    fn bin_globs() -> Globs {
        Globs::new(&["**/bin/**".to_owned()]).expect("the globs can be matched")
    }

    #[test]
    fn a_member_outside_the_globs_or_under_an_excluded_folder_is_refused() {
        let root = scratch("globs");
        let manifest = Manifest::default();
        for path in ["obj/a.dll", "bin/.tmp/a.dll", ".git/bin/a", "src/bin/TestResults/a.trx"] {
            let refused = checked(&root, &[entry("bin/ok.dll"), entry(path)], &bin_globs(), &manifest);
            assert_eq!(refused, Err(format!("the archive member {path} is not under the output globs")), "{path}");
        }
        assert_eq!(checked(&root, &[entry("src/bin/a.dll")], &bin_globs(), &manifest), Ok(Vec::new()));
    }

    #[test]
    fn a_pc_file_changed_since_the_manifest_is_refused() {
        let root = scratch("unchanged");
        put(&root, "bin/size.dll", b"abc");
        let written = put(&root, "bin/time.dll", b"abc");
        put(&root, "bin/same.dll", b"abc");
        let manifest = manifest_of(&root, &["bin/size.dll", "bin/time.dll", "bin/same.dll"]);
        put(&root, "bin/size.dll", b"abcd");
        File::options()
            .write(true)
            .open(&written)
            .and_then(|file| file.set_modified(SystemTime::now() - Duration::from_hours(1)))
            .expect("the last write can be set");
        put(&root, "bin/new.dll", b"made during the run");

        for path in ["bin/size.dll", "bin/time.dll", "bin/new.dll"] {
            let refused = checked(&root, &[entry(path)], &bin_globs(), &manifest);
            assert_eq!(refused, Err(format!("{path} changed during the run")), "{path}");
        }
        fs::create_dir_all(root.join("bin").join("folder.dll")).expect("a folder in a file's place");
        let folder = checked(&root, &[entry("bin/folder.dll")], &bin_globs(), &manifest);
        assert_eq!(folder, Err("bin/folder.dll is not a file".to_owned()));
        assert_eq!(
            checked(&root, &[entry("bin/same.dll"), entry("bin/absent.dll")], &bin_globs(), &manifest),
            Ok(Vec::new())
        );
    }

    #[test]
    fn a_pc_file_held_open_sharing_nothing_is_skipped() {
        let root = scratch("writable");
        let full = put(&root, "bin/held.dll", b"abc");
        put(&root, "bin/free.dll", b"abc");
        let manifest = manifest_of(&root, &["bin/held.dll", "bin/free.dll"]);
        let held = fs::OpenOptions::new().read(true).share_mode(0).open(&full).expect("the file can be held");
        let entries = [entry("bin/free.dll"), entry("bin/held.dll"), entry("bin/new.dll")];

        let skipped = checked(&root, &entries, &bin_globs(), &manifest);
        drop(held);

        assert_eq!(skipped, Ok(vec!["bin/held.dll".to_owned()]), "only the held file is skipped");
        assert_eq!(checked(&root, &entries, &bin_globs(), &manifest), Ok(Vec::new()), "released");
    }

    #[test]
    fn the_box_work_folder_is_rewritten_in_the_files_that_record_it() {
        let stage = scratch("rewrite");
        let escaped_box = BOX_WORK.replace('\\', r"\\");
        let json = format!(r#"{{"App":"{}\\src\\App","Other":"{}\\x"}}"#, escaped_box.to_uppercase(), escaped_box);
        let mut bom_json = b"\xEF\xBB\xBF".to_vec();
        bom_json.extend_from_slice(json.as_bytes());
        let raw = format!("{}\\src;{BOX_WORK}\\tests\n", BOX_WORK.to_lowercase());
        let manifest = put(&stage, "bin/MvcTestingAppManifest.json", &bom_json);
        let coverage = put(&stage, "bin/.msCoverageSourceRootsMapping_App", raw.as_bytes());
        let untouched = put(&stage, "bin/app.deps.json", raw.as_bytes());
        let old = SystemTime::now() - Duration::from_hours(2);
        for full in [&manifest, &coverage] {
            File::options()
                .write(true)
                .open(full)
                .and_then(|file| file.set_modified(old))
                .expect("the last write can be set");
        }
        let entries = [
            entry("bin/MvcTestingAppManifest.json"),
            entry("bin/.msCoverageSourceRootsMapping_App"),
            entry("bin/app.deps.json"),
        ];

        assert_eq!(rewritten(&stage, &entries, BOX_WORK, PC_ROOT), Ok(()));

        let pc_escaped = PC_ROOT.replace('\\', r"\\");
        let mut expected = b"\xEF\xBB\xBF".to_vec();
        expected.extend_from_slice(format!(r#"{{"App":"{pc_escaped}\\src\\App","Other":"{pc_escaped}\\x"}}"#).as_bytes());
        assert_eq!(
            fs::read(&manifest).expect("the file is there"),
            expected,
            "escaped, any case, its BOM kept"
        );
        let coverage_text = fs::read_to_string(&coverage).expect("the file is there");
        assert_eq!(coverage_text, format!("{PC_ROOT}\\src;{PC_ROOT}\\tests\n"), "raw, any case");
        for full in [&manifest, &coverage] {
            let modified = fs::metadata(full).and_then(|metadata| metadata.modified()).expect("a last write");
            assert_eq!(modified, old, "{} keeps its last write", full.display());
        }
        assert_eq!(
            fs::read_to_string(&untouched).expect("the file is there"),
            raw,
            "another file is left alone"
        );
    }

    #[test]
    fn a_file_to_rewrite_over_16_mib_is_refused_and_one_of_16_mib_rewritten() {
        let stage = scratch("rewrite-large");
        let full = put(&stage, "bin/MvcTestingAppManifest.json", b"");
        let entries = [entry("bin/MvcTestingAppManifest.json")];
        let sized = |length: u64| {
            File::options()
                .write(true)
                .open(&full)
                .and_then(|file| file.set_len(length))
                .expect("the file can be sized");
        };

        sized(16 * 1024 * 1024 + 1);
        assert_eq!(
            rewritten(&stage, &entries, BOX_WORK, PC_ROOT),
            Err("bin/MvcTestingAppManifest.json is too large to rewrite (16777217 bytes, over 16 MiB)".to_owned())
        );
        sized(16 * 1024 * 1024);
        assert_eq!(rewritten(&stage, &entries, BOX_WORK, PC_ROOT), Ok(()), "16 MiB is not over");
    }

    #[test]
    fn a_file_to_rewrite_that_is_not_utf8_is_refused() {
        let stage = scratch("rewrite-not-utf8");
        put(&stage, "bin/.msCoverageSourceRootsMapping_App", b"\xEF\xBB\xBF\xFF\xFE");

        assert_eq!(
            rewritten(&stage, &[entry("bin/.msCoverageSourceRootsMapping_App")], BOX_WORK, PC_ROOT),
            Err("cannot rewrite bin/.msCoverageSourceRootsMapping_App: it is not UTF-8".to_owned())
        );
    }

    #[test]
    fn the_success_line_rounds_as_powershell_does() {
        let installed = Installed {
            count: 3,
            bytes: 3_145_728,
            compressed: 1_048_576,
            skipped: vec![("bin/held.dll".to_owned(), Skip::InUse), ("bin/kept.dll".to_owned(), Skip::Tracked)],
            moved: Vec::new(),
        };
        let success = "gate: outputs: 3 file(s), 3.0 MB (1.0 MB compressed) in 2.3 s";
        assert_eq!(success_line(&installed, 2.25), success, "half a tenth rounds away from zero");
        let in_use = "gate: remote outputs not fully brought back: bin/held.dll is in use or read-only";
        let tracked = "gate: remote outputs not fully brought back: bin/kept.dll is tracked by git";
        assert_eq!(
            said(&installed, 2.25),
            [in_use, tracked, success],
            "each skipped file and why, then the success line"
        );
    }

    /// The globs of no `--bring-tracked`.
    fn none_brought() -> Globs {
        Globs::bring_tracked(&[]).expect("no globs can be matched")
    }

    /// A `git ls-files -z -s` listing of `records`, each a mode and a path.
    fn listed(records: &[(&str, &[u8])]) -> Vec<u8> {
        let mut listing = Vec::new();
        for (mode, path) in records {
            listing.extend_from_slice(format!("{mode} {} 0\t", "1".repeat(40)).as_bytes());
            listing.extend_from_slice(path);
            listing.push(0);
        }
        listing
    }

    /// A file's listing of each of `paths`.
    fn files(paths: &[&[u8]]) -> Vec<u8> {
        listed(&paths.iter().map(|path| ("100644", *path)).collect::<Vec<_>>())
    }

    /// The members among `entries` the index `listing` makes tracked, below a worktree `root` and with `bring` the
    /// `--bring-tracked` globs.
    fn tracked_of<'e>(listing: &[u8], root: &Path, entries: &'e [Entry], bring: &Globs) -> HashSet<&'e str> {
        tracked(&Index::of(listing).expect("the listing can be read"), root, entries, bring)
    }

    fn set<'p>(paths: &[&'p str]) -> HashSet<&'p str> {
        paths.iter().copied().collect()
    }

    #[test]
    fn a_member_the_index_lists_is_tracked_whatever_its_case() {
        let root = scratch("tracked-case");
        let entries = [entry("bin/a.dll"), entry("BIN/B.dll"), entry("bin/c.dll")];
        let listing = files(&[b"src/main.rs", b"bin/a.dll", b"bin/b.DLL"]);

        assert_eq!(tracked_of(&listing, &root, &entries, &none_brought()), set(&["bin/a.dll", "BIN/B.dll"]));
        assert_eq!(tracked_of(b"", &root, &entries, &none_brought()), set(&[]), "an empty index");
    }

    #[test]
    fn a_member_under_a_submodule_is_tracked() {
        let root = scratch("tracked-gitlink");
        let entries = [
            entry("lib/sub/bin/a.dll"),
            entry("LIB/SUB/x.dll"),
            entry("lib/sub"),
            entry("lib/subway/a.dll"),
            entry("lib/a.dll"),
        ];
        let listing = listed(&[("160000", b"lib/sub"), ("100644", b"src/main.rs")]);

        assert_eq!(
            tracked_of(&listing, &root, &entries, &none_brought()),
            set(&["lib/sub/bin/a.dll", "LIB/SUB/x.dll", "lib/sub"])
        );
    }

    #[test]
    fn a_member_below_a_folder_holding_its_own_git_is_tracked() {
        let root = scratch("tracked-nested");
        put(&root, ".git/HEAD", b"ref: refs/heads/main");
        put(&root, "vendor/lib/.git", b"gitdir: elsewhere");
        put(&root, "tools/.git/HEAD", b"ref: refs/heads/main");
        let entries = [
            entry("vendor/lib/bin/a.dll"),
            entry("vendor/bin/a.dll"),
            entry("Tools/bin/x.dll"),
            entry("bin/a.dll"),
            entry("missing/bin/a.dll"),
        ];

        assert_eq!(
            tracked_of(b"", &root, &entries, &none_brought()),
            set(&["vendor/lib/bin/a.dll", "Tools/bin/x.dll"]),
            "the worktree's own .git does not count"
        );
    }

    #[test]
    fn an_index_path_that_is_not_utf8_makes_its_folder_tracked() {
        let root = scratch("tracked-not-utf8");
        let entries = [
            entry("bin/\u{FFFD}.dll"),
            entry("BIN/a.dll"),
            entry("bin/sub/a.dll"),
            entry("obj/a.dll"),
            entry("obj/sub/a.dll"),
            entry("a.dll"),
            entry("lib/a.dll"),
        ];

        let file_name = files(&[b"bin/\xFF.dll"]);
        assert_eq!(
            tracked_of(&file_name, &root, &entries, &none_brought()),
            set(&["bin/\u{FFFD}.dll", "BIN/a.dll"]),
            "the members in its folder, none below"
        );
        let folder_name = files(&[b"obj/\xFF/a.dll"]);
        assert_eq!(
            tracked_of(&folder_name, &root, &entries, &none_brought()),
            set(&["obj/a.dll", "obj/sub/a.dll"]),
            "a folder name that is not UTF-8: every member below the folder holding it"
        );
        let at_the_root = files(&[b"\xFF.txt"]);
        assert_eq!(tracked_of(&at_the_root, &root, &entries, &none_brought()), set(&["a.dll"]));
    }

    #[test]
    fn a_listing_record_git_does_not_write_refuses_the_install() {
        assert_eq!(
            Index::of(b"100644 1111 0\tbin/a.dll\0bin/b.dll\0").err(),
            Some("git ls-files -z -s wrote a record that is not '<mode> <object> <stage>\\t<path>': bin/b.dll".to_owned())
        );
    }

    #[test]
    fn a_bring_tracked_glob_installs_the_members_it_matches_and_no_excluded_one() {
        let root = scratch("tracked-bring");
        let listing = files(&[b"bin/a.dll", b"obj/a.dll", b"bin/TestResults/a.trx"]);
        let entries = [entry("bin/a.dll"), entry("obj/a.dll"), entry("bin/TestResults/a.trx")];
        let bin = Globs::bring_tracked(&["BIN/**".to_owned()]).expect("the glob can be matched");
        assert_eq!(
            tracked_of(&listing, &root, &entries, &bin),
            set(&["obj/a.dll", "bin/TestResults/a.trx"]),
            "matched in any case"
        );

        let everything = Globs::bring_tracked(&["**".to_owned()]).expect("the glob can be matched");
        assert_eq!(
            tracked_of(&listing, &root, &entries, &everything),
            set(&["bin/TestResults/a.trx"]),
            "an excluded path is never brought"
        );
    }

    #[test]
    fn a_member_tracked_and_in_use_is_said_once_as_tracked() {
        let entries = [entry("bin/a.dll"), entry("bin/b.dll"), entry("bin/c.dll"), entry("bin/d.dll")];
        let tracked = set(&["bin/c.dll", "bin/b.dll"]);
        let in_use = set(&["bin/b.dll", "bin/d.dll"]);

        assert_eq!(
            reasons(&entries, &tracked, &in_use),
            [
                ("bin/b.dll".to_owned(), Skip::Tracked),
                ("bin/c.dll".to_owned(), Skip::Tracked),
                ("bin/d.dll".to_owned(), Skip::InUse)
            ]
        );
    }

    #[test]
    fn an_index_that_cannot_be_read_whole_refuses_the_install_and_an_interrupt_ends_it() {
        for error in [
            GitError::Failed("git ls-files -z failed in C:\\src\\app (status 128): fatal: bad index".to_owned()),
            GitError::OverCap("git ls-files -z in C:\\src\\app wrote more than 16 MiB".to_owned()),
        ] {
            let why = String::from(match &error {
                GitError::Failed(why) | GitError::OverCap(why) => why.as_str(),
                GitError::Interrupted => "",
            });
            let (end, line) = ending(unmet_of(error));
            assert!(matches!(end, End::Status(1)), "{end:?}");
            assert_eq!(line, Some(format!("gate: remote outputs not brought back: {why}")));
        }
        assert_eq!(unmet_of(GitError::Interrupted), Unmet::Interrupted { done: 0, total: 0 });
        let (end, line) = ending(unmet_of(GitError::Interrupted));
        assert!(matches!(end, End::Interrupted) && line.is_none(), "{end:?} {line:?}");
    }

    /// The reason of a move that failed; else what stopped it, which no failure's reason starts with.
    fn failure(unmoved: Unmoved) -> String {
        match unmoved {
            Unmoved::Failed(reason) => reason,
            other @ Unmoved::Interrupted { .. } => format!("{other:?}"),
        }
    }

    /// The paths of the files below `root`, `/`-separated and sorted.
    fn files_below(root: &Path) -> Vec<String> {
        let found = walk(root, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the folder can be walked");
        found.into_iter().map(|file| file.path).collect()
    }

    #[test]
    fn a_move_that_fails_says_how_many_were_installed_and_keeps_them() {
        let folder = scratch("move-fails");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        for path in ["bin/a.txt", "bin/b.txt", "bin/c.txt"] {
            put(&stage, path, b"box");
        }
        fs::create_dir_all(root.join("bin").join("b.txt")).expect("a folder in the file's place");
        let entries = [entry("bin/a.txt"), entry("bin/b.txt"), entry("bin/c.txt")];

        let unmoved = moved(&stage, &root, &entries, &Manifest::default(), |_| true).expect_err("b cannot be moved over a folder");

        let reason = failure(unmoved);
        assert!(reason.starts_with("1 of 3 files were installed before bin/b.txt failed: "), "{reason}");
        assert_eq!(fs::read_to_string(root.join("bin").join("a.txt")).expect("a stays"), "box");
        assert!(!root.join("bin").join("c.txt").exists(), "nothing after the failure is moved");
    }

    #[test]
    fn a_move_stops_between_files_once_the_interrupt_is_raised() {
        let folder = scratch("move-interrupted");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        put(&stage, "bin/a.txt", b"a");
        put(&stage, "bin/b.txt", b"b");
        fs::create_dir_all(&root).expect("the root can be made");
        let entries = [entry("bin/a.txt"), entry("bin/b.txt")];

        let unmoved = moved(&stage, &root, &entries, &Manifest::default(), |done| done < 1);

        assert_eq!(unmoved, Err(Unmoved::Interrupted { done: 1 }));
        assert_eq!(files_below(&root), ["bin/a.txt"]);
    }

    /// Makes `link` a junction to the folder `target`.
    #[expect(clippy::disallowed_methods, reason = "a test's one mklink takes no spawn lock")]
    fn junction(link: &Path, target: &Path) {
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd.exe runs");
        assert!(made.status.success(), "a junction needs no privilege: {made:?}");
    }

    #[test]
    fn a_move_through_a_junction_is_refused() {
        let folder = scratch("move-junction");
        let (stage, root, outside) = (folder.join("stage"), folder.join("root"), folder.join("outside"));
        put(&stage, "bin/a.txt", b"a");
        fs::create_dir_all(&root).expect("the root can be made");
        fs::create_dir_all(&outside).expect("the outside folder can be made");
        junction(&root.join("bin"), &outside);

        let unmoved = moved(&stage, &root, &[entry("bin/a.txt")], &Manifest::default(), |_| true).expect_err("the junction is refused");

        let reason = failure(unmoved);
        assert!(reason.starts_with("0 of 1 files were installed before bin/a.txt failed: "), "{reason}");
        assert!(reason.ends_with("is a link or a junction, not a folder"), "{reason}");
        assert_eq!(
            fs::read_dir(&outside).expect("the outside folder").count(),
            0,
            "nothing is moved through it"
        );
    }

    #[test]
    fn a_last_write_past_this_clock_is_clamped_and_one_before_it_kept() {
        let folder = scratch("move-clamp");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        let ahead = put(&stage, "bin/ahead.txt", b"a");
        let behind = put(&stage, "bin/behind.txt", b"b");
        fs::create_dir_all(&root).expect("the root can be made");
        let past = SystemTime::now() - Duration::from_hours(3);
        for (full, time) in [(&ahead, SystemTime::now() + Duration::from_hours(3)), (&behind, past)] {
            File::options()
                .write(true)
                .open(full)
                .and_then(|file| file.set_modified(time))
                .expect("the last write can be set");
        }

        let entries = [entry("bin/ahead.txt"), entry("bin/behind.txt")];
        assert!(moved(&stage, &root, &entries, &Manifest::default(), |_| true).is_ok());

        let modified = |name: &str| {
            fs::metadata(root.join("bin").join(name))
                .and_then(|metadata| metadata.modified())
                .expect("a last write")
        };
        assert!(modified("ahead.txt") <= SystemTime::now(), "clamped to this clock");
        assert_eq!(modified("behind.txt"), past, "kept");
    }

    /// An install that moved `moved` into place and skipped no file.
    fn installed_of(moved: Vec<Moved>) -> Installed {
        Installed {
            count: moved.len(),
            bytes: 0,
            compressed: 0,
            skipped: Vec::new(),
            moved,
        }
    }

    /// Sets the last write of the file at `full` to `age` before now, so a run takes it as settled.
    fn aged(full: &Path, age: Duration) {
        File::options()
            .write(true)
            .open(full)
            .and_then(|file| file.set_modified(SystemTime::now() - age))
            .expect("the last write can be set");
    }

    /// The hash cache path of a scratch `folder`, as the client names it.
    fn cache_path(folder: &Path) -> PathBuf {
        folder.join("app-1a2b3c4d.outputs-hashes")
    }

    #[test]
    fn a_moved_file_is_returned_with_its_installed_stamp() {
        let folder = scratch("moved-stamp");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        aged(&put(&stage, "bin/a.txt", b"box"), Duration::from_hours(2));
        fs::create_dir_all(&root).expect("the root can be made");

        let moved = moved(&stage, &root, &[entry("bin/a.txt")], &Manifest::default(), |_| true).expect("the file is moved");

        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0].entry.path, "bin/a.txt");
        assert_eq!(
            moved[0].stamp,
            Some(stamp(&root.join("bin").join("a.txt"))),
            "the stamp the installed file's handle reads"
        );
    }

    #[test]
    fn a_file_clamped_to_now_is_not_cached() {
        let folder = scratch("cache-clamped");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        let ahead = put(&stage, "bin/ahead.txt", b"a");
        aged(&put(&stage, "bin/behind.txt", b"b"), Duration::from_hours(3));
        File::options()
            .write(true)
            .open(&ahead)
            .and_then(|file| file.set_modified(SystemTime::now() + Duration::from_hours(3)))
            .expect("the last write can be set");
        fs::create_dir_all(&root).expect("the root can be made");
        let entries = [entry("bin/ahead.txt"), entry("bin/behind.txt")];
        // Taken before the moves, as the install takes it: the clamp to this clock then falls at or after it.
        let started = file_time_now();
        let moved = moved(&stage, &root, &entries, &Manifest::default(), |_| true).expect("both files are moved");
        let clamped = moved
            .iter()
            .find(|file| file.entry.path == "bin/ahead.txt")
            .and_then(|file| file.stamp)
            .expect("the clamped file's stamp is read");
        assert!(clamped.modified >= started, "clamped at or after the moves began: {clamped:?}, {started}");
        let path = cache_path(&folder);

        cached(&path, &installed_of(moved), started);

        let cache = HashCache::load(&path).expect("the install saves the cache");
        assert!(cache.held("bin/behind.txt").is_some(), "a file written long before the moves is kept");
        assert_eq!(cache.held("bin/ahead.txt"), None, "a file clamped to this clock is not kept");
    }

    #[test]
    fn a_rewritten_member_is_not_cached() {
        let folder = scratch("cache-rewritten");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        let paths = [
            "bin/MvcTestingAppManifest.json",
            "bin/.msCoverageSourceRootsMapping_App",
            "bin/app.deps.json",
        ];
        for path in paths {
            aged(&put(&stage, path, b"text"), Duration::from_hours(2));
        }
        fs::create_dir_all(&root).expect("the root can be made");
        let entries: Vec<Entry> = paths.iter().map(|path| entry(path)).collect();
        let moved = moved(&stage, &root, &entries, &Manifest::default(), |_| true).expect("every file is moved");
        let path = cache_path(&folder);

        cached(&path, &installed_of(moved), file_time_now());

        let cache = HashCache::load(&path).expect("the install saves the cache");
        for rewritten in ["bin/MvcTestingAppManifest.json", "bin/.msCoverageSourceRootsMapping_App"] {
            assert_eq!(cache.held(rewritten), None, "a file the install rewrites is not cached");
        }
        assert!(cache.held("bin/app.deps.json").is_some(), "a file it leaves alone is cached");
    }

    #[test]
    fn a_partial_install_writes_no_cache_entry() {
        let folder = scratch("cache-partial");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        aged(&put(&stage, "bin/a.txt", b"a"), Duration::from_hours(2));
        fs::create_dir_all(&root).expect("the root can be made");
        let moved = moved(&stage, &root, &[entry("bin/a.txt")], &Manifest::default(), |_| true).expect("the file is moved");
        let mut installed = installed_of(moved);
        installed.skipped = vec![("bin/held.dll".to_owned(), Skip::InUse)];
        let path = cache_path(&folder);

        cached(&path, &installed, file_time_now());

        assert!(!path.exists(), "an install that skipped a file keeps no hash");
    }

    /// The staged file is held, sharing only its deletion, so it moves with its handle open and no one may open it for
    /// writing after: only a clamp set on the stage, before the move, can succeed.
    #[test]
    fn a_future_last_write_is_clamped_in_the_stage_never_on_the_target() {
        let folder = scratch("move-clamp-staged");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        let staged = put(&stage, "bin/ahead.txt", b"a");
        fs::create_dir_all(&root).expect("the root can be made");
        File::options()
            .write(true)
            .open(&staged)
            .and_then(|file| file.set_modified(SystemTime::now() + Duration::from_hours(3)))
            .expect("the last write can be set");
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_DELETE)
            .open(&staged)
            .expect("the file can be held");

        let result = moved(&stage, &root, &[entry("bin/ahead.txt")], &Manifest::default(), |_| true);
        drop(held);

        assert!(result.is_ok(), "{result:?}");
        let target = root.join("bin").join("ahead.txt");
        let modified = fs::metadata(&target).and_then(|metadata| metadata.modified()).expect("a last write");
        assert!(modified <= SystemTime::now(), "clamped to this clock");
    }

    #[test]
    fn a_pc_file_changed_after_the_checks_is_refused_just_before_its_move() {
        let folder = scratch("move-changed");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        put(&stage, "bin/a.txt", b"box");
        put(&stage, "bin/b.txt", b"box");
        let pc = put(&root, "bin/b.txt", b"pc");
        let manifest = manifest_of(&root, &["bin/b.txt"]);
        let entries = [entry("bin/a.txt"), entry("bin/b.txt")];

        let unmoved = moved(&stage, &root, &entries, &manifest, |done| {
            if done == 1 {
                fs::write(&pc, b"edited after the checks").expect("the file can be written");
            }
            true
        });

        let reason = "1 of 2 files were installed before bin/b.txt failed: bin/b.txt changed during the run";
        assert_eq!(unmoved, Err(Unmoved::Failed(reason.to_owned())));
        assert_eq!(fs::read_to_string(&pc).expect("b stays"), "edited after the checks");
        assert_eq!(fs::read_to_string(root.join("bin").join("a.txt")).expect("a is moved"), "box");
    }

    #[test]
    fn a_stage_folder_there_before_the_install_is_not_its_to_remove() {
        let folder = scratch("stage-there");
        let source = folder.join("source");
        put(&source, "bin/a.txt", b"a");
        let found = walk(&source, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        let chosen: Vec<_> = found.iter().collect();
        let mut archive = Vec::new();
        pack(&mut archive, &source, &chosen).expect("the file packs");
        let stage = folder.join("stage");
        put(&stage, "kept.txt", b"another run's");

        let refused = unpacked(&archive[..], &stage, OUTPUTS_BUDGET, &|| true, &Cell::new(false));

        assert!(
            refused.as_ref().is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists),
            "{refused:?}"
        );
        assert!(!stage_made(&refused), "a folder there before is not the install's to remove");
        assert_eq!(fs::read_to_string(stage.join("kept.txt")).expect("its file survives"), "another run's");
        let fresh = unpacked(&archive[..], &folder.join("fresh"), OUTPUTS_BUDGET, &|| true, &Cell::new(false));
        assert!(fresh.is_ok() && stage_made(&fresh), "{fresh:?}");
        let garbage = unpacked(
            &b"not an archive"[..],
            &folder.join("garbage"),
            OUTPUTS_BUDGET,
            &|| true,
            &Cell::new(false),
        );
        assert!(garbage.is_err() && stage_made(&garbage), "{garbage:?}");
    }

    #[test]
    fn an_interrupt_before_the_first_move_says_no_count() {
        let (end, line) = ending(Unmet::Interrupted { done: 0, total: 3 });
        assert!(matches!(end, End::Interrupted) && line.is_none(), "{end:?} {line:?}");

        let (end, line) = ending(Unmet::Interrupted { done: 1, total: 3 });
        assert!(matches!(end, End::Interrupted), "{end:?}");
        let counted = "gate: remote outputs not brought back: 1 of 3 files were installed before the interrupt";
        assert_eq!(line.as_deref(), Some(counted));

        let (end, line) = ending(Unmet::Refused("bin/a.dll changed during the run".to_owned()));
        assert!(matches!(end, End::Status(1)), "{end:?}");
        let refused = "gate: remote outputs not brought back: bin/a.dll changed during the run";
        assert_eq!(line.as_deref(), Some(refused));
    }

    #[test]
    fn the_checks_refuse_in_their_order_across_members() {
        let root = scratch("check-order");
        put(&root, "bin/changed.dll", b"abc");
        let both = put(&root, "bin/both.dll", b"abc");
        let manifest = manifest_of(&root, &["bin/changed.dll", "bin/both.dll"]);
        put(&root, "bin/changed.dll", b"abcd");

        let globs_first = checked(&root, &[entry("bin/changed.dll"), entry("obj/a.dll")], &bin_globs(), &manifest);
        assert_eq!(globs_first, Err("the archive member obj/a.dll is not under the output globs".to_owned()));

        fs::write(&both, b"abcd").expect("the file can be written");
        let held = fs::OpenOptions::new().read(true).share_mode(0).open(&both).expect("the file can be held");
        let changed_first = checked(&root, &[entry("bin/both.dll")], &bin_globs(), &manifest);
        drop(held);
        assert_eq!(changed_first, Err("bin/both.dll changed during the run".to_owned()));
    }

    /// The size of the member the streaming case unpacks.
    const LARGE: u64 = 64 * 1024 * 1024;
    /// The most the unpack may hold back between the archive it reads and the file it writes: a quarter of [`LARGE`],
    /// twice the 8 MiB zstd window the reader takes. One that held the member whole would hold back all of it.
    const HELD_BACK: u64 = 16 * 1024 * 1024;

    /// The archive file `inner`, read while noting, at each read, how far the bytes read so far run ahead of what the
    /// file at `target` holds: the widest such gap is the most the unpack held back.
    struct Watched<'w> {
        inner: File,
        target: &'w Path,
        given: u64,
        widest: &'w Cell<u64>,
    }

    impl Read for Watched<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let written = File::open(self.target)
                .and_then(|file| file.metadata())
                .map_or(0, |metadata| metadata.len());
            self.widest.set(self.widest.get().max(self.given.saturating_sub(written)));
            let read = self.inner.read(buffer)?;
            self.given += read as u64;
            Ok(read)
        }
    }

    /// Writes `size` pseudo-random bytes, which zstd cannot shrink, to `path`.
    fn random_file(path: &Path, size: u64) {
        let mut file = io::BufWriter::new(File::create(path).expect("the file can be made"));
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..size / 8 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            file.write_all(&state.to_le_bytes()).expect("the file can be written");
        }
        file.flush().expect("the file can be written");
    }

    /// Removes the folder it holds once dropped, so a test that fails leaves no scratch behind either.
    struct Removed(PathBuf);

    impl Drop for Removed {
        fn drop(&mut self) {
            // A folder that cannot be removed is left to the system's temp cleanup; the test's own result stands.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_install_unpacks_a_member_larger_than_any_buffer_it_holds_as_a_stream() {
        let folder = scratch("streams");
        let _removed = Removed(folder.clone());
        let source = folder.join("source");
        fs::create_dir_all(source.join("bin")).expect("the folder can be made");
        random_file(&source.join("bin").join("large.bin"), LARGE);
        let found = walk(&source, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        let archive = folder.join("archive.tar.zst");
        let chosen: Vec<_> = found.iter().collect();
        pack(File::create(&archive).expect("the archive can be made"), &source, &chosen).expect("the file packs");
        let stage = folder.join("stage");
        let target = stage.join("bin").join("large.bin");
        let widest = Cell::new(0);
        let watched = Watched {
            inner: File::open(&archive).expect("the archive opens"),
            target: &target,
            given: 0,
            widest: &widest,
        };

        let entries = unpacked(watched, &stage, OUTPUTS_BUDGET, &|| true, &Cell::new(false)).expect("the archive unpacks");

        assert_eq!(entries.iter().map(|entry| entry.size).collect::<Vec<_>>(), [LARGE]);
        assert!(widest.get() <= HELD_BACK, "the unpack held back {} bytes", widest.get());
    }

    #[test]
    fn an_unpack_stops_once_the_interrupt_is_raised() {
        let folder = scratch("unpack-interrupted");
        let source = folder.join("source");
        put(&source, "bin/a.txt", b"a");
        let found = walk(&source, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        let chosen: Vec<_> = found.iter().collect();
        let mut archive = Vec::new();
        pack(&mut archive, &source, &chosen).expect("the file packs");
        let stopped = Cell::new(false);

        let unpacked = unpacked(&archive[..], &folder.join("stage"), OUTPUTS_BUDGET, &|| false, &stopped);

        assert!(unpacked.is_err() && stopped.get(), "{unpacked:?}");
        assert!(!folder.join("stage").exists(), "the stage is removed");
    }
}
