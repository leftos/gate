//! A routed run's outputs on the box. A run with output globs reads the PC's manifest on stdin once its heartbeat beats,
//! removes the outputs archives older than a day, lists the files under the globs before the command, and after a
//! passing command and its back commit packs the files the PC lacks or holds otherwise into `<key>-<nonce>.tar.zst` in
//! the output folder, as the PowerShell wrapper's `Publish-RunOutput` does. The `outputs` verb sends one such archive
//! raw on stdout, then removes it; it takes no work folder lock, so it never waits on a run. A pack that pushes the
//! folder past its size budget removes the archives last written before its own, oldest first, never one written with
//! or after it, and removes its own when that alone is over the budget, ending the run's outputs as not brought back.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufWriter, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gate_args::BoxSeams;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::payload::{Payload, archive_file_name, archive_key, escaped};
use crate::boxside::run::FILE_FLAG_OPEN_REPARSE_POINT;
use crate::console::Console;
use crate::outputs::archive::pack;
use crate::outputs::glob::Globs;
use crate::outputs::hash::file_sha256;
use crate::outputs::manifest::{Listing, Manifest, select};
use crate::outputs::walk::{Found, in_context, walk, walk_with_refused};

/// How long an outputs archive is kept in the output folder before a run with output globs removes it, as the wrapper's
/// `$OutputArchiveHours` keeps it.
const ARCHIVE_AGE: Duration = Duration::from_hours(24);
/// The most the PC's manifest line may hold, its newline included: the shared limit, as the reads count it.
const MANIFEST_LIMIT: u64 = crate::outputs::manifest::MANIFEST_LIMIT as u64;
/// The bytes in one gibibyte, as [`OUTPUTS_BUDGET`] and an over-large archive's size in the line saying so are counted.
const GIB: u64 = 1024 * 1024 * 1024;
/// The most of the output folder's outputs archives the box keeps, in bytes: 50 GiB. A pack that pushes the folder past
/// it removes the other archives, oldest first, until it fits again. The client unpacks an archive to no more bytes.
pub(crate) const OUTPUTS_BUDGET: u64 = 50 * GIB;
/// The line a pack says when there was nothing to bring back, and so no archive was written.
pub(super) const NOTHING_PACKED: &str = "gate-remote-outputs: 0";

/// What a run with output globs asks for: the globs, whether every file counts as changed (`OutputsFull`), the name of
/// its archive and the folder it goes in.
pub(super) struct Asked<'a> {
    globs: Globs,
    full: bool,
    /// The name of the run's own outputs archive in its output folder.
    pub(super) archive: String,
    /// The output folder the archive is written in.
    pub(super) folder: &'a str,
}

impl<'a> Asked<'a> {
    /// The outputs of a run of the work folder `key`, its outputs nonce `nonce`: `None` when `globs` is empty; else
    /// the usage line saying why a glob cannot be matched.
    pub(super) fn new(globs: &[String], full: bool, key: &str, nonce: &str, folder: &'a str) -> Result<Option<Asked<'a>>, String> {
        if globs.is_empty() {
            return Ok(None);
        }
        let globs = Globs::new(globs).map_err(|reason| format!("gate: box payload Outputs {reason}"))?;
        Ok(Some(Asked {
            globs,
            full,
            archive: archive_file_name(key, nonce),
            folder,
        }))
    }
}

/// The PC's manifest, read from `input` up to its first newline or its end, never waiting past the newline and never
/// reading more than [`MANIFEST_LIMIT`] bytes; else why it cannot be read, which the run says once its command has
/// passed. A line that reaches the limit without its newline cannot be read. The read has no deadline: a client that
/// stops sending is ended by its own watchdog, which closes the connection and so ends the input.
pub(super) fn read_manifest(mut input: impl BufRead) -> Result<Manifest, String> {
    let mut line = Vec::new();
    input
        .by_ref()
        .take(MANIFEST_LIMIT)
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("the outputs manifest could not be read: {error}"))?;
    if !line.ends_with(b"\n") && line.len() as u64 >= MANIFEST_LIMIT {
        return Err("the outputs manifest reaches 64 MiB without a newline".to_owned());
    }
    let line = String::from_utf8(line).map_err(|_| "the outputs manifest is not UTF-8".to_owned())?;
    Manifest::parse(&line)
}

/// Removes each outputs archive in `asked`'s output folder, a file named `<key>-<nonce>.tar.zst` as the box names one,
/// last written more than [`ARCHIVE_AGE`] before `now`; every other file is left alone. The lines saying each archive
/// that could not be removed, or that the folder could not be listed. A missing folder holds nothing to remove, and an
/// archive gone before its removal, as another run's prune may take it, counts as removed.
pub(super) fn prune(asked: &Asked<'_>, now: SystemTime) -> Vec<String> {
    let folder = Path::new(asked.folder);
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![not_listed(folder, &error)],
    };
    let mut lines = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // Not read further: a listing that failed once can fail the same way at each later entry.
                lines.push(not_listed(folder, &error));
                break;
            }
        };
        if let Err(error) = remove_if_old(&entry, now) {
            lines.push(could_not_remove(&entry.path(), &error));
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

/// Removes the file `entry` when it is an outputs archive last written more than [`ARCHIVE_AGE`] before `now`; one
/// already gone counts as removed.
fn remove_if_old(entry: &fs::DirEntry, now: SystemTime) -> io::Result<()> {
    if entry.file_name().to_str().and_then(archive_key).is_none() {
        return Ok(());
    }
    let metadata = entry.metadata()?;
    let old = now.duration_since(metadata.modified()?).is_ok_and(|age| age > ARCHIVE_AGE);
    if !(metadata.is_file() && old) {
        return Ok(());
    }
    remove_archive(&entry.path())
}

/// Removes the outputs archive at `path`; one already gone, as another run's trim may take it, counts as removed.
fn remove_archive(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// An outputs archive in the output folder: where it is, how big it is, its last write, and whether it is the run's own.
struct Archive {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    own: bool,
}

/// What a trim of the output folder found.
pub(super) struct Trimmed {
    /// The lines saying each archive that could not be removed, in the order they were tried.
    pub(super) lines: Vec<String>,
    /// Why the run's own archive is not brought back, `None` when it is: [`over_budget_reason`]'s line for one alone
    /// over the budget, which has been removed, or [`GONE_BEFORE_SENT`] for one gone before the trim.
    pub(super) reason: Option<String>,
}

/// The output folder's size budget: the `--test-outputs-budget-bytes` seam's bytes when given, else [`OUTPUTS_BUDGET`].
pub(super) fn budget(seams: &BoxSeams) -> u64 {
    seams.outputs_budget_bytes.unwrap_or(OUTPUTS_BUDGET)
}

/// Brings the output folder back within `budget` bytes after a pack wrote the run's own archive named `own`. An own
/// archive gone before the trim is not brought back ([`GONE_BEFORE_SENT`]); one alone larger than the budget is removed
/// and no other archive is touched, since none of them could bring the folder within it and they belong to runs whose
/// PCs may still fetch them; else the archives last written strictly before the run's own are removed, oldest first,
/// until the folder fits. An archive last written with the run's own or after it is never removed, so two runs packing
/// at once never remove each other's fresh archives: the newer run's own trim covers the older ones. The lines saying
/// each archive that could not be removed are returned beside why the run's own archive is not brought back, `None`
/// when it is. A folder that could not be listed whole is left as it is, its line returned.
pub(super) fn trim(folder: &Path, own: &str, budget: u64) -> Trimmed {
    let mut lines = Vec::new();
    let Some(archives) = archives(folder, own, &mut lines) else {
        return Trimmed { lines, reason: None };
    };
    let Some(mine) = archives.iter().find(|archive| archive.own) else {
        return Trimmed {
            lines,
            reason: Some(GONE_BEFORE_SENT.to_owned()),
        };
    };
    if mine.size > budget {
        if let Err(error) = remove_archive(&mine.path) {
            lines.push(could_not_remove(&mine.path, &error));
        }
        return Trimmed {
            lines,
            reason: Some(over_budget_reason(mine.size)),
        };
    }
    let total = archives.iter().fold(0_u64, |total, archive| total.saturating_add(archive.size));
    trim_to_fit(&archives, mine, total, budget, &mut lines);
    Trimmed { lines, reason: None }
}

/// The reason a passing run's outputs are not brought back when the run's own archive was gone before the trim, as
/// another run's trim or the age prune may take it.
const GONE_BEFORE_SENT: &str = "the archive was removed before it could be sent";

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

/// The outputs archives in `folder`, the run's own named `own` marked, with their sizes and last writes; `None` when
/// the folder or an entry of the listing could not be read, its line pushed to `lines`, so a trim is never taken from a
/// listing that is not whole. An entry that is not an outputs archive, or is not a plain file, is left out, and one
/// whose metadata cannot be read is said and left out too.
fn archives(folder: &Path, own: &str, lines: &mut Vec<String>) -> Option<Vec<Archive>> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(error) => {
            lines.push(not_listed(folder, &error));
            return None;
        }
    };
    let mut archives = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                lines.push(not_listed(folder, &error));
                return None;
            }
        };
        match archive_of(&entry, own) {
            Ok(Some(archive)) => archives.push(archive),
            Ok(None) => {}
            Err(error) => lines.push(could_not_read(&entry.path(), &error)),
        }
    }
    Some(archives)
}

/// The outputs archive `entry` is, the run's own when its name is `own`; `None` when it is not an outputs archive or is
/// not a plain file; else why it could not be read.
fn archive_of(entry: &fs::DirEntry, own: &str) -> io::Result<Option<Archive>> {
    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
        return Ok(None);
    };
    if archive_key(&name).is_none() {
        return Ok(None);
    }
    let metadata = entry.metadata()?;
    if !metadata.is_file() {
        return Ok(None);
    }
    Ok(Some(Archive {
        path: entry.path(),
        size: metadata.len(),
        modified: metadata.modified()?,
        own: name == own,
    }))
}

/// Removes the archives of `archives` last written strictly before the run's own `mine`, oldest first and by path among
/// ones last written together, until their `total` size is within `budget`, saying each that cannot be removed and going
/// on to the next. An archive written with the run's own or after it is left alone, and the folder stays over the budget
/// when no older one is left.
fn trim_to_fit(archives: &[Archive], mine: &Archive, mut total: u64, budget: u64, lines: &mut Vec<String>) {
    let mut older: Vec<&Archive> = archives.iter().filter(|archive| archive.modified < mine.modified).collect();
    older.sort_by(|left, right| left.modified.cmp(&right.modified).then_with(|| left.path.cmp(&right.path)));
    for archive in older {
        if total <= budget {
            return;
        }
        match remove_archive(&archive.path) {
            Ok(()) => total = total.saturating_sub(archive.size),
            Err(error) => lines.push(could_not_remove(&archive.path, &error)),
        }
    }
}

/// The stat listing of the files under `asked`'s globs in `work`, taken before the command, and the line saying it could
/// not be taken, when it could not: then it is empty, so every file counts as changed. `None` under `OutputsFull`.
pub(super) fn listing(asked: &Asked<'_>, work: &Path) -> (Option<Listing>, Option<String>) {
    if asked.full {
        return (None, None);
    }
    match walk(work, &asked.globs) {
        Ok(found) => (Some(Listing::of(&found)), None),
        Err(error) => (
            Some(Listing::default()),
            Some(format!(
                "gate: remote could not list the outputs before the command: {}",
                escaped(&error.to_string())
            )),
        ),
    }
}

/// Packs the files under `asked`'s globs in `work` to bring back after a passing command ([`select`]), leaving out
/// `skip`, the paths of the run's back commit: the line saying how many were packed and the archive's name, or
/// `gate-remote-outputs: 0` with no archive written when none were, and before it the line saying the files [`select`]
/// would have packed but for a name this PC cannot take ([`skipped_line`]), when there were any. Else why the outputs
/// are not brought back; an archive left half written is removed. The archive is made anew: a file or a link already
/// at its name fails the pack and is left as it is. A file whose final path is not inside `work` fails the pack
/// ([`pack`]). The archive is opened sharing reads only while it is packed, so another run's trim cannot remove it
/// before it is whole.
pub(super) fn bring_back(
    asked: &Asked<'_>,
    manifest: &Result<Manifest, String>,
    listing: Option<&Listing>,
    work: &Path,
    skip: &[String],
) -> Result<(Option<String>, String), String> {
    let manifest = manifest.as_ref().map_err(Clone::clone)?;
    let walked = walk_with_refused(work, &asked.globs).map_err(|error| format!("the outputs could not be listed: {error}"))?;
    let hash = |file: &Found| file_sha256(&file.full);
    let skipped = skipped_line(&select(&walked.refused, manifest, listing, skip, hash).map_err(|error| error.to_string())?);
    let chosen = select(&walked.found, manifest, listing, skip, hash).map_err(|error| error.to_string())?;
    if chosen.is_empty() {
        return Ok((skipped, NOTHING_PACKED.to_owned()));
    }
    fs::create_dir_all(asked.folder).map_err(|error| format!("cannot make the output folder {}: {error}", asked.folder))?;
    let path = Path::new(asked.folder).join(&asked.archive);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    match pack(BufWriter::new(file), work, &chosen) {
        Ok(packed) => Ok((skipped, format!("gate-remote-outputs: {} {}", packed.entries.len(), asked.archive))),
        Err(error) => match fs::remove_file(&path) {
            Ok(()) => Err(error.to_string()),
            Err(removal) => Err(format!("{error}; and {} could not be removed: {removal}", path.display())),
        },
    }
}

/// The start of the line saying the run's outputs leave out files for names this PC cannot take ([`skipped_line`]),
/// which the PC's client reads on standard error. A command can print it too, which only makes the PC count the run's
/// outputs as not brought back.
pub(crate) const NAMES_SKIPPED: &str = "gate: remote outputs: skipped ";

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
pub(super) fn not_brought_back(reason: &str) -> String {
    format!("gate: remote outputs not brought back: {}", escaped(reason))
}

/// Answers `gate box outputs`: the archive the payload's `Name` names in its `OutputFolder`, sent raw on stdout, then
/// removed. A work folder or output folder the payload gets wrong, or a payload without a `Name`, is a usage error (2);
/// a `Name` given that is not an outputs archive of the work folder, an archive that does not exist or one that cannot
/// be read or sent ends with status 1, and a sent archive that cannot be removed is said and left for a later run's
/// [`prune`], status 0; one already gone says nothing.
pub(super) fn outputs(payload: &Payload, console: &mut Console) -> u32 {
    let folder = match payload.work_folder().and_then(|(_, work)| payload.output_folder(work)) {
        Ok(folder) => folder,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    let name = match payload.archive_name() {
        Ok(name) => name,
        Err(line) => {
            console.err(&line);
            let lacking = payload.text("Name").is_err();
            return u32::from(if lacking { USAGE_STATUS } else { FAILED_STATUS });
        }
    };
    let path = Path::new(folder).join(name);
    if let Err(unsent) = send(&path, &mut io::stdout().lock()) {
        console.err(&unsent.line(name));
        return u32::from(FAILED_STATUS);
    }
    if let Err(error) = remove_archive(&path) {
        console.err(&format!(
            "gate: remote could not remove the outputs archive {name}: {}",
            escaped(&error.to_string())
        ));
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
    /// The line saying so of the archive `name`, an archive name the payload's check let through.
    fn line(&self, name: &str) -> String {
        match self {
            Unsent::Missing => format!("gate: remote outputs archive {name} does not exist"),
            Unsent::Unread(error) => format!("gate: remote could not read the outputs archive {name}: {}", escaped(&error.to_string())),
            Unsent::Unwritten(error) => format!("gate: remote could not send the outputs archive {name}: {}", escaped(&error.to_string())),
        }
    }
}

/// Copies the plain file at `path`, never a link's target, to `sink` through a fixed buffer, then flushes it.
fn send(path: &Path, sink: &mut impl Write) -> Result<(), Unsent> {
    let mut archive = match OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path) {
        Ok(archive) => archive,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(Unsent::Missing),
        Err(error) => return Err(Unsent::Unread(error)),
    };
    gate_win32::check_plain_file(&archive).map_err(Unsent::Unread)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match io::Read::read(&mut archive, &mut buffer) {
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
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::fs::{self, File};
    use std::io::Cursor;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::{ARCHIVE_AGE, Asked, GONE_BEFORE_SENT, MANIFEST_LIMIT, prune, read_manifest, remove_archive, remove_if_old, skipped_line, trim};
    use crate::outputs::walk::{Found, Stamp};

    /// The name of an outputs archive of the work folder `k`, its nonce 32 copies of `digit`.
    fn archive(digit: char) -> String {
        format!("k-{}.tar.zst", digit.to_string().repeat(32))
    }

    /// A scratch folder for one test, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-box-outputs-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("an earlier scratch folder can be removed");
        }
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
    }

    #[test]
    fn a_manifest_is_read_up_to_its_newline_and_what_follows_is_left_unread() {
        let mut input = Cursor::new(b"{\"files\":{}}\nmore".to_vec());
        assert_eq!(read_manifest(&mut input), Ok(super::Manifest::default()));
        assert_eq!(input.position(), 13, "nothing past the newline is read");
        assert_eq!(read_manifest(Cursor::new(Vec::new())), Ok(super::Manifest::default()), "an empty input");
        assert_eq!(
            read_manifest(Cursor::new(b"\xff\n".to_vec())),
            Err("the outputs manifest is not UTF-8".to_owned())
        );
    }

    #[test]
    fn a_manifest_reaching_the_limit_without_a_newline_is_refused_unread_past_it() {
        let limit = usize::try_from(MANIFEST_LIMIT).expect("the limit fits a usize");
        let mut input = Cursor::new(vec![b'x'; limit + 1]);
        assert_eq!(
            read_manifest(&mut input),
            Err("the outputs manifest reaches 64 MiB without a newline".to_owned())
        );
        assert_eq!(input.position(), MANIFEST_LIMIT, "nothing past the limit is read");
    }

    #[test]
    fn no_globs_ask_for_no_outputs() {
        let asked = Asked::new(&[], true, "k", "n", r"C:\outputs").expect("no globs are fine");
        assert!(asked.is_none());
    }

    /// The outputs of a run of the work folder `k` asking for every file, its output folder `folder`.
    fn asked(folder: &str) -> Asked<'_> {
        Asked::new(&["**".to_owned()], false, "k", "n", folder)
            .expect("the glob is fine")
            .expect("globs ask")
    }

    /// Makes an empty file `name` in `folder` last written `age` before `now`.
    fn aged(folder: &std::path::Path, name: &str, now: SystemTime, age: Duration) {
        let file = File::create(folder.join(name)).expect("a file can be made");
        file.set_modified(now - age).expect("its last write can be set");
    }

    #[test]
    fn prune_removes_only_outputs_archives_older_than_a_day() {
        let folder = scratch("prune");
        let text = folder.to_str().expect("the scratch folder is Unicode").to_owned();
        let now = SystemTime::now();
        let old = ARCHIVE_AGE + Duration::from_secs(60);
        let removed = [archive('0'), format!("other_key-2{}.tar.zst", "0".repeat(31))];
        let mut kept = vec![
            "k-old.tar.zst".to_owned(),
            "notes.tar.zst".to_owned(),
            ".tar.zst".to_owned(),
            "old.txt".to_owned(),
            format!("k-{}.tar.zst", "0".repeat(31)),
            format!("k-{}.tar.zst", "A".repeat(32)),
            format!("k-{}.TAR.ZST", "f".repeat(32)),
        ];
        for name in removed.iter().chain(&kept) {
            aged(&folder, name, now, old);
        }
        aged(&folder, &archive('1'), now, Duration::from_hours(23));
        kept.push(archive('1'));
        kept.sort();
        let asked = asked(&text);

        assert!(prune(&asked, now).is_empty(), "nothing failed");

        let mut left: Vec<String> = fs::read_dir(&folder)
            .expect("the folder can be listed")
            .map(|entry| entry.expect("an entry").file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, kept, "only old files named as outputs archives are removed");
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
        assert!(prune(&asked, now).is_empty(), "a missing folder holds nothing to remove");
    }

    #[test]
    fn removing_an_archive_already_gone_says_nothing() {
        let folder = scratch("prune-gone");
        let now = SystemTime::now();
        aged(&folder, &archive('0'), now, ARCHIVE_AGE + Duration::from_secs(60));
        let entry = fs::read_dir(&folder)
            .expect("the folder can be listed")
            .next()
            .expect("the archive is listed")
            .expect("its entry can be read");
        fs::remove_file(entry.path()).expect("the archive can be removed first");

        assert_eq!(
            remove_if_old(&entry, now).map_err(|error| error.to_string()),
            Ok(()),
            "an archive another run removed counts as removed"
        );
    }

    #[test]
    fn an_output_folder_that_cannot_be_listed_is_named_with_its_control_characters_escaped() {
        // In a folder that exists, so the name itself is refused, not found missing.
        let parent = std::env::temp_dir();
        let parent = parent.to_str().expect("the temp folder is Unicode").trim_end_matches('\\');
        let lines = prune(&asked(&format!("{parent}\\a\u{1}b")), SystemTime::now());

        assert_eq!(lines.len(), 1, "{lines:?}");
        let listing = format!(r"gate: remote could not list the old outputs archives in {parent}\a\u{{1}}b: ");
        assert!(lines[0].starts_with(&listing), "{listing:?} in {lines:?}");
        assert!(!lines[0].contains('\u{1}'), "{lines:?}");
    }

    /// Makes a file `name` in `folder` holding `bytes` zero bytes, last written `age` before `now`.
    fn sized(folder: &std::path::Path, name: &str, bytes: usize, now: SystemTime, age: Duration) {
        let path = folder.join(name);
        fs::write(&path, vec![0_u8; bytes]).expect("a file of many bytes can be written");
        let file = fs::OpenOptions::new().write(true).open(&path).expect("the file can be opened");
        file.set_modified(now - age).expect("its last write can be set");
    }

    #[test]
    fn the_trim_removes_the_oldest_archives_until_the_folder_fits() {
        let folder = scratch("trim-fits");
        let now = SystemTime::now();
        sized(&folder, &archive('9'), 100, now, Duration::ZERO);
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(3));
        sized(&folder, &archive('2'), 100, now, Duration::from_secs(2));
        sized(&folder, &archive('3'), 100, now, Duration::from_secs(1));

        let trimmed = trim(&folder, &archive('9'), 250);

        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert!(trimmed.reason.is_none(), "the run's own archive fits the budget");
        assert!(!folder.join(archive('1')).exists(), "the oldest archive is removed");
        assert!(!folder.join(archive('2')).exists(), "so is the next oldest");
        assert!(
            folder.join(archive('3')).exists(),
            "the folder fits once they are gone, so the newest stays"
        );
        assert!(folder.join(archive('9')).exists(), "the run's own archive stays");
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    #[test]
    fn the_trim_never_removes_the_runs_own_archive() {
        let folder = scratch("trim-own");
        let now = SystemTime::now();
        sized(&folder, &archive('9'), 100, now, Duration::from_secs(5));
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(9));
        sized(&folder, &archive('2'), 100, now, Duration::from_secs(8));

        let trimmed = trim(&folder, &archive('9'), 100);

        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert!(trimmed.reason.is_none(), "the run's own archive is within the budget");
        assert!(folder.join(archive('9')).exists(), "the run's own archive stays");
        assert!(!folder.join(archive('1')).exists(), "an older archive is removed instead");
        assert!(!folder.join(archive('2')).exists(), "and the next older one, until none is left");
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    #[test]
    fn the_trim_never_removes_an_archive_newer_than_its_own() {
        let folder = scratch("trim-newer");
        let now = SystemTime::now();
        sized(&folder, &archive('9'), 100, now, Duration::from_secs(5));
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(5));
        sized(&folder, &archive('2'), 100, now, Duration::from_secs(1));
        sized(&folder, &archive('3'), 100, now, Duration::from_secs(9));

        let trimmed = trim(&folder, &archive('9'), 100);

        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert!(trimmed.reason.is_none());
        assert!(!folder.join(archive('3')).exists(), "an archive older than the run's own is removed");
        assert!(
            folder.join(archive('1')).exists(),
            "an archive last written with the run's own is left alone"
        );
        assert!(folder.join(archive('2')).exists(), "an archive newer than the run's own is left alone");
        assert!(folder.join(archive('9')).exists(), "the run's own archive stays");
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    #[test]
    fn files_that_are_not_archives_are_neither_counted_nor_removed() {
        let folder = scratch("trim-not-archives");
        let now = SystemTime::now();
        sized(&folder, &archive('9'), 100, now, Duration::ZERO);
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(2));
        sized(&folder, "old.txt", 10_000, now, Duration::from_secs(2));
        sized(&folder, "k-old.tar.zst", 10_000, now, Duration::from_secs(2));

        let trimmed = trim(&folder, &archive('9'), 250);

        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert!(trimmed.reason.is_none());
        assert!(
            folder.join(archive('1')).exists(),
            "no archive is removed to make room for a file that is not counted"
        );
        assert!(folder.join("old.txt").exists(), "a file that is not an archive is left alone");
        assert!(
            folder.join("k-old.tar.zst").exists(),
            "a .tar.zst file not named as an outputs archive is left alone"
        );
        assert!(folder.join(archive('9')).exists());
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    #[test]
    fn an_own_archive_larger_than_the_budget_is_reported() {
        let folder = scratch("trim-own-large");
        let now = SystemTime::now();
        sized(&folder, &archive('9'), 1_000, now, Duration::from_secs(1));
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(9));

        let trimmed = trim(&folder, &archive('9'), 500);

        assert_eq!(
            trimmed.reason.as_deref(),
            Some("the archive is 0.0 GiB, more than the box keeps (50 GiB)"),
            "the run's own archive alone is over the budget"
        );
        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert!(!folder.join(archive('9')).exists(), "the run's own archive is removed");
        assert!(folder.join(archive('1')).exists(), "every other archive is left alone");
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    #[test]
    fn an_own_archive_gone_before_the_trim_is_not_brought_back() {
        let folder = scratch("trim-own-gone");
        let now = SystemTime::now();
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(9));

        let trimmed = trim(&folder, &archive('9'), 100);

        assert_eq!(
            trimmed.reason.as_deref(),
            Some(GONE_BEFORE_SENT),
            "an own archive gone before the trim is not brought back"
        );
        assert!(trimmed.lines.is_empty(), "{:?}", trimmed.lines);
        assert!(
            folder.join(archive('1')).exists(),
            "nothing is removed while the run's own archive cannot be found"
        );
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    #[test]
    fn a_failed_removal_moves_on_to_the_next_oldest() {
        let folder = scratch("trim-failed-removal");
        let now = SystemTime::now();
        sized(&folder, &archive('9'), 100, now, Duration::ZERO);
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(9));
        sized(&folder, &archive('2'), 100, now, Duration::from_secs(8));
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(folder.join(archive('1')))
            .expect("the oldest archive can be held open sharing nothing");

        let trimmed = trim(&folder, &archive('9'), 100);

        let refused = format!(
            "gate: remote could not remove the old outputs archive {}: ",
            folder.join(archive('1')).display()
        );
        assert_eq!(trimmed.lines.len(), 1, "{:?}", trimmed.lines);
        assert!(trimmed.lines[0].starts_with(&refused), "{refused:?} in {:?}", trimmed.lines);
        assert!(folder.join(archive('1')).exists(), "an archive that cannot be removed stays");
        assert!(!folder.join(archive('2')).exists(), "the trim goes on to the next oldest");
        drop(held);
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    #[test]
    fn an_archive_gone_before_its_removal_counts_as_removed() {
        let folder = scratch("trim-gone");
        let now = SystemTime::now();
        sized(&folder, &archive('1'), 100, now, Duration::from_secs(1));
        let path = folder.join(archive('1'));
        fs::remove_file(&path).expect("the archive can be removed first, as another run's trim may");

        assert_eq!(
            remove_archive(&path).map_err(|error| error.to_string()),
            Ok(()),
            "an archive another run removed counts as removed"
        );
        fs::remove_dir_all(&folder).expect("the scratch folder can be removed");
    }

    /// A refused file at the relative `path`.
    fn refused(path: &str) -> Found {
        Found {
            path: path.to_owned(),
            full: PathBuf::from(path),
            stamp: Stamp { size: 1, modified: 1 },
        }
    }

    #[test]
    fn the_skipped_line_names_the_first_path_whole_when_it_is_short() {
        let (first, second) = (refused("bin/m~10.txt"), refused("bin/report."));
        assert_eq!(skipped_line(&[]), None);
        assert_eq!(
            skipped_line(&[&first]).as_deref(),
            Some("gate: remote outputs: skipped 1 file(s) with names this PC cannot take: bin/m~10.txt")
        );
        assert_eq!(
            skipped_line(&[&first, &second]).as_deref(),
            Some("gate: remote outputs: skipped 2 file(s) with names this PC cannot take: bin/m~10.txt, \u{2026}")
        );
    }

    #[test]
    fn the_skipped_line_cuts_a_long_first_path_at_a_char_boundary_below_one_kib() {
        let long = refused(&format!("a{}", "\u{e9}".repeat(600)));
        let line = skipped_line(&[&long, &long]).expect("two files were skipped");
        assert_eq!(
            line,
            format!(
                "gate: remote outputs: skipped 2 file(s) with names this PC cannot take: a{}\u{2026}, \u{2026}",
                "\u{e9}".repeat(511)
            )
        );
    }
}
