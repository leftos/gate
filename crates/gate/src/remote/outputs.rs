//! A routed run's outputs on the PC, up to its run call's end: the manifest of the files the PC holds under the run's
//! output globs, hashed before the run call, each file whose stamp the hash cache holds taking its hash from it, and
//! sent on its standard input; the pending marker, written after the push and before the run call and removed once the
//! outputs step completes, whose presence makes the next routed run of the worktree ask for every output that differs
//! from the PC's (`OutputsFull`); the box's `gate-remote-outputs:` line the client takes from the run call; and the
//! outputs call that fetches the archive it names into the worktree's `.tmp` folder, for [`install`](super::install)
//! to put into the worktree.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::num::NonZeroU64;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gate_router::escaped;
use gate_state::v1;
use gate_wire::{PathForm, archive_file_name, trees};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, GetFileInformationByHandle, SetFileInformationByHandle,
};

use super::client::{self, Attempt, Called, End, RawSink};
use super::git::{FAILURE_BYTES, first_bytes};
use super::install::{self, Fetched};
use super::payload::Location;
use super::syncback::Synced;
use super::watchdog::Cut;
use super::{RemoteCall, Sibling};
use crate::outputs::archive::final_path;
use crate::outputs::cache::{HashCache, file_time_now};
use crate::outputs::glob::Globs;
use crate::outputs::hash::sha256_of;
use crate::outputs::manifest::{MANIFEST_LIMIT, Manifest, build_manifest_with};
use crate::outputs::walk::{Found, Stamp, in_context, stamp_of, walk};

/// Why a manifest line longer than [`MANIFEST_LIMIT`] is not sent.
const PAST_THE_CAP: &str = "it is larger than the box takes (64 MiB)";
/// The start of the box's line saying what it packed: `gate-remote-outputs: 0`, or `gate-remote-outputs: <n> <archive>`.
const OUTPUTS: &str = "gate-remote-outputs: ";
/// The start of the box's line, said just before its outputs line when it packed an archive, saying the archive's size:
/// `gate-remote-outputs-size: <bytes>`.
const OUTPUTS_SIZE: &str = "gate-remote-outputs-size: ";
/// Why a passing run whose box said no outputs line the client accepts brings nothing back, as the PowerShell client
/// says it.
const NO_OUTPUTS_LINE: &str = "the box printed no 'gate-remote-outputs: <n> <archive>' line";
/// The worktree's folder the fetched archive is kept in, made when missing.
const TMP: &str = ".tmp";
/// Why a fetch the watchdog stopped brings nothing back, as the PowerShell client says it.
const STOPPED: &str = "the fetch was stopped by the watchdog";
/// What an archive a fetch was cut short of, or that was not installed for the run's end, is called when it cannot be
/// removed.
const PARTIAL: &str = "partial";
/// `GENERIC_WRITE`, which lives in `Win32_Foundation`, a feature this crate does not enable: the access the archive
/// file is made with, beside [`DELETE`] so a make it refuses can remove that file through the same handle.
pub(super) const GENERIC_WRITE: u32 = 0x4000_0000;
/// `DELETE`, from `Win32_Foundation` too: the access that lets [`Archive::create`] remove a file it refuses through the
/// handle it made it with, whatever its name now resolves to.
pub(super) const DELETE: u32 = 0x0001_0000;
/// The most of an outputs cap record that is read: its one count fits many times over.
const CAP_RECORD_BYTES: u64 = 4096;
/// What an archive fetched whole and handed to the install is called when it cannot be removed.
pub(super) const FETCHED: &str = "fetched";

/// How a run's run call ended, for its outputs step: the work folder's place on the builder, the outputs nonce the run
/// sent, the call's status, the command's own, and whether the box said it skipped outputs for names this PC cannot
/// take.
pub(super) struct Ran<'r> {
    pub(super) location: &'r Location,
    pub(super) nonce: &'r str,
    pub(super) status: i64,
    pub(super) names_skipped: bool,
}

/// The file in the worktree a fetched archive is written to, the worktree it must sit inside, the `.tmp` folder held
/// open for the run's life, and whether an attempt of this run made the file.
pub(super) struct Archive {
    root: PathBuf,
    path: PathBuf,
    /// The archive's own folder, opened and held from the first make until this run's end: the handle shares neither
    /// its delete nor its rename, so the folder cannot be moved or removed while the archive and the stage the install
    /// makes beside it are in it, and it is the worktree's own `.tmp`, not a link or a junction.
    hold: Option<File>,
    made: bool,
}

impl Archive {
    /// The file at `path` in the worktree `root`, which no attempt made yet.
    pub(super) fn new(root: &Path, path: PathBuf) -> Archive {
        Archive {
            root: root.to_path_buf(),
            path,
            hold: None,
            made: false,
        }
    }

    /// The file's path.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// The file an outputs call's attempt writes its standard output to raw: made new, its folder made when missing
    /// and held from then on, once the file an earlier attempt of this run made is removed; a file of the name this run
    /// did not make is never removed, and fails the make. The copy into it is capped at `cap` bytes. The folder must be
    /// a folder itself, and the worktree's own `.tmp`, or the make fails; a file whose own final path is outside the
    /// worktree fails the make too, and is removed through its own handle. Else why it cannot be written.
    pub(super) fn create(&mut self, cap: NonZeroU64) -> Result<RawSink, String> {
        let shown = self.path.display().to_string();
        let cannot = |error: io::Error| format!("cannot write {shown}: {error}");
        if self.made {
            match fs::remove_file(&self.path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(cannot(error)),
                _ => self.made = false,
            }
        }
        if let Some(folder) = self.path.parent().map(Path::to_path_buf) {
            fs::create_dir_all(&folder).map_err(cannot)?;
            self.hold(&folder).map_err(cannot)?;
        }
        let inside = fs::canonicalize(&self.root).map_err(cannot)?;
        let file = OpenOptions::new()
            .write(true)
            .access_mode(GENERIC_WRITE | DELETE)
            .create_new(true)
            .open(&self.path)
            .map_err(cannot)?;
        self.made = true;
        if !final_path(&file).map_err(cannot)?.starts_with(&inside) {
            let outside = format!("the outputs archive {} is not inside the worktree", self.path.display());
            if let Err(error) = delete_through(&file) {
                // Still made: the failed fetch removes it by name once more, and says its own line when that fails.
                return Err(format!("{outside}; and it could not be removed: {error}"));
            }
            self.made = false;
            return Err(outside);
        }
        Ok(RawSink { file, shown, cap })
    }

    /// Holds the folder `folder` for the rest of this run, opening it once: a handle that shares neither its delete
    /// nor its rename, so nothing can move the folder or remove it while the archive, and the stage the install makes
    /// beside it, are in it. The handle also says through the folder itself that it is a folder, not a link or a
    /// junction, and that its final path is the worktree's own `.tmp`. Else why it cannot be held.
    pub(super) fn hold(&mut self, folder: &Path) -> io::Result<()> {
        if self.hold.is_some() {
            return Ok(());
        }
        let opened = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(folder)?;
        if attributes(&opened)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other("it is a link or a junction, not a folder"));
        }
        let inside = fs::canonicalize(&self.root)?;
        if final_path(&opened)? != inside.join(TMP) {
            return Err(io::Error::other(format!(
                "it is not the worktree's {TMP} folder, but {}",
                folder.display()
            )));
        }
        self.hold = Some(opened);
        Ok(())
    }

    /// Removes the file an attempt of this run made, when one did; one already gone counts as removed. Else why it
    /// could not be removed, the archive called `which`: [`PARTIAL`] or [`FETCHED`].
    fn remove(&self, which: &str) -> Result<(), String> {
        if !self.made {
            return Ok(());
        }
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(format!("cannot remove the {which} outputs archive {}: {error}", self.path.display()))
            }
            _ => Ok(()),
        }
    }
}

/// The file attributes the system holds for the open file `opened`.
fn attributes(opened: &File) -> io::Result<u32> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `opened` owns an open handle for the whole call, and `info` is a writable `BY_HANDLE_FILE_INFORMATION`.
    if unsafe { GetFileInformationByHandle(opened.as_raw_handle(), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.dwFileAttributes)
}

/// Removes the open file `file` by setting its delete on the handle it is already open with, so it goes however its
/// name now resolves: a name a link or a junction was swapped in at cannot redirect the removal.
fn delete_through(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).map_err(io::Error::other)?;
    // SAFETY: `file` owns the handle for the whole call and was made with [`DELETE`] access, `info` is a
    // `FILE_DISPOSITION_INFO` that outlives the call, and `size` is its size in bytes.
    let set = unsafe { SetFileInformationByHandle(file.as_raw_handle(), FileDispositionInfo, std::ptr::from_ref(&info).cast(), size) };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Why the manifest was not built: the console interrupt was raised while it was, or a reason it could not be.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Unbuilt {
    Interrupted,
    Failed(String),
}

/// What the box's accepted outputs line says it packed: nothing, or `count` files in the archive `name`, `size` bytes
/// long when the box said its size (a box not yet pushed says none).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Brought {
    Nothing,
    Archive { count: u64, name: String, size: Option<u64> },
}

/// What a run call carries of the run's outputs, and what the client keeps for after it.
pub(super) struct Sent<'a> {
    /// The output globs the run call carries: the run's, or none when it has none or its manifest was not built.
    pub(super) globs: &'a [String],
    /// Whether the run asks for every output that differs from the PC's: the pending marker was there.
    pub(super) full: bool,
    /// The manifest line, fed on the run call's standard input.
    pub(super) manifest: Option<Arc<str>>,
    /// The pending marker, written after the push by [`marked`]; `None` for a run with no output globs.
    marker: Option<PathBuf>,
    /// The pending marker a run whose outputs the route withheld writes after the push ([`marked`]) and never clears:
    /// the builder builds and nothing comes back, so the next run asks for every output that differs from the PC's.
    withheld: Option<PathBuf>,
    /// The manifest the line holds, its files' stamps, which the install checks the PC's files against; empty for a
    /// run with no output globs. Else why it could not be built, which a passing run says as it fails.
    built: Result<Manifest, String>,
}

impl Sent<'_> {
    /// What a call that touches no outputs state carries: no globs, no manifest line, and no pending marker to write.
    pub(super) fn nothing() -> Self {
        Sent {
            globs: &[],
            full: false,
            manifest: None,
            marker: None,
            withheld: None,
            built: Ok(Manifest::default()),
        }
    }

    /// Takes out the withheld-outputs marker [`marked`] would write, for a run whose send of the PC's build landed: it
    /// keeps its `--no-build` and builds nothing on the builder, so it leaves no marker. The marker taken, for a run
    /// the box then refuses for that build, which runs again with `--no-build` dropped ([`Sent::withhold`]).
    pub(super) fn take_withheld(&mut self) -> Option<PathBuf> {
        self.withheld.take()
    }

    /// Puts back the withheld-outputs marker [`Sent::take_withheld`] took, for [`marked`] to write.
    pub(super) fn withhold(&mut self, withheld: Option<PathBuf>) {
        self.withheld = withheld;
    }
}

/// What `attempt`'s run call carries of its outputs. A run with no output globs carries none, and reads no pending
/// marker; one whose outputs the route withheld records the marker's path for [`marked`] to write. One with globs reads
/// whether the marker is there, builds the manifest, polling the console interrupt as it hashes and taking each hash
/// the worktree's hash cache holds at the file's stamp, unless the marker is there, saves the cache that build leaves,
/// whatever the run's end, unless it equals the one it read, and records the marker's path for [`marked`] to write
/// once the snapshot is pushed; a manifest that cannot be built is kept for the outputs step, saves no cache, and the
/// run call then carries no globs. Else the interrupt that ended the build.
pub(super) fn sent<'a>(attempt: &mut Attempt<'_, 'a>) -> Result<Sent<'a>, End> {
    let call: &RemoteCall<'a> = attempt.call;
    let mut sent = Sent::nothing();
    if call.outputs.is_empty() {
        if call.outputs_withheld {
            sent.withheld = Some(v1::outputs_pending_path(call.state_dir, call.key));
        }
        return Ok(sent);
    }
    let marker = v1::outputs_pending_path(call.state_dir, call.key);
    let full = pending(&marker);
    let hashes = v1::outputs_hashes_path(call.state_dir, call.builder.id.as_deref(), call.key);
    // The full send is the recovery path: it reuses no hash that may be wrong, and its own then replace the cache's.
    let previous = if full { None } else { HashCache::load(&hashes) };
    let none = HashCache::default();
    let cache = previous.as_ref().unwrap_or(&none);
    match manifest_line(Path::new(call.root), call.outputs, MANIFEST_LIMIT, cache, || attempt.go_on().is_ok()) {
        Ok((line, files, next)) => {
            if previous.as_ref() != Some(&next) {
                next.save(&hashes);
            }
            sent.globs = call.outputs;
            sent.full = full;
            sent.manifest = Some(Arc::from(line));
            sent.built = Ok(files);
        }
        Err(Unbuilt::Interrupted) => return Err(End::Interrupted),
        Err(Unbuilt::Failed(reason)) => sent.built = Err(reason),
    }
    sent.marker = Some(marker);
    Ok(sent)
}

/// Writes the pending marker `sent` records, once the snapshot is pushed and before the run call, so a run the client
/// sends after the push leaves it on every end but a passing run whose box packed nothing, and a run refused before
/// the push, at the preflight or by the parity check, leaves none. A run with no output globs records none, unless the
/// route withheld its outputs, whose marker no end clears. Else why it could not be written.
pub(super) fn marked(sent: &Sent<'_>) -> Result<(), End> {
    match sent.marker.as_ref().or(sent.withheld.as_ref()) {
        Some(marker) => mark_pending(marker).map_err(End::Failed),
        None => Ok(()),
    }
}

/// The end of `attempt`'s run once its sync-back ended it `synced`, its run call having ended as `ran` says, and the
/// box having said it `brought` its outputs or not, for a run that carried `sent`: the outputs step of a run with
/// output globs. A run whose passing status the sync-back turned to 1 says its outputs were not brought back, for the
/// sync-back's reason ([`turned_reason`]). A passing run whose manifest was not built, or whose box said no outputs line the client
/// accepts, says why and ends 1. A passing run whose box packed an archive fetches it, as [`fetched`] says. A passing
/// run whose box packed nothing removes the pending marker and the outputs cap record ([`uncapped`]); one that cannot be
/// removed is said, and the run's status stands. The other ends that do not reach [`fetched`] leave both.
///
/// The attempt's outputs are marked brought back only when the install ends complete, as [`install::installed`] says,
/// or when the box packed nothing for a manifest that held files; never when the box said it skipped outputs for names
/// this PC cannot take. Every other end marks them not brought back.
pub(super) fn ended(attempt: &mut Attempt<'_, '_>, sent: &Sent<'_>, ran: &Ran<'_>, synced: Synced, brought: Option<&Brought>) -> End {
    let (end, complete) = step_end(attempt, sent, ran, synced, brought);
    attempt.brought_back = Some(complete && !ran.names_skipped);
    end
}

/// Why the outputs of a run whose command ended `status` were not brought back once its sync-back ended as `synced`:
/// for a passing run the sync-back turned to 1, the sync-back's own reason, and for a single repository's, whose
/// sync-back gives none, the worktree that changed during the run. `None` for any other end.
fn turned_reason(status: i64, synced: &Synced) -> Option<&'static str> {
    (status == 0 && matches!(synced.end, End::Status(1))).then(|| synced.reason.unwrap_or(WORKTREE_CHANGED))
}

/// Why a single repository's passing run that its sync-back turned to 1 brings no outputs back.
const WORKTREE_CHANGED: &str = "the worktree changed during the run";

/// The end of the outputs step [`ended`] says, and whether it left the PC every output the box packed or found the PC
/// holding them all, the box's names skip aside.
fn step_end(attempt: &mut Attempt<'_, '_>, sent: &Sent<'_>, ran: &Ran<'_>, synced: Synced, brought: Option<&Brought>) -> (End, bool) {
    let reason = turned_reason(ran.status, &synced);
    let synced = synced.end;
    let (Some(marker), End::Status(now)) = (&sent.marker, &synced) else {
        return (synced, false);
    };
    if let Some(reason) = reason {
        attempt.say(not_brought_back(reason));
        return (synced, false);
    }
    if *now != 0 {
        return (synced, false);
    }
    let reason = match (&sent.built, brought) {
        (Err(reason), _) => format!("the manifest could not be built: {reason}"),
        (Ok(_), None) => NO_OUTPUTS_LINE.to_owned(),
        (Ok(files), Some(Brought::Archive { count, size, .. })) => {
            let kept = Kept {
                globs: sent.globs,
                files,
                marker,
                count: *count,
                size: *size,
            };
            return fetched(attempt, &kept, ran, synced);
        }
        (Ok(files), Some(Brought::Nothing)) => {
            if let Err(why) = clear_pending(marker) {
                attempt.say(format!("gate: {why}"));
            }
            uncapped(attempt);
            return (synced, *files != Manifest::default());
        }
    };
    attempt.say(not_brought_back(&reason));
    (End::Status(1), false)
}

/// What the outputs step of a passing run whose box packed an archive keeps for its pull and install: the run's output
/// globs, the PC's manifest and the pending marker; and the files the box said it packed, and the archive's size in
/// bytes when it said one.
struct Kept<'k> {
    globs: &'k [String],
    files: &'k Manifest,
    marker: &'k Path,
    count: u64,
    size: Option<u64>,
}

/// Each tree the box checked the run out in, as [`trees`] places it in the work folder of `location`, with the PC root
/// it stands for: the main repo's tree with the worktree root, then each sibling's with the sibling's root, in the
/// pair's order. A sibling's output is never brought back; its tree is here for a main repo's file that records it.
///
/// # Panics
///
/// When the location's pair names another number of siblings than the call carries: the location is built from the
/// call's siblings, so that is a bug, and a sibling paired with no root would keep its box path unseen.
fn box_trees<'a>(location: &Location, call: &RemoteCall<'a>) -> (String, Vec<(String, &'a str)>) {
    let names: Vec<&str> = location.pair.iter().flat_map(|pair| pair.siblings.iter().map(String::as_str)).collect();
    assert_eq!(
        names.len(),
        call.siblings.len(),
        "the run's Pair names {} siblings but the call carries {}",
        names.len(),
        call.siblings.len()
    );
    let siblings: Vec<(&str, &'a str)> = names.into_iter().zip(call.siblings.iter().map(Sibling::root)).collect();
    let leaf = location.pair.as_ref().map_or(call.leaf, |pair| pair.leaf.as_str());
    let ((main, _), siblings) = trees_of(&location.work, leaf, call.root, &siblings, location.form);
    (main, siblings)
}

/// Each tree a routed run is checked out in, as [`trees`] places it in the work folder `work` of a box reading paths
/// under `form`, with the PC root it stands for: the main repo's tree (`work` itself with no sibling, else
/// `{work}\{leaf}`, `/` under [`PathForm::Unix`]) with `main_root`, then each of `siblings`, a folder name and its root,
/// in order. The outputs' install ([`box_trees`]) and the path-map word ([`super::box_trees`]) both pair the trees
/// through this.
pub(super) fn trees_of<'a>(
    work: &str,
    leaf: &str,
    main_root: &'a str,
    siblings: &[(&str, &'a str)],
    form: PathForm,
) -> ((String, &'a str), Vec<(String, &'a str)>) {
    let names: Vec<&str> = siblings.iter().map(|(name, _)| *name).collect();
    let pair = (!names.is_empty()).then_some((leaf, names.as_slice()));
    let (main, trees) = trees(work, pair, form);
    ((main, main_root), trees.into_iter().zip(siblings.iter().map(|(_, root)| *root)).collect())
}

/// The outputs call of a passing run whose box packed an archive, `synced` its end so far: the archive the run's own
/// key and nonce name, never the box's line, is fetched raw into the worktree's `.tmp` folder, which is held until this
/// run's end. One fetched whole is installed, as [`install::installed`] says, through a stage folder beside it named
/// for the same key and nonce. A fetch the watchdog stops says the watchdog's line and ends the run killed; a Ctrl+C
/// ends it as an interrupt, with no `stop` call; any other failure says why, the box's last standard error line when it
/// said one, and fails the run, but one whose archive passed the fetch cap installs nothing and is judged as [`capped`]
/// says. Each of these removes what was fetched, and says so when it cannot; each says the outputs were not brought back
/// but the interrupt. The worktree's outputs cap record ([`uncapped`]) is removed once the fetch ends whole, before the
/// install, and on every other end but the cap and a Ctrl+C during the fetch. With the end, whether the install was
/// complete; a fetch that did not end whole installs nothing.
///
/// The pull says its start ([`pull_start_line`]) before the outputs call, and its progress while it runs, as the call's
/// watch does; one that did not end whole says how long it ran and what it received ([`pull_ended_line`]) before any
/// other line of its end.
fn fetched(attempt: &mut Attempt<'_, '_>, kept: &Kept<'_>, ran: &Ran<'_>, synced: End) -> (End, bool) {
    let started = Instant::now();
    attempt.say(pull_start_line(attempt.call.host(), kept.count, kept.size));
    let name = archive_file_name(attempt.call.key, ran.nonce);
    let tmp = Path::new(attempt.call.root).join(TMP);
    let mut archive = Archive::new(Path::new(attempt.call.root), tmp.join(&name));
    let limits = attempt.limits_for("outputs");
    let pull = client::Pull { total: kept.size, started };
    let called = attempt.outputs_call(ran.location, &name, &mut archive, limits, pull);
    attempt.split.outputs = Some(started.elapsed());
    // The fetch alone runs to the archive's last byte; one that wrote none, to the call's end.
    let pulled = attempt
        .pulled_last
        .map_or_else(|| started.elapsed(), |last| last.saturating_duration_since(started));
    // A Ctrl+C raised by the call's end is read once, so the early end's line and `judged` agree on a pull not whole.
    let interrupted = attempt.go_on().is_err();
    if !matches!(&called, Ok(called) if pulled_whole(called, interrupted)) {
        attempt.say(pull_ended_line(pulled, attempt.pulled_bytes));
    }
    match called {
        // `judged` hands back the run's passing end only when the archive was fetched whole, and `called` a cut when it
        // was not: an archive past the fetch cap installs nothing.
        Ok(called) => {
            let at_the_cap = matches!(called.cut, Some(Cut::ArchiveTooBig(_)));
            match judged(attempt, &archive, &called, synced, interrupted) {
                End::Status(0) if called.cut.is_none() => {
                    // The fetch ended whole, under the cap: how the install ends no longer bears on the count.
                    uncapped(attempt);
                    let (main_tree, sibling_trees) = box_trees(ran.location, attempt.call);
                    let fetched = Fetched {
                        archive: &archive,
                        stage: tmp.join(format!("{}-{}", attempt.call.key, ran.nonce)),
                        marker: kept.marker,
                        manifest: kept.files,
                        globs: kept.globs,
                        main_tree,
                        sibling_trees,
                        started,
                        pulled,
                    };
                    install::installed(attempt, &fetched)
                }
                end => (settled(attempt, at_the_cap, end), false),
            }
        }
        Err(End::Failed(reason)) => {
            let end = unfetched(attempt, &archive, &reason, End::Status(1));
            (settled(attempt, false, end), false)
        }
        Err(end) => {
            removed(attempt, &archive, PARTIAL);
            (settled(attempt, false, end), false)
        }
    }
}

/// Whether the outputs call that ended as `called` brought its archive whole: it ended 0, uncut, and no Ctrl+C was
/// raised by its end (`interrupted`). [`judged`] hands back the run's end so far for such a pull alone.
fn pulled_whole(called: &Called, interrupted: bool) -> bool {
    !interrupted && called.cut.is_none() && called.status == 0
}

/// `end`, that of a fetch that did not end whole, the worktree's outputs cap record removed first ([`uncapped`]) unless
/// the fetch ended `at_the_cap` or by a Ctrl+C.
fn settled(attempt: &mut Attempt<'_, '_>, at_the_cap: bool, end: End) -> End {
    if !at_the_cap && !matches!(end, End::Interrupted) {
        uncapped(attempt);
    }
    end
}

/// The end of the outputs step once its outputs call ended as `called`, having written to `archive`, a Ctrl+C raised by
/// its end or not (`interrupted`, read once by the caller), the run's end so far `synced`: as [`fetched`] says, `synced`
/// for a pull whole ([`pulled_whole`]).
pub(super) fn judged(attempt: &mut Attempt<'_, '_>, archive: &Archive, called: &Called, synced: End, interrupted: bool) -> End {
    if pulled_whole(called, interrupted) {
        return synced;
    }
    // A Ctrl+C raised by the call's end is the run's end, however the call ended, as after the run call.
    if interrupted {
        removed(attempt, archive, PARTIAL);
        return End::Interrupted;
    }
    if let Some(cut) = called.cut {
        if let Some(line) = client::watchdog_line(cut, "fetching the outputs") {
            attempt.say(line);
            attempt.noted(called);
            return unfetched(attempt, archive, &stopped_reason(called.received), End::Killed);
        }
        attempt.noted(called);
        if let Cut::ArchiveTooBig(_) = cut {
            return capped(attempt, archive, &cut.reason("the archive"), synced);
        }
        return unfetched(attempt, archive, &cut.reason("the outputs call"), End::Status(1));
    }
    // Neither whole, nor interrupted, nor cut: the call failed.
    let reason = match &called.last_error {
        Some(line) => first_bytes(line, FAILURE_BYTES).to_owned(),
        None => format!("the outputs call failed (status {})", called.status),
    };
    unfetched(attempt, archive, &reason, End::Status(1))
}

/// Why a fetch the watchdog stopped brings nothing back: [`STOPPED`], and, when its raw copy's count is known
/// (`received`), the bytes it had written into the archive by the cut, the whole seconds the call had run by then, at
/// least 1, and the average rate over them, each size as [`size`](super::watchdog::size) spells it.
fn stopped_reason(received: Option<client::Received>) -> String {
    let Some(client::Received { bytes, after }) = received else {
        return STOPPED.to_owned();
    };
    let seconds = after.as_secs().max(1);
    let (fetched, rate) = (super::watchdog::size(bytes), super::watchdog::size(bytes / seconds));
    format!("{STOPPED} after {fetched} in {seconds} s ({rate}/s)")
}

/// The end of a fetch cut for the archive it passed, `reason` saying so, the run's end so far `synced`: the outputs are
/// said not brought back, `archive` is removed and the hit is counted in the worktree's outputs cap record. The first
/// hit in a row keeps the run's status: the box's work is sound, the archive of it is not, and the next routed run asks
/// for it again. A later hit in a row says the cap was passed again and fails the run.
fn capped(attempt: &mut Attempt<'_, '_>, archive: &Archive, reason: &str, synced: End) -> End {
    let end = unfetched(attempt, archive, reason, synced);
    let hits = counted_hit(attempt);
    if hits < 2 {
        return end;
    }
    attempt.say(format!(
        "gate: the builder's outputs passed the cap again, {hits} runs in a row; this run fails"
    ));
    End::Status(1)
}

/// The cap hits in a row of the worktree's outputs cap record once this one is added, the record written with them. A
/// record that is missing or cannot be read holds none; one that cannot be written is said, and the count stands.
fn counted_hit(attempt: &mut Attempt<'_, '_>) -> u64 {
    let path = v1::outputs_capped_path(attempt.call.state_dir, attempt.call.builder.id.as_deref(), attempt.call.key);
    let hits = hits_read(&path).saturating_add(1);
    if let Err(error) = v1::write_outputs_capped(&path, &v1::OutputsCappedRecord { hits }) {
        attempt.say(format!("gate: cannot write the outputs cap record {}: {error}", path.display()));
    }
    hits
}

/// The hits the outputs cap record at `path` holds: a link or junction at its name is read as itself, and at most
/// [`CAP_RECORD_BYTES`] of it. One that is missing, cannot be opened or read, or is not of its format holds none.
fn hits_read(path: &Path) -> u64 {
    let Ok(file) = OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path) else {
        return 0;
    };
    let mut bytes = Vec::new();
    if file.take(CAP_RECORD_BYTES).read_to_end(&mut bytes).is_err() {
        return 0;
    }
    v1::outputs_capped_record(&bytes).map_or(0, |record| record.hits)
}

/// Removes the worktree's outputs cap record, its outputs step having ended other than at the fetch cap; one already
/// gone counts as removed, and one that cannot be removed is said and changes no end.
pub(super) fn uncapped(attempt: &mut Attempt<'_, '_>) {
    let path = v1::outputs_capped_path(attempt.call.state_dir, attempt.call.builder.id.as_deref(), attempt.call.key);
    if let Err(error) = fs::remove_file(&path)
        && error.kind() != io::ErrorKind::NotFound
    {
        attempt.say(format!("gate: cannot remove the outputs cap record {}: {error}", path.display()));
    }
}

/// Says the run's outputs were not brought back, for `reason`, and removes what `archive` holds of them; `end`.
fn unfetched(attempt: &mut Attempt<'_, '_>, archive: &Archive, reason: &str, end: End) -> End {
    attempt.say(not_brought_back(reason));
    removed(attempt, archive, PARTIAL);
    end
}

/// Removes what `archive`, called `which` ([`Archive::remove`]), holds of a fetch; a removal that fails is said, and
/// changes no end.
pub(super) fn removed(attempt: &mut Attempt<'_, '_>, archive: &Archive, which: &str) {
    if let Err(why) = archive.remove(which) {
        attempt.say(format!("gate: {why}"));
    }
}

/// The manifest line, newline included, of the files under `globs` in the worktree `root`, the manifest it holds, and
/// the hash cache the next build may reuse. `go_on` is asked before a file is opened, and again before each of its
/// reads; the file is opened once and its stamp read from that handle (`GetFileInformationByHandle`), which the folder
/// listing the walk read can lag behind. Each file whose path `cache` holds takes its hash from it when that stamp is
/// the one the entry was kept at, and is hashed otherwise; either is refused when its final path is not inside `root`'s.
/// The cache returned holds each file last written at least [`WINDOW`](crate::outputs::cache::WINDOW) before
/// the walk began. A line longer than `cap` bytes is refused, its files' entries counted before each is hashed and the
/// whole line checked at the end. [`Unbuilt::Interrupted`] once `go_on` says no; else why the globs, the walk or a file cannot be read, or why
/// the line is not sent.
fn manifest_line(
    root: &Path,
    globs: &[String],
    cap: usize,
    cache: &HashCache,
    go_on: impl Fn() -> bool,
) -> Result<(String, Manifest, HashCache), Unbuilt> {
    let failed = |error: io::Error| Unbuilt::Failed(error.to_string());
    let globs = Globs::new(globs).map_err(Unbuilt::Failed)?;
    let started = file_time_now();
    let found = walk(root, &globs).map_err(failed)?;
    let inside = fs::canonicalize(root).map_err(|error| failed(in_context(root, &error)))?;
    let stopped = Cell::new(false);
    let mut next = HashCache::default();
    let built = build_manifest_with(&found, cap, |file| {
        let (sha256, stamp) = sha256_for(file, &inside, cache, &go_on, &stopped)?;
        next.keep(&file.path, stamp, &sha256, started);
        Ok((sha256, stamp))
    });
    if stopped.get() {
        return Err(Unbuilt::Interrupted);
    }
    let manifest = match built {
        Ok(manifest) => manifest,
        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => return Err(Unbuilt::Failed(PAST_THE_CAP.to_owned())),
        Err(error) => return Err(failed(error)),
    };
    let line = manifest.line();
    within_cap(line.len(), cap).map_err(Unbuilt::Failed)?;
    Ok((line, manifest, next))
}

/// The SHA-256 of the file a walk `found` and the stamp its handle reads, taken before the hash. `go_on` is asked first,
/// as a read asks it, so an interrupt stops the file before it is opened. A file whose path the cache does not hold is
/// then read by [`hashed`]; one it holds is opened as [`opened_inside`] checks it and takes the hash the cache holds at
/// the stamp that handle reads: the walk's stamp comes from the folder listing, which can lag a file still being
/// written, and only the handle's is what the install compares. On a mismatch the file is hashed through the handle
/// already opened. The stamp is taken before the hash, never after: a write during the hash then leaves the file refused
/// by the install as changed during the run, where a stamp read after would pair the old hash with the new stamp and let
/// the install replace the write.
pub(super) fn sha256_for(
    file: &Found,
    inside: &Path,
    cache: &HashCache,
    go_on: &dyn Fn() -> bool,
    stopped: &Cell<bool>,
) -> io::Result<(String, Stamp)> {
    asked(go_on, stopped)?;
    let Some(held) = cache.held(&file.path) else {
        return hashed(&file.full, inside, go_on, stopped);
    };
    let opened = opened_inside(&file.full, inside)?;
    let stamp = stamp_of(&opened.metadata().map_err(|error| in_context(&file.full, &error))?);
    if held.stamp == stamp {
        return Ok((held.sha256.clone(), stamp));
    }
    Ok((read_sha256(&opened, &file.full, go_on, stopped)?, stamp))
}

/// Asks `go_on` whether to go on; once it says no, marks `stopped` and fails as the console interrupt.
fn asked(go_on: &dyn Fn() -> bool, stopped: &Cell<bool>) -> io::Result<()> {
    if go_on() {
        return Ok(());
    }
    stopped.set(true);
    // Not `ErrorKind::Interrupted`, which a copy retries.
    Err(io::Error::other("the console interrupt was raised"))
}

/// The file at `path`, opened, once its final path, as the system resolves the open file, is found inside `inside`,
/// the worktree's own final path: a folder swapped for a junction after the walk is not followed out of it. Else why
/// it cannot be opened or is refused, its path named.
fn opened_inside(path: &Path, inside: &Path) -> io::Result<File> {
    let file = File::open(path).map_err(|error| in_context(path, &error))?;
    if !final_path(&file).map_err(|error| in_context(path, &error))?.starts_with(inside) {
        return Err(io::Error::other(format!("{} resolves outside the worktree", path.display())));
    }
    Ok(file)
}

/// The SHA-256 of the file at `path` and the stamp its handle reads, taken before the hash, opened once as
/// [`opened_inside`] finds it inside `inside`. Else why it cannot be read or is refused, its path named.
fn hashed(path: &Path, inside: &Path, go_on: &dyn Fn() -> bool, stopped: &Cell<bool>) -> io::Result<(String, Stamp)> {
    let file = opened_inside(path, inside)?;
    let stamp = stamp_of(&file.metadata().map_err(|error| in_context(path, &error))?);
    Ok((read_sha256(&file, path, go_on, stopped)?, stamp))
}

/// The SHA-256 of an opened `file` at `path`, read through a [`Polled`] reader that asks `go_on` before each of its
/// reads; else why its read failed, its path named.
fn read_sha256(file: &File, path: &Path, go_on: &dyn Fn() -> bool, stopped: &Cell<bool>) -> io::Result<String> {
    let polled = Polled { inner: file, go_on, stopped };
    sha256_of(polled).map(|(sha256, _)| sha256).map_err(|error| in_context(path, &error))
}

/// A reader that asks `go_on` before each read, and fails the read once it says no, marking `stopped`.
pub(super) struct Polled<'p, R> {
    pub(super) inner: R,
    pub(super) go_on: &'p dyn Fn() -> bool,
    pub(super) stopped: &'p Cell<bool>,
}

impl<R: Read> Read for Polled<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        asked(self.go_on, self.stopped)?;
        self.inner.read(buffer)
    }
}

/// Whether a manifest line `length` bytes long, its newline included, is within `cap`; else why not.
fn within_cap(length: usize, cap: usize) -> Result<(), String> {
    if length > cap {
        return Err(PAST_THE_CAP.to_owned());
    }
    Ok(())
}

/// The line saying a run's outputs are not brought back, for `reason`.
pub(super) fn not_brought_back(reason: &str) -> String {
    format!("gate: remote outputs not brought back: {}", escaped(reason))
}

/// Whether the pending marker at `marker` is there. One whose presence cannot be read counts as there: the run then
/// asks for every output that differs from the PC's, which is never wrong, only slower.
fn pending(marker: &Path) -> bool {
    marker.try_exists().unwrap_or(true)
}

/// Writes the pending marker at `marker`, its folder made when missing; else why it could not be written.
fn mark_pending(marker: &Path) -> Result<(), String> {
    let cannot = |error: io::Error| format!("cannot write the outputs pending marker {}: {error}", marker.display());
    if let Some(folder) = marker.parent() {
        fs::create_dir_all(folder).map_err(cannot)?;
    }
    fs::write(marker, "").map_err(cannot)
}

/// Removes the pending marker at `marker`; one already gone counts as removed. Else why it could not be removed.
pub(super) fn clear_pending(marker: &Path) -> Result<(), String> {
    match fs::remove_file(marker) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot remove the outputs pending marker {}: {error}", marker.display()))
        }
        _ => Ok(()),
    }
}

/// A call's last outputs line on standard error, and the size line heard just before it, when there was one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OutputsLines {
    pub(super) line: String,
    pub(super) size: Option<String>,
}

/// Keeps `line` as `kept`, a call's last outputs line, when it came on standard error (`stderr`) and starts
/// `gate-remote-outputs: `, with `previous`, the standard error line heard just before it, as its size line when that
/// starts `gate-remote-outputs-size: `. A line on standard output never counts, and a later outputs line replaces an
/// earlier one with its own size line or none. The box says its size line just before its outputs line, so a size line
/// the command printed anywhere else is never taken; the size line does not start as the outputs line does, so a PC that
/// reads only the outputs line passes it by. With a box too old to say the size line, a command's own last standard
/// error line that starts `gate-remote-outputs-size: ` is read as the size, which only misstates the size and the
/// estimate the pull's start and progress lines say.
pub(super) fn keep_outputs_lines(kept: &mut Option<OutputsLines>, previous: Option<&str>, line: &str, stderr: bool) {
    if stderr && line.starts_with(OUTPUTS) {
        *kept = Some(OutputsLines {
            line: line.to_owned(),
            size: previous.filter(|previous| previous.starts_with(OUTPUTS_SIZE)).map(str::to_owned),
        });
    }
}

/// What the box packed, from `heard`, the run call's last outputs line on standard error and its size line
/// ([`keep_outputs_lines`]): when the outputs line is `gate-remote-outputs: 0` or names `<count> <key>-<nonce>.tar.zst`,
/// the archive of the work folder `key` and of this run's outputs `nonce`, its size in bytes read from the size line
/// when that is `gate-remote-outputs-size: <bytes>`; a box not yet pushed says none, and any other size line gives no
/// size. Any other outputs line, or none, brings nothing back: the command's own output can print such a line, but not
/// after the box's own, which comes last.
pub(super) fn accepted(heard: Option<&OutputsLines>, key: &str, nonce: &str) -> Option<Brought> {
    let heard = heard?;
    let size_line = heard.size.as_deref();
    let said = heard.line.strip_prefix(OUTPUTS)?;
    if said == "0" {
        return Some(Brought::Nothing);
    }
    let (count, name) = said.split_once(' ')?;
    let count = whole_number(count).filter(|count| *count > 0)?;
    let size = size_line.and_then(|line| line.strip_prefix(OUTPUTS_SIZE)).and_then(whole_number);
    (name == archive_file_name(key, nonce)).then(|| Brought::Archive {
        count,
        name: name.to_owned(),
        size,
    })
}

/// `text` as a whole number when it is ASCII digits alone and fits a `u64`; else `None`.
fn whole_number(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    digits.then(|| text.parse().ok()).flatten()
}

/// `bytes` in the outputs success line's megabytes (1048576 bytes, [`install::MB`]) to one decimal, a half rounded away
/// from zero, as `{x.x} MB`, so every `MB` figure one pull says is the same number for the same bytes. Every figure of
/// the outputs pull's lines is said through this; a rate is this of the bytes per second, followed by `/s`
/// ([`mb_rate`]).
pub(super) fn mb_size(bytes: f64) -> String {
    format!("{} MB", install::one_decimal(bytes / install::MB))
}

/// The mean rate of `bytes` over `seconds`, as [`mb_size`] says it, followed by `/s`; 0 over no time at all.
#[expect(clippy::cast_precision_loss, reason = "a rate said to one decimal of a megabyte")]
pub(super) fn mb_rate(bytes: u64, seconds: f64) -> String {
    let per_second = if seconds > 0.0 { bytes as f64 / seconds } else { 0.0 };
    format!("{}/s", mb_size(per_second))
}

/// The line an outputs pull says as it starts, from the builder `builder` (its alias), for `count` files and, when the
/// box said it, the archive's `size` in bytes.
#[expect(clippy::cast_precision_loss, reason = "a size said to one decimal of a megabyte")]
fn pull_start_line(builder: &str, count: u64, size: Option<u64>) -> String {
    match size {
        Some(size) => format!("gate: pulling outputs from {builder}: {count} file(s), {}", mb_size(size as f64)),
        None => format!("gate: pulling outputs from {builder}: {count} file(s)"),
    }
}

/// The line an outputs pull says every period while it runs, `done` bytes in after `elapsed`, of the archive's `total`
/// when the box said it: how much, the mean rate since the pull's start and, with a total and a rate above 0, about how
/// long is left at that rate ([`left_text`]).
#[expect(clippy::cast_precision_loss, reason = "sizes said to one decimal of a megabyte")]
pub(super) fn progress_line(done: u64, total: Option<u64>, elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    let rate = mb_rate(done, seconds);
    let shown = mb_size(done as f64);
    match total {
        None => format!("gate: pulling outputs: {shown} so far, {rate}"),
        Some(total) if done > 0 && seconds > 0.0 => {
            let left = total.saturating_sub(done) as f64 / (done as f64 / seconds);
            format!(
                "gate: pulling outputs: {shown} of {}, {rate}, about {} left",
                mb_size(total as f64),
                left_text(left)
            )
        }
        Some(total) => format!("gate: pulling outputs: {shown} of {}, {rate}", mb_size(total as f64)),
    }
}

/// How long an outputs pull has left, `seconds`: in whole minutes, rounded to the nearest, from 60 s, else in whole
/// seconds, rounded to the nearest.
fn left_text(seconds: f64) -> String {
    if seconds >= 60.0 {
        return format!("{} min", (seconds / 60.0).round());
    }
    format!("{} s", seconds.round())
}

/// The line an outputs pull that ended other than whole says before its end's own lines: how long it ran, `pulled`,
/// and the bytes it had `received` by then.
#[expect(clippy::cast_precision_loss, reason = "a size said to one decimal of a megabyte")]
fn pull_ended_line(pulled: Duration, received: u64) -> String {
    format!(
        "gate: pulling outputs ended after {} s with {} received",
        install::one_decimal(pulled.as_secs_f64()),
        mb_size(received as f64)
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant, SystemTime};

    use gate_router::DEFAULT_OUTPUTS;

    use super::{
        Archive, Brought, MANIFEST_LIMIT, NonZeroU64, PARTIAL, PAST_THE_CAP, Unbuilt, accepted, hashed, keep_outputs_lines, manifest_line, mb_size,
        pending, sha256_for, stopped_reason, turned_reason, within_cap,
    };
    use crate::outputs::cache::{HashCache, file_time_now};
    use crate::outputs::glob::Globs;
    use crate::outputs::hash::file_sha256;
    use crate::outputs::walk::{Found, Stamp, stamp_of, walk};
    use crate::remote::client::End;
    use crate::remote::syncback::{PAIR_NOT_APPLIED, Synced};

    const KEY: &str = "app-1a2b3c4d";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const OTHER_NONCE: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn a_stopped_fetch_says_the_bytes_it_had_fetched_in_whole_seconds_and_their_rate() {
        let stopped = |bytes: u64, millis: u64| {
            stopped_reason(Some(crate::remote::client::Received {
                bytes,
                after: Duration::from_millis(millis),
            }))
        };
        assert_eq!(
            stopped(3 << 20, 2_000),
            "the fetch was stopped by the watchdog after 3.0 MiB in 2 s (1.5 MiB/s)"
        );
        assert_eq!(
            stopped(1 << 30, 1_800_400),
            "the fetch was stopped by the watchdog after 1.0 GiB in 1800 s (582.5 KiB/s)",
            "the seconds are whole, cut down"
        );
        assert_eq!(
            stopped(100, 300),
            "the fetch was stopped by the watchdog after 100 B in 1 s (100 B/s)",
            "under 1 s counts as 1 s"
        );
        assert_eq!(
            stopped(0, 0),
            "the fetch was stopped by the watchdog after 0 B in 1 s (0 B/s)",
            "nothing came"
        );
        assert_eq!(
            stopped_reason(None),
            "the fetch was stopped by the watchdog",
            "an unknown count says what it always said"
        );
    }
    const ERR: bool = true;
    const OUT: bool = false;

    /// What the client accepts of a run call that wrote `lines`, each on standard error or not, in order.
    fn accepted_of(lines: &[(bool, &str)]) -> Option<Brought> {
        let (mut kept, mut previous) = (None, None);
        for (stderr, line) in lines {
            keep_outputs_lines(&mut kept, previous, line, *stderr);
            if *stderr {
                previous = Some(*line);
            }
        }
        accepted(kept.as_ref(), KEY, NONCE)
    }

    /// The box's line for `count` files packed in the archive of `key` and `nonce`.
    fn named(count: u64, key: &str, nonce: &str) -> String {
        format!("gate-remote-outputs: {count} {key}-{nonce}.tar.zst")
    }

    /// What the box's line for `count` files of this run's archive says it brought.
    fn archive(count: u64) -> Brought {
        Brought::Archive {
            count,
            name: format!("{KEY}-{NONCE}.tar.zst"),
            size: None,
        }
    }

    /// What the box's size line and outputs line for `count` files of this run's archive, `size` bytes, say it brought.
    fn sized(count: u64, size: u64) -> Brought {
        Brought::Archive {
            count,
            name: format!("{KEY}-{NONCE}.tar.zst"),
            size: Some(size),
        }
    }

    #[test]
    fn the_pc_reads_the_box_s_size_line() {
        let line = named(2, KEY, NONCE);
        assert_eq!(
            accepted_of(&[(ERR, "gate-remote-outputs-size: 4096"), (ERR, line.as_str())]),
            Some(sized(2, 4096))
        );
        assert_eq!(
            accepted_of(&[(ERR, "gate-remote-outputs-size: 0"), (ERR, line.as_str())]),
            Some(sized(2, 0)),
            "a size of 0 is a size"
        );
        assert_eq!(
            accepted_of(&[
                (ERR, "gate-remote-outputs-size: 1"),
                (ERR, "gate-remote-outputs-size: 7"),
                (ERR, line.as_str())
            ]),
            Some(sized(2, 7)),
            "the last size line counts"
        );
        assert_eq!(
            accepted_of(&[(OUT, "gate-remote-outputs-size: 4096"), (ERR, line.as_str())]),
            Some(archive(2)),
            "a size line on standard output never counts"
        );
        assert_eq!(
            accepted_of(&[(ERR, "gate-remote-outputs-size: 4096"), (ERR, "gate-remote-outputs: 0")]),
            Some(Brought::Nothing),
            "no archive has no size"
        );
    }

    #[test]
    fn only_the_size_line_just_before_the_outputs_line_gives_a_size() {
        let line = named(2, KEY, NONCE);
        assert_eq!(
            accepted_of(&[(ERR, "gate-remote-outputs-size: 4096"), (ERR, line.as_str())]),
            Some(sized(2, 4096)),
            "the size line just before the outputs line"
        );
        assert_eq!(
            accepted_of(&[
                (ERR, "gate-remote-outputs-size: 4096"),
                (ERR, "the command's own line"),
                (ERR, line.as_str())
            ]),
            Some(archive(2)),
            "a size line with another standard error line between gives no size"
        );
        assert_eq!(
            accepted_of(&[
                (ERR, "gate-remote-outputs-size: 4096"),
                (ERR, line.as_str()),
                (ERR, "the command's own line"),
                (ERR, line.as_str())
            ]),
            Some(archive(2)),
            "the size goes with the last outputs line, not an earlier one"
        );
        assert_eq!(
            accepted_of(&[
                (ERR, "gate-remote-outputs-size: 4096"),
                (OUT, "a line on standard output"),
                (ERR, line.as_str())
            ]),
            Some(sized(2, 4096)),
            "a standard output line between does not part them"
        );
    }

    #[test]
    fn an_outputs_line_without_a_size_line_still_reads() {
        let bare = named(2, KEY, NONCE);
        assert_eq!(
            accepted_of(&[(ERR, bare.as_str())]),
            Some(archive(2)),
            "a builder not yet pushed says no size"
        );
    }

    #[test]
    fn a_size_line_whose_size_is_not_a_number_gives_no_size() {
        let line = named(1, KEY, NONCE);
        for size in ["x", "+5", "-5", "1.5", "", " 5", "5 ", "5 6", "99999999999999999999999"] {
            let said = format!("gate-remote-outputs-size: {size}");
            assert_eq!(accepted_of(&[(ERR, said.as_str()), (ERR, line.as_str())]), Some(archive(1)), "{said:?}");
        }
    }

    /// An older PC keeps the last line starting `gate-remote-outputs: `, as [`keep_outputs_lines`] does: the size line
    /// does not start so, so it is never kept as the outputs line and cannot replace the box's own.
    #[test]
    fn an_older_pc_s_reading_ignores_the_size_line() {
        let mut kept = None;
        keep_outputs_lines(&mut kept, None, "gate-remote-outputs-size: 4096", ERR);
        assert_eq!(kept, None, "the size line is no outputs line");
        let line = named(1, KEY, NONCE);
        keep_outputs_lines(&mut kept, None, &line, ERR);
        keep_outputs_lines(&mut kept, Some(line.as_str()), "gate-remote-outputs-size: 4096", ERR);
        assert_eq!(
            kept.map(|kept| kept.line),
            Some(line),
            "a size line after it leaves the outputs line kept"
        );
    }

    #[test]
    fn sizes_read_in_the_success_line_s_megabytes() {
        assert_eq!(mb_size(0.0), "0.0 MB");
        assert_eq!(mb_size(52_428.0), "0.0 MB", "just under half a tenth of 1048576 bytes");
        assert_eq!(mb_size(52_429.0), "0.1 MB", "a half rounds away from zero");
        assert_eq!(mb_size(300_000.0), "0.3 MB");
        assert_eq!(mb_size(1_048_576.0), "1.0 MB", "1048576 bytes, as the success line counts a megabyte");
        assert_eq!(mb_size(1_000_000_000.0), "953.7 MB");
        assert_eq!(mb_size(12_345_678_901.0), "11773.8 MB", "never another unit");
    }

    #[test]
    fn a_pair_sync_back_s_refusal_is_said_as_its_own_reason_and_a_single_repo_s_as_a_changed_worktree() {
        let synced = |status: i64, reason: Option<&'static str>| Synced {
            end: End::Status(status),
            reason,
        };

        assert_eq!(
            turned_reason(0, &synced(1, Some(PAIR_NOT_APPLIED))),
            Some("the pair's remote changes were not applied"),
            "a pair's refusal is not a changed worktree"
        );
        assert_eq!(turned_reason(0, &synced(1, None)), Some("the worktree changed during the run"));
        assert_eq!(turned_reason(0, &synced(0, None)), None, "a passing run that synced back");
        assert_eq!(
            turned_reason(3, &synced(3, Some(PAIR_NOT_APPLIED))),
            None,
            "a failing run keeps its own status"
        );
        let failed = Synced {
            end: End::Failed("git apply failed in lib".to_owned()),
            reason: None,
        };
        assert_eq!(turned_reason(0, &failed), None, "a partial apply fails the run and says so itself");
    }

    /// A scratch folder for one test, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-remote-outputs-{}-{name}", std::process::id()));
        // A folder left by an earlier run of the test may not be there.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
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
    fn the_last_outputs_line_on_stderr_wins() {
        let named_line = named(2, KEY, NONCE);
        let zero_then_named = [(ERR, "gate-remote-outputs: 0"), (OUT, "built"), (ERR, named_line.as_str()), (ERR, "done")];
        assert_eq!(accepted_of(&zero_then_named), Some(archive(2)));
        let named_then_zero = [(ERR, named_line.as_str()), (ERR, "gate-remote-outputs: 0")];
        assert_eq!(accepted_of(&named_then_zero), Some(Brought::Nothing));
        assert_eq!(accepted_of(&[(ERR, "gate: remote said something else")]), None, "no outputs line");
        assert_eq!(accepted_of(&[]), None, "no line at all");
    }

    #[test]
    fn a_line_naming_another_nonce_is_not_accepted() {
        let foreign = named(1, KEY, OTHER_NONCE);
        assert_eq!(accepted_of(&[(ERR, foreign.as_str())]), None);
        assert_eq!(accepted_of(&[(ERR, named(1, "app-99999999", NONCE).as_str())]), None, "another key");
        let own = named(1, KEY, NONCE);
        assert_eq!(
            accepted_of(&[(ERR, own.as_str()), (ERR, foreign.as_str())]),
            None,
            "only the last line counts"
        );
    }

    #[test]
    fn an_outputs_line_on_stdout_is_ignored() {
        let box_line = named(3, KEY, NONCE);
        assert_eq!(
            accepted_of(&[(ERR, box_line.as_str()), (OUT, "gate-remote-outputs: 0")]),
            Some(archive(3))
        );
        assert_eq!(accepted_of(&[(OUT, "gate-remote-outputs: 0")]), None);
        assert_eq!(accepted_of(&[(OUT, named(1, KEY, NONCE).as_str())]), None);
    }

    #[test]
    fn outputs_zero_is_accepted_without_a_name() {
        assert_eq!(accepted_of(&[(ERR, "gate-remote-outputs: 0")]), Some(Brought::Nothing));
        let zero_named = format!("gate-remote-outputs: 0 {KEY}-{NONCE}.tar.zst");
        assert_eq!(accepted_of(&[(ERR, zero_named.as_str())]), None, "no archive holds no file");
    }

    #[test]
    fn a_malformed_outputs_line_is_not_accepted() {
        let name = format!("{KEY}-{NONCE}.tar.zst");
        for line in [
            "gate-remote-outputs: ".to_owned(),
            "gate-remote-outputs: 1".to_owned(),
            "gate-remote-outputs: 00".to_owned(),
            "gate-remote-outputs: 0 ".to_owned(),
            format!("gate-remote-outputs: x {name}"),
            format!("gate-remote-outputs: +1 {name}"),
            format!("gate-remote-outputs: -1 {name}"),
            format!("gate-remote-outputs:  1 {name}"),
            format!("gate-remote-outputs: 1 {name} "),
            format!("gate-remote-outputs: 1 {name} more"),
            format!("gate-remote-outputs: 1 {name} 4096"),
            format!("gate-remote-outputs: 1 {KEY}-{NONCE}.tar.xz"),
        ] {
            assert_eq!(accepted_of(&[(ERR, line.as_str())]), None, "{line:?}");
        }
    }

    #[test]
    fn a_manifest_over_64_mib_is_refused() {
        assert_eq!(MANIFEST_LIMIT, 64 * 1024 * 1024);
        assert_eq!(within_cap(MANIFEST_LIMIT, MANIFEST_LIMIT), Ok(()), "64 MiB, its newline included, fits");
        assert_eq!(
            within_cap(MANIFEST_LIMIT + 1, MANIFEST_LIMIT),
            Err("it is larger than the box takes (64 MiB)".to_owned())
        );
    }

    /// The entries' count stops short of the line's outer braces, so only the check of the whole line refuses a line
    /// one byte past the cap.
    #[test]
    fn the_whole_manifest_line_is_checked_against_the_cap() {
        let root = scratch("cap");
        fs::create_dir_all(root.join("bin")).expect("the folder can be made");
        fs::write(root.join("bin").join("a.dll"), "abc").expect("the file can be written");
        let globs = ["**".to_owned()];
        let none = HashCache::default();
        let (line, _, _) = manifest_line(&root, &globs, MANIFEST_LIMIT, &none, || true).expect("the manifest is built");

        let fits = manifest_line(&root, &globs, line.len(), &none, || true).map(|(fitting, _, _)| fitting);
        assert_eq!(fits, Ok(line.clone()));
        let refused = Err(Unbuilt::Failed("it is larger than the box takes (64 MiB)".to_owned()));
        assert_eq!(manifest_line(&root, &globs, line.len() - 1, &none, || true), refused, "the whole line");
        assert_eq!(manifest_line(&root, &globs, 10, &none, || true), refused, "the entries, before any hash");
        assert_eq!(manifest_line(&root, &globs, MANIFEST_LIMIT, &none, || false), Err(Unbuilt::Interrupted));
    }

    /// An hour, the age a file is given so a build takes it as settled.
    const HOUR: Duration = Duration::from_hours(1);

    /// A SHA-256 no file of a test holds, which a build can take only from the cache.
    fn sentinel() -> String {
        "f".repeat(64)
    }

    /// Sets the last write of the file at `path` to `at`.
    fn written_at(path: &Path, at: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(at))
            .expect("the file's last write can be set");
    }

    /// The files below `root`, as a build's walk finds them.
    fn walked(root: &Path) -> Vec<Found> {
        walk(root, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk")
    }

    /// A worktree holding `bin/a.dll`, last written an hour ago, and a cache holding the sentinel for it at its stamp.
    fn cached_worktree(name: &str) -> (PathBuf, HashCache) {
        let root = scratch(name);
        fs::create_dir_all(root.join("bin")).expect("the folder can be made");
        let file = root.join("bin").join("a.dll");
        fs::write(&file, "abc").expect("the file can be written");
        written_at(&file, SystemTime::now() - HOUR);
        let mut cache = HashCache::default();
        let file = &walked(&root)[0];
        cache.keep(&file.path, file.stamp, &sentinel(), file_time_now());
        (root, cache)
    }

    /// The SHA-256 a manifest built of the worktree `root` with `cache` holds for `bin/a.dll`.
    fn built_sha256(root: &Path, cache: &HashCache) -> Option<String> {
        let (_, files, _) = manifest_line(root, &["**".to_owned()], MANIFEST_LIMIT, cache, || true).expect("the manifest is built");
        files.get("bin/a.dll").map(|held| held.sha256.clone())
    }

    /// A hit takes the sentinel, which no file holds, and asks `go_on` as a read does: a `go_on` that says no stops a
    /// build that would read no byte.
    #[test]
    fn a_cached_stamp_is_not_hashed_again() {
        let (root, cache) = cached_worktree("cache-hit");
        let globs = ["**".to_owned()];

        let (line, files, next) = manifest_line(&root, &globs, MANIFEST_LIMIT, &cache, || true).expect("the manifest is built");

        assert!(line.contains(&sentinel()), "{line}");
        assert_eq!(files.get("bin/a.dll").map(|held| held.sha256.clone()), Some(sentinel()));
        assert_eq!(next, cache, "the hash taken from the cache is kept");
        let asked = manifest_line(&root, &globs, MANIFEST_LIMIT, &cache, || false).map(|(asked, _, _)| asked);
        assert_eq!(asked, Err(Unbuilt::Interrupted), "a hit asks go_on");
    }

    /// The cache is keyed by the stamp the opened file's handle reads, which the walk's listing can lag behind: an entry
    /// saved at the listing's stamp is a miss, and the file is hashed.
    #[test]
    fn an_entry_saved_at_the_listing_stamp_misses_when_the_handle_stamp_differs() {
        let (root, _) = cached_worktree("cache-handle-stamp");
        let mut listed = walked(&root).remove(0);
        listed.stamp.modified -= 10_000_000;
        let mut cache = HashCache::default();
        cache.keep(&listed.path, listed.stamp, &sentinel(), file_time_now());
        assert_eq!(
            cache.held(&listed.path).map(|held| held.stamp),
            Some(listed.stamp),
            "the cache holds the listed stamp"
        );
        let inside = fs::canonicalize(&root).expect("the worktree resolves");

        let taken = sha256_for(&listed, &inside, &cache, &|| true, &Cell::new(false));

        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(
            taken.map(|(sha256, _)| sha256).map_err(|error| error.to_string()),
            Ok(abc.to_owned()),
            "an entry at the listing's stamp is not the handle's"
        );
    }

    /// A `go_on` that writes each of `writes` in turn over the file at `path`, setting its last write to `at` after
    /// each, and says to go on; once every write has landed, later asks only say to go on. The build asks `go_on` before
    /// it opens a file and again before each read of one, so which of the two a write lands between is the test's choice.
    fn written_during<'a>(path: &'a Path, writes: &'static [&'static [u8]], at: SystemTime) -> impl Fn() -> bool + 'a {
        let done = Cell::new(0_usize);
        move || {
            if let Some(bytes) = writes.get(done.get()) {
                let mut file = File::options().write(true).open(path).expect("the file can be written during the run");
                file.write_all(bytes).expect("the file can be written during the run");
                file.set_modified(at).expect("the file's last write can be set");
                done.set(done.get() + 1);
            }
            true
        }
    }

    /// The stamp the file at `path` reads from its handle, as a cache lookup takes it.
    fn read_stamp(path: &Path) -> Stamp {
        let held = File::open(path)
            .expect("the file opens")
            .metadata()
            .expect("the file's handle reads its stamp");
        stamp_of(&held)
    }

    /// A worktree holding `bin/a.dll` written `abc`, its last write `at`, and that file's path.
    fn abc_worktree(name: &str, at: SystemTime) -> (PathBuf, PathBuf) {
        let root = scratch(name);
        fs::create_dir_all(root.join("bin")).expect("the folder can be made");
        let file = root.join("bin").join("a.dll");
        fs::write(&file, "abc").expect("the file can be written");
        written_at(&file, at);
        (root, file)
    }

    /// The manifest's stamp is the one the file's handle read at the open, not the walk's: the walk's comes from the
    /// folder listing, which can lag a file still being written, while the install compares the manifest's stamp
    /// against a handle read of its own. The file is written during the run, after the walk and before the open.
    #[test]
    fn a_miss_takes_its_manifest_stamp_from_the_handle() {
        let at = SystemTime::now() - HOUR;
        let (root, file) = abc_worktree("miss-handle-stamp", at);
        let listed = walked(&root).remove(0);

        let (_, files, _) = manifest_line(
            &root,
            &["**".to_owned()],
            MANIFEST_LIMIT,
            &HashCache::default(),
            written_during(&file, &[b"abcd"], at),
        )
        .expect("the manifest is built");

        assert_eq!(
            files.get("bin/a.dll").map(|held| held.stamp),
            Some(Stamp {
                size: 4,
                modified: listed.stamp.modified
            }),
            "the stamp the handle read of the written file, not the walk's of the one before it"
        );
    }

    /// The cache is keyed by the stamp the file's handle reads, not the walk's: an entry saved at the handle's stamp
    /// hits a file whose listing stamp is another, so the run takes the saved hash instead of reading the file.
    #[test]
    fn a_hit_is_keyed_by_the_handle_stamp_not_the_listing_one() {
        let at = SystemTime::now() - HOUR;
        let (root, file) = abc_worktree("hit-handle-key", at);
        fs::write(&file, "abcd").expect("the file can be written");
        written_at(&file, at);
        let handled = walked(&root).remove(0).stamp;
        fs::write(&file, "abc").expect("the file can be written back");
        written_at(&file, at);
        let path = scratch("hit-handle-key-cache").join("app-1a2b3c4d.outputs-hashes");
        let entry = format!(
            "{{\"files\":{{\"bin/a.dll\":{{\"size\":{},\"modified\":{},\"sha256\":\"{}\"}}}}}}\n",
            handled.size,
            handled.modified,
            sentinel()
        );
        fs::write(&path, entry).expect("the cache can be written");
        let cache = HashCache::load(&path).expect("the cache loads");

        let (_, files, _) =
            manifest_line(&root, &["**".to_owned()], MANIFEST_LIMIT, &cache, written_during(&file, &[b"abcd"], at)).expect("the manifest is built");

        assert_eq!(files.get("bin/a.dll").map(|held| held.sha256.clone()), Some(sentinel()));
    }

    /// A file written while it is hashed keeps the stamp read before the hash, never one read after: the install then
    /// refuses it as changed during the run rather than replacing the write. This build is asked to go on before it
    /// opens the file and again before each read of it, and the file is written on each of the first two asks: `abcd`
    /// before the open, so the handle reads the written file, and `abcde` between the stamp read and the hash. A stamp
    /// read after the hash would be the last write's, letting the install replace the write.
    #[test]
    fn a_write_during_the_hash_leaves_the_stamp_read_before_it() {
        let at = SystemTime::now() - HOUR;
        let (root, file) = abc_worktree("write-during-hash", at);
        let listed = walked(&root).remove(0);

        let (_, files, _) = manifest_line(
            &root,
            &["**".to_owned()],
            MANIFEST_LIMIT,
            &HashCache::default(),
            written_during(&file, &[b"abcd", b"abcde"], at),
        )
        .expect("the manifest is built");

        assert_eq!(
            files.get("bin/a.dll").map(|held| held.stamp),
            Some(Stamp {
                size: 4,
                modified: listed.stamp.modified
            }),
            "the stamp the handle read before the hash, not the one the last write left after it"
        );
    }

    /// The cap counts the entry the line will hold, not the walk's: a file written during the run whose handle stamp
    /// carries a size digit the walk's does not pushes the count past the cap, so the file after it is never read. That
    /// file is removed on the same ask, so a build that reaches it fails some other way than the cap.
    #[test]
    fn a_written_files_entry_is_counted_as_the_line_holds_it() {
        let at = SystemTime::now() - HOUR;
        let (one, one_file) = abc_worktree("cap-count-one", at);
        let line = manifest_line(
            &one,
            &["**".to_owned()],
            MANIFEST_LIMIT,
            &HashCache::default(),
            written_during(&one_file, &[b"0123456789"], at),
        )
        .map(|(line, _, _)| line)
        .expect("the manifest is built");
        // The bytes one entry of `bin/a.dll` counts, from the line of one file: that line is the entry without its last
        // comma, plus `{"files":{}}` and a newline. Both files here are `abc` at the same instant, so the walk's own
        // count of the two is that entry twice — one byte under what the count becomes once the write's ten bytes, with
        // a size digit the walk's three does not have, are read.
        let entry = line.len() - "{\"files\":{}}\n".len();
        let cap = 2 * entry;

        let (two, two_file) = abc_worktree("cap-count-two", at);
        let later = two.join("bin").join("b.dll");
        fs::write(&later, "abc").expect("the file can be written");
        written_at(&later, at);
        let done = Cell::new(false);
        let during = || {
            if !done.replace(true) {
                let mut file = File::options()
                    .write(true)
                    .open(&two_file)
                    .expect("the file can be written during the run");
                file.write_all(b"0123456789").expect("the file can be written during the run");
                file.set_modified(at).expect("the file's last write can be set");
                fs::remove_file(&later).expect("the file after it can be removed");
            }
            true
        };

        assert_eq!(
            manifest_line(&two, &["**".to_owned()], cap, &HashCache::default(), during),
            Err(Unbuilt::Failed(PAST_THE_CAP.to_owned())),
            "the count of the entry the line holds stops the build before the file after it"
        );
    }

    #[test]
    fn a_changed_size_or_modified_is_hashed_again() {
        let (root, cache) = cached_worktree("cache-miss");
        let file = root.join("bin").join("a.dll");

        written_at(&file, SystemTime::now() - 2 * HOUR);
        let other_write = built_sha256(&root, &cache);
        fs::write(&file, "abcd").expect("the file can be rewritten");
        written_at(&file, SystemTime::now() - HOUR);
        let other_size = built_sha256(&root, &cache);

        assert_eq!(
            other_write.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            "the same size, another last write"
        );
        assert_eq!(other_size, Some(file_sha256(&file).expect("the file can be hashed")), "another size");
    }

    #[test]
    fn a_build_keeps_the_hashes_of_settled_files_only() {
        let root = scratch("cache-keep");
        fs::create_dir_all(root.join("bin")).expect("the folder can be made");
        for (name, at) in [("settled.dll", SystemTime::now() - HOUR), ("ahead.dll", SystemTime::now() + HOUR)] {
            fs::write(root.join("bin").join(name), "abc").expect("the file can be written");
            written_at(&root.join("bin").join(name), at);
        }

        let (_, files, next) =
            manifest_line(&root, &["**".to_owned()], MANIFEST_LIMIT, &HashCache::default(), || true).expect("the manifest is built");

        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let found = walked(&root);
        let (ahead, settled) = (&found[0], &found[1]);
        assert_eq!(
            next.held(&settled.path).map(|held| held.sha256.as_str()),
            Some(abc),
            "a file written an hour before the build"
        );
        assert_eq!(next.held(&ahead.path), None, "a file written after the build began");
        assert!(files.get("bin/ahead.dll").is_some(), "the manifest holds every file");
    }

    /// The junction check of [`hashed`] holds for a hash taken from the cache too: a folder swapped for a junction
    /// after the walk is not followed out of the worktree.
    #[test]
    fn a_cache_hit_still_refuses_a_file_that_leaves_the_worktree() {
        let folder = scratch("cache-final-path");
        let (root, elsewhere) = (folder.join("worktree"), folder.join("elsewhere"));
        fs::create_dir_all(&root).expect("the worktree can be made");
        fs::create_dir_all(&elsewhere).expect("the outside folder can be made");
        fs::write(elsewhere.join("secret.dll"), "abc").expect("the file can be written");
        written_at(&elsewhere.join("secret.dll"), SystemTime::now() - HOUR);
        junction(&root.join("bin"), &elsewhere);
        let through = Found {
            path: "bin/secret.dll".to_owned(),
            full: root.join("bin").join("secret.dll"),
            stamp: walked(&elsewhere)[0].stamp,
        };
        let mut cache = HashCache::default();
        cache.keep(&through.path, through.stamp, &sentinel(), file_time_now());
        assert_eq!(
            cache.held(&through.path).map(|held| held.sha256.as_str()),
            Some(sentinel().as_str()),
            "the cache holds the file"
        );
        let inside = fs::canonicalize(&root).expect("the worktree resolves");

        let taken = sha256_for(&through, &inside, &cache, &|| true, &Cell::new(false));

        let refused = taken.expect_err("a file reached through a junction is refused").to_string();
        assert_eq!(refused, format!("{} resolves outside the worktree", through.full.display()));
    }

    #[test]
    fn a_file_whose_final_path_leaves_the_worktree_is_refused() {
        let folder = scratch("final-path");
        let (root, elsewhere) = (folder.join("worktree"), folder.join("elsewhere"));
        fs::create_dir_all(&root).expect("the worktree can be made");
        fs::create_dir_all(&elsewhere).expect("the outside folder can be made");
        fs::write(root.join("own.dll"), "abc").expect("the file can be written");
        fs::write(elsewhere.join("secret.dll"), "abc").expect("the file can be written");
        junction(&root.join("bin"), &elsewhere);
        let inside = fs::canonicalize(&root).expect("the worktree resolves");
        let stopped = Cell::new(false);

        let own = hashed(&root.join("own.dll"), &inside, &|| true, &stopped);
        let through = root.join("bin").join("secret.dll");
        let outside = hashed(&through, &inside, &|| true, &stopped);

        assert!(own.is_ok(), "{own:?}");
        let refused = outside.expect_err("a file reached through a junction is refused").to_string();
        assert_eq!(refused, format!("{} resolves outside the worktree", through.display()));
    }

    #[test]
    fn an_archive_create_whose_tmp_is_a_junction_is_refused_and_writes_nothing_through_it() {
        let folder = scratch("archive-create-junction");
        let (root, elsewhere) = (folder.join("worktree"), folder.join("elsewhere"));
        fs::create_dir_all(&root).expect("the worktree can be made");
        fs::create_dir_all(&elsewhere).expect("the outside folder can be made");
        junction(&root.join(".tmp"), &elsewhere);
        let path = root.join(".tmp").join("app.tar.zst");
        let mut archive = Archive::new(&root, path.clone());

        let refused = archive.create(NonZeroU64::MAX).map(drop);

        let why = refused.expect_err("a junction .tmp is refused");
        assert!(why.ends_with("it is a link or a junction, not a folder"), "{why}");
        assert!(!elsewhere.join("app.tar.zst").exists(), "nothing is written through the junction");
        assert!(!path.exists(), "no archive is made");
    }

    /// The `.tmp` folder is held for the archive's and the stage's life: while it is held it can be neither renamed nor
    /// removed, so no one can swap it for a junction under the fetch or the install that follow.
    #[test]
    fn an_archive_holds_its_tmp_folder_against_a_rename_until_it_is_dropped() {
        let root = scratch("archive-hold");
        let path = root.join(".tmp").join("app.tar.zst");
        let mut archive = Archive::new(&root, path.clone());
        let made = archive.create(NonZeroU64::MAX).expect("the archive is made");
        drop(made);
        let elsewhere = root.join("elsewhere");

        let renamed = fs::rename(root.join(".tmp"), &elsewhere);

        assert!(renamed.is_err(), "a held .tmp cannot be renamed: {renamed:?}");
        assert!(root.join(".tmp").is_dir(), "it is still the worktree's .tmp");
        drop(archive);
        let freed = fs::rename(root.join(".tmp"), &elsewhere);
        assert!(freed.is_ok(), "the hold lasted as long as the archive: {freed:?}");
    }

    /// A path whose folder is not the worktree's own `.tmp` is refused before anything is made there.
    #[test]
    fn an_archive_made_outside_the_worktree_s_tmp_folder_is_refused() {
        let folder = scratch("archive-outside");
        let (root, elsewhere) = (folder.join("worktree"), folder.join("elsewhere"));
        fs::create_dir_all(&root).expect("the worktree can be made");
        fs::create_dir_all(&elsewhere).expect("the outside folder can be made");
        let path = elsewhere.join("app.tar.zst");
        let mut archive = Archive::new(&root, path.clone());

        let refused = archive.create(NonZeroU64::MAX).map(drop);

        let why = refused.expect_err("a folder that is not the worktree's .tmp is refused");
        assert!(why.contains("is not the worktree's .tmp folder"), "{why}");
        assert!(!path.exists(), "no archive is made");
        assert_eq!(archive.remove(PARTIAL), Ok(()), "nothing this run made is removed");
    }

    #[test]
    fn an_archive_file_is_made_anew_for_each_attempt_and_removed_only_when_made() {
        let root = scratch("archive");
        let path = root.join(".tmp").join("app.tar.zst");
        let mut archive = Archive::new(&root, path.clone());
        assert_eq!(archive.remove(PARTIAL), Ok(()), "nothing made, nothing removed");

        let mut first = archive.create(NonZeroU64::MAX).expect("the first attempt's file is made, its folder too");
        first.file.write_all(b"partial").expect("the file can be written");
        drop(first);
        drop(
            archive
                .create(NonZeroU64::MAX)
                .expect("a retry removes the first attempt's file and makes it anew"),
        );
        assert_eq!(
            fs::read(&path).expect("the file can be read"),
            Vec::<u8>::new(),
            "the retry's file starts empty"
        );
        assert_eq!(archive.remove(PARTIAL), Ok(()));
        assert!(!path.exists(), "a file this run made is removed");

        fs::write(&path, "another run's").expect("the file can be written");
        let mut fresh = Archive::new(&root, path.clone());
        let refused = fresh.create(NonZeroU64::MAX).map(drop);
        let cannot = format!("cannot write {}: ", path.display());
        assert!(refused.as_ref().is_err_and(|why| why.starts_with(&cannot)), "{refused:?}");
        assert_eq!(fresh.remove(PARTIAL), Ok(()));
        assert_eq!(
            fs::read_to_string(&path).expect("the file is there"),
            "another run's",
            "a file this run did not make stays"
        );
    }

    #[test]
    fn a_marker_whose_presence_cannot_be_read_counts_as_pending() {
        let folder = scratch("pending");
        assert!(!pending(&folder.join("app-1a2b3c4d.outputs-pending")), "a missing marker");
        fs::write(folder.join("app-1a2b3c4d.outputs-pending"), "").expect("the marker can be written");
        assert!(pending(&folder.join("app-1a2b3c4d.outputs-pending")), "a marker that is there");
        assert!(pending(Path::new("app\0.outputs-pending")), "a path the system cannot look up");
    }

    /// A lab measurement, not a check: how long the walk and the whole manifest build take on a real built worktree,
    /// cold, with an empty hash cache, and warm, with the cache the cold build saves loaded back. Reads the tree from
    /// `GATE_MEASURE_ROOT` and its globs from `GATE_MEASURE_GLOBS` (`;`-separated, [`DEFAULT_OUTPUTS`] when unset),
    /// prints one line of figures, and asserts nothing about time.
    #[test]
    #[ignore = "a lab measurement: set GATE_MEASURE_ROOT to a built worktree"]
    #[expect(clippy::print_stdout, reason = "a lab measurement prints its figures for the operator to read")]
    #[expect(clippy::cast_precision_loss, reason = "a size said to one decimal of a megabyte")]
    fn measure_the_manifest_time_of_a_real_tree() {
        let Ok(root) = std::env::var("GATE_MEASURE_ROOT") else {
            println!("measure: set GATE_MEASURE_ROOT to a built worktree to run this measurement");
            return;
        };
        let root = PathBuf::from(root);
        let globs: Vec<String> = match std::env::var("GATE_MEASURE_GLOBS") {
            Ok(list) => list.split(';').map(str::to_owned).collect(),
            Err(_) => DEFAULT_OUTPUTS.map(str::to_owned).to_vec(),
        };

        let started = Instant::now();
        let parsed = Globs::new(&globs).expect("the measurement's globs are well formed");
        let found = walk(&root, &parsed).expect("the measured tree can be walked");
        let walk_seconds = started.elapsed().as_secs_f64();
        let bytes: u64 = found.iter().map(|file| file.stamp.size).sum();

        let started = Instant::now();
        let built = manifest_line(&root, &globs, usize::MAX, &HashCache::default(), || true);
        let (line, _, saved) = built.expect("the measured tree can be hashed");
        let cold_seconds = started.elapsed().as_secs_f64();
        let path = scratch("measure").join("measure.outputs-hashes");
        saved.save(&path);

        let started = Instant::now();
        let cache = HashCache::load(&path).expect("the saved cache loads");
        manifest_line(&root, &globs, usize::MAX, &cache, || true).expect("the measured tree can be hashed again");
        let warm_seconds = started.elapsed().as_secs_f64();
        let cached = found
            .iter()
            .filter(|file| cache.held(&file.path).is_some_and(|held| held.stamp == read_stamp(&file.full)))
            .count();

        let mib = bytes as f64 / (1024.0 * 1024.0);
        println!(
            "manifest: {}: {} files, {mib:.1} MiB; walk {walk_seconds:.3} s; manifest_line cold {cold_seconds:.3} s, warm {warm_seconds:.3} s \
             ({cached} files cached); line {} bytes",
            root.display(),
            found.len(),
            line.len()
        );
    }
}
