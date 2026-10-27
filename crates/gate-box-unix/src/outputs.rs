//! A routed run's outputs on the Unix box, line for line as the Windows box's `boxside::outputs` says them: the steps a
//! run with output globs takes, each its own function for the run's outputs wiring to call, and the `box outputs` verb.
//! [`read_manifest`] reads the PC's manifest line, [`prune`] removes the outputs archives older than a day, [`listing`]
//! lists the files under the globs before the command, [`bring_back`] packs the files the PC lacks or holds otherwise
//! into `<key>-<nonce>.tar.zst` in the output folder, [`trim`] keeps the folder within its [`budget`], and
//! [`not_brought_back`] says why a run's outputs did not come back. A trim removes the archives last written before the
//! run's own, oldest first, never one written with or after it nor one a pack still writes, and removes the run's own
//! when that alone is over the budget. [`outputs`] answers the verb: it sends one archive raw on stdout, then removes
//! it, taking no work folder lock, so it never waits on a run. The archive is the only state outputs keep.
//!
//! The output folder is `<root>/outputs`, reached beneath the judged work root and the box user's own, 0700. The
//! command the box runs is the box's own user and may rename or link anything in the work folder, so the walk and the
//! pack reach every name beneath its folder's descriptor, one name at a time, a link never followed and never listed
//! ([`crate::beneath`]): only plain files are listed and packed, a FIFO never waited on. A file's stamp is its size and
//! its mtime in nanoseconds, compared box against box: the listing before the command against the file after it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate_args::BoxSeams;
use gate_inputs::archive::{Item, held_size, refused};
use gate_inputs::failure::{self, Capping, Caps};
use gate_inputs::glob::Globs;
use gate_inputs::hash::sha256_of;
use gate_inputs::manifest::{Chosen, HeldBy, Listing, MANIFEST_LIMIT, Manifest, Stamp, hash_possible_copies, select};
use gate_inputs::pack::{Packed, Packer, window_log};
use gate_inputs::walk::{Found, Walked, relative_path};
use gate_wire::{Payload, archive_file_name, archive_key, escaped};

use crate::beneath::{self, Folder};
use crate::console::Console;
use crate::cross::{self, Portable};
use crate::drain;
use crate::lines::{FAILED_STATUS, USAGE_STATUS};
use crate::request::{Unread, bounded_line};

/// How long an outputs archive is kept in the output folder before a run with output globs removes it, as the wrapper's
/// `$OutputArchiveHours` keeps it.
const ARCHIVE_AGE: Duration = Duration::from_hours(24);
/// The bytes in one gibibyte, as [`OUTPUTS_BUDGET`] and an over-large archive's size in the line saying so are counted.
const GIB: u64 = 1024 * 1024 * 1024;
/// The most of the output folder's outputs archives the box keeps, in bytes: 50 GiB. A pack that pushes the folder past
/// it removes the other archives, oldest first, until it fits again. The client unpacks an archive to no more bytes.
pub const OUTPUTS_BUDGET: u64 = 50 * GIB;
/// The line a pack says when there was nothing to bring back, and so no archive was written.
pub const NOTHING_PACKED: &str = "gate-remote-outputs: 0";
/// The bytes the `outputs` verb reads of its archive at a time.
const SEND_BUFFER: usize = 64 * 1024;
/// The nanoseconds in a second.
const NANOS: i128 = 1_000_000_000;

/// What a run with output or failure output globs asks for: the globs, the failure output globs, whether every file
/// counts as changed (`OutputsFull`), the caps a failed command's set is held to, the name of its archive and the folder
/// it goes in.
pub struct Asked<'a> {
    globs: Globs,
    /// The run's failure output globs, which a failed command brings back beside its test reports; `None` for a run
    /// without any.
    failure: Option<Globs>,
    full: bool,
    capping: Capping,
    /// The runtime identifier of the run's cross build, whose outputs the walk lists at their PC paths
    /// ([`cross::crossed`]); `None` for a run without one.
    rid: Option<String>,
    /// The name of the run's own outputs archive in its output folder.
    pub archive: String,
    /// The output folder the archive is written in, `<root>/outputs`.
    pub folder: &'a str,
}

impl<'a> Asked<'a> {
    /// The outputs of a run of the work folder `key`, its outputs nonce `nonce`, asking for the output globs `globs`
    /// and the failure output globs `failure`: `None` when both are empty.
    ///
    /// # Errors
    ///
    /// The usage line saying why a glob cannot be matched.
    pub fn new((globs, failure): (&[String], &[String]), full: bool, key: &str, nonce: &str, folder: &'a str) -> Result<Option<Asked<'a>>, String> {
        if globs.is_empty() && failure.is_empty() {
            return Ok(None);
        }
        let globs = Globs::new(globs).map_err(|reason| format!("gate: box payload Outputs {reason}"))?;
        let failure = if failure.is_empty() {
            None
        } else {
            Some(Globs::new(failure).map_err(|reason| format!("gate: box payload FailureOutputs {reason}"))?)
        };
        Ok(Some(Asked {
            globs,
            failure,
            full,
            capping: Capping::Uncapped,
            rid: None,
            archive: archive_file_name(key, nonce),
            folder,
        }))
    }

    /// What a failed command brings back, in the same archive and folder and without a cross build's: the failed run's
    /// set of these outputs ([`failure::failed_run_globs`]), its failure outputs held to `caps` ([`Capping`]); `None`
    /// when no glob names a `TestResults` folder and there are no failure output globs.
    #[must_use]
    pub fn failed_run(&self, caps: Caps) -> Option<Asked<'a>> {
        let (globs, capping) = failure::failed_run_globs(&self.globs, self.failure.as_ref(), caps)?;
        Some(Asked {
            globs,
            failure: None,
            full: self.full,
            capping,
            rid: None,
            archive: self.archive.clone(),
            folder: self.folder,
        })
    }

    /// These outputs, brought back from a run whose cross build is for `rid`, `None` for a run without one.
    #[must_use]
    pub fn crossed_for(self, rid: Option<&str>) -> Asked<'a> {
        Asked {
            rid: rid.map(str::to_owned),
            ..self
        }
    }

    /// The files `found`, listed at their PC paths, less each native file a cross build's file replaces
    /// ([`cross::crossed`]); all of them for a run without a cross build.
    fn deduped(&self, found: Vec<Found>) -> Vec<Found> {
        if self.rid.is_some() { cross::crossed(found) } else { found }
    }
}

/// The PC's manifest, read from `input` up to its first newline or its end, never waiting past the newline and never
/// reading more than [`MANIFEST_LIMIT`] bytes. The read has no deadline: a client that stops sending is ended by its
/// own watchdog, which closes the connection and so ends the input.
///
/// # Errors
///
/// Why the manifest cannot be read, which the run says once its command has passed: a line that reaches the limit
/// without its newline, a failed read, a line that is not UTF-8, or one that is not a manifest.
pub fn read_manifest(input: impl BufRead) -> Result<Manifest, String> {
    let line = match bounded_line(input, MANIFEST_LIMIT as u64) {
        Ok(line) => line,
        Err(Unread::OverLimit) => return Err("the outputs manifest reaches 64 MiB without a newline".to_owned()),
        Err(Unread::Failed(error)) => return Err(format!("the outputs manifest could not be read: {error}")),
    };
    let line = String::from_utf8(line).map_err(|_| "the outputs manifest is not UTF-8".to_owned())?;
    Manifest::parse(&line)
}

/// The output folder `folder`, `<root>/outputs`: reached beneath its work root, opened and judged as
/// [`Folder::open_root`] judges a root, a link at the output folder's name never followed. When nothing is at the
/// name and `make`, it is made 0700 ([`Folder::make_child`]); else the folder there is adopted only when it is the box
/// user's own and no group or other bit is set in its mode ([`Folder::open_private_child`]).
///
/// # Errors
///
/// The root's open or judging, a missing folder when not `make` (`NotFound`), a folder refused, or the making.
fn output_folder(folder: &str, make: bool) -> io::Result<Folder> {
    let path = Path::new(folder);
    let (Some(root), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the output folder is not <root>/outputs"));
    };
    let root = Folder::open_root(root)?;
    match root.open_private_child(name) {
        Err(error) if make && error.kind() == io::ErrorKind::NotFound => match root.make_child(name) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => root.open_private_child(name),
            made => made,
        },
        opened => opened,
    }
}

/// The folder `tree`, the work folder `<root>/work/<key>` of `asked`'s output folder's root or a folder below it, as a
/// pair's sibling tree is: the work folder opened beneath its judged root ([`beneath::work_beneath_root`]), then each
/// name below it beneath the one before, a link at any of them never followed.
///
/// # Errors
///
/// `tree` not the work folder of that root or below it (`InvalidInput`), the root's open or judging, or an open on the
/// way.
fn tree_folder(asked: &Asked<'_>, tree: &Path) -> io::Result<Folder> {
    let not_below = || io::Error::new(io::ErrorKind::InvalidInput, "the work folder is not <root>/work/<key>");
    let root = Path::new(asked.folder).parent().ok_or_else(not_below)?;
    let mut below = tree.strip_prefix(root).map_err(|_| not_below())?.iter();
    let (Some(folders), Some(key)) = (below.next(), below.next()) else {
        return Err(not_below());
    };
    if folders != OsStr::new("work") {
        return Err(not_below());
    }
    let mut folder = beneath::work_beneath_root(&root.join(folders).join(key))?;
    for name in below {
        folder = folder.open_child(name)?;
    }
    Ok(folder)
}

/// Removes each outputs archive in `asked`'s output folder, a plain file named `<key>-<nonce>.tar.zst` as the box names
/// one, last written more than [`ARCHIVE_AGE`] before `now`; every other name, a link of the archive's form included, is
/// left alone. The lines saying each archive that could not be removed, or that the folder could not be listed. A
/// missing folder holds nothing to remove, and an archive gone before its removal, as another run's prune may take it,
/// counts as removed.
#[must_use]
pub fn prune(asked: &Asked<'_>, now: SystemTime) -> Vec<String> {
    let path = Path::new(asked.folder);
    let folder = match output_folder(asked.folder, false) {
        Ok(folder) => folder,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![not_listed(path, &error)],
    };
    let names = match folder.entries() {
        Ok(names) => names,
        Err(error) => return vec![not_listed(path, &error)],
    };
    let mut lines = Vec::new();
    for name in names {
        if let Err(error) = remove_if_old(&folder, &name, now) {
            lines.push(could_not_remove(&path.join(&name), &error));
        }
    }
    lines
}

/// The line saying the outputs archive at `path` could not be removed, for `error`.
fn could_not_remove(path: &Path, error: &io::Error) -> String {
    format!(
        "gate: remote could not remove the old outputs archive {}",
        escaped(&in_context(path, error).to_string())
    )
}

/// The line saying the outputs archive at `path` could not be read, for `error`.
fn could_not_read(path: &Path, error: &io::Error) -> String {
    format!(
        "gate: remote could not read the outputs archive {}",
        escaped(&in_context(path, error).to_string())
    )
}

/// The line saying the output folder `folder` could not be listed for its old archives, for `error`.
fn not_listed(folder: &Path, error: &io::Error) -> String {
    format!(
        "gate: remote could not list the old outputs archives in {}",
        escaped(&in_context(folder, error).to_string())
    )
}

/// `error` with the `path` it happened at.
fn in_context(path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// Removes `name` in the output folder `folder` when it is an outputs archive, a plain file looked at as itself, last
/// written more than [`ARCHIVE_AGE`] before `now`; one already gone counts as removed.
fn remove_if_old(folder: &Folder, name: &OsStr, now: SystemTime) -> io::Result<()> {
    if name.to_str().and_then(archive_key).is_none() {
        return Ok(());
    }
    let stat = match folder.stat_child(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        looked => looked?,
    };
    let old = now.duration_since(modified_of(&stat)).is_ok_and(|age| age > ARCHIVE_AGE);
    if !(is_plain(&stat) && old) {
        return Ok(());
    }
    remove_archive(folder, name)
}

/// Removes the outputs archive `name` in `folder` as itself; one already gone, as another run's trim may take it,
/// counts as removed.
fn remove_archive(folder: &Folder, name: &OsStr) -> io::Result<()> {
    match folder.remove_child(name, false) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// Whether `stat` says a plain file.
fn is_plain(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFREG
}

/// The mtime `stat` holds, in nanoseconds since 1970, negative before it.
fn mtime_nanos(stat: &libc::stat) -> i128 {
    i128::from(stat.st_mtime) * NANOS + i128::from(stat.st_mtime_nsec)
}

/// The mtime `stat` holds, as a time.
fn modified_of(stat: &libc::stat) -> SystemTime {
    let nanos = mtime_nanos(stat);
    let since = Duration::from_nanos(u64::try_from(nanos.unsigned_abs()).unwrap_or(u64::MAX));
    if nanos >= 0 { UNIX_EPOCH + since } else { UNIX_EPOCH - since }
}

/// The size `stat` holds; a size is never negative.
fn size_of(stat: &libc::stat) -> u64 {
    u64::try_from(stat.st_size).unwrap_or(0)
}

/// The stamp `stat` holds: the file's size and its mtime in nanoseconds since 1970. A file last written before 1970,
/// which the pack refuses whatever its stamp, stamps 0.
fn stamp_of(stat: &libc::stat) -> Stamp {
    Stamp {
        size: size_of(stat),
        modified: u64::try_from(mtime_nanos(stat)).unwrap_or(0),
    }
}

/// An outputs archive in the output folder: its name, how big it is, its last write in nanoseconds, and whether it is
/// the run's own.
struct Archive {
    name: String,
    size: u64,
    modified: i128,
    own: bool,
}

/// What a trim of the output folder found.
pub struct Trimmed {
    /// The lines saying each archive that could not be removed, in the order they were tried.
    pub lines: Vec<String>,
    /// Why the run's own archive is not brought back, `None` when it is: [`over_budget_reason`]'s line for one alone
    /// over the budget, which has been removed, or [`GONE_BEFORE_SENT`] for one gone before the trim.
    pub reason: Option<String>,
}

/// The output folder's size budget: the `--test-outputs-budget-bytes` seam's bytes when given, else [`OUTPUTS_BUDGET`].
#[must_use]
pub fn budget(seams: &BoxSeams) -> u64 {
    seams.outputs_budget_bytes.unwrap_or(OUTPUTS_BUDGET)
}

/// The output folder, held, and its path, as the trim names it.
struct Outputs<'a> {
    folder: Folder,
    path: &'a Path,
}

/// Brings `asked`'s output folder back within `budget` bytes after a pack wrote the run's own archive. An own archive
/// gone before the trim, the folder with it, is not brought back ([`GONE_BEFORE_SENT`]); one alone larger than the
/// budget is removed and no other archive is touched, since none of them could bring the folder within it and they
/// belong to runs whose PCs may still fetch them; else the archives last written strictly before the run's own are
/// removed, oldest first, until the folder fits. An archive last written with the run's own or after it is never
/// removed, so two runs packing at once never remove each other's fresh archives: the newer run's own trim covers the
/// older ones. The lines saying each archive that could not be removed are returned beside why the run's own archive is
/// not brought back, `None` when it is. A folder that could not be listed whole is left as it is, its line returned; one
/// that could not be opened at all, no longer the box user's own or its root no longer trusted, is not brought back,
/// for that reason. An archive another run still writes, which holds a shared `flock` on it ([`bring_back`]), is never
/// removed: the trim passes over it as it does one already gone.
#[must_use]
pub fn trim(asked: &Asked<'_>, budget: u64) -> Trimmed {
    let path = Path::new(asked.folder);
    let mut lines = Vec::new();
    let folder = match output_folder(asked.folder, false) {
        Ok(folder) => folder,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Trimmed {
                lines,
                reason: Some(GONE_BEFORE_SENT.to_owned()),
            };
        }
        Err(error) => {
            return Trimmed {
                lines,
                reason: Some(format!("cannot open the output folder {}: {error}", asked.folder)),
            };
        }
    };
    let outputs = Outputs { folder, path };
    let Some(archives) = archives(&outputs, &asked.archive, &mut lines) else {
        return Trimmed { lines, reason: None };
    };
    let Some(mine) = archives.iter().find(|archive| archive.own) else {
        return Trimmed {
            lines,
            reason: Some(GONE_BEFORE_SENT.to_owned()),
        };
    };
    if mine.size > budget {
        if let Err(error) = remove_archive(&outputs.folder, OsStr::new(&mine.name)) {
            lines.push(could_not_remove(&path.join(&mine.name), &error));
        }
        return Trimmed {
            lines,
            reason: Some(over_budget_reason(mine.size)),
        };
    }
    trim_to_fit(&outputs, &archives, mine, budget, &mut lines);
    Trimmed { lines, reason: None }
}

/// The reason a passing run's outputs are not brought back when the run's own archive was gone before the trim, as
/// another run's trim or the age prune may take it.
pub const GONE_BEFORE_SENT: &str = "the archive was removed before it could be sent";

/// The reason a passing run's outputs are not brought back when the run's own archive alone is over the budget: its
/// size in GiB to one decimal, and the budget the box keeps.
fn over_budget_reason(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10) / GIB;
    format!(
        "the archive is {}.{} GiB, more than the box keeps ({} GiB)",
        tenths / 10,
        tenths % 10,
        OUTPUTS_BUDGET / GIB
    )
}

/// The outputs archives in `outputs`, the run's own named `own` marked, with their sizes and last writes; `None` when
/// the folder could not be listed, its line pushed to `lines`, so a trim is never taken from a listing that is not
/// whole. A name that is not an outputs archive, or is not a plain file looked at as itself, is left out, and one whose
/// look fails is said and left out too.
fn archives(outputs: &Outputs<'_>, own: &str, lines: &mut Vec<String>) -> Option<Vec<Archive>> {
    let names = match outputs.folder.entries() {
        Ok(names) => names,
        Err(error) => {
            lines.push(not_listed(outputs.path, &error));
            return None;
        }
    };
    let mut archives = Vec::new();
    for name in names {
        match archive_of(&outputs.folder, &name, own) {
            Ok(Some(archive)) => archives.push(archive),
            Ok(None) => {}
            Err(error) => lines.push(could_not_read(&outputs.path.join(&name), &error)),
        }
    }
    Some(archives)
}

/// The outputs archive `name` in `folder` is, the run's own when its name is `own`; `None` when it is not an outputs
/// archive, not a plain file, or gone; else why it could not be looked at.
fn archive_of(folder: &Folder, name: &OsStr, own: &str) -> io::Result<Option<Archive>> {
    let Some(name) = name.to_str() else {
        return Ok(None);
    };
    if archive_key(name).is_none() {
        return Ok(None);
    }
    let stat = match folder.stat_child(OsStr::new(name)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        looked => looked?,
    };
    if !is_plain(&stat) {
        return Ok(None);
    }
    Ok(Some(Archive {
        name: name.to_owned(),
        size: size_of(&stat),
        modified: mtime_nanos(&stat),
        own: name == own,
    }))
}

/// Removes the archives of `archives` last written strictly before the run's own `mine`, oldest first and by name among
/// ones last written together, until their total size is within `budget`, saying each that cannot be removed and going
/// on to the next. An archive written with the run's own or after it is left alone, and so is one still being written
/// ([`remove_unless_written`]); the folder stays over the budget when no older one is left.
fn trim_to_fit(outputs: &Outputs<'_>, archives: &[Archive], mine: &Archive, budget: u64, lines: &mut Vec<String>) {
    let mut total = archives.iter().fold(0_u64, |total, archive| total.saturating_add(archive.size));
    let mut older: Vec<&Archive> = archives.iter().filter(|archive| archive.modified < mine.modified).collect();
    older.sort_by(|left, right| left.modified.cmp(&right.modified).then_with(|| left.name.cmp(&right.name)));
    for archive in older {
        if total <= budget {
            return;
        }
        match remove_unless_written(&outputs.folder, OsStr::new(&archive.name)) {
            Ok(true) => total = total.saturating_sub(archive.size),
            Ok(false) => {}
            Err(error) => lines.push(could_not_remove(&outputs.path.join(&archive.name), &error)),
        }
    }
}

/// Removes the outputs archive `name` in `folder` unless a pack still writes it: the archive is opened beneath the
/// folder, a link never followed, and removed only under an exclusive `flock` taken without waiting, which the shared
/// one its pack holds refuses; and only while the name still names the file locked, its device and inode looked at
/// again, so a file another run's pack made at the name since the open is never removed ([`remove_held`]). Whether the
/// archive opened is gone, one already gone or no longer at its name counting as removed; `false` when a pack holds it.
fn remove_unless_written(folder: &Folder, name: &OsStr) -> io::Result<bool> {
    match folder.open_file(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        opened => remove_held(folder, name, &opened?),
    }
}

/// [`remove_unless_written`] once the archive `name` in `folder` is open as `archive`: removed under an exclusive
/// `flock` taken without waiting, `false` when a pack holds it. The archive opened counts as gone, the folder's total
/// no longer holding it, when the name no longer names it after the lock, gone or another file in its place, which is
/// then left alone.
fn remove_held(folder: &Folder, name: &OsStr, archive: &File) -> io::Result<bool> {
    match flock(archive, libc::LOCK_EX | libc::LOCK_NB) {
        Err(error) if error.raw_os_error() == Some(libc::EWOULDBLOCK) => return Ok(false),
        locked => locked?,
    }
    let locked = beneath::file_stat(archive)?;
    let named = match folder.stat_child(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        looked => looked?,
    };
    if (named.st_dev, named.st_ino) != (locked.st_dev, locked.st_ino) {
        return Ok(true);
    }
    remove_archive(folder, name).map(|()| true)
}

/// Applies the `flock` `operation` to the open file `file`, a wait a signal broke tried again.
fn flock(file: &File, operation: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: the descriptor is `file`'s, open for the whole call; `flock` touches no memory.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The stat listing of the files under `asked`'s globs in the folder `work`, taken before the command, and the line
/// saying it could not be taken, when it could not: then it is empty, so every file counts as changed. `None` under
/// `OutputsFull`.
#[must_use]
pub fn listing(asked: &Asked<'_>, work: &Path) -> (Option<Listing>, Option<String>) {
    if asked.full {
        return (None, None);
    }
    if asked.globs.is_empty() {
        return (Some(Listing::default()), None);
    }
    let walked = tree_folder(asked, work)
        .map_err(|error| in_context(work, &error))
        .and_then(|top| walk(&top, work, asked, false));
    match walked {
        Ok(walked) => (Some(Listing::of(&asked.deduped(walked.found))), None),
        Err(error) => (
            Some(Listing::default()),
            Some(format!(
                "gate: remote could not list the outputs before the command: {}",
                escaped(&error.to_string())
            )),
        ),
    }
}

/// A folder a walk reads: the names leading to it from the walk's top, its path relative to the top, and whether the
/// walk reads it only to count the files below it.
struct Queued {
    names: Vec<OsString>,
    relative: String,
    counting: bool,
}

/// One walk's state: the top it walks below and its path, whether it notes the refused files, what it found and the
/// folders left to read.
struct Walker<'w> {
    top: &'w Folder,
    top_path: &'w Path,
    globs: &'w Globs,
    /// The runtime identifier of the run's cross build, whose files are listed and matched at their PC paths.
    rid: Option<&'w str>,
    noting: bool,
    walked: Walked,
    folders: Vec<Queued>,
}

/// Every plain file below the folder `top`, at `top_path`, that `asked`'s globs match, and, when `noting`, beside them the
/// files the globs refuse for their names ([`Globs::refuses_name`]), a name that is not Unicode included, named lossily;
/// each at its PC path, a cross build's file mapped ([`cross::mapped_path`]) before the globs judge it, and ordered by
/// that path. Each folder is opened beneath the top one name at a time, a link never followed; a link,
/// a FIFO, a socket or a device is neither followed nor listed. A folder whose name is not plain or not Unicode is
/// read, when `noting`, only to count the files below it ([`Globs::counts_below`]), which are only ever refused. Else
/// the first folder that cannot be read, or entry that cannot be looked at, its path in the error; a name gone before
/// its look is passed over.
fn walk(top: &Folder, top_path: &Path, asked: &Asked<'_>, noting: bool) -> io::Result<Walked> {
    let mut walker = Walker {
        top,
        top_path,
        globs: &asked.globs,
        rid: asked.rid.as_deref(),
        noting,
        walked: Walked::default(),
        folders: vec![Queued {
            names: Vec::new(),
            relative: String::new(),
            counting: false,
        }],
    };
    while let Some(queued) = walker.folders.pop() {
        if let Err(error) = walker.read(&queued)
            && !queued.counting
        {
            return Err(error);
        }
    }
    let mut walked = walker.walked;
    walked.found.sort_by(|first, second| first.path.cmp(&second.path));
    walked.refused.sort_by(|first, second| first.path.cmp(&second.path));
    Ok(walked)
}

impl Walker<'_> {
    /// Visits each entry of the folder `queued`; else the error that stopped the read. A folder read only to count goes
    /// on past an entry that cannot be looked at, which is then not counted: the count only names files the run leaves
    /// out.
    fn read(&mut self, queued: &Queued) -> io::Result<()> {
        let full = queued.names.iter().fold(self.top_path.to_path_buf(), |full, name| full.join(name));
        let folder = folders_beneath(self.top, queued.names.iter().map(OsString::as_os_str)).map_err(|error| in_context(&full, &error))?;
        for name in folder.entries().map_err(|error| in_context(&full, &error))? {
            let visited = self.visit(&folder, &full, queued, name);
            if !queued.counting {
                visited?;
            }
        }
        Ok(())
    }

    /// Lists the entry `name` of the folder `queued`, held as `folder` at `full`, when it is a plain file the globs
    /// match or refuse, or queues it when it is a folder to enter or to count below.
    fn visit(&mut self, folder: &Folder, full: &Path, queued: &Queued, name: OsString) -> io::Result<()> {
        let file = full.join(&name);
        let stat = match folder.stat_child(&name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            looked => looked.map_err(|error| in_context(&file, &error))?,
        };
        let path = relative_path(&queued.relative, &name);
        let unfit = queued.counting || name.to_str().is_none();
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            self.queue_folder(queued, name, path, unfit);
        } else if is_plain(&stat) {
            let path = self.rid.and_then(|rid| cross::mapped_path(&path, rid)).unwrap_or(path);
            if let Some(list) = self.walked.list_for(self.globs, &path, unfit) {
                list.push(Found {
                    path,
                    full: file,
                    stamp: stamp_of(&stat),
                });
            }
        }
        Ok(())
    }

    /// Queues the folder `name` of `parent`, at the relative `path`, to enter when the globs enter it, judged at its PC
    /// path ([`cross::mapped_folder`]), and it is not `unfit`, else to count below when the walk notes refused files
    /// and the globs count below it.
    fn queue_folder(&mut self, parent: &Queued, name: OsString, path: String, unfit: bool) {
        let pc = self.rid.and_then(|rid| cross::mapped_folder(&path, rid));
        let judged = pc.as_deref().unwrap_or(&path);
        let counting = if !unfit && self.globs.enters(judged) {
            false
        } else if self.noting && self.globs.counts_below(judged) {
            true
        } else {
            return;
        };
        let mut names = parent.names.clone();
        names.push(name);
        self.folders.push(Queued {
            names,
            relative: path,
            counting,
        });
    }
}

/// The folder `names` lead to below `top`, each opened beneath the one before, a link never followed.
fn folders_beneath<'n>(top: &Folder, names: impl IntoIterator<Item = &'n OsStr>) -> io::Result<Folder> {
    let mut folder = top.duplicate()?;
    for name in names {
        folder = folder.open_child(name)?;
    }
    Ok(folder)
}

/// A walked folder held by its descriptor, its path, and the folder of the file last opened below it, held with its
/// path relative to the top: files packed one after another from the same folder reach it by one chain of opens, and a
/// file of another folder between them, as an order by path may put one, only costs the chain opened again. No more
/// than that one folder is held between opens.
struct Tree<'t> {
    top: &'t Folder,
    top_path: &'t Path,
    last: RefCell<Option<(PathBuf, Folder)>>,
}

impl<'t> Tree<'t> {
    /// The folder `top`, at `top_path`, holding no folder below it yet.
    fn new(top: &'t Folder, top_path: &'t Path) -> Tree<'t> {
        Tree {
            top,
            top_path,
            last: RefCell::new(None),
        }
    }

    /// The file a walk below the top found as `file`, opened to read beneath its own folder, reached from the top one
    /// name at a time unless it is the folder of the file opened before, a link never followed and a FIFO never waited
    /// on: a plain file only.
    fn open(&self, file: &Found) -> io::Result<File> {
        let below = file
            .full
            .strip_prefix(self.top_path)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the file is not below the work folder"))?;
        let (Some(parent), Some(leaf)) = (below.parent(), below.file_name()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the file is the work folder"));
        };
        let mut last = self.last.borrow_mut();
        // The folder held before is let go before another is opened, so one chain at most is held.
        let reused = last.take().filter(|(path, _)| path == parent);
        let held = match reused {
            Some(held) => held,
            None => (parent.to_path_buf(), folders_beneath(self.top, parent)?),
        };
        let opened = held.1.open_file(leaf);
        *last = Some(held);
        opened
    }

    /// The SHA-256 of the file a walk found as `file`, read as [`Tree::open`] opens it; else why not, its path named.
    fn sha256(&self, file: &Found) -> io::Result<String> {
        self.open(file)
            .and_then(sha256_of)
            .map(|(sha256, _)| sha256)
            .map_err(|error| in_context(&file.full, &error))
    }
}

/// What [`bring_back`] brought: the line saying the files skipped for their names, when any were, and the line saying
/// what was packed. A pack that wrote an archive holds it open under a shared `flock` until this is dropped, so no
/// other run's trim removes it ([`remove_unless_written`]): the run keeps it until its own [`trim`] is done.
#[derive(Debug)]
pub struct Brought {
    /// The line saying the files left out for names this PC cannot take, `None` when there were none.
    pub skipped: Option<String>,
    /// The line saying each failure output left on the builder for a cap, in walk order, said after `skipped`.
    pub left: Vec<String>,
    /// `gate-remote-outputs-size: <bytes>`, the archive's size, said just before `line`; `None` when nothing was packed.
    pub size: Option<String>,
    /// `gate-remote-outputs: <n> <archive>`, or [`NOTHING_PACKED`].
    pub line: String,
    /// The line refusing a cross build's outputs for a program the PC cannot run among them ([`cross::refused_line`]),
    /// `None` when they were not refused. Refused, nothing was packed, and neither `skipped` nor `line` is said.
    pub refused: Option<String>,
    /// The archive written, held under its shared `flock`; `None` when nothing was packed.
    archive: Option<File>,
}

impl Brought {
    /// Whether the pack wrote an archive, which the run's [`trim`] then keeps within the budget.
    #[must_use]
    pub fn wrote_archive(&self) -> bool {
        self.archive.is_some()
    }
}

/// Packs the files under `asked`'s globs in the folder `work` to bring back after a passing command ([`select`]), or a
/// failed one's test reports and failure outputs ([`Asked::failed_run`]), the failure outputs held to their caps,
/// leaving out `skip`, the paths of the run's back commit, which
/// the sync-back replaces on the PC, so no copy is made from the PC's file at one of them either: the line saying how
/// many were packed and the archive's name, with the line saying the archive's size in bytes to say before it, or
/// `gate-remote-outputs: 0` with no archive written when none were, and before it the line saying the files [`select`]
/// would have packed but for a name this PC cannot take ([`skipped_line`]), when there were any. The output folder is
/// made when missing ([`output_folder`]), and the archive is made anew beneath it, 0600: a file or a link already at
/// its name fails the pack and is left as it is; an archive left half written is removed. The archive is held under a
/// shared `flock` from its making until the [`Brought`] returned is dropped.
///
/// # Errors
///
/// Why the outputs are not brought back: the manifest's own reason, a walk, hash or pack that failed, or an output
/// folder or archive that could not be made or locked.
pub fn bring_back(
    asked: &Asked<'_>,
    manifest: &Result<Manifest, String>,
    listing: Option<&Listing>,
    work: &Path,
    skip: &[String],
) -> Result<Brought, String> {
    let manifest = manifest.as_ref().map_err(Clone::clone)?;
    if asked.globs.is_empty() {
        return Ok(Brought {
            skipped: None,
            left: Vec::new(),
            size: None,
            line: NOTHING_PACKED.to_owned(),
            refused: None,
            archive: None,
        });
    }
    let walked = tree_folder(asked, work)
        .map_err(|error| in_context(work, &error))
        .and_then(|top| walk(&top, work, asked, true).map(|walked| (walked, top)));
    let (walked, top) = walked.map_err(|error| format!("the outputs could not be listed: {error}"))?;
    let tree = Tree::new(&top, work);
    let found = asked.deduped(walked.found);
    let hash = |file: &Found| tree.sha256(file);
    let refused = select(&walked.refused, manifest, listing, skip, hash).map_err(|error| error.to_string())?;
    let skipped = skipped_line(&refused.iter().map(|chosen| chosen.file).collect::<Vec<_>>());
    let chosen = select(&found, manifest, listing, skip, hash).map_err(|error| error.to_string())?;
    let chosen = match &asked.rid {
        None => chosen,
        Some(rid) => match portable_chosen(&tree, chosen)? {
            Portable::Kept(kept) => kept,
            Portable::Refused(path) => {
                return Ok(Brought {
                    skipped: None,
                    left: Vec::new(),
                    size: None,
                    line: NOTHING_PACKED.to_owned(),
                    refused: Some(cross::refused_line(rid, &path)),
                    archive: None,
                });
            }
        },
    };
    let (mut chosen, left) = asked.capping.capped(chosen);
    if chosen.is_empty() {
        return Ok(Brought {
            skipped,
            left,
            size: None,
            line: NOTHING_PACKED.to_owned(),
            refused: None,
            archive: None,
        });
    }
    hash_possible_copies(&mut chosen, manifest, hash).map_err(|error| error.to_string())?;
    let folder = output_folder(asked.folder, true).map_err(|error| format!("cannot make the output folder {}: {error}", asked.folder))?;
    let path = Path::new(asked.folder).join(&asked.archive);
    let name = OsStr::new(&asked.archive);
    let file = folder
        .create_file(name)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    let packed = flock(&file, libc::LOCK_SH)
        .map_err(|error| in_context(&path, &error))
        .and_then(|()| pack(BufWriter::new(&file), &tree, &chosen, &manifest.by_hash(skip)));
    match packed {
        Ok(packed) => Ok(Brought {
            skipped,
            left,
            size: Some(format!("gate-remote-outputs-size: {}", packed.compressed)),
            line: format!("gate-remote-outputs: {} {}", packed.entries.len(), asked.archive),
            refused: None,
            archive: Some(file),
        }),
        Err(error) => match folder.remove_child(name, false) {
            Ok(()) => Err(error.to_string()),
            Err(removal) => Err(format!("{error}; and {} could not be removed: {removal}", path.display())),
        },
    }
}

/// The files of `chosen`, found below `tree` and picked to send, that a run with a cross build sends
/// ([`cross::portable`]), each's first [`cross::HEAD_BYTES`] bytes read as [`Tree::open`] opens it; else why not.
fn portable_chosen<'f>(tree: &Tree<'_>, chosen: Vec<Chosen<'f>>) -> Result<Portable<'f>, String> {
    let head = |file: &Found| {
        let mut head = Vec::new();
        tree.open(file)
            .and_then(|opened| opened.take(cross::HEAD_BYTES).read_to_end(&mut head))
            .map(|_| head)
            .map_err(|error| in_context(&file.full, &error))
    };
    cross::portable(chosen, head).map_err(|error| format!("the outputs could not be read: {error}"))
}

/// Packs `files`, found below `tree`, into `sink` as a `.tar.zst` stream, in their order, over a zstd window sized to
/// their bytes ([`window_log`]). Returns the entries of every path listed and the compressed size; else why the pack
/// failed, the file's path named.
///
/// Each file is opened at its turn beneath its own folder ([`Tree::open`]): a link, anything but a plain file, and a
/// file last written before 1970 fail the pack. A file hashed before the pack is then listed as a copy, none of its
/// bytes packed and its own last write listed, when its stamp is still the one the walk found before it was hashed, and
/// that hash is the SHA-256 a member packed before it was read to or one `held` holds. Every other file is packed as a
/// member: read once through its descriptor, hashed as it is read, its header holding the size and last write it had
/// when it was opened; one whose size or last write changes while it is read fails the pack.
fn pack(sink: impl Write, tree: &Tree<'_>, files: &[Chosen<'_>], held: &HeldBy<'_>) -> io::Result<Packed> {
    let total = files.iter().fold(0_u64, |total, chosen| total.saturating_add(chosen.file.stamp.size));
    let mut sent: HashMap<String, u64> = HashMap::new();
    let mut packer = Packer::new(sink, window_log(total))?;
    for chosen in files {
        let known = |sha256: &str| sent.get(sha256).copied().or_else(|| held_size(held, sha256));
        pack_one(&mut packer, tree, chosen, known).map_err(|error| in_context(&chosen.file.full, &error))?;
        if let Some(Item { entry, copy: None }) = packer.items().last() {
            sent.entry(entry.sha256.clone()).or_insert(entry.size);
        }
    }
    packer.finish()
}

/// A file the pack opened, and what it was when it was opened: its look on its descriptor, and its last write in
/// seconds since 1970.
struct Opened {
    file: File,
    before: libc::stat,
    modified: u64,
}

impl Opened {
    /// The open file `file`, found as `found`, looked at now; else the look, or the refusal of a file last written
    /// before 1970.
    fn of(file: File, found: &Found) -> io::Result<Opened> {
        let before = beneath::file_stat(&file)?;
        let modified = u64::try_from(before.st_mtime).map_err(|_| refused(format!("{} was last written before 1970", found.path)))?;
        Ok(Opened { file, before, modified })
    }
}

/// Packs `chosen`, found below `tree`, as [`pack`] does: a copy when it was hashed before the pack, it still has the
/// stamp the walk found, and `known` gives the size of a content with that SHA-256; else a member, read now. Else why
/// it failed.
fn pack_one<W: Write>(packer: &mut Packer<W>, tree: &Tree<'_>, chosen: &Chosen<'_>, known: impl Fn(&str) -> Option<u64>) -> io::Result<()> {
    let file = chosen.file;
    let opened = Opened::of(tree.open(file)?, file)?;
    let copied = chosen
        .sha256
        .as_deref()
        .filter(|_| stamp_of(&opened.before) == file.stamp)
        .and_then(|sha256| Some((sha256, known(sha256)?)));
    match copied {
        Some((sha256, size)) => packer.copy(&file.path, size, opened.modified, sha256),
        None => pack_file(packer, file, &opened, &opened.file),
    }
}

/// Packs the open file `opened`, found as `file`, its bytes read from `data`: its header holds the size and last write
/// `opened` had when it was opened, and a file whose size or last write differs after the read, looked at on the same
/// descriptor, fails the pack.
fn pack_file<W: Write>(packer: &mut Packer<W>, file: &Found, opened: &Opened, data: impl Read) -> io::Result<()> {
    let before = &opened.before;
    packer.add(&file.path, size_of(before), opened.modified, data)?;
    let after = beneath::file_stat(&opened.file)?;
    if after.st_size == before.st_size && mtime_nanos(&after) == mtime_nanos(before) {
        Ok(())
    } else {
        Err(refused(format!(
            "{} changed while it was packed: its size or last write differs from when it was opened",
            file.path
        )))
    }
}

/// The start of the line saying the run's outputs leave out files for names this PC cannot take ([`skipped_line`]),
/// which the PC's client reads on standard error. A command can print it too, which only makes the PC count the run's
/// outputs as not brought back.
pub const NAMES_SKIPPED: &str = "gate: remote outputs: skipped ";

/// The line saying the run's outputs leave out the files `skipped` for names this PC cannot take, naming the first, cut
/// to [`NAMED_PATH_LIMIT`] bytes, and marking that more follow; `None` when there are none.
fn skipped_line(skipped: &[&Found]) -> Option<String> {
    let first = skipped.first()?;
    let named = &first.path[..first.path.floor_char_boundary(NAMED_PATH_LIMIT)];
    let cut = if named.len() < first.path.len() { "\u{2026}" } else { "" };
    let more = if skipped.len() > 1 { ", \u{2026}" } else { "" };
    Some(format!(
        "{NAMES_SKIPPED}{} file(s) with names this PC cannot take: {}{cut}{more}",
        skipped.len(),
        escaped(named)
    ))
}

/// The most bytes of a path [`skipped_line`] names, which keeps the line well under the PC's line cap.
const NAMED_PATH_LIMIT: usize = 1024;

/// The line saying a passing run's outputs are not brought back, for `reason`.
#[must_use]
pub fn not_brought_back(reason: &str) -> String {
    format!("gate: remote outputs not brought back: {}", escaped(reason))
}

/// Answers `gate-box box outputs`: the archive the payload's `Name` names in its `OutputFolder`, sent raw on stdout,
/// then removed, as the Windows box answers it. A work folder or output folder the payload gets wrong, or a payload
/// without a `Name`, is a usage error (2); a `Name` given that is not an outputs archive of the work folder, an archive
/// that does not exist or one that cannot be read or sent ends with status 1, and a sent archive that cannot be removed
/// is said and left for a later run's [`prune`], status 0; one already gone says nothing. The archive is opened beneath
/// the output folder, itself reached beneath the judged root: a link at its name or anything but a plain file is not
/// sent.
///
/// Before it sends, the call renews the payload's drain session ([`drain::renewed_at`]), its clock as `seams` set it,
/// and holds it until it returns. It is never refused: an archive of a session the drain found dead while a barrier
/// stands is sent and kept, said with [`KEPT_DRAINED`], as is one sent when the drain could not be read, said
/// `gate: drain failed: {why}`.
#[must_use]
pub fn outputs(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u8 {
    answered(payload, seams, &mut io::stdout().lock(), console)
}

/// The line an `outputs` call says when it keeps the archive it sent, its session dead under a standing barrier.
const KEPT_DRAINED: &str = "gate: remote outputs kept: the builder is drained";

/// What an `outputs` call does with the archive it sent.
enum Removal {
    /// Removes it.
    Remove,
    /// Keeps it, its session dead under a standing barrier, and says [`KEPT_DRAINED`].
    KeepDrained,
    /// Keeps it, the drain unread, its failure said.
    Keep,
}

/// [`outputs`], the archive sent to `sink`.
fn answered(payload: &Payload, seams: &BoxSeams, sink: &mut impl Write, console: &mut Console) -> u8 {
    let read = payload
        .work_folder()
        .and_then(|(_, work)| Ok((work, payload.output_folder(work)?, payload.session()?)));
    let (work, folder, session) = match read {
        Ok(read) => read,
        Err(line) => {
            console.err(&line);
            return USAGE_STATUS;
        }
    };
    let name = match payload.archive_name() {
        Ok(name) => name,
        Err(line) => {
            console.err(&line);
            let lacking = payload.text("Name").is_err();
            return if lacking { USAGE_STATUS } else { FAILED_STATUS };
        }
    };
    let (_held, removal) = match drain::renewed_at(work, session, seams, console) {
        Ok(renewal) if renewal.may_delete => (renewal.held, Removal::Remove),
        Ok(renewal) => (renewal.held, Removal::KeepDrained),
        Err(why) => {
            console.err(&drain::failed_line(&why));
            (None, Removal::Keep)
        }
    };
    let sent = output_folder(folder, false)
        .map_err(Unsent::of_open)
        .and_then(|outputs| send(&outputs, name, sink).map(|()| outputs));
    let outputs = match sent {
        Ok(outputs) => outputs,
        Err(unsent) => {
            console.err(&unsent.line(name));
            return FAILED_STATUS;
        }
    };
    match removal {
        Removal::Remove => {
            if let Err(error) = remove_archive(&outputs, OsStr::new(name)) {
                console.err(&format!(
                    "gate: remote could not remove the outputs archive {name}: {}",
                    escaped(&error.to_string())
                ));
            }
        }
        Removal::KeepDrained => console.err(KEPT_DRAINED),
        Removal::Keep => {}
    }
    0
}

/// Why an archive was not sent.
enum Unsent {
    Missing,
    Unread(io::Error),
    Unwritten(io::Error),
}

impl Unsent {
    /// Why an archive whose open, or its folder's, failed with `error` was not sent: missing when nothing is there.
    fn of_open(error: io::Error) -> Unsent {
        if error.kind() == io::ErrorKind::NotFound {
            Unsent::Missing
        } else {
            Unsent::Unread(error)
        }
    }

    /// The line saying so of the archive `name`, an archive name the payload's check let through.
    fn line(&self, name: &str) -> String {
        match self {
            Unsent::Missing => format!("gate: remote outputs archive {name} does not exist"),
            Unsent::Unread(error) => format!("gate: remote could not read the outputs archive {name}: {}", escaped(&error.to_string())),
            Unsent::Unwritten(error) => format!("gate: remote could not send the outputs archive {name}: {}", escaped(&error.to_string())),
        }
    }
}

/// Copies the plain file `name` in `folder`, never a link's target, to `sink` through a fixed buffer, then flushes it.
fn send(folder: &Folder, name: &str, sink: &mut impl Write) -> Result<(), Unsent> {
    let mut archive = folder.open_file(OsStr::new(name)).map_err(Unsent::of_open)?;
    let mut buffer = vec![0_u8; SEND_BUFFER];
    loop {
        let read = match archive.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Unsent::Unread(error)),
        };
        sink.write_all(&buffer[..read]).map_err(Unsent::Unwritten)?;
    }
    sink.flush().map_err(Unsent::Unwritten)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test whose folders, files, payloads or archives cannot be made must fail loudly"
)]
pub(crate) mod tests {
    use std::ffi::{CString, OsStr};
    use std::fs::{self, File};
    use std::io::{self, Cursor, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_args::BoxSeams;
    use gate_inputs::archive::{Item, UnpackError, read_members};
    use gate_inputs::hash::sha256_of;
    use gate_inputs::manifest::{Held, Listing, MANIFEST_LIMIT, Manifest, Stamp};
    use gate_inputs::pack::Packer;
    use gate_inputs::walk::Found;
    use gate_wire::{PathForm, Payload};
    use serde_json::{Value, json};

    use super::{
        ARCHIVE_AGE, Asked, Brought, GONE_BEFORE_SENT, NOTHING_PACKED, OUTPUTS_BUDGET, Opened, answered, bring_back, budget, listing,
        not_brought_back, output_folder, pack_file, prune, read_manifest, remove_held, trim,
    };
    use crate::beneath::Folder;
    use crate::console::Console;
    use crate::lines::{FAILED_STATUS, USAGE_STATUS};

    const KEY: &str = "repo-0123abcd";
    /// How long a pack may take before the case calls it hung: a hang guard, never reached by a pack that works.
    const HANG_GUARD: Duration = Duration::from_secs(300);

    /// Files of a manifest or a work folder: each path and its bytes.
    type Files<'a> = [(&'a str, &'a [u8])];

    /// A work root `<base>/gate-remote`, private to the box user, with its work folder `<root>/work/<KEY>`, in a temp
    /// folder of the case `name`, private to this process and removed when the case ends.
    struct Case {
        base: PathBuf,
        /// The output folder, `<root>/outputs`.
        folder: String,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-outputs-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            let root = base.join("gate-remote");
            // Mode 0700 as the box demands of a root and its `<root>/work`, whatever the umask.
            std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o700)
                .create(root.join("work").join(KEY))
                .expect("the work folder is made");
            let folder = format!("{}/outputs", root.to_str().expect("the temp folder is UTF-8"));
            Case { base, folder }
        }

        fn work(&self) -> PathBuf {
            self.base.join("gate-remote").join("work").join(KEY)
        }

        fn outputs(&self) -> PathBuf {
            PathBuf::from(&self.folder)
        }

        /// Writes `bytes` at the `/`-separated `path` below the work folder, its folders made.
        fn write(&self, path: &str, bytes: &[u8]) {
            let full = self.work().join(path);
            fs::create_dir_all(full.parent().expect("a file has a folder")).expect("the folder is made");
            fs::write(full, bytes).expect("the file is written");
        }

        /// The outputs of a run asking for `globs`, every file counted as changed when `full`, its archive's nonce 32
        /// copies of `digit`.
        fn asked(&self, globs: &[&str], full: bool, digit: char) -> Asked<'_> {
            let globs: Vec<String> = globs.iter().map(|glob| (*glob).to_owned()).collect();
            Asked::new((&globs, &[]), full, KEY, &digit.to_string().repeat(32), &self.folder)
                .expect("the globs are fine")
                .expect("globs ask")
        }

        /// The output folder, made as a pack makes it.
        fn made_outputs(&self) {
            output_folder(&self.folder, true).expect("the output folder is made");
        }

        /// The verb's payload for this case's work folder, naming `name` when given.
        fn payload(&self, name: Option<&str>) -> Payload {
            let mut value = json!({
                "Key": KEY,
                "Work": self.work().to_str().expect("the temp folder is UTF-8"),
                "OutputFolder": self.folder,
            });
            if let Some(name) = name {
                value["Name"] = Value::from(name);
            }
            Payload::decode(&STANDARD.encode(value.to_string()), PathForm::Unix).expect("the payload decodes")
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::set_permissions(self.outputs(), fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// The name of an outputs archive of the work folder, its nonce 32 copies of `digit`.
    fn archive(digit: char) -> String {
        format!("{KEY}-{}.tar.zst", digit.to_string().repeat(32))
    }

    /// The permission bits of `path`, looked at as itself.
    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).expect("the path is looked at").permissions().mode() & 0o7777
    }

    /// The names in the folder `path`, sorted.
    fn names_in(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(path)
            .expect("the folder is listed")
            .map(|entry| entry.expect("an entry is read").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Makes a file `name` in `folder` holding `bytes` zero bytes, last written `age` before `now`.
    fn sized(folder: &Path, name: &str, bytes: usize, now: SystemTime, age: Duration) {
        let path = folder.join(name);
        fs::write(&path, vec![0_u8; bytes]).expect("the file is written");
        let file = fs::OpenOptions::new().write(true).open(&path).expect("the file opens");
        file.set_modified(now - age).expect("its last write is set");
    }

    /// The umask a case set, set back when this is dropped, a case that panics included.
    struct Umask(libc::mode_t);

    impl Drop for Umask {
        fn drop(&mut self) {
            // SAFETY: `umask` only sets the process's mask and returns the old one.
            unsafe { libc::umask(self.0) };
        }
    }

    #[test]
    fn the_output_folder_is_made_0700_under_a_022_umask() {
        let case = Case::new("folder-made");
        {
            // The umask is the whole process's: nextest runs each case in a process of its own.
            // SAFETY: `umask` only sets the process's mask and returns the old one.
            let _restored = Umask(unsafe { libc::umask(0o022) });
            case.made_outputs();
        }
        assert_eq!(mode_of(&case.outputs()), 0o700);
        assert!(output_folder(&case.folder, true).is_ok(), "the folder made is adopted");
    }

    #[test]
    fn an_output_folder_others_may_enter_or_a_link_at_its_name_is_refused() {
        let case = Case::new("folder-refused");
        fs::create_dir(case.outputs()).expect("the output folder is made");
        fs::set_permissions(case.outputs(), fs::Permissions::from_mode(0o755)).expect("its mode is set");
        let kind = output_folder(&case.folder, true).map(drop).map_err(|error| error.kind());
        assert_eq!(kind, Err(io::ErrorKind::PermissionDenied), "a folder others may enter");
        let lines = prune(&case.asked(&["**"], false, '9'), SystemTime::now());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with(&format!("gate: remote could not list the old outputs archives in {}: ", case.folder)),
            "{lines:?}"
        );

        fs::remove_dir(case.outputs()).expect("the folder is removed");
        let elsewhere = case.base.join("elsewhere");
        fs::create_dir(&elsewhere).expect("the folder a link leads to is made");
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700)).expect("its mode is set");
        symlink(&elsewhere, case.outputs()).expect("a link at the output folder's name is made");
        let refused = output_folder(&case.folder, true).map(drop).map_err(|error| error.kind());
        assert!(
            matches!(refused, Err(kind) if kind != io::ErrorKind::NotFound),
            "a link is neither followed nor taken for a missing folder: {refused:?}"
        );
        assert!(fs::symlink_metadata(case.outputs()).expect("the link is looked at").is_symlink());
    }

    #[test]
    fn prune_removes_only_plain_outputs_archives_older_than_a_day() {
        let case = Case::new("prune");
        case.made_outputs();
        let folder = case.outputs();
        let now = SystemTime::now();
        let old = ARCHIVE_AGE + Duration::from_secs(60);
        let removed = [archive('0'), format!("other_key-2{}.tar.zst", "0".repeat(31))];
        let mut kept = vec![
            format!("{KEY}-old.tar.zst"),
            "notes.tar.zst".to_owned(),
            ".tar.zst".to_owned(),
            "old.txt".to_owned(),
            format!("{KEY}-{}.tar.zst", "0".repeat(31)),
            format!("{KEY}-{}.tar.zst", "A".repeat(32)),
            format!("{KEY}-{}.TAR.ZST", "f".repeat(32)),
        ];
        for name in removed.iter().chain(&kept) {
            sized(&folder, name, 0, now, old);
        }
        sized(&folder, &archive('1'), 0, now, Duration::from_hours(23));
        kept.push(archive('1'));
        sized(&case.base, "old-target", 0, now, old);
        symlink(case.base.join("old-target"), folder.join(archive('2'))).expect("a link of the archive's form is made");
        kept.push(archive('2'));
        kept.sort();
        let asked = case.asked(&["**"], false, '9');

        assert!(prune(&asked, now).is_empty(), "nothing failed");

        assert_eq!(names_in(&folder), kept, "only old plain files named as outputs archives are removed");
        assert!(case.base.join("old-target").exists(), "a link's target is never removed");
        fs::remove_dir_all(&folder).expect("the output folder is removed");
        assert!(prune(&asked, now).is_empty(), "a missing folder holds nothing to remove");
    }

    #[test]
    fn the_trim_removes_the_oldest_archives_until_the_folder_fits_and_never_a_newer_one() {
        let case = Case::new("trim-fits");
        case.made_outputs();
        let folder = case.outputs();
        let now = SystemTime::now();
        sized(&folder, &archive('9'), 100, now, Duration::from_secs(5));
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(30));
        sized(&folder, &archive('2'), 100, now, Duration::from_secs(20));
        sized(&folder, &archive('3'), 100, now, Duration::from_secs(10));
        sized(&folder, &archive('4'), 100, now, Duration::from_secs(1));

        let trimmed = trim(&case.asked(&["**"], false, '9'), 300);

        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert!(trimmed.reason.is_none(), "the run's own archive fits the budget");
        assert_eq!(
            names_in(&folder),
            [archive('3'), archive('4'), archive('9')],
            "the two oldest go, the folder then fits, and a newer archive than the run's own stays"
        );
    }

    #[test]
    fn an_own_archive_alone_over_the_budget_is_removed_with_its_reason_and_one_gone_is_not_brought_back() {
        let case = Case::new("trim-own");
        case.made_outputs();
        let folder = case.outputs();
        let now = SystemTime::now();
        sized(&folder, &archive('9'), 1_000, now, Duration::from_secs(1));
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(9));

        let trimmed = trim(&case.asked(&["**"], false, '9'), 500);

        assert_eq!(
            trimmed.reason.as_deref(),
            Some("the archive is 0.0 GiB, more than the box keeps (50 GiB)")
        );
        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert_eq!(names_in(&folder), [archive('1')], "the run's own goes and every other archive stays");
        assert_eq!(
            not_brought_back(trimmed.reason.as_deref().expect("a reason")),
            "gate: remote outputs not brought back: the archive is 0.0 GiB, more than the box keeps (50 GiB)"
        );

        let gone = trim(&case.asked(&["**"], false, '9'), 100);
        assert_eq!(gone.reason.as_deref(), Some(GONE_BEFORE_SENT));
        assert_eq!(GONE_BEFORE_SENT, "the archive was removed before it could be sent");
        assert_eq!(
            names_in(&folder),
            [archive('1')],
            "nothing is removed while the own archive cannot be found"
        );
        fs::remove_dir_all(&folder).expect("the output folder is removed");
        let missing = trim(&case.asked(&["**"], false, '9'), 100);
        assert_eq!(missing.reason.as_deref(), Some(GONE_BEFORE_SENT), "a folder gone holds no archive");
    }

    #[test]
    fn the_budget_is_50_gib_unless_the_seam_says_otherwise() {
        assert_eq!(OUTPUTS_BUDGET, 50 * 1024 * 1024 * 1024);
        assert_eq!(budget(&BoxSeams::default()), OUTPUTS_BUDGET);
        let seams = BoxSeams {
            outputs_budget_bytes: Some(5),
            ..BoxSeams::default()
        };
        assert_eq!(budget(&seams), 5);
    }

    /// The verb's answer to `payload`: its status, the bytes it sent and what it said.
    fn verb(payload: &Payload) -> (u8, Vec<u8>, Console) {
        let mut console = Console::captured();
        let mut sent = Vec::new();
        let status = answered(payload, &BoxSeams::default(), &mut sent, &mut console);
        (status, sent, console)
    }

    /// Bytes of more than one send buffer, none of the buffers alike.
    fn many_bytes() -> Vec<u8> {
        (0..200_000_u32).map(|index| u8::try_from(index % 251).expect("below 251")).collect()
    }

    #[test]
    fn the_outputs_verb_sends_an_archive_whole_then_removes_it() {
        let case = Case::new("verb-sent");
        case.made_outputs();
        let bytes = many_bytes();
        fs::write(case.outputs().join(archive('1')), &bytes).expect("the archive is written");

        let (status, sent, console) = verb(&case.payload(Some(&archive('1'))));

        assert_eq!(status, 0, "{:?}", console.err);
        assert!(sent == bytes, "the archive's bytes are sent exactly");
        assert!(console.err.is_empty() && console.out.is_empty(), "{:?} {:?}", console.err, console.out);
        assert!(names_in(&case.outputs()).is_empty(), "the sent archive is removed");
    }

    #[test]
    fn a_missing_archive_or_output_folder_is_status_1() {
        let case = Case::new("verb-missing");
        let missing = format!("gate: remote outputs archive {} does not exist", archive('1'));
        let (status, sent, console) = verb(&case.payload(Some(&archive('1'))));
        assert_eq!((status, console.err), (FAILED_STATUS, vec![missing.clone()]), "no output folder");
        assert_eq!(sent, Vec::<u8>::new(), "nothing is sent");

        case.made_outputs();
        let (status, _, console) = verb(&case.payload(Some(&archive('1'))));
        assert_eq!((status, console.err), (FAILED_STATUS, vec![missing]), "no archive");
    }

    #[test]
    fn a_link_at_the_archive_s_name_is_refused_and_left() {
        let case = Case::new("verb-link");
        case.made_outputs();
        fs::write(case.base.join("secret"), b"secret").expect("the file a link leads to is written");
        let link = case.outputs().join(archive('1'));
        symlink(case.base.join("secret"), &link).expect("a link at the archive's name is made");

        let (status, sent, console) = verb(&case.payload(Some(&archive('1'))));

        assert_eq!(status, FAILED_STATUS);
        assert!(sent.is_empty(), "nothing a link leads to is sent");
        let unread = format!("gate: remote could not read the outputs archive {}: ", archive('1'));
        assert!(console.err.len() == 1 && console.err[0].starts_with(&unread), "{:?}", console.err);
        assert!(
            fs::symlink_metadata(&link).expect("the link is looked at").is_symlink(),
            "the link is left"
        );
        assert!(case.base.join("secret").exists());
    }

    #[test]
    fn a_sent_archive_that_cannot_be_removed_is_said_and_the_verb_passes() {
        let case = Case::new("verb-kept");
        case.made_outputs();
        fs::write(case.outputs().join(archive('1')), b"archive").expect("the archive is written");
        fs::set_permissions(case.outputs(), fs::Permissions::from_mode(0o500)).expect("the folder is made read-only");

        let (status, sent, console) = verb(&case.payload(Some(&archive('1'))));

        fs::set_permissions(case.outputs(), fs::Permissions::from_mode(0o700)).expect("the folder is made writable again");
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(sent, b"archive");
        let kept = format!("gate: remote could not remove the outputs archive {}: ", archive('1'));
        assert!(console.err.len() == 1 && console.err[0].starts_with(&kept), "{:?}", console.err);
        assert_eq!(names_in(&case.outputs()), [archive('1')]);
    }

    #[test]
    fn a_payload_without_a_name_is_usage_and_a_name_of_another_form_is_status_1() {
        let case = Case::new("verb-name");
        let (status, _, console) = verb(&case.payload(None));
        assert_eq!(status, USAGE_STATUS, "{:?}", console.err);

        let (status, sent, console) = verb(&case.payload(Some("x.tar.zst")));
        assert_eq!(status, FAILED_STATUS);
        assert_eq!(sent, Vec::<u8>::new(), "nothing is sent");
        assert_eq!(
            console.err,
            ["gate: remote outputs archive name 'x.tar.zst' is not an outputs archive of this work folder"]
        );
    }

    #[test]
    fn a_manifest_is_read_up_to_its_newline_and_what_follows_is_left_unread() {
        let mut input = Cursor::new(b"{\"files\":{}}\nmore".to_vec());
        assert_eq!(read_manifest(&mut input), Ok(Manifest::default()));
        assert_eq!(input.position(), 13, "nothing past the newline is read");
        assert_eq!(read_manifest(Cursor::new(Vec::new())), Ok(Manifest::default()), "an empty input");
        assert_eq!(
            read_manifest(Cursor::new(b"\xff\n".to_vec())),
            Err("the outputs manifest is not UTF-8".to_owned())
        );
        let malformed = read_manifest(Cursor::new(b"{\"files\":\n".to_vec())).expect_err("a malformed manifest is refused");
        assert!(malformed.starts_with("the outputs manifest is not JSON: "), "{malformed}");
    }

    #[test]
    fn a_manifest_reaching_the_limit_without_a_newline_is_refused_unread_past_it() {
        let mut input = Cursor::new(vec![b'x'; MANIFEST_LIMIT + 1]);
        assert_eq!(
            read_manifest(&mut input),
            Err("the outputs manifest reaches 64 MiB without a newline".to_owned())
        );
        assert_eq!(input.position(), MANIFEST_LIMIT as u64, "nothing past the limit is read");
    }

    fn sha(bytes: &[u8]) -> String {
        sha256_of(bytes).expect("bytes hash").0
    }

    /// The PC's manifest of `files`.
    fn manifest_of(files: &Files<'_>) -> Manifest {
        let mut manifest = Manifest::default();
        for (path, bytes) in files {
            let stamp = Stamp {
                size: bytes.len() as u64,
                modified: 0,
            };
            manifest.insert((*path).to_owned(), Held { stamp, sha256: sha(bytes) });
        }
        manifest
    }

    #[test]
    fn an_archive_whose_name_another_file_took_after_its_open_counts_as_gone_and_the_other_is_left() {
        let case = Case::new("trim-swapped");
        case.made_outputs();
        let folder = case.outputs();
        let now = SystemTime::now();
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(30));
        let outputs = output_folder(&case.folder, false).expect("the output folder opens");
        let name = archive('1');
        let opened = outputs.open_file(OsStr::new(&name)).expect("the archive opens");
        sized(&case.base, "newer", 7, now, Duration::ZERO);
        fs::rename(case.base.join("newer"), folder.join(&name)).expect("another file takes the archive's name");

        let gone = remove_held(&outputs, OsStr::new(&name), &opened).expect("the removal is tried");

        assert!(gone, "the archive opened is no longer at its name, so it counts as gone");
        let left = fs::metadata(folder.join(&name)).expect("the other file is there");
        assert_eq!(left.len(), 7, "the file that took the name is left alone");
    }

    /// The archive `name` of `case`'s output folder read back, copies of the PC's files taken from `manifest` without
    /// `skip`: each member's path and bytes, and every item listed.
    fn read_back(case: &Case, name: &str, manifest: &Manifest, skip: &[String]) -> (Vec<(String, Vec<u8>)>, Vec<Item>) {
        read_archive(&case.outputs().join(name), manifest, skip)
    }

    /// The outputs archive at `path` read back as the PC's reader reads it, copies of the PC's files taken from
    /// `manifest` without `skip`: each member's path and bytes, and every item listed.
    pub(crate) fn read_archive(path: &Path, manifest: &Manifest, skip: &[String]) -> (Vec<(String, Vec<u8>)>, Vec<Item>) {
        let bytes = fs::read(path).expect("the archive is read");
        let mut members = Vec::new();
        let items = read_members(&bytes[..], 1 << 30, 50_000, &manifest.by_hash(skip), |path, _, data| {
            let mut read = Vec::new();
            data.read_to_end(&mut read).map_err(UnpackError::from)?;
            members.push((path.to_owned(), read));
            Ok(())
        })
        .expect("the archive reads back");
        (members, items)
    }

    /// The paths of `items` listed as copies.
    fn copies(items: &[Item]) -> Vec<&str> {
        items
            .iter()
            .filter(|item| item.copy.is_some())
            .map(|item| item.entry.path.as_str())
            .collect()
    }

    /// The line a pack of `count` files into the archive whose nonce is 32 copies of `digit` says.
    fn packed_line(count: usize, digit: char) -> String {
        format!("gate-remote-outputs: {count} {}", archive(digit))
    }

    /// [`bring_back`]'s lines, the archive it held let go, once its size line is found to name the size of the archive
    /// it wrote, and to be there only when it wrote one.
    #[expect(
        clippy::panic_in_result_fn,
        reason = "a test helper: a wrong size line fails the test, never the Result it hands on"
    )]
    fn bring(
        asked: &Asked<'_>,
        manifest: &Result<Manifest, String>,
        listing: Option<&Listing>,
        work: &Path,
        skip: &[String],
    ) -> Result<(Option<String>, String), String> {
        bring_back(asked, manifest, listing, work, skip).map(|brought: Brought| {
            let size = brought.wrote_archive().then(|| {
                let written = fs::metadata(Path::new(asked.folder).join(&asked.archive)).expect("the archive packed is there");
                format!("gate-remote-outputs-size: {}", written.len())
            });
            assert_eq!(brought.size, size, "the size line names the archive's size");
            (brought.skipped, brought.line)
        })
    }

    /// Whether an exclusive `flock` on the file at `path`, asked without waiting on a descriptor of its own, is refused.
    fn locked_by_another(path: &Path) -> bool {
        let file = File::open(path).expect("the file opens");
        // SAFETY: the descriptor is `file`'s, open for the whole call; `flock` touches no memory.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        taken != 0
    }

    #[test]
    fn a_packed_archive_is_held_under_a_shared_flock_until_what_was_brought_is_dropped() {
        let case = Case::new("pack-held");
        case.write("bin/a.txt", b"a");
        let asked = case.asked(&["bin/**"], true, '1');
        let path = case.outputs().join(archive('1'));

        let brought = bring_back(&asked, &Ok(Manifest::default()), None, &case.work(), &[]).expect("the pack is made");

        assert!(locked_by_another(&path), "the run holds its archive while it trims");
        drop(brought);
        assert!(!locked_by_another(&path), "and lets it go once what was brought is dropped");
    }

    #[test]
    fn the_trim_passes_over_an_archive_a_pack_still_writes_and_removes_it_once_let_go() {
        let case = Case::new("trim-written");
        case.made_outputs();
        let folder = case.outputs();
        let now = SystemTime::now();
        sized(&folder, &archive('9'), 100, now, Duration::from_secs(1));
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(9));
        let held = File::open(folder.join(archive('1'))).expect("the older archive opens");
        // SAFETY: the descriptor is `held`'s, open for the whole call; `flock` touches no memory.
        let locked = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_SH) };
        assert_eq!(locked, 0, "{}", io::Error::last_os_error());
        let asked = case.asked(&["**"], false, '9');

        let trimmed = trim(&asked, 100);

        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert_eq!(trimmed.reason, None);
        assert_eq!(
            names_in(&folder),
            [archive('1'), archive('9')],
            "an archive a pack still writes is not removed"
        );
        drop(held);
        let trimmed = trim(&asked, 100);
        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert_eq!(names_in(&folder), [archive('9')], "once let go it is removed");
    }

    #[test]
    fn a_trim_whose_output_folder_others_may_enter_does_not_bring_the_outputs_back() {
        let case = Case::new("trim-not-private");
        case.made_outputs();
        sized(&case.outputs(), &archive('9'), 100, SystemTime::now(), Duration::ZERO);
        fs::set_permissions(case.outputs(), fs::Permissions::from_mode(0o755)).expect("the folder's mode is set");

        let trimmed = trim(&case.asked(&["**"], false, '9'), 100);

        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        let reason = trimmed.reason.expect("the outputs are not brought back");
        let cannot = format!("cannot open the output folder {}: ", case.folder);
        assert!(reason.starts_with(&cannot), "{reason}");
    }

    #[test]
    fn a_fifo_swapped_in_for_a_file_is_refused_at_its_open_without_waiting() {
        let case = Case::new("open-fifo");
        let work = case.work();
        let fifo = CString::new(work.join("fifo").as_os_str().as_bytes()).expect("the path holds no NUL");
        // SAFETY: `fifo` is a NUL-terminated path that lives across the call; `mkfifo` only reads it.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "{}", io::Error::last_os_error());

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let opened = Folder::open(&work).and_then(|folder| folder.open_file(OsStr::new("fifo")));
            sender.send(opened.map(drop).map_err(|error| error.to_string()))
        });
        let opened = receiver.recv_timeout(HANG_GUARD).expect("the open does not wait on the FIFO");

        assert_eq!(opened, Err("it is not a plain file".to_owned()));
    }

    #[test]
    fn a_new_or_changed_file_is_packed_and_one_the_pc_holds_unchanged_is_not() {
        let case = Case::new("pack-changed");
        case.write("bin/new.txt", b"new");
        case.write("bin/changed.txt", b"old");
        case.write("bin/same.txt", b"same");
        let manifest = manifest_of(&[("bin/changed.txt", b"old"), ("bin/same.txt", b"same")]);
        let asked = case.asked(&["bin/**"], false, '1');
        let (before, line) = listing(&asked, &case.work());
        assert!(line.is_none(), "{line:?}");
        case.write("bin/changed.txt", b"changed!");

        let brought = bring(&asked, &Ok(manifest_of(&[])), before.as_ref(), &case.work(), &[]);
        assert_eq!(
            brought,
            Ok((None, packed_line(3, '1'))),
            "against an empty manifest every file comes back"
        );
        fs::remove_file(case.outputs().join(archive('1'))).expect("the archive is removed");

        let brought = bring(
            &asked,
            &Ok(manifest_of(&[("bin/changed.txt", b"old"), ("bin/same.txt", b"same")])),
            before.as_ref(),
            &case.work(),
            &[],
        );

        assert_eq!(brought, Ok((None, packed_line(2, '1'))));
        let (members, _) = read_back(&case, &archive('1'), &manifest, &[]);
        assert_eq!(
            members,
            [
                ("bin/changed.txt".to_owned(), b"changed!".to_vec()),
                ("bin/new.txt".to_owned(), b"new".to_vec())
            ]
        );
        assert_eq!(mode_of(&case.outputs()), 0o700, "the output folder is made the box user's own");
        assert_eq!(mode_of(&case.outputs().join(archive('1'))), 0o600);
    }

    #[test]
    fn a_cross_build_s_file_matches_a_glob_on_its_pc_path() {
        let case = Case::new("cross-glob");
        case.write("tests/T/bin/Debug/net10.0/win-x64/T.dll", b"MZ\x90\x00dll");
        case.write("tests/T/bin/Debug/net10.0/T.dll", b"native");
        let asked = case.asked(&["tests/*/bin/*/*/*.dll"], true, '1').crossed_for(Some("win-x64"));

        let brought = bring(&asked, &Ok(manifest_of(&[])), None, &case.work(), &[]);

        assert_eq!(brought, Ok((None, packed_line(1, '1'))));
        let (members, _) = read_back(&case, &archive('1'), &Manifest::default(), &[]);
        assert_eq!(members, [("tests/T/bin/Debug/net10.0/T.dll".to_owned(), b"MZ\x90\x00dll".to_vec())]);
    }

    #[test]
    fn a_cross_build_never_opens_a_file_it_does_not_send() {
        let case = Case::new("cross-unsent");
        let native = "bin/Debug/net10.0/A";
        let mach_o = [0xCF, 0xFA, 0xED, 0xFE, 0, 0, 0, 0];
        case.write(native, &mach_o);
        fs::set_permissions(case.work().join(native), fs::Permissions::from_mode(0o000)).expect("the file is made unreadable");
        let asked = case.asked(&["bin/**"], false, '1').crossed_for(Some("win-x64"));
        let (before, line) = listing(&asked, &case.work());
        assert!(line.is_none(), "{line:?}");
        case.write("bin/Debug/net10.0/win-x64/A.exe", b"MZ\x90\x00");

        let brought = bring(&asked, &Ok(manifest_of(&[(native, &mach_o)])), before.as_ref(), &case.work(), &[]);

        assert_eq!(
            brought,
            Ok((None, packed_line(1, '1'))),
            "the unreadable file the PC holds unchanged is never read"
        );
        let (members, _) = read_back(&case, &archive('1'), &Manifest::default(), &[]);
        assert_eq!(members, [("bin/Debug/net10.0/A.exe".to_owned(), b"MZ\x90\x00".to_vec())]);
    }

    #[test]
    fn a_copy_of_an_earlier_member_or_a_pc_file_is_listed_and_a_back_commit_path_is_neither_packed_nor_a_source() {
        let case = Case::new("pack-copies");
        case.write("bin/a.txt", b"same bytes");
        case.write("bin/b.txt", b"same bytes");
        case.write("bin/c.txt", b"pc bytes");
        case.write("bin/d.txt", b"skipped bytes");
        case.write("bin/back.txt", b"back");
        let manifest = manifest_of(&[("pc/held.txt", b"pc bytes"), ("bin/back.txt", b"skipped bytes")]);
        let skip = vec!["bin/back.txt".to_owned()];
        let asked = case.asked(&["bin/**"], false, '1');

        let brought = bring(
            &asked,
            &Ok(manifest_of(&[("pc/held.txt", b"pc bytes"), ("bin/back.txt", b"skipped bytes")])),
            None,
            &case.work(),
            &skip,
        );

        assert_eq!(brought, Ok((None, packed_line(4, '1'))));
        let (members, items) = read_back(&case, &archive('1'), &manifest, &skip);
        let paths: Vec<&str> = members.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            ["bin/a.txt", "bin/d.txt"],
            "only the first of a content and one no allowed PC file holds"
        );
        assert_eq!(copies(&items), ["bin/b.txt", "bin/c.txt"]);
        assert!(
            items.iter().all(|item| item.entry.path != "bin/back.txt"),
            "a back-commit path is not listed"
        );
    }

    #[test]
    fn a_link_is_never_followed_and_a_hard_linked_file_is_packed() {
        let case = Case::new("pack-links");
        let outside = case.base.join("outside");
        fs::create_dir(&outside).expect("a folder outside the work folder is made");
        fs::write(outside.join("secret.txt"), b"secret").expect("a file outside is written");
        fs::write(outside.join("hard.txt"), b"hard").expect("a file outside is written");
        case.write("bin/plain.txt", b"plain");
        symlink(outside.join("secret.txt"), case.work().join("bin/file-link.txt")).expect("a link to a file is made");
        symlink(&outside, case.work().join("bin/folder-link")).expect("a link to a folder is made");
        fs::hard_link(outside.join("hard.txt"), case.work().join("bin/hard.txt")).expect("a hard link is made");
        let asked = case.asked(&["bin/**"], true, '1');

        let brought = bring(&asked, &Ok(Manifest::default()), None, &case.work(), &[]);

        assert_eq!(brought, Ok((None, packed_line(2, '1'))));
        let (members, _) = read_back(&case, &archive('1'), &Manifest::default(), &[]);
        assert_eq!(
            members,
            [
                ("bin/hard.txt".to_owned(), b"hard".to_vec()),
                ("bin/plain.txt".to_owned(), b"plain".to_vec())
            ]
        );
    }

    #[test]
    fn a_fifo_is_skipped_without_waiting_on_it() {
        let case = Case::new("pack-fifo");
        case.write("bin/a.txt", b"a");
        let fifo = CString::new(case.work().join("bin/fifo").as_os_str().as_bytes()).expect("the path holds no NUL");
        // SAFETY: `fifo` is a NUL-terminated path that lives across the call; `mkfifo` only reads it.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "{}", io::Error::last_os_error());
        let (folder, work) = (case.folder.clone(), case.work());

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let globs = ["bin/**".to_owned()];
            let asked = Asked::new((&globs, &[]), true, KEY, &"1".repeat(32), &folder)
                .expect("the globs are fine")
                .expect("globs ask");
            sender.send(bring(&asked, &Ok(Manifest::default()), None, &work, &[]))
        });
        let brought = receiver.recv_timeout(HANG_GUARD).expect("the pack does not wait on the FIFO");

        assert_eq!(brought, Ok((None, packed_line(1, '1'))));
        let (members, _) = read_back(&case, &archive('1'), &Manifest::default(), &[]);
        assert_eq!(members, [("bin/a.txt".to_owned(), b"a".to_vec())]);
    }

    #[test]
    fn a_file_whose_name_this_pc_cannot_take_is_skipped_and_said() {
        let case = Case::new("pack-unfit");
        case.write("bin/ok.txt", b"ok");
        case.write("bin/bad:name.txt", b"bad");
        case.write("bin/report.", b"report");
        case.write("bin/dir./inner.txt", b"inner");
        let asked = case.asked(&["bin/**"], true, '1');

        let brought = bring(&asked, &Ok(Manifest::default()), None, &case.work(), &[]);

        let skipped = "gate: remote outputs: skipped 3 file(s) with names this PC cannot take: bin/bad:name.txt, \u{2026}";
        assert_eq!(brought, Ok((Some(skipped.to_owned()), packed_line(1, '1'))));
        let (members, _) = read_back(&case, &archive('1'), &Manifest::default(), &[]);
        assert_eq!(members, [("bin/ok.txt".to_owned(), b"ok".to_vec())]);
    }

    #[test]
    fn a_trx_below_test_results_is_packed_when_a_glob_names_test_results() {
        let case = Case::new("pack-trx");
        let trx = "tests/Unit/bin/Release/net9.0/TestResults/run.trx";
        case.write(trx, b"<TestRun/>");
        case.write("tests/Unit/bin/Release/net9.0/TestResults/other.log", b"log");
        case.write("tests/Unit/bin/Release/net9.0/TestResults/deeper/TestResults/nested.trx", b"nested");
        let asked = case.asked(&["tests/*/bin/*/*/TestResults/*.trx"], true, '1');

        let brought = bring(&asked, &Ok(Manifest::default()), None, &case.work(), &[]);

        assert_eq!(brought, Ok((None, packed_line(1, '1'))));
        let (members, _) = read_back(&case, &archive('1'), &Manifest::default(), &[]);
        assert_eq!(members, [(trx.to_owned(), b"<TestRun/>".to_vec())]);
        let elsewhere = case.asked(&["tests/**"], true, '2');
        let brought = bring(&elsewhere, &Ok(Manifest::default()), None, &case.work(), &[]);
        assert_eq!(
            brought,
            Ok((None, NOTHING_PACKED.to_owned())),
            "a glob that does not name TestResults never reaches below it"
        );
    }

    /// A reader of a file that, at its first read, appends to the file at `path`, as a command still writing it does.
    struct Growing<'f> {
        file: &'f File,
        path: PathBuf,
        grown: bool,
    }

    impl Read for Growing<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.grown {
                self.grown = true;
                let mut appending = fs::OpenOptions::new().append(true).open(&self.path)?;
                appending.write_all(b" and more")?;
            }
            self.file.read(buffer)
        }
    }

    #[test]
    fn a_file_whose_size_changes_while_it_is_read_fails_the_pack() {
        let case = Case::new("pack-growing");
        case.write("bin/grow.txt", b"abc");
        let full = case.work().join("bin/grow.txt");
        let found = Found {
            path: "bin/grow.txt".to_owned(),
            full: full.clone(),
            stamp: Stamp { size: 3, modified: 0 },
        };
        let opened = Opened::of(File::open(&full).expect("the file opens"), &found).expect("the file is looked at");
        let mut packer = Packer::new(io::sink(), 20).expect("a packer");
        let growing = Growing {
            file: &opened.file,
            path: full,
            grown: false,
        };

        let error = pack_file(&mut packer, &found, &opened, growing).expect_err("a file that grew is refused");

        assert_eq!(
            error.to_string(),
            "bin/grow.txt changed while it was packed: its size or last write differs from when it was opened"
        );
    }

    #[test]
    fn nothing_to_pack_says_so_and_makes_no_archive() {
        let case = Case::new("pack-nothing");
        case.write("bin/same.txt", b"same");
        let asked = case.asked(&["bin/**"], false, '1');
        let (before, _) = listing(&asked, &case.work());

        let brought = bring(&asked, &Ok(manifest_of(&[("bin/same.txt", b"same")])), before.as_ref(), &case.work(), &[]);

        assert_eq!(brought, Ok((None, NOTHING_PACKED.to_owned())));
        assert_eq!(NOTHING_PACKED, "gate-remote-outputs: 0");
        assert!(!case.outputs().exists(), "no output folder is made for nothing");
    }

    #[test]
    fn an_archive_that_cannot_be_written_is_not_brought_back_and_what_was_there_is_left() {
        let case = Case::new("pack-unwritten");
        case.write("bin/a.txt", b"a");
        case.made_outputs();
        fs::write(case.outputs().join(archive('1')), b"kept").expect("a file at the archive's name is written");
        let asked = case.asked(&["bin/**"], true, '1');

        let reason = bring(&asked, &Ok(Manifest::default()), None, &case.work(), &[]).expect_err("the archive cannot be made");

        let cannot = format!("cannot create {}/{}: ", case.folder, archive('1'));
        assert!(reason.starts_with(&cannot), "{reason}");
        assert!(not_brought_back(&reason).starts_with(&format!("gate: remote outputs not brought back: {cannot}")));
        assert_eq!(fs::read(case.outputs().join(archive('1'))).expect("the file is read"), b"kept");
    }

    #[test]
    fn a_manifest_that_could_not_be_read_is_the_reason_nothing_is_brought_back() {
        let case = Case::new("pack-unread");
        case.write("bin/a.txt", b"a");
        let asked = case.asked(&["bin/**"], true, '1');
        let unread = Err("the outputs manifest is not UTF-8".to_owned());

        assert_eq!(
            bring(&asked, &unread, None, &case.work(), &[]),
            Err("the outputs manifest is not UTF-8".to_owned())
        );
        assert!(!case.outputs().exists());
    }
}
