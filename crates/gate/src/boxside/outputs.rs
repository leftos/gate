//! A routed run's outputs on the box. A run with output globs reads the PC's manifest on stdin once its heartbeat beats,
//! removes the outputs archives older than a day, lists the files under the globs before the command, and after a
//! passing command and its back commit packs the files the PC lacks or holds otherwise into `<key>-<nonce>.tar.zst` in
//! the output folder, as the PowerShell wrapper's `Publish-RunOutput` does. The `outputs` verb sends one such archive
//! raw on stdout, then removes it; it takes no work folder lock, so it never waits on a run.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufWriter, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::payload::{Payload, archive_file_name, archive_key, escaped};
use crate::boxside::run::FILE_FLAG_OPEN_REPARSE_POINT;
use crate::console::Console;
use crate::outputs::archive::pack;
use crate::outputs::glob::Globs;
use crate::outputs::hash::file_sha256;
use crate::outputs::manifest::{Listing, Manifest, select};
use crate::outputs::walk::{in_context, walk};

/// How long an outputs archive is kept in the output folder before a run with output globs removes it, as the wrapper's
/// `$OutputArchiveHours` keeps it.
const ARCHIVE_AGE: Duration = Duration::from_hours(24);
/// The most the PC's manifest line may hold, its newline included.
const MANIFEST_LIMIT: u64 = 64 * 1024 * 1024;

/// What a run with output globs asks for: the globs, whether every file counts as changed (`OutputsFull`), the name of
/// its archive and the folder it goes in.
pub(super) struct Asked<'a> {
    globs: Globs,
    full: bool,
    archive: String,
    folder: &'a str,
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
            lines.push(format!(
                "gate: remote could not remove the old outputs archive {}",
                escaped(&in_context(&entry.path(), &error).to_string())
            ));
        }
    }
    lines
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
    match fs::remove_file(entry.path()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
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
/// `gate-remote-outputs: 0` with no archive written when none were. Else why the outputs are not brought back; an
/// archive left half written is removed. The archive is made anew: a file or a link already at its name fails the pack
/// and is left as it is. A file whose final path is not inside `work` fails the pack ([`pack`]).
pub(super) fn bring_back(
    asked: &Asked<'_>,
    manifest: &Result<Manifest, String>,
    listing: Option<&Listing>,
    work: &Path,
    skip: &[String],
) -> Result<String, String> {
    let manifest = manifest.as_ref().map_err(Clone::clone)?;
    let found = walk(work, &asked.globs).map_err(|error| format!("the outputs could not be listed: {error}"))?;
    let chosen = select(&found, manifest, listing, skip, |file| file_sha256(&file.full)).map_err(|error| error.to_string())?;
    if chosen.is_empty() {
        return Ok("gate-remote-outputs: 0".to_owned());
    }
    fs::create_dir_all(asked.folder).map_err(|error| format!("cannot make the output folder {}: {error}", asked.folder))?;
    let path = Path::new(asked.folder).join(&asked.archive);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    match pack(BufWriter::new(file), work, &chosen) {
        Ok(packed) => Ok(format!("gate-remote-outputs: {} {}", packed.entries.len(), asked.archive)),
        Err(error) => match fs::remove_file(&path) {
            Ok(()) => Err(error.to_string()),
            Err(removal) => Err(format!("{error}; and {} could not be removed: {removal}", path.display())),
        },
    }
}

/// The line saying a passing run's outputs are not brought back, for `reason`.
pub(super) fn not_brought_back(reason: &str) -> String {
    format!("gate: remote outputs not brought back: {}", escaped(reason))
}

/// Answers `gate box outputs`: the archive the payload's `Name` names in its `OutputFolder`, sent raw on stdout, then
/// removed. A work folder or output folder the payload gets wrong, or a payload without a `Name`, is a usage error (2);
/// a `Name` given that is not an outputs archive of the work folder, an archive that does not exist or one that cannot
/// be read or sent ends with status 1, and a sent archive that cannot be removed is said and left for a later run's
/// [`prune`], status 0.
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
    if let Err(error) = fs::remove_file(&path) {
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
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::{ARCHIVE_AGE, Asked, MANIFEST_LIMIT, prune, read_manifest, remove_if_old};

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
}
