//! A routed run's fetched outputs archive put into the worktree: unpacked into a stage folder beside it, which checks
//! every member's path and hash and makes each copy the archive lists from its member, or from a PC file holding its
//! bytes, checked by hashing them as they are copied; each member then checked against the run's output globs, against
//! the PC's manifest (a file it held must still have its size and last write, one it lacked must still be missing, else
//! the PC's file must already hold the member's bytes, which leaves it in place; a file that records a root is left in
//! place whenever a PC writer changed or made it, and one a PC writer deleted counts as unchanged; every member failing
//! this is named), and one that counts as tracked (its git index, a submodule or a nested repository holds it) unless a
//! `--bring-tracked` glob matches it, or whose PC file is in use or read-only, or that records one of the run's box
//! trees but is not text the install can rewrite, skipped and said; each of the run's trees on the box rewritten to its
//! root on this PC in the files that record it (the work folder to the worktree for one repo; for a pair, the main's
//! tree to the worktree and each sibling's to the sibling's root); and each file, its last write clamped to this PC's
//! clock in the stage and checked against the manifest once more, moved into place, unless its PC file is now in place
//! by the same rule, which is left as it is, or something holds it open against the move, which is skipped as in use
//! and said. A complete install also keeps the hash of each file it put in place, at the stamp that file's handle
//! reads after the move, in the worktree's hash cache, so the next run takes it instead of reading the file again. The
//! success line says what came back; every end removes the archive and the stage this run made, and the pending
//! marker is cleared by a success whose skips a refetch would not clear, and by a refusal a refetch of the same
//! archive would be refused the same way.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use gate_router::is_layout_char;
use gate_state::v1;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
};

use super::client::{Attempt, End};
use super::git::{Git, GitError};
use super::outputs::{self, Archive, Polled};
use crate::boxside::payload::{escaped, ordinal_upper};
use crate::outputs::archive::{Entry, PcFiles, UnpackError, below, check_folder, unpack};
use crate::outputs::cache::{HashCache, file_time_now};
use crate::outputs::glob::Globs;
use crate::outputs::hash::file_sha256;
use crate::outputs::manifest::Manifest;
use crate::outputs::walk::{Stamp, in_context};

/// The name of the ASP.NET test host's file of content roots, which records the box's work folder.
const TEST_HOST_MANIFEST: &str = "MvcTestingAppManifest.json";
/// The start of the name of the coverage collector's file of source roots, which records the box's work folder.
const COVERAGE_ROOTS: &str = ".msCoverageSourceRootsMapping_";
/// The bytes a UTF-8 text file may start with to say it is UTF-8.
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
/// The bytes a UTF-16 text file starts with to say its body is 16-bit units in little-endian byte order.
const UTF16_LE_BOM: &[u8] = b"\xFF\xFE";
/// The bytes a UTF-16 text file starts with to say its body is 16-bit units in big-endian byte order.
const UTF16_BE_BOM: &[u8] = b"\xFE\xFF";
/// The bytes a UTF-32 text file starts with to say its body is 32-bit units in little-endian byte order, which would
/// otherwise read as UTF-16 little-endian with a leading NUL unit: a file the install rewrites is never one.
const UTF32_LE_BOM: &[u8] = b"\xFF\xFE\x00\x00";
/// The largest file the install rewrites, 16 MiB: a larger one is skipped before it is read.
const REWRITE_LIMIT: u64 = 16 * 1024 * 1024;
/// The bytes in a megabyte, as the success line counts them.
const MB: f64 = 1024.0 * 1024.0;
/// The seam's name for the install, to `--test-remote-interrupt-at`: the interrupt is raised before the second file's
/// move, never for an install of a single file.
const INSTALL: &str = "install";
/// The most members whose PC files changed during the run a refusal names, one line each; the rest are counted.
const CHANGED_NAMED: usize = 10;

/// What the install of a fetched archive needs: the archive, the stage folder beside it, the pending marker, the PC's
/// manifest and the run's output globs, the box path of each of the run's trees with the PC root it stands for, and
/// when the fetch started.
pub(super) struct Fetched<'f> {
    pub(super) archive: &'f Archive,
    pub(super) stage: PathBuf,
    pub(super) marker: &'f Path,
    pub(super) manifest: &'f Manifest,
    pub(super) globs: &'f [String],
    /// The main repo's tree on the box, which stands for the worktree: the work folder for a run of one repo,
    /// `{work}\{leaf}` for a pair.
    pub(super) main_tree: String,
    /// Each sibling's tree on the box and the sibling's root it stands for, in the pair's order; none for one repo.
    pub(super) sibling_trees: Vec<(String, &'f str)>,
    pub(super) started: Instant,
}

/// What an install put into the worktree: the files it moved, their bytes, and the archive's bytes; the paths of the
/// files it skipped, each with why; and the files it moved into place, for the hash cache it keeps ([`cached`]).
struct Installed {
    count: usize,
    bytes: u64,
    compressed: u64,
    skipped: Vec<(String, Skip)>,
    /// Each file the install moved into place, with the stamp its handle read after the move; never a member left in
    /// place ([`Placed::InPlace`]).
    moved: Vec<Moved>,
}

/// A file the install moved into place: the archive's entry for it, whose SHA-256 [`unpack`] verified against the
/// staged bytes, and the stamp the installed file's handle reads after the move; none when that read failed.
#[derive(Debug, PartialEq, Eq)]
struct Moved {
    entry: Entry,
    stamp: Option<Stamp>,
}

impl Installed {
    /// Whether the install left the PC every output the box packed: false only for a skip that keeps the pending marker,
    /// so a file the install cannot rewrite leaves an install whole, since a later run would skip it the same way and
    /// the skip line says so each time. Every other end is incomplete.
    fn complete(&self) -> bool {
        self.skipped.iter().all(|(_, skip)| !skip.keeps_marker())
    }
}

/// Why the install skipped a member, which then counts as not brought back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Skip {
    /// The worktree's git index holds its path, and no `--bring-tracked` glob matches it.
    Tracked,
    /// The PC's file is in use or read-only ([`writable`]).
    InUse,
    /// It records the box's work folder but is not text the install can rewrite ([`Unrewritable`]).
    Unrewritable(Unrewritable),
}

impl Skip {
    /// The end of the line that says a member was skipped for this reason.
    fn said(self) -> String {
        match self {
            Skip::Tracked => "is tracked by git".to_owned(),
            Skip::InUse => "is in use or read-only".to_owned(),
            Skip::Unrewritable(Unrewritable::NotText) => "cannot be rewritten (it is not UTF-8 or UTF-16 with a byte order mark)".to_owned(),
            Skip::Unrewritable(Unrewritable::TooLarge(bytes)) => format!("cannot be rewritten ({bytes} bytes, over 16 MiB)"),
        }
    }

    /// Whether this skip keeps the pending marker and makes the install incomplete, so a later run asks for every output
    /// again: a file git tracks, or one in use or read-only, differs from the PC's next run, while a file the install
    /// cannot rewrite is the same on every run, so it needs no full send and does not stop an install counting whole.
    fn keeps_marker(self) -> bool {
        match self {
            Skip::Tracked | Skip::InUse => true,
            Skip::Unrewritable(_) => false,
        }
    }
}

/// Why a file that records the box's work folder is one the install cannot rewrite, and so is skipped rather than
/// installed with the box's paths in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unrewritable {
    /// It is not UTF-8, nor UTF-16 whose byte order mark names the order of its body.
    NotText,
    /// It is over [`REWRITE_LIMIT`] bytes, which the rewrite refuses before it is read.
    TooLarge(u64),
}

/// Why an install did not complete.
#[derive(Debug, PartialEq, Eq)]
enum Unmet {
    /// The console interrupt, once `done` of `total` files were moved; none when it came before the move.
    Interrupted { done: usize, total: usize },
    /// Why the outputs were not brought back.
    Refused(Refused),
}

/// Why an install refused to bring the outputs back, each reason said on a line of its own, and whether a refetch of the
/// same outputs would be refused the same way.
#[derive(Debug, PartialEq, Eq)]
struct Refused {
    reasons: Vec<String>,
    kind: Kind,
}

/// Whether a refusal would happen again for the same archive. The archive's own members and the run's globs decide one
/// of the first kind, which leaves the pending marker cleared: a later run of the worktree would refetch everything only
/// to refuse again, so it is left to send its normal manifest. Any other refusal keeps the marker, since the next fetch
/// may succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The archive's own entries and the globs decide it.
    Deterministic,
    /// Anything else: a refetch may clear it.
    Transient,
}

impl Refused {
    /// The refusal a refetch of the same outputs would be refused the same way.
    fn deterministic(reason: impl Into<String>) -> Refused {
        Refused {
            reasons: vec![reason.into()],
            kind: Kind::Deterministic,
        }
    }

    /// The refusal of anything a refetch may clear.
    fn transient(reason: impl Into<String>) -> Refused {
        Refused {
            reasons: vec![reason.into()],
            kind: Kind::Transient,
        }
    }

    /// The refusal of the members whose PC files changed during the run or could not be checked, `found` holding why for
    /// each, in archive order: the first [`CHANGED_NAMED`] of them, then one counting the rest when there are more. The
    /// PC's own worktree decides it, so a refetch may clear it ([`Refused::transient`]).
    fn changed(mut found: Vec<String>) -> Refused {
        let more = found.len().saturating_sub(CHANGED_NAMED);
        found.truncate(CHANGED_NAMED);
        if more > 0 {
            found.push(format!("{more} more files changed during the run"));
        }
        Refused {
            reasons: found,
            kind: Kind::Transient,
        }
    }
}

/// The refusal of anything but the archive's own members and the run's globs ([`Refused::transient`]), in the [`Unmet`]
/// it ends an install with.
fn transient(reason: String) -> Unmet {
    Unmet::Refused(Refused::transient(reason))
}

/// The refusal of an archive `detail` says the install could not read: one whose own members `members` reports the
/// archive's reader refusing for what they are is refused for good, and any other fault may be cleared by a refetch.
fn unpack_refused(detail: String, members: bool) -> Unmet {
    if members {
        Unmet::Refused(Refused::deterministic(detail))
    } else {
        transient(detail)
    }
}

/// Why the move of the files stopped.
#[derive(Debug, PartialEq, Eq)]
enum Unmoved {
    /// The console interrupt, once `done` files were moved.
    Interrupted { done: usize },
    /// Why a file failed, and how many were moved before it.
    Failed(String),
}

/// A PC root as the install pairs it with its box tree: read against the working directory, with `\` separators. The
/// worktree root and each sibling's are spelled through this, for the install and for the path-map word alike.
pub(super) fn pc_spelling(root: &str) -> io::Result<PathBuf> {
    std::path::absolute(root)
}

/// The end of a passing run whose outputs archive `fetched` holds was fetched whole: installed, it says the success
/// line and ends 0, clearing the pending marker unless it skipped a file that keeps it ([`Skip::keeps_marker`]): one git
/// tracks, one in use or read-only, which it says ([`said`]).
/// A refusal says why the outputs were not brought back and ends the run 1, clearing the marker when a refetch of the
/// same archive would be refused the same way ([`Kind`]); a Ctrl+C ends it as an interrupt, saying how many files were
/// installed when any were ([`ending`]). Every end removes the archive and the stage folder this run made, never one
/// that was there before, saying a removal that fails; only a success that skipped nothing keeps the hashes of the files
/// it put in place ([`cached`]), which no other end does.
/// With the end, whether the install was complete ([`Installed::complete`]): a success whose skips are all ones a
/// refetch of the same outputs would clear, and so the one whose files the hash cache may keep.
pub(super) fn installed(attempt: &mut Attempt<'_, '_>, fetched: &Fetched<'_>) -> (End, bool) {
    let staged = Cell::new(false);
    let (end, lines, complete, clear) = match install(attempt, fetched, &staged) {
        Ok((done, started)) => {
            cached(&v1::outputs_hashes_path(attempt.call.state_dir, attempt.call.key), &done, started);
            let lines = said(&done, fetched.started.elapsed().as_secs_f64());
            let complete = done.complete();
            (End::Status(0), lines, complete, complete)
        }
        Err(unmet) => {
            let clear = matches!(&unmet, Unmet::Refused(refused) if refused.kind == Kind::Deterministic);
            let (end, lines) = ending(unmet);
            (end, lines, false, clear)
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
    if clear && let Err(why) = outputs::clear_pending(fetched.marker) {
        attempt.say(format!("gate: {why}"));
    }
    (end, complete)
}

/// The end of an install that did not complete as `unmet` says, and the lines it says: one for each reason of a refusal,
/// none for a Ctrl+C before any file was moved, which the run's own interrupt handling says.
fn ending(unmet: Unmet) -> (End, Vec<String>) {
    match unmet {
        Unmet::Interrupted { done: 0, .. } => (End::Interrupted, Vec::new()),
        Unmet::Interrupted { done, total } => {
            let reason = format!("{done} of {total} files were installed before the interrupt");
            (End::Interrupted, vec![outputs::not_brought_back(&reason)])
        }
        Unmet::Refused(refused) => (
            End::Status(1),
            refused.reasons.iter().map(|reason| outputs::not_brought_back(reason)).collect(),
        ),
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
    let unopened = |error: &UnpackError| unpack_refused(format!("the outputs archive {name} cannot be opened: {error}"), error.members());
    let file = File::open(path).map_err(|error| unopened(&error.into()))?;
    let compressed = file.metadata().map_err(|error| unopened(&error.into()))?.len();
    let root = pc_spelling(attempt.call.root).map_err(|error| transient(in_context(Path::new(attempt.call.root), &error).to_string()))?;
    let stopped = Cell::new(false);
    let pc = PcFiles {
        root: &root,
        manifest: fetched.manifest,
        go_on: &|| attempt.go_on().is_ok(),
    };
    let unpacking = unpacked(file, &fetched.stage, attempt.outputs_budget, &pc, &stopped);
    staged.set(stage_made(&unpacking));
    let entries = unpacking.map_err(|error| {
        if stopped.get() {
            Unmet::Interrupted { done: 0, total: 0 }
        } else {
            unopened(&error)
        }
    })?;
    let globs = Globs::new(fetched.globs).map_err(transient)?;
    let checks = checked(&root, &entries, &globs, fetched.manifest).map_err(Unmet::Refused)?;
    let in_place: HashSet<&str> = checks.in_place.iter().map(String::as_str).collect();
    let entries: Vec<Entry> = entries.into_iter().filter(|entry| !in_place.contains(entry.path.as_str())).collect();
    let mut skipped = skips(attempt, &root, &entries, &checks.in_use)?;
    let skipped_paths: HashSet<&str> = skipped.iter().map(|(path, _)| path.as_str()).collect();
    let entries: Vec<Entry> = entries.into_iter().filter(|entry| !skipped_paths.contains(entry.path.as_str())).collect();
    let siblings = fetched.sibling_trees.iter().map(|(tree, pc_root)| {
        pc_spelling(pc_root)
            .map(|pc_root| (tree.clone(), pc_root.display().to_string()))
            .map_err(|error| transient(in_context(Path::new(pc_root), &error).to_string()))
    });
    let trees = std::iter::once(Ok((fetched.main_tree.clone(), root.display().to_string())))
        .chain(siblings)
        .collect::<Result<Vec<_>, _>>()?;
    let unrewritable = rewritten(&fetched.stage, &entries, &trees).map_err(Unmet::Refused)?;
    let unrewritten: HashSet<&str> = unrewritable.iter().map(|(path, _)| path.as_str()).collect();
    let entries: Vec<Entry> = entries.into_iter().filter(|entry| !unrewritten.contains(entry.path.as_str())).collect();
    skipped.extend(unrewritable);
    let raise = attempt.call.interrupt.filter(|_| attempt.raises_at(INSTALL));
    let total = entries.len();
    // The file time the moves begin at, from which the racy rule keeps the hash of a file written long enough before.
    let started = file_time_now();
    let moves = moved(&fetched.stage, &root, &entries, fetched.manifest, |done| {
        if done == 1
            && let Some(interrupt) = raise
        {
            interrupt.raise();
        }
        attempt.go_on().is_ok()
    })
    .map_err(|unmoved| match unmoved {
        Unmoved::Interrupted { done } => Unmet::Interrupted { done, total },
        Unmoved::Failed(reason) => transient(reason),
    })?;
    skipped.extend(moves.in_use.into_iter().map(|path| (path, Skip::InUse)));
    let moved = moves.moved;
    Ok((
        Installed {
            count: moved.len(),
            bytes: moved.iter().map(|moved| moved.entry.size).sum(),
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
    let bring = Globs::bring_tracked(attempt.call.bring_tracked).map_err(transient)?;
    let git = Git::find(Path::new(attempt.call.root)).map_err(transient)?;
    let listing = attempt.git_bytes(&git, &["ls-files", "-z", "-s"]).map_err(unmet_of)?;
    let index = Index::of(&listing).map_err(transient)?;
    let in_use: HashSet<&str> = in_use.iter().map(String::as_str).collect();
    Ok(reasons(entries, &tracked(&index, root, entries, &bring), &in_use))
}

/// The end of an install whose read of the git index failed as `error`: the console interrupt is an interrupt before
/// any move; a failure, or a listing past the git runner's cap, refuses the install.
fn unmet_of(error: GitError) -> Unmet {
    match error {
        GitError::Interrupted => Unmet::Interrupted { done: 0, total: 0 },
        GitError::Failed(why) | GitError::OverCap(why) => transient(why),
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
fn stage_made(unpacking: &Result<Vec<Entry>, UnpackError>) -> bool {
    !matches!(unpacking, Err(error) if error.is_fault(io::ErrorKind::AlreadyExists))
}

/// Unpacks the archive `source` into the folder `stage`, which must not exist yet, to at most `limit` bytes, each copy
/// it lists made from its member or from a file of `pc` whose bytes still hash to it, asking `pc`'s `go_on` before each
/// read of the archive, before each copy and after each buffer a copy writes, and marking `stopped` once it says no.
/// The entries of every path, else why not.
fn unpacked(source: impl Read, stage: &Path, limit: u64, pc: &PcFiles<'_>, stopped: &Cell<bool>) -> Result<Vec<Entry>, UnpackError> {
    let go_on = || {
        let on = (pc.go_on)();
        if !on {
            stopped.set(true);
        }
        on
    };
    unpack(
        Polled {
            inner: source,
            go_on: pc.go_on,
            stopped,
        },
        stage,
        limit,
        &PcFiles {
            root: pc.root,
            manifest: pc.manifest,
            go_on: &go_on,
        },
    )
}

/// What the checks before the moves found of an archive's members, each list holding paths in archive order.
#[derive(Debug, Default, PartialEq, Eq)]
struct Checked {
    /// The members whose PC file is as the manifest holds it but not replaceable ([`writable`]), which the install skips.
    in_use: Vec<String>,
    /// The members whose PC file changed during the run to the member's own bytes, or that record a root and whose PC
    /// file changed during the run at all ([`Pc::InPlace`]), which the install neither moves nor says.
    in_place: Vec<String>,
}

/// Whether each of `entries`, to be installed below the worktree `root`, may be: every one under `globs`, then every
/// one unchanged since `manifest` or in place, a root-recording one whose PC file changed counted so ([`pc_file`]); else
/// the refusal. The one kind a refetch of the same archive would be refused for again is a member outside the globs
/// ([`Refused::deterministic`]); the PC's own worktree decides the rest. Every member whose PC file changed otherwise,
/// is not a file or cannot be read is collected, in archive order, and all are refused together ([`Refused::changed`]).
/// What the checks found ([`Checked`]).
fn checked(root: &Path, entries: &[Entry], globs: &Globs, manifest: &Manifest) -> Result<Checked, Refused> {
    if let Some(outside) = entries.iter().find(|entry| !globs.matches(&entry.path)) {
        return Err(Refused::deterministic(format!(
            "the archive member {} is not under the output globs",
            outside.path
        )));
    }
    let mut checks = Checked::default();
    let mut found = Vec::new();
    for entry in entries {
        match pc_file(root, entry, manifest) {
            Ok(Pc::AsHeld) if !writable(root, entry) => checks.in_use.push(entry.path.clone()),
            Ok(Pc::AsHeld) => {}
            Ok(Pc::InPlace) => checks.in_place.push(entry.path.clone()),
            Ok(Pc::Changed) => found.push(format!("{} changed during the run", entry.path)),
            Err(why) => found.push(why),
        }
    }
    if found.is_empty() { Ok(checks) } else { Err(Refused::changed(found)) }
}

/// What the PC's file of a member is, set against the manifest the run sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pc {
    /// As the manifest holds it: one it holds still has its size and last write, one it lacks is still missing. So is
    /// one that records a root ([`records_a_root`]) which the manifest held and a PC writer deleted during the run (a
    /// clean, a rebuild): the rewritten member fills its empty place.
    AsHeld,
    /// Changed during the run, but to the member's own bytes ([`holds_the_member`]): the member is already in place. So is
    /// a member that records a root ([`records_a_root`]) whose PC file a PC writer changed or made during the run: that
    /// file already records the PC's roots, and the PC's file is kept.
    InPlace,
    /// Changed during the run otherwise.
    Changed,
}

/// What the PC's file of `entry` below `root` is against `manifest` ([`Pc`]); else why it cannot be told, which names a
/// folder in the file's place as not a file. Every one of these depends on the PC's own worktree, whose next run may find
/// it otherwise, so each refuses as one a refetch may clear ([`Refused::transient`]).
fn pc_file(root: &Path, entry: &Entry, manifest: &Manifest) -> Result<Pc, String> {
    let target = below(root, &entry.path);
    let now = match fs::metadata(&target) {
        Ok(metadata) if !metadata.is_file() => return Err(format!("{} is not a file", entry.path)),
        Ok(metadata) => Some(Stamp::of(&metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(in_context(&target, &error).to_string()),
    };
    Ok(match (manifest.get(&entry.path), now) {
        (Some(held), Some(stamp)) if held.stamp == stamp => Pc::AsHeld,
        (None, None) => Pc::AsHeld,
        (Some(_), None) if records_a_root(&entry.path) => Pc::AsHeld,
        (_, Some(_)) if records_a_root(&entry.path) => Pc::InPlace,
        (_, Some(stamp)) if holds_the_member(&target, entry, stamp.size) => Pc::InPlace,
        _ => Pc::Changed,
    })
}

/// Whether the PC file at `target`, `size` bytes long, holds the bytes of `entry`, the archive's member for it: the same
/// size and SHA-256. A file that cannot be read for its hash does not, nor does one that records a root
/// ([`records_a_root`]), whose installed bytes are the rewritten ones, never the member's; [`pc_file`] keeps such a file
/// in place by its own rule.
fn holds_the_member(target: &Path, entry: &Entry, size: u64) -> bool {
    !records_a_root(&entry.path) && size == entry.size && file_sha256(target).is_ok_and(|sha256| sha256 == entry.sha256)
}

/// Whether the PC's file of `entry` below `root`, when there is one, opens for reading and writing with no sharing,
/// so a move can replace it.
fn writable(root: &Path, entry: &Entry) -> bool {
    let target = below(root, &entry.path);
    // Opened and closed at once: only whether it opens counts, and the file is then said as in use or read-only.
    !target.is_file() || OpenOptions::new().read(true).write(true).share_mode(0).open(&target).is_ok()
}

/// Rewrites each box tree of `trees` to the PC root paired with it in each of the staged `entries` below `stage` that
/// records a root ([`records_a_root`]), as [`rewrite_file`] does: the paths, in order, of those that are not text the
/// install can rewrite, each with its [`Skip`], since one installed as it stands would keep the box's paths; else why
/// one cannot be read or written.
fn rewritten(stage: &Path, entries: &[Entry], trees: &[(String, String)]) -> Result<Vec<(String, Skip)>, Refused> {
    let trees: Vec<(String, String)> = trees
        .iter()
        .map(|(tree, pc_root)| (tree.trim_end_matches('\\').to_owned(), pc_root.trim_end_matches('\\').to_owned()))
        .collect();
    let mut skipped = Vec::new();
    for entry in entries.iter().filter(|entry| records_a_root(&entry.path)) {
        match rewrite_file(&below(stage, &entry.path), &entry.path, &trees) {
            Ok(()) => {}
            Err(CannotRewrite::Skipped(why)) => skipped.push((entry.path.clone(), Skip::Unrewritable(why))),
            Err(CannotRewrite::Failed(refused)) => return Err(refused),
        }
    }
    Ok(skipped)
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

/// Rewrites each box tree of `trees` to the PC root paired with it in the text file at `path`, shown as `shown`, as
/// [`replaced`] matches them: their JSON-escaped forms (each `\` doubled) first, then their raw forms, each without
/// regard to case as .NET's `OrdinalIgnoreCase` compares. The file is read in the encoding its byte order mark names
/// ([`Encoding`]): UTF-8, or UTF-16 in the byte order its mark gives, read as 16-bit units. A file whose text changes
/// is written back in the same encoding, with the same byte order mark, and its last write kept. Else the skip
/// ([`CannotRewrite::Skipped`]): a file over [`REWRITE_LIMIT`] bytes, refused before it is read, or one whose body is
/// not text in the encoding its mark names. A file that cannot be read or written is refused.
fn rewrite_file(path: &Path, shown: &str, trees: &[(String, String)]) -> Result<(), CannotRewrite> {
    let cannot = |error: io::Error| CannotRewrite::Failed(Refused::transient(format!("cannot rewrite {shown}: {error}")));
    let mut file = File::open(path).map_err(cannot)?;
    let metadata = file.metadata().map_err(cannot)?;
    if metadata.len() > REWRITE_LIMIT {
        return Err(CannotRewrite::Skipped(Unrewritable::TooLarge(metadata.len())));
    }
    let modified = metadata.modified().map_err(cannot)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(cannot)?;
    drop(file);
    // Which byte is not text in the encoding the mark names is no use to the reader; that the file is not is the one
    // reason said.
    let (encoding, text) = decoded(&bytes).ok_or(CannotRewrite::Skipped(Unrewritable::NotText))?;
    let escaped_trees: Vec<(String, String)> = trees
        .iter()
        .map(|(tree, pc_root)| (tree.replace('\\', r"\\"), pc_root.replace('\\', r"\\")))
        .collect();
    let escaped = replaced(&text, &escaped_trees);
    let new = replaced(&escaped, trees);
    if new == text {
        return Ok(());
    }
    fs::write(path, encoded(&new, encoding)).map_err(cannot)?;
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(modified))
        .map_err(cannot)
}

/// Why a file the install rewrites was not: either its member is skipped, or the install is refused.
#[derive(Debug, PartialEq, Eq)]
enum CannotRewrite {
    /// The file is not one the install rewrites, or is over its limit: its member is skipped ([`Skip::Unrewritable`]).
    Skipped(Unrewritable),
    /// Why the file could not be read or written: the install is refused, as any other I/O failure.
    Failed(Refused),
}

/// How a file the install rewrites is encoded, as its byte order mark says: UTF-8 with or without its mark, or UTF-16
/// whose mark names the byte order of its body. A file with no byte order mark is read as UTF-8; no other encoding is
/// guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    /// UTF-8, with no byte order mark.
    Utf8,
    /// UTF-8 with its byte order mark.
    Utf8Bom,
    /// UTF-16 in the byte order, whose byte order mark the file has.
    Utf16(Order),
}

/// The byte order a UTF-16 body's 16-bit units are in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    /// Little-endian.
    Little,
    /// Big-endian.
    Big,
}

/// The encoding `bytes` is in and the text it holds: UTF-8 when it starts with the UTF-8 mark or with none, UTF-16 in
/// the byte order its mark names. `None` for a body that is not text in the encoding its mark names: not UTF-8, a
/// UTF-16 body of an odd length, or one holding an unpaired surrogate, and for the mark of a UTF-32 little-endian file,
/// which this never reads as UTF-16.
fn decoded(bytes: &[u8]) -> Option<(Encoding, String)> {
    if let Some(body) = bytes.strip_prefix(UTF8_BOM) {
        return std::str::from_utf8(body).ok().map(|text| (Encoding::Utf8Bom, text.to_owned()));
    }
    if bytes.starts_with(UTF32_LE_BOM) {
        return None;
    }
    if let Some(body) = bytes.strip_prefix(UTF16_LE_BOM) {
        return from_utf16(body, Order::Little).map(|text| (Encoding::Utf16(Order::Little), text));
    }
    if let Some(body) = bytes.strip_prefix(UTF16_BE_BOM) {
        return from_utf16(body, Order::Big).map(|text| (Encoding::Utf16(Order::Big), text));
    }
    std::str::from_utf8(bytes).ok().map(|text| (Encoding::Utf8, text.to_owned()))
}

/// The text the UTF-16 body `body` holds, each pair of bytes a 16-bit unit in `order`'s byte order; `None` for an odd
/// body, or one holding an unpaired surrogate.
fn from_utf16(body: &[u8], order: Order) -> Option<String> {
    let (pairs, rest) = body.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let units: Vec<u16> = pairs.iter().map(|pair| bytes_to_unit(*pair, order)).collect();
    String::from_utf16(&units).ok()
}

/// The 16-bit unit the two bytes `pair` hold, in `order`'s byte order.
fn bytes_to_unit(pair: [u8; 2], order: Order) -> u16 {
    match order {
        Order::Little => u16::from_le_bytes(pair),
        Order::Big => u16::from_be_bytes(pair),
    }
}

/// The two bytes `unit` is written as, in `order`'s byte order.
fn unit_to_bytes(unit: u16, order: Order) -> [u8; 2] {
    match order {
        Order::Little => unit.to_le_bytes(),
        Order::Big => unit.to_be_bytes(),
    }
}

/// `text` in `encoding`, the byte order mark it had written before it.
fn encoded(text: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf8Bom => [UTF8_BOM, text.as_bytes()].concat(),
        Encoding::Utf16(order) => {
            let mut bytes = match order {
                Order::Little => UTF16_LE_BOM.to_vec(),
                Order::Big => UTF16_BE_BOM.to_vec(),
            };
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit_to_bytes(unit, order));
            }
            bytes
        }
    }
}

/// `text` with each run of characters equal to the `from` of a `(from, to)` of `pairs`, without regard to case, that
/// ends a path component replaced by its `to`, the longest `from` tried first. The match on whole components
/// ([`ends_a_component`]) is what keeps a tree off another whose name extends its own (`yaat`, `yaat-server`), so each
/// path is rewritten from its own tree. An empty `from` matches nothing.
fn replaced(text: &str, pairs: &[(String, String)]) -> String {
    // `ordinal_upper` maps each character to one, so the characters of `text` and of its uppercase line up.
    let mut wanted: Vec<(Vec<char>, &str)> = pairs
        .iter()
        .filter(|(from, _)| !from.is_empty())
        .map(|(from, to)| (ordinal_upper(from).chars().collect(), to.as_str()))
        .collect();
    wanted.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    let upper: Vec<char> = ordinal_upper(text).chars().collect();
    let characters: Vec<char> = text.chars().collect();
    let mut new = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(&character) = characters.get(index) {
        let rest = &upper[index..];
        let hit = wanted
            .iter()
            .find(|(from, _)| rest.starts_with(from) && ends_a_component(rest.get(from.len()).copied()));
        if let Some((from, to)) = hit {
            new.push_str(to);
            index += from.len();
        } else {
            new.push(character);
            index += 1;
        }
    }
    new
}

/// Whether `next`, the character after a box tree's path in a file, ends the tree's last path component: the end of the
/// text, or any character but one a folder name the box lays out is made of ([`is_layout_char`]), which would go on
/// with the name: so `{work}\yaat` is not matched at the start of `{work}\yaat-server`, nor `{work}` at the start of
/// `{work}.lock`, and each is matched before a separator, a quote or a list's `;`.
fn ends_a_component(next: Option<char>) -> bool {
    next.is_none_or(|next| !is_layout_char(next))
}

/// What the moves did, each list in archive order.
#[derive(Debug, PartialEq, Eq)]
struct Moves {
    /// Each file moved into place, with the stamp its handle read after the move, none when that read failed.
    moved: Vec<Moved>,
    /// The paths of the members, any of them, whose PC file something held open against the move ([`Placed::InUse`]),
    /// which the install skips as in use ([`Skip::InUse`]).
    in_use: Vec<String>,
}

/// Moves each of `entries` from `stage` into place below `root`, as [`move_one`] does against `manifest`, asking
/// `go_on`, given the files moved so far, before each one; else the interrupt once `go_on` says no, or why a file
/// failed, the files handled before it counted and left in place. What the moves did ([`Moves`]); a member left in place
/// ([`Placed::InPlace`]) is in neither list.
fn moved(stage: &Path, root: &Path, entries: &[Entry], manifest: &Manifest, mut go_on: impl FnMut(usize) -> bool) -> Result<Moves, Unmoved> {
    let now = SystemTime::now();
    let total = entries.len();
    let mut moves = Moves {
        moved: Vec::with_capacity(total),
        in_use: Vec::new(),
    };
    for (done, entry) in entries.iter().enumerate() {
        if !go_on(done) {
            return Err(Unmoved::Interrupted { done });
        }
        let placed = move_one(stage, root, entry, manifest, now)
            .map_err(|error| Unmoved::Failed(format!("{done} of {total} files were installed before {} failed: {error}", entry.path)))?;
        match placed {
            Placed::Moved(stamp) => moves.moved.push(Moved { entry: entry.clone(), stamp }),
            Placed::InPlace => {}
            Placed::InUse => moves.in_use.push(entry.path.clone()),
        }
    }
    Ok(moves)
}

/// How [`move_one`] put a member in place.
#[derive(Debug, PartialEq, Eq)]
enum Placed {
    /// Moved from the stage, with the stamp its handle read after the move; none when that read failed.
    Moved(Option<Stamp>),
    /// Left as it was: the PC's file changed during the run to the member's own bytes, or it records a root and changed
    /// during the run at all ([`Pc::InPlace`]).
    InPlace,
    /// Not moved: something holds its PC file open against the rename ([`held_open`], [`held_against_replacing`]),
    /// whatever the member.
    InUse,
}

/// Moves the staged file of `entry` below `stage` to the same path below `root`, replacing a file there, each folder
/// between `root` and it made when missing and refused when it is a link, a junction or not a folder. Its last write,
/// when past `now`, is set to `now` in the stage, before the move; the PC's file is then checked against `manifest`
/// once more ([`pc_file`]), just before the move, and one in place is left ([`Placed::InPlace`]): one already holding the
/// member's bytes, or one recording a root that a PC writer changed or made. Any member whose rename is refused because
/// something holds a file open ([`held_open`]) is not moved when its PC file is the one held, as it is when that file
/// cannot be opened for deletion ([`held_against_replacing`]) ([`Placed::InUse`]); else the rename is tried once more,
/// and its error, a held staged file's included, is the failure. Any other rename error is the failure at once.
/// The stamp read after the move through a handle opened on the staged file before it, the clamp included, is returned:
/// the moved file's own, never that of a file put at its path after the move. None when the handle cannot be opened or
/// read, which costs the file its hash cache entry only.
fn move_one(stage: &Path, root: &Path, entry: &Entry, manifest: &Manifest, now: SystemTime) -> io::Result<Placed> {
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
    // A file refused here was checked before the moves; one that changed in between is the reason said, of the kind a
    // refetch may clear ([`Refused::transient`]), and one in place is left as it is.
    match pc_file(root, entry, manifest).map_err(io::Error::other)? {
        Pc::AsHeld => {}
        Pc::InPlace => return Ok(Placed::InPlace),
        Pc::Changed => return Err(io::Error::other(format!("{path} changed during the run"))),
    }
    // Its attributes alone, every sharing given: the handle neither refuses the move nor any other opener.
    let handle = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&source);
    let target = below(root, path);
    if let Err(error) = fs::rename(&source, &target) {
        if !held_open(&error) {
            return Err(error);
        }
        if held_against_replacing(&target) {
            return Ok(Placed::InUse);
        }
        // The PC's file is free or missing: the staged file was held, or the PC file's holder let go since. One more
        // rename tells them apart, and its error is the failure.
        fs::rename(&source, &target)?;
    }
    // No stamp is the right answer for a handle that cannot be opened or read: the file is installed, its hash not kept.
    Ok(Placed::Moved(
        handle.and_then(|file| file.metadata()).ok().map(|metadata| Stamp::of(&metadata)),
    ))
}

/// Whether a rename failed because something holds one of its files open: a sharing or lock violation, or the access
/// denial a rename of or onto a file open without delete sharing ends with.
fn held_open(error: &io::Error) -> bool {
    error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        .is_some_and(|code| [ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, ERROR_ACCESS_DENIED].contains(&code))
}

/// Whether something holds the PC's file at `target` open against a rename over it: it cannot be opened for deletion
/// with every sharing given, failing as a held file does ([`held_open`]). A missing file, or one whose holders all share
/// its deletion, is not held.
fn held_against_replacing(target: &Path) -> bool {
    // Opened and closed at once: only whether it opens counts.
    OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(target)
        .is_err_and(|error| held_open(&error))
}

/// Writes the hash cache at `path` for the files an install put in place: an entry for each of `installed.moved`, at
/// the stamp its handle read after the move and the hash [`unpack`] verified, kept against `started`, the file time the
/// moves began, so the next run reads no file whose size and last write still match. Nothing is written when the install
/// is not complete ([`Installed::complete`]): only an archive whose skips a later run repeats none of is one whose files
/// the run after it may take as the PC's, an incomplete one costing the next run a rehash. A file the install rewrote
/// ([`records_a_root`]) is left out, its bytes
/// no longer the hash's, and so is one whose stamp could not be read after its move. The cache the file at `path` holds
/// is read first, so entries another run saved meanwhile are kept; one missing or corrupt starts empty. The cache is
/// saved only when it kept at least one entry.
fn cached(path: &Path, installed: &Installed, started: u64) {
    if !installed.complete() {
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

    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    use super::{
        Checked, Index, Installed, Moved, Moves, Refused, Skip, Unmet, Unmoved, Unrewritable, cached, checked, ending, moved, reasons, rewritten,
        said, stage_made, success_line, tracked, unmet_of, unpacked,
    };
    use crate::boxside::outputs::OUTPUTS_BUDGET;
    use crate::outputs::archive::{Entry, PcFiles, UnpackError, pack};
    use crate::outputs::cache::{HashCache, file_time_now};
    use crate::outputs::glob::Globs;
    use crate::outputs::hash::{file_sha256, sha256_of};
    use crate::outputs::manifest::{Chosen, Held, HeldBy, Manifest};
    use crate::outputs::walk::{Stamp, walk};
    use crate::remote::client::End;
    use crate::remote::git::GitError;

    const BOX_WORK: &str = r"D:\gate-remote\work\app-1a2b3c4d";
    const PC_ROOT: &str = r"C:\src\app";

    /// The trees of a run of one repo: the box's work folder, rewritten to the worktree's root.
    fn one_tree() -> Vec<(String, String)> {
        vec![(BOX_WORK.to_owned(), PC_ROOT.to_owned())]
    }

    /// A scratch folder for one test, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-install-{}-{name}", std::process::id()));
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

    /// The archive's entry for `path` holding `bytes`, its size and hash theirs.
    fn member(path: &str, bytes: &[u8]) -> Entry {
        let (sha256, size) = sha256_of(bytes).expect("the bytes hash");
        Entry {
            path: path.to_owned(),
            size,
            sha256,
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

    /// `text` as UTF-16 little-endian bytes, the byte order mark first.
    fn utf16_le(text: &str) -> Vec<u8> {
        let mut bytes = b"\xFF\xFE".to_vec();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    /// `text` as UTF-16 big-endian bytes, the byte order mark first.
    fn utf16_be(text: &str) -> Vec<u8> {
        let mut bytes = b"\xFE\xFF".to_vec();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        bytes
    }

    #[test]
    fn a_member_outside_the_globs_or_under_an_excluded_folder_is_refused() {
        let root = scratch("globs");
        let manifest = Manifest::default();
        for path in ["obj/a.dll", "bin/.tmp/a.dll", ".git/bin/a", "src/bin/TestResults/a.trx"] {
            let refused = checked(&root, &[entry("bin/ok.dll"), entry(path)], &bin_globs(), &manifest);
            assert_eq!(
                refused,
                Err(Refused::deterministic(format!("the archive member {path} is not under the output globs"))),
                "{path}"
            );
        }
        assert_eq!(checked(&root, &[entry("src/bin/a.dll")], &bin_globs(), &manifest), Ok(Checked::default()));
    }

    #[test]
    fn a_fetched_trx_a_glob_names_is_installed() {
        let root = scratch("named-trx");
        let manifest = Manifest::default();
        let naming = Globs::new(&["**/bin/**".to_owned(), "tests/*/bin/*/*/TestResults/*.trx".to_owned()]).expect("the globs can be matched");
        let trx = "tests/p/bin/Release/net10.0/TestResults/sim.trx";

        assert_eq!(
            checked(&root, &[entry(trx)], &naming, &manifest),
            Ok(Checked::default()),
            "the glob that names TestResults brings the TRX in"
        );
        assert_eq!(
            checked(&root, &[entry(trx)], &bin_globs(), &manifest),
            Err(Refused::deterministic(format!("the archive member {trx} is not under the output globs"))),
            "the default globs bring in nothing under TestResults"
        );
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
            assert_eq!(refused, Err(Refused::transient(format!("{path} changed during the run"))), "{path}");
        }
        fs::create_dir_all(root.join("bin").join("folder.dll")).expect("a folder in a file's place");
        let folder = checked(&root, &[entry("bin/folder.dll")], &bin_globs(), &manifest);
        assert_eq!(folder, Err(Refused::transient("bin/folder.dll is not a file".to_owned())));
        assert_eq!(
            checked(&root, &[entry("bin/same.dll"), entry("bin/absent.dll")], &bin_globs(), &manifest),
            Ok(Checked::default())
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

        let held_only = Checked {
            in_use: vec!["bin/held.dll".to_owned()],
            in_place: Vec::new(),
        };
        assert_eq!(skipped, Ok(held_only), "only the held file is skipped");
        assert_eq!(checked(&root, &entries, &bin_globs(), &manifest), Ok(Checked::default()), "released");
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

        assert_eq!(rewritten(&stage, &entries, &one_tree()), Ok(Vec::new()));

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
    fn a_one_repo_work_folder_is_rewritten_only_where_its_last_name_ends() {
        let stage = scratch("rewrite-boundary");
        let doubled = |path: &str| path.replace('\\', r"\\");
        let box_escaped = doubled(BOX_WORK);
        // Each root is followed by `"`, `;`, `|`, `/`, `\` and the end of the text; the work folder's `.lock` and a
        // folder whose name runs on past the work folder's are no paths in it.
        let raw = |root: &str| format!(r#"{root}" {root}; {root}| {root}/x {root}\y {BOX_WORK}.lock {BOX_WORK}-x\a {root}"#);
        let escaped = |root: &str| format!(r#"{root}" {root}; {root}| {root}/x {root}\\y {box_escaped}.lock {box_escaped}-x\\a {root}"#);
        let coverage = put(&stage, "bin/.msCoverageSourceRootsMapping_App", raw(BOX_WORK).as_bytes());
        let manifest = put(&stage, "bin/MvcTestingAppManifest.json", escaped(&box_escaped).as_bytes());
        let entries = [entry("bin/.msCoverageSourceRootsMapping_App"), entry("bin/MvcTestingAppManifest.json")];

        assert_eq!(rewritten(&stage, &entries, &one_tree()), Ok(Vec::new()));

        assert_eq!(fs::read_to_string(&coverage).expect("the file is there"), raw(PC_ROOT), "raw");
        assert_eq!(
            fs::read_to_string(&manifest).expect("the file is there"),
            escaped(&doubled(PC_ROOT)),
            "escaped"
        );
    }

    #[test]
    fn a_pair_rewrites_each_tree_to_its_own_root() {
        let stage = scratch("rewrite-pair");
        let trees = vec![
            (format!(r"{BOX_WORK}\app"), PC_ROOT.to_owned()),
            (format!(r"{BOX_WORK}\lib"), r"C:\src\lib".to_owned()),
        ];
        let escaped_box = BOX_WORK.replace('\\', r"\\");
        let json = format!(
            r#"{{"App":"{}\\app\\src","Lib":"{escaped_box}\\LIB\\src","Work":"{escaped_box}\\other"}}"#,
            escaped_box.to_uppercase()
        );
        let raw = format!("{}\\app\\src;{BOX_WORK}\\Lib\\tests;{BOX_WORK}\\other\n", BOX_WORK.to_lowercase());
        let manifest = put(&stage, "bin/MvcTestingAppManifest.json", json.as_bytes());
        let coverage = put(&stage, "bin/.msCoverageSourceRootsMapping_App", raw.as_bytes());
        let entries = [entry("bin/MvcTestingAppManifest.json"), entry("bin/.msCoverageSourceRootsMapping_App")];

        assert_eq!(rewritten(&stage, &entries, &trees), Ok(Vec::new()));

        assert_eq!(
            fs::read_to_string(&manifest).expect("the file is there"),
            format!(r#"{{"App":"C:\\src\\app\\src","Lib":"C:\\src\\lib\\src","Work":"{escaped_box}\\other"}}"#),
            "escaped, any case: each tree to its own root, the work folder's other folder left alone"
        );
        assert_eq!(
            fs::read_to_string(&coverage).expect("the file is there"),
            format!("C:\\src\\app\\src;C:\\src\\lib\\tests;{BOX_WORK}\\other\n"),
            "raw, any case"
        );
    }

    #[test]
    fn a_sibling_whose_name_extends_another_s_is_rewritten_to_its_own_root() {
        let stage = scratch("rewrite-prefix");
        let raw = format!("{BOX_WORK}\\yaat-server\\src;{BOX_WORK}\\yaat\\src;{BOX_WORK}\\yaat;{BOX_WORK}\\yaatx\n");
        let escaped_box = BOX_WORK.replace('\\', r"\\");
        let json = format!(r#"{{"Server":"{escaped_box}\\yaat-server","App":"{escaped_box}\\yaat"}}"#);
        let coverage = put(&stage, "bin/.msCoverageSourceRootsMapping_App", raw.as_bytes());
        let manifest = put(&stage, "bin/MvcTestingAppManifest.json", json.as_bytes());
        let entries = [entry("bin/.msCoverageSourceRootsMapping_App"), entry("bin/MvcTestingAppManifest.json")];
        // The shorter name first, so only the match on whole components keeps it off the longer one.
        for trees in [
            vec![
                (format!(r"{BOX_WORK}\yaat"), r"C:\src\yaat".to_owned()),
                (format!(r"{BOX_WORK}\yaat-server"), r"C:\src\yaat-server".to_owned()),
            ],
            vec![
                (format!(r"{BOX_WORK}\yaat-server"), r"C:\src\yaat-server".to_owned()),
                (format!(r"{BOX_WORK}\yaat"), r"C:\src\yaat".to_owned()),
            ],
        ] {
            fs::write(&coverage, &raw).expect("the file can be written");
            fs::write(&manifest, &json).expect("the file can be written");

            assert_eq!(rewritten(&stage, &entries, &trees), Ok(Vec::new()));

            assert_eq!(
                fs::read_to_string(&coverage).expect("the file is there"),
                format!("C:\\src\\yaat-server\\src;C:\\src\\yaat\\src;C:\\src\\yaat;{BOX_WORK}\\yaatx\n"),
                "raw: {trees:?}"
            );
            assert_eq!(
                fs::read_to_string(&manifest).expect("the file is there"),
                r#"{"Server":"C:\\src\\yaat-server","App":"C:\\src\\yaat"}"#,
                "escaped: {trees:?}"
            );
        }
    }

    #[test]
    fn a_utf16_file_to_rewrite_is_rewritten_in_its_own_byte_order_with_its_bom() {
        for (order, encode) in [("LE", utf16_le as fn(&str) -> Vec<u8>), ("BE", utf16_be)] {
            let stage = scratch(&format!("rewrite-utf16-{}", order.to_lowercase()));
            let recorded = format!("{BOX_WORK}\\src;{BOX_WORK}\\tests\n");
            let full = put(&stage, "bin/.msCoverageSourceRootsMapping_App", &encode(&recorded));
            let old = SystemTime::now() - Duration::from_hours(2);
            File::options()
                .write(true)
                .open(&full)
                .and_then(|file| file.set_modified(old))
                .expect("the last write can be set");
            let entries = [entry("bin/.msCoverageSourceRootsMapping_App")];

            assert_eq!(rewritten(&stage, &entries, &one_tree()), Ok(Vec::new()), "{order}");

            let expected = encode(&format!("{PC_ROOT}\\src;{PC_ROOT}\\tests\n"));
            assert_eq!(fs::read(&full).expect("the file is there"), expected, "UTF-16 {order}, its BOM kept");
            let modified = fs::metadata(&full).and_then(|metadata| metadata.modified()).expect("a last write");
            assert_eq!(modified, old, "{order} keeps its last write");
        }
    }

    /// Asserts that the coverage mapping of `bytes`, in a stage of its own named `name`, is skipped as text the install
    /// cannot rewrite, and so is not installed with the box's paths in it.
    fn assert_not_text(name: &str, bytes: &[u8]) {
        let stage = scratch(name);
        put(&stage, "bin/.msCoverageSourceRootsMapping_App", bytes);

        assert_eq!(
            rewritten(&stage, &[entry("bin/.msCoverageSourceRootsMapping_App")], &one_tree()),
            Ok(vec![(
                "bin/.msCoverageSourceRootsMapping_App".to_owned(),
                Skip::Unrewritable(Unrewritable::NotText)
            )])
        );
    }

    #[test]
    fn a_file_to_rewrite_that_is_not_text_is_skipped() {
        assert_not_text("rewrite-not-text", b"\xEF\xBB\xBF\xFF\xFE");
    }

    #[test]
    fn a_utf16_file_of_odd_length_cannot_be_rewritten_and_is_skipped() {
        assert_not_text("rewrite-utf16-odd", b"\xFF\xFEa");
    }

    #[test]
    fn a_utf16_file_with_an_unpaired_surrogate_cannot_be_rewritten_and_is_skipped() {
        assert_not_text("rewrite-utf16-surrogate", b"\xFF\xFE\x00\xD8");
    }

    #[test]
    fn a_utf32_le_file_cannot_be_rewritten_and_is_skipped() {
        // `FF FE 00 00` then `A` as a 32-bit little-endian unit: the mark of a UTF-32 LE file, never read as UTF-16 LE.
        assert_not_text("rewrite-utf32-le", b"\xFF\xFE\x00\x00A\x00\x00\x00");
    }

    #[test]
    fn a_file_to_rewrite_over_16_mib_is_skipped_and_one_of_16_mib_rewritten() {
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
            rewritten(&stage, &entries, &one_tree()),
            Ok(vec![(
                "bin/MvcTestingAppManifest.json".to_owned(),
                Skip::Unrewritable(Unrewritable::TooLarge(16 * 1024 * 1024 + 1))
            )])
        );
        sized(16 * 1024 * 1024);
        assert_eq!(rewritten(&stage, &entries, &one_tree()), Ok(Vec::new()), "16 MiB is not over");
    }

    #[test]
    fn an_unrewritable_file_is_said_with_its_size() {
        let installed = Installed {
            count: 0,
            bytes: 0,
            compressed: 0,
            skipped: vec![(
                "bin/big.json".to_owned(),
                Skip::Unrewritable(Unrewritable::TooLarge(16 * 1024 * 1024 + 1)),
            )],
            moved: Vec::new(),
        };

        let line = "gate: remote outputs not fully brought back: bin/big.json cannot be rewritten (16777217 bytes, over 16 MiB)";
        assert_eq!(said(&installed, 0.0).first().map(String::as_str), Some(line));
    }

    /// An install that skipped a file it cannot rewrite is whole, which the marker, `brought_back` and the hash cache
    /// all follow; one in-use skip beside it makes it short again, since a later run finds the holder gone.
    #[test]
    fn an_in_use_skip_beside_an_unrewritable_one_leaves_the_install_incomplete() {
        let mapping = "bin/.msCoverageSourceRootsMapping_App".to_owned();
        let unrewritable = Installed {
            count: 1,
            bytes: 1,
            compressed: 1,
            skipped: vec![(mapping.clone(), Skip::Unrewritable(Unrewritable::NotText))],
            moved: Vec::new(),
        };
        assert!(unrewritable.complete(), "a file it cannot rewrite leaves the install whole");

        let held = Installed {
            count: 1,
            bytes: 1,
            compressed: 1,
            skipped: vec![
                (mapping, Skip::Unrewritable(Unrewritable::NotText)),
                ("bin/held.dll".to_owned(), Skip::InUse),
            ],
            moved: Vec::new(),
        };
        assert!(!held.complete(), "the in-use skip leaves it short of the box's outputs");
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
            assert_eq!(line, [format!("gate: remote outputs not brought back: {why}")]);
        }
        assert_eq!(unmet_of(GitError::Interrupted), Unmet::Interrupted { done: 0, total: 0 });
        let (end, line) = ending(unmet_of(GitError::Interrupted));
        assert!(matches!(end, End::Interrupted) && line.is_empty(), "{end:?} {line:?}");
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

        let moved = moved(&stage, &root, &[entry("bin/a.txt")], &Manifest::default(), |_| true)
            .expect("the file is moved")
            .moved;

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
        let moved = moved(&stage, &root, &entries, &Manifest::default(), |_| true)
            .expect("both files are moved")
            .moved;
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
        let moved = moved(&stage, &root, &entries, &Manifest::default(), |_| true)
            .expect("every file is moved")
            .moved;
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
        let moved = moved(&stage, &root, &[entry("bin/a.txt")], &Manifest::default(), |_| true)
            .expect("the file is moved")
            .moved;
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
    fn a_pc_file_rewritten_with_the_member_s_bytes_after_the_checks_is_left_in_place() {
        let folder = scratch("move-same-bytes");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        put(&stage, "bin/a.txt", b"box");
        let staged = put(&stage, "bin/b.txt", b"box");
        let pc = put(&root, "bin/b.txt", b"pc");
        let manifest = manifest_of(&root, &["bin/b.txt"]);
        let entries = [entry("bin/a.txt"), member("bin/b.txt", b"box")];

        let installed = moved(&stage, &root, &entries, &manifest, |done| {
            if done == 1 {
                fs::write(&pc, b"box").expect("the file can be written");
            }
            true
        });

        let paths: Vec<&str> = installed
            .as_ref()
            .map(|moves| moves.moved.iter().map(|moved| moved.entry.path.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(paths, ["bin/a.txt"], "only a is moved: {installed:?}");
        assert_eq!(fs::read_to_string(&pc).expect("b stays"), "box");
        assert!(staged.exists(), "the staged b is not moved");
        assert_eq!(fs::read_to_string(root.join("bin").join("a.txt")).expect("a is moved"), "box");
    }

    #[test]
    fn a_pc_file_rewritten_after_the_checks_to_the_member_s_size_but_other_bytes_is_refused_just_before_its_move() {
        let folder = scratch("move-same-size");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        put(&stage, "bin/a.txt", b"box");
        put(&stage, "bin/b.txt", b"box");
        let pc = put(&root, "bin/b.txt", b"pc");
        let manifest = manifest_of(&root, &["bin/b.txt"]);
        let entries = [entry("bin/a.txt"), member("bin/b.txt", b"box")];

        let unmoved = moved(&stage, &root, &entries, &manifest, |done| {
            if done == 1 {
                fs::write(&pc, b"xyz").expect("the file can be written");
            }
            true
        });

        let reason = "1 of 2 files were installed before bin/b.txt failed: bin/b.txt changed during the run";
        assert_eq!(unmoved, Err(Unmoved::Failed(reason.to_owned())));
        assert_eq!(fs::read_to_string(&pc).expect("b stays"), "xyz");
    }

    #[test]
    fn a_stage_folder_there_before_the_install_is_not_its_to_remove() {
        let folder = scratch("stage-there");
        let source = folder.join("source");
        put(&source, "bin/a.txt", b"a");
        let found = walk(&source, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        let chosen: Vec<Chosen<'_>> = found.iter().map(Chosen::from).collect();
        let mut archive = Vec::new();
        pack(&mut archive, &source, &chosen, &HeldBy::new()).expect("the file packs");
        let stage = folder.join("stage");
        put(&stage, "kept.txt", b"another run's");

        let refused = unpacked_from(&archive[..], &stage, &folder, &Manifest::default());

        assert!(
            refused.as_ref().is_err_and(|error| error.is_fault(io::ErrorKind::AlreadyExists)),
            "{refused:?}"
        );
        assert!(!stage_made(&refused), "a folder there before is not the install's to remove");
        assert_eq!(fs::read_to_string(stage.join("kept.txt")).expect("its file survives"), "another run's");
        let fresh = unpacked_from(&archive[..], &folder.join("fresh"), &folder, &Manifest::default());
        assert!(fresh.is_ok() && stage_made(&fresh), "{fresh:?}");
        let garbage = unpacked_from(&b"not an archive"[..], &folder.join("garbage"), &folder, &Manifest::default());
        assert!(garbage.is_err() && stage_made(&garbage), "{garbage:?}");
    }

    #[test]
    fn an_interrupt_before_the_first_move_says_no_count() {
        let (end, line) = ending(Unmet::Interrupted { done: 0, total: 3 });
        assert!(matches!(end, End::Interrupted) && line.is_empty(), "{end:?} {line:?}");

        let (end, line) = ending(Unmet::Interrupted { done: 1, total: 3 });
        assert!(matches!(end, End::Interrupted), "{end:?}");
        let counted = "gate: remote outputs not brought back: 1 of 3 files were installed before the interrupt";
        assert_eq!(line, [counted]);

        let (end, line) = ending(Unmet::Refused(Refused::transient("bin/a.dll changed during the run".to_owned())));
        assert!(matches!(end, End::Status(1)), "{end:?}");
        let refused = "gate: remote outputs not brought back: bin/a.dll changed during the run";
        assert_eq!(line, [refused]);
    }

    #[test]
    fn the_checks_refuse_in_their_order_across_members() {
        let root = scratch("check-order");
        put(&root, "bin/changed.dll", b"abc");
        let both = put(&root, "bin/both.dll", b"abc");
        let manifest = manifest_of(&root, &["bin/changed.dll", "bin/both.dll"]);
        put(&root, "bin/changed.dll", b"abcd");

        let globs_first = checked(&root, &[entry("bin/changed.dll"), entry("obj/a.dll")], &bin_globs(), &manifest);
        assert_eq!(
            globs_first,
            Err(Refused::deterministic(
                "the archive member obj/a.dll is not under the output globs".to_owned()
            ))
        );

        fs::write(&both, b"abcd").expect("the file can be written");
        let held = fs::OpenOptions::new().read(true).share_mode(0).open(&both).expect("the file can be held");
        let changed_first = checked(&root, &[entry("bin/both.dll")], &bin_globs(), &manifest);
        drop(held);
        assert_eq!(changed_first, Err(Refused::transient("bin/both.dll changed during the run".to_owned())));
    }

    #[test]
    fn a_refusal_names_ten_changed_files_then_counts_the_rest() {
        let root = scratch("changed-many");
        let paths: Vec<String> = (10..22).map(|number| format!("bin/f{number}.dll")).collect();
        for path in &paths {
            put(&root, path, b"abc");
        }
        let manifest = manifest_of(&root, &paths.iter().map(String::as_str).collect::<Vec<_>>());
        for path in &paths {
            put(&root, path, b"abcd");
        }
        let entries: Vec<Entry> = paths.iter().map(|path| entry(path)).collect();

        let refused = checked(&root, &entries, &bin_globs(), &manifest).expect_err("every file changed");
        let (end, lines) = ending(Unmet::Refused(refused));

        assert!(matches!(end, End::Status(1)), "{end:?}");
        let mut expected: Vec<String> = paths[..10]
            .iter()
            .map(|path| format!("gate: remote outputs not brought back: {path} changed during the run"))
            .collect();
        expected.push("gate: remote outputs not brought back: 2 more files changed during the run".to_owned());
        assert_eq!(lines, expected);
    }

    #[test]
    fn a_refusal_of_exactly_ten_changed_files_names_each_and_counts_none() {
        let root = scratch("changed-ten");
        let paths: Vec<String> = (10..20).map(|number| format!("bin/f{number}.dll")).collect();
        for path in &paths {
            put(&root, path, b"abc");
        }
        let manifest = manifest_of(&root, &paths.iter().map(String::as_str).collect::<Vec<_>>());
        for path in &paths {
            put(&root, path, b"abcd");
        }
        let entries: Vec<Entry> = paths.iter().map(|path| entry(path)).collect();

        let refused = checked(&root, &entries, &bin_globs(), &manifest).expect_err("every file changed");
        let (_, lines) = ending(Unmet::Refused(refused));

        let expected: Vec<String> = paths
            .iter()
            .map(|path| format!("gate: remote outputs not brought back: {path} changed during the run"))
            .collect();
        assert_eq!(lines, expected);
    }

    #[test]
    fn a_pc_file_that_is_not_a_file_is_named_among_the_changed_ones_in_archive_order() {
        let root = scratch("changed-and-folder");
        put(&root, "bin/a.dll", b"abc");
        put(&root, "bin/c.dll", b"abc");
        let manifest = manifest_of(&root, &["bin/a.dll", "bin/c.dll"]);
        put(&root, "bin/a.dll", b"abcd");
        fs::create_dir_all(root.join("bin").join("b.dll")).expect("a folder in a file's place");
        put(&root, "bin/c.dll", b"abcd");
        let entries = [entry("bin/a.dll"), entry("bin/b.dll"), entry("bin/c.dll")];

        let refused = checked(&root, &entries, &bin_globs(), &manifest).expect_err("the files changed");
        let (_, lines) = ending(Unmet::Refused(refused));

        assert_eq!(
            lines,
            [
                "gate: remote outputs not brought back: bin/a.dll changed during the run",
                "gate: remote outputs not brought back: bin/b.dll is not a file",
                "gate: remote outputs not brought back: bin/c.dll changed during the run",
            ]
        );
    }

    #[test]
    fn a_pc_file_of_the_member_s_size_counts_in_place_only_with_the_member_s_bytes() {
        let root = scratch("same-size");
        put(&root, "bin/other.dll", b"pc");
        put(&root, "bin/same.dll", b"pc");
        let manifest = manifest_of(&root, &["bin/other.dll", "bin/same.dll"]);
        put(&root, "bin/other.dll", b"xyz");
        put(&root, "bin/same.dll", b"box");

        let other = checked(&root, &[member("bin/other.dll", b"box")], &bin_globs(), &manifest);
        assert_eq!(other, Err(Refused::transient("bin/other.dll changed during the run")));
        let same = checked(&root, &[member("bin/same.dll", b"box")], &bin_globs(), &manifest);
        let in_place = Checked {
            in_use: Vec::new(),
            in_place: vec!["bin/same.dll".to_owned()],
        };
        assert_eq!(same, Ok(in_place));
    }

    #[test]
    fn a_pc_file_the_manifest_lacked_that_now_holds_the_member_s_bytes_counts_in_place() {
        let root = scratch("absent-in-place");
        let manifest = Manifest::default();
        put(&root, "bin/absent.dll", b"box");

        let checks = checked(&root, &[member("bin/absent.dll", b"box")], &bin_globs(), &manifest);

        let in_place = Checked {
            in_use: Vec::new(),
            in_place: vec!["bin/absent.dll".to_owned()],
        };
        assert_eq!(checks, Ok(in_place));
    }

    /// The checks of `path`, a member recording a root whose PC file a PC writer rewrote during the run after the
    /// manifest recorded it as `before`, or made when `before` is none: it counts in place, which the install neither
    /// moves nor says.
    fn assert_root_recording_pc_file_kept(name: &str, path: &str, before: Option<&[u8]>) {
        let root = scratch(name);
        if let Some(before) = before {
            put(&root, path, before);
        }
        let held: Vec<&str> = before.iter().map(|_| path).collect();
        let manifest = manifest_of(&root, &held);
        put(&root, path, b"the PC writer's roots");

        let checks = checked(&root, &[member(path, b"the builder's roots")], &bin_globs(), &manifest);

        let kept = Checked {
            in_use: Vec::new(),
            in_place: vec![path.to_owned()],
        };
        assert_eq!(checks, Ok(kept));
    }

    #[test]
    fn the_checks_take_a_root_recording_pc_file_untouched_or_deleted_as_held_and_refuse_another_deleted_one() {
        let root = scratch("roots-edges");
        put(&root, "bin/.msCoverageSourceRootsMapping_A", b"pc");
        let deleted = put(&root, "bin/MvcTestingAppManifest.json", b"pc");
        let gone = put(&root, "bin/gone.dll", b"pc");
        let manifest = manifest_of(
            &root,
            &["bin/.msCoverageSourceRootsMapping_A", "bin/MvcTestingAppManifest.json", "bin/gone.dll"],
        );
        fs::remove_file(&deleted).expect("the file can be removed");
        fs::remove_file(&gone).expect("the file can be removed");
        let roots = [
            member("bin/.msCoverageSourceRootsMapping_A", b"box"),
            member("bin/MvcTestingAppManifest.json", b"box"),
        ];

        assert_eq!(checked(&root, &roots, &bin_globs(), &manifest), Ok(Checked::default()));
        let other = checked(&root, &[member("bin/gone.dll", b"box")], &bin_globs(), &manifest);
        assert_eq!(other, Err(Refused::transient("bin/gone.dll changed during the run")));
    }

    #[test]
    fn a_root_recording_pc_file_deleted_during_the_run_is_installed() {
        let folder = scratch("roots-deleted");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        let path = "bin/.msCoverageSourceRootsMapping_X";
        let recorded = format!("{BOX_WORK}\\src\n");
        put(&stage, path, recorded.as_bytes());
        let pc = put(&root, path, b"pc");
        let manifest = manifest_of(&root, &[path]);
        fs::remove_file(&pc).expect("the file can be removed");
        let entries = [member(path, recorded.as_bytes())];

        assert_eq!(checked(&root, &entries, &bin_globs(), &manifest), Ok(Checked::default()));
        assert_eq!(rewritten(&stage, &entries, &one_tree()), Ok(Vec::new()));
        let installed = moved(&stage, &root, &entries, &manifest, |_| true);

        let paths: Vec<&str> = installed
            .as_ref()
            .map(|moves| moves.moved.iter().map(|moved| moved.entry.path.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(paths, [path], "the member is moved: {installed:?}");
        assert_eq!(fs::read_to_string(&pc).expect("the file lands"), format!("{PC_ROOT}\\src\n"));
    }

    /// The sharing a holder gives that lets a move rename over the held file or away from it.
    const SHARING_DELETION: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    /// The sharing a holder gives that refuses a move the held file.
    const NOT_SHARING_DELETION: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;

    /// Moves `bin/a.txt`, `path` and `bin/c.txt`, the PC file of `path` held open from the second move on, sharing
    /// reads and writes but not its deletion ([`moved_holding`]); what the moves returned, and the PC file's bytes after
    /// them.
    fn moved_onto_a_held_pc_file(name: &str, path: &str) -> (Result<Moves, Unmoved>, Vec<u8>) {
        let folder = scratch(name);
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        put(&stage, "bin/a.txt", b"box");
        put(&stage, path, b"box");
        put(&stage, "bin/c.txt", b"box");
        let pc = put(&root, path, b"pc");
        let manifest = manifest_of(&root, &[path]);
        let entries = [entry("bin/a.txt"), member(path, b"box"), entry("bin/c.txt")];

        let installed = moved_holding(&stage, &root, &entries, &manifest, &[(&pc, NOT_SHARING_DELETION)]);

        (installed, fs::read(&pc).expect("the PC file stays"))
    }

    /// Moves `entries` from `stage` to `root` against `manifest`, holding each file of `holds` open from the second move
    /// on for reading, with the sharing paired with it; what the moves returned.
    fn moved_holding(stage: &Path, root: &Path, entries: &[Entry], manifest: &Manifest, holds: &[(&Path, u32)]) -> Result<Moves, Unmoved> {
        let mut held = Vec::new();
        let installed = moved(stage, root, entries, manifest, |done| {
            if done == 1 {
                for (full, sharing) in holds {
                    let file = fs::OpenOptions::new()
                        .read(true)
                        .share_mode(*sharing)
                        .open(full)
                        .expect("the file can be held");
                    held.push(file);
                }
            }
            true
        });
        drop(held);
        installed
    }

    /// Asserts that a PC file held open at its move, `path` recording a root or not, is skipped as in use, the members
    /// around it moved and its bytes untouched.
    fn assert_held_at_its_move_is_skipped(name: &str, path: &str) {
        let (installed, pc) = moved_onto_a_held_pc_file(name, path);

        let moves = installed.expect("the moves end");
        let paths: Vec<&str> = moves.moved.iter().map(|moved| moved.entry.path.as_str()).collect();
        assert_eq!(paths, ["bin/a.txt", "bin/c.txt"], "the members around it are moved: {moves:?}");
        assert_eq!(moves.in_use, [path], "the held file is skipped as in use");
        assert_eq!(pc, b"pc");
    }

    #[test]
    fn a_root_recording_pc_file_held_open_at_its_move_is_skipped_as_in_use() {
        assert_held_at_its_move_is_skipped("move-roots-held", "bin/.msCoverageSourceRootsMapping_X");
    }

    #[test]
    fn a_pc_file_held_open_at_its_move_is_skipped_as_in_use() {
        assert_held_at_its_move_is_skipped("move-held", "bin/b.dll");
    }

    /// How the PC's file of a member whose staged file is held at its move stands.
    #[derive(Clone, Copy)]
    enum PcSide {
        /// There is none.
        Missing,
        /// It is there, and nothing holds it.
        Free,
        /// It is there, held open sharing its deletion, so a rename over it goes through.
        HeldSharingDeletion,
    }

    /// Asserts that the staged file of a member held open at its move, sharing reads and writes but not its deletion,
    /// fails the install whatever the PC's file is (`pc_side`), for a member recording a root and one that does not:
    /// the file handled before it counted, and the PC's file untouched or still missing.
    fn assert_staged_hold_fails(name: &str, pc_side: PcSide) {
        for (kind, path) in [("roots", "bin/.msCoverageSourceRootsMapping_X"), ("plain", "bin/b.dll")] {
            let folder = scratch(&format!("{name}-{kind}"));
            let (stage, root) = (folder.join("stage"), folder.join("root"));
            put(&stage, "bin/a.txt", b"box");
            let staged = put(&stage, path, b"box");
            fs::create_dir_all(&root).expect("the root can be made");
            let pc = match pc_side {
                PcSide::Missing => None,
                PcSide::Free | PcSide::HeldSharingDeletion => Some(put(&root, path, b"pc")),
            };
            let manifest = pc.as_ref().map_or_else(Manifest::default, |_| manifest_of(&root, &[path]));
            let mut holds = vec![(staged.as_path(), NOT_SHARING_DELETION)];
            if let (PcSide::HeldSharingDeletion, Some(pc)) = (pc_side, &pc) {
                holds.push((pc.as_path(), SHARING_DELETION));
            }
            let entries = [entry("bin/a.txt"), member(path, b"box")];

            let installed = moved_holding(&stage, &root, &entries, &manifest, &holds);

            let reason = format!("1 of 2 files were installed before {path} failed:");
            assert!(
                matches!(&installed, Err(Unmoved::Failed(why)) if why.starts_with(&reason)),
                "{path}: {installed:?}"
            );
            match &pc {
                Some(pc) => assert_eq!(fs::read(pc).expect("the PC file stays"), b"pc", "{path}"),
                None => assert!(!super::below(&root, path).exists(), "nothing is put in the place of {path}"),
            }
        }
    }

    #[test]
    fn a_member_whose_staged_file_is_held_open_with_no_pc_file_fails_the_install() {
        assert_staged_hold_fails("move-staged-held", PcSide::Missing);
    }

    #[test]
    fn a_member_whose_staged_file_is_held_open_over_a_free_pc_file_fails_the_install() {
        assert_staged_hold_fails("move-staged-held-pc-free", PcSide::Free);
    }

    #[test]
    fn a_member_whose_staged_file_is_held_open_over_a_pc_file_held_sharing_its_deletion_fails_the_install() {
        assert_staged_hold_fails("move-staged-held-pc-shared", PcSide::HeldSharingDeletion);
    }

    #[test]
    fn a_failed_move_counts_a_member_left_in_place_among_the_files_before_it() {
        let folder = scratch("move-count");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        put(&stage, "bin/.msCoverageSourceRootsMapping_X", b"box");
        put(&stage, "bin/b.txt", b"box");
        let roots = put(&root, "bin/.msCoverageSourceRootsMapping_X", b"pc");
        let pc = put(&root, "bin/b.txt", b"pc");
        let manifest = manifest_of(&root, &["bin/.msCoverageSourceRootsMapping_X", "bin/b.txt"]);
        fs::write(&roots, b"the PC writer's roots").expect("the file can be written");
        let entries = [member("bin/.msCoverageSourceRootsMapping_X", b"box"), entry("bin/b.txt")];

        let unmoved = moved(&stage, &root, &entries, &manifest, |done| {
            if done == 1 {
                fs::write(&pc, b"edited after the checks").expect("the file can be written");
            }
            true
        });

        let reason = "1 of 2 files were installed before bin/b.txt failed: bin/b.txt changed during the run";
        assert_eq!(unmoved, Err(Unmoved::Failed(reason.to_owned())));
    }

    #[test]
    fn a_root_recording_pc_file_changed_during_the_run_is_kept() {
        assert_root_recording_pc_file_kept("roots-changed", "bin/.msCoverageSourceRootsMapping_X", Some(b"pc"));
    }

    #[test]
    fn a_root_recording_pc_file_the_manifest_lacked_is_kept() {
        assert_root_recording_pc_file_kept("roots-absent", "bin/.msCoverageSourceRootsMapping_X", None);
    }

    #[test]
    fn an_mvc_testing_app_manifest_changed_during_the_run_is_kept() {
        assert_root_recording_pc_file_kept("mvc-changed", "bin/MvcTestingAppManifest.json", Some(b"pc"));
    }

    #[test]
    fn a_root_recording_pc_file_rewritten_after_the_checks_is_kept_just_before_its_move() {
        let folder = scratch("move-roots-changed");
        let (stage, root) = (folder.join("stage"), folder.join("root"));
        put(&stage, "bin/a.txt", b"box");
        let staged = put(&stage, "bin/.msCoverageSourceRootsMapping_X", b"the builder's roots");
        let pc = put(&root, "bin/.msCoverageSourceRootsMapping_X", b"pc");
        let manifest = manifest_of(&root, &["bin/.msCoverageSourceRootsMapping_X"]);
        let entries = [entry("bin/a.txt"), member("bin/.msCoverageSourceRootsMapping_X", b"the builder's roots")];

        let installed = moved(&stage, &root, &entries, &manifest, |done| {
            if done == 1 {
                fs::write(&pc, b"the PC writer's roots").expect("the file can be written");
            }
            true
        });

        let paths: Vec<&str> = installed
            .as_ref()
            .map(|moves| moves.moved.iter().map(|moved| moved.entry.path.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(paths, ["bin/a.txt"], "only a is moved: {installed:?}");
        assert_eq!(fs::read(&pc).expect("the PC file stays"), b"the PC writer's roots");
        assert!(staged.exists(), "the staged roots file is not moved");
    }

    /// The size of the member the streaming case unpacks.
    const LARGE: u64 = 64 * 1024 * 1024;
    /// The most the unpack may hold back between the archive it reads and the file it writes: a quarter of [`LARGE`].
    /// One that held the member whole would hold back all of it.
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
        let chosen: Vec<Chosen<'_>> = found.iter().map(Chosen::from).collect();
        pack(File::create(&archive).expect("the archive can be made"), &source, &chosen, &HeldBy::new()).expect("the file packs");
        let stage = folder.join("stage");
        let target = stage.join("bin").join("large.bin");
        let widest = Cell::new(0);
        let watched = Watched {
            inner: File::open(&archive).expect("the archive opens"),
            target: &target,
            given: 0,
            widest: &widest,
        };

        let entries = unpacked_from(watched, &stage, &folder, &Manifest::default()).expect("the archive unpacks");

        assert_eq!(entries.iter().map(|entry| entry.size).collect::<Vec<_>>(), [LARGE]);
        assert!(widest.get() <= HELD_BACK, "the unpack held back {} bytes", widest.get());
    }

    #[test]
    fn an_unpack_stops_once_the_interrupt_is_raised() {
        let folder = scratch("unpack-interrupted");
        let source = folder.join("source");
        put(&source, "bin/a.txt", b"a");
        let found = walk(&source, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        let chosen: Vec<Chosen<'_>> = found.iter().map(Chosen::from).collect();
        let mut archive = Vec::new();
        pack(&mut archive, &source, &chosen, &HeldBy::new()).expect("the file packs");
        let stopped = Cell::new(false);

        let manifest = Manifest::default();
        let pc = PcFiles {
            root: &folder,
            manifest: &manifest,
            go_on: &|| false,
        };

        let unpacked = unpacked(&archive[..], &folder.join("stage"), OUTPUTS_BUDGET, &pc, &stopped);

        assert!(unpacked.is_err() && stopped.get(), "{unpacked:?}");
        assert!(!folder.join("stage").exists(), "the stage is removed");
    }

    /// `unpacked` of `source` into `stage` under the box's budget, never stopped, each copy from its member or from
    /// the file below `root` that `manifest` holds with its bytes.
    fn unpacked_from(source: impl Read, stage: &Path, root: &Path, manifest: &Manifest) -> Result<Vec<Entry>, UnpackError> {
        let pc = PcFiles {
            root,
            manifest,
            go_on: &|| true,
        };
        unpacked(source, stage, OUTPUTS_BUDGET, &pc, &Cell::new(false))
    }

    /// A manifest holding each of `paths` below `root` with its stamp and its SHA-256 now.
    fn manifest_hashed(root: &Path, paths: &[&str]) -> Manifest {
        let mut manifest = Manifest::default();
        for path in paths {
            let full = super::below(root, path);
            let held = Held {
                stamp: stamp(&full),
                sha256: file_sha256(&full).expect("the file hashes"),
            };
            manifest.insert((*path).to_owned(), held);
        }
        manifest
    }

    /// The archive the box packs of every file below `source` against the PC's `manifest`, each file hashed before the
    /// pack: a content an earlier file or the manifest holds is listed as a copy.
    fn deduplicated(source: &Path, manifest: &Manifest) -> Vec<u8> {
        let found = walk(source, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        let chosen: Vec<Chosen<'_>> = found
            .iter()
            .map(|file| Chosen {
                file,
                sha256: Some(file_sha256(&file.full).expect("the file hashes")),
            })
            .collect();
        let mut archive = Vec::new();
        pack(&mut archive, source, &chosen, &manifest.by_hash(&[])).expect("the files pack");
        archive
    }

    #[test]
    fn a_copy_from_a_pc_file_installs_with_that_content() {
        let folder = scratch("copy-from-pc");
        let root = folder.join("pc");
        put(&root, "bin/held.dll", b"kept");
        let manifest = manifest_hashed(&root, &["bin/held.dll"]);
        let source = folder.join("box");
        put(&source, "bin/new.dll", b"kept");
        put(&source, "bin/other.dll", b"fresh");
        let archive = deduplicated(&source, &manifest);
        let stage = folder.join("stage");

        let entries = unpacked_from(&archive[..], &stage, &root, &manifest).expect("the archive unpacks");

        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, ["bin/new.dll", "bin/other.dll"], "the copy is listed with the member");
        moved(&stage, &root, &entries, &manifest, |_| true).expect("the files move");
        assert_eq!(fs::read(root.join("bin").join("new.dll")).expect("the copy is installed"), b"kept");
        assert_eq!(fs::read(root.join("bin").join("other.dll")).expect("the member is installed"), b"fresh");
        assert_eq!(fs::read(root.join("bin").join("held.dll")).expect("the source stays"), b"kept");
    }

    #[test]
    fn a_copy_whose_pc_source_changed_content_is_refused_when_no_other_holds_it() {
        let folder = scratch("copy-source-changed");
        let root = folder.join("pc");
        put(&root, "bin/held.dll", b"kept");
        let manifest = manifest_hashed(&root, &["bin/held.dll"]);
        let source = folder.join("box");
        put(&source, "bin/new.dll", b"kept");
        let archive = deduplicated(&source, &manifest);
        put(&root, "bin/held.dll", b"knot");
        let stage = folder.join("stage");

        let error = unpacked_from(&archive[..], &stage, &root, &manifest).expect_err("the copy is refused");

        assert!(!error.members(), "a refetch may clear it: {error}");
        let (sha256, _) = sha256_of(&b"kept"[..]).expect("bytes hash");
        assert_eq!(
            error.to_string(),
            format!("no PC file still holds the content of bin/new.dll, SHA-256 {sha256}: tried bin/held.dll (other content)")
        );
        assert!(!stage.exists(), "the stage is removed");
    }

    #[test]
    fn a_copy_falls_back_to_another_pc_path_holding_the_same_content() {
        let folder = scratch("copy-source-fallback");
        let root = folder.join("pc");
        put(&root, "bin/a.dll", b"kept");
        put(&root, "bin/b.dll", b"kept");
        let manifest = manifest_hashed(&root, &["bin/a.dll", "bin/b.dll"]);
        let source = folder.join("box");
        put(&source, "bin/new.dll", b"kept");
        let archive = deduplicated(&source, &manifest);
        put(&root, "bin/a.dll", b"knot");
        let stage = folder.join("stage");

        let entries = unpacked_from(&archive[..], &stage, &root, &manifest).expect("the second path holds the content");

        assert_eq!(entries.len(), 1);
        assert_eq!(fs::read(stage.join("bin").join("new.dll")).expect("the copy was made"), b"kept");
    }

    #[test]
    fn a_copy_whose_pc_source_was_touched_with_the_same_bytes_installs() {
        let folder = scratch("copy-source-touched");
        let root = folder.join("pc");
        let held = put(&root, "bin/held.dll", b"kept");
        let manifest = manifest_hashed(&root, &["bin/held.dll"]);
        let source = folder.join("box");
        put(&source, "bin/new.dll", b"kept");
        let archive = deduplicated(&source, &manifest);
        File::options()
            .write(true)
            .open(&held)
            .and_then(|file| file.set_modified(SystemTime::now() - Duration::from_hours(1)))
            .expect("the last write can be set");
        assert_ne!(stamp(&held), manifest.get("bin/held.dll").expect("held").stamp, "the source was touched");
        let stage = folder.join("stage");

        let entries = unpacked_from(&archive[..], &stage, &root, &manifest).expect("the same bytes are a source");

        moved(&stage, &root, &entries, &manifest, |_| true).expect("the copy moves");
        assert_eq!(fs::read(root.join("bin").join("new.dll")).expect("the copy is installed"), b"kept");
    }

    #[test]
    fn a_copy_that_records_the_box_root_is_rewritten_on_its_own() {
        let folder = scratch("copy-rewritten");
        let source = folder.join("box");
        let recorded = |root: &str| format!(r#"{{"ContentRoot":"{}\\src"}}"#, root.replace('\\', r"\\"));
        put(&source, "a/bin/MvcTestingAppManifest.json", recorded(BOX_WORK).as_bytes());
        put(&source, "b/bin/MvcTestingAppManifest.json", recorded(BOX_WORK).as_bytes());
        let archive = deduplicated(&source, &Manifest::default());
        let stage = folder.join("stage");
        let entries = unpacked_from(&archive[..], &stage, &folder, &Manifest::default()).expect("the archive unpacks");
        let read = |path: &str| fs::read_to_string(super::below(&stage, path)).expect("the staged file is there");

        let skipped = rewritten(&stage, &entries[1..], &one_tree());

        assert_eq!(skipped, Ok(Vec::new()));
        assert_eq!(read("b/bin/MvcTestingAppManifest.json"), recorded(PC_ROOT), "the copy is rewritten");
        assert_eq!(
            read("a/bin/MvcTestingAppManifest.json"),
            recorded(BOX_WORK),
            "the member it was copied from is a file of its own"
        );
        assert_eq!(rewritten(&stage, &entries[..1], &one_tree()), Ok(Vec::new()));
        assert_eq!(read("a/bin/MvcTestingAppManifest.json"), recorded(PC_ROOT));
    }
}
