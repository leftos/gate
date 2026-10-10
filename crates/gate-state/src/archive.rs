//! The archive of a rolled journal: the `release` lines of `<root>\events.1.jsonl`, gzipped to
//! `<root>\archive\events-<yyyyMMddTHHmmssZ>.jsonl.gz`, and the prune of archives past keeping. Age is judged by the
//! stamp in a file's name, never by its write time.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

use crate::journal_writer::ROLLED_JOURNAL_FILE;

/// The archive folder's name under the state root.
pub const ARCHIVE_DIR: &str = "archive";
/// The text that marks a journal line as a candidate release line.
const RELEASE_TEXT: &[u8] = br#""event":"release""#;
/// How long an archive is kept.
const ARCHIVE_KEPT: SignedDuration = SignedDuration::from_hours(180 * 24);
/// How long a temporary file a killed archive write left is kept.
const TEMP_KEPT: SignedDuration = SignedDuration::from_hours(24);
/// The stamp format in an archive's name.
const STAMP_FORMAT: &str = "%Y%m%dT%H%M%SZ";

/// Archives the release lines of `root`'s `events.1.jsonl` to `archive\events-<now's UTC stamp>.jsonl.gz`, through
/// `<archive>.<pid>.tmp` hard-linked into place, which fails rather than replace an archive already there, then deletes
/// the archives stamped more than 180 days before `now` and the temporary files stamped more than a day before it. A
/// line holding `"event":"release"` that does not parse is left out and counted. Called under the journal lock, so no
/// other gate writes the same archive name at once.
///
/// Returns what there is to say, each a line the gate prints after `gate: `: the lines left out, why the archive could
/// not be written (the temporary file is then deleted, and nothing is pruned), a temporary file that could not be
/// deleted once the archive was in place, and each file that could not be pruned.
#[must_use]
pub fn archive_rolled_journal(root: &Path, now: Timestamp) -> Vec<String> {
    let rolled = root.join(ROLLED_JOURNAL_FILE);
    let dir = root.join(ARCHIVE_DIR);
    let written = match write_archive(&rolled, &dir, now) {
        Ok(written) => written,
        Err(error) => {
            return vec![format!(
                "could not archive the release lines of {} in {}: {error}",
                rolled.display(),
                dir.display()
            )];
        }
    };
    let mut notes = Vec::new();
    if written.skipped > 0 {
        notes.push(format!(
            "archived the release lines of {}; skipped {} line(s) that do not parse",
            rolled.display(),
            written.skipped
        ));
    }
    if let Some((temp, error)) = written.temp_left {
        notes.push(format!(
            "archived the release lines of {} but could not delete {}: {error}",
            rolled.display(),
            temp.display()
        ));
    }
    notes.extend(prune(&dir, now));
    notes
}

/// What writing an archive did besides the archive itself.
struct Written {
    /// How many candidate lines did not parse.
    skipped: usize,
    /// The temporary file left once the archive was in place, and why it could not be deleted.
    temp_left: Option<(PathBuf, io::Error)>,
}

/// Writes the archive of `rolled` into `dir`.
fn write_archive(rolled: &Path, dir: &Path, now: Timestamp) -> io::Result<Written> {
    fs::create_dir_all(dir)?;
    let bytes = fs::read(rolled)?;
    let (lines, skipped) = release_lines(&bytes);
    let stamp = jiff::fmt::strtime::format(STAMP_FORMAT, now).map_err(io::Error::other)?;
    let target = dir.join(format!("events-{stamp}.jsonl.gz"));
    let temp = dir.join(format!("events-{stamp}.jsonl.gz.{}.tmp", std::process::id()));
    // A hard link fails when the target exists, on Windows and Linux alike, so an archive already there is never replaced.
    let linked = write_gzip(&temp, &lines).and_then(|()| fs::hard_link(&temp, &target));
    if let Err(error) = linked {
        // The write's own failure is the one to report; a temporary file left behind is pruned once it is a day old.
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    let temp_left = fs::remove_file(&temp).err().map(|error| (temp, error));
    Ok(Written { skipped, temp_left })
}

/// The `event` of a journal line, all the archive reads of it; every other key is ignored.
#[derive(Deserialize)]
struct EventOnly {
    #[serde(default)]
    event: Option<String>,
}

/// The lines of `bytes` holding `"event":"release"` that parse as an object whose `event` is `release`, as written,
/// and how many of the lines holding that text did not parse.
fn release_lines(bytes: &[u8]) -> (Vec<&[u8]>, usize) {
    let mut kept = Vec::new();
    let mut skipped = 0;
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !line.windows(RELEASE_TEXT.len()).any(|window| window == RELEASE_TEXT) {
            continue;
        }
        match serde_json::from_slice::<EventOnly>(line) {
            Ok(parsed) if parsed.event.as_deref() == Some("release") => kept.push(line),
            Ok(_) => {}
            Err(_) => skipped += 1,
        }
    }
    (kept, skipped)
}

fn write_gzip(path: &Path, lines: &[&[u8]]) -> io::Result<()> {
    let mut encoder = GzEncoder::new(File::create(path)?, Compression::default());
    for line in lines {
        encoder.write_all(line)?;
        encoder.write_all(b"\n")?;
    }
    encoder.finish()?;
    Ok(())
}

/// Deletes the files of `dir` past keeping, each on its own; returns a line for each that could not be deleted.
fn prune(dir: &Path, now: Timestamp) -> Vec<String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => return vec![format!("could not list the archive folder {}: {error}", dir.display())],
    };
    let mut failures = Vec::new();
    for entry in entries {
        let name = match entry {
            Ok(entry) => entry.file_name().to_string_lossy().into_owned(),
            Err(error) => {
                failures.push(format!("could not list the archive folder {}: {error}", dir.display()));
                continue;
            }
        };
        if !expired(&name, now) {
            continue;
        }
        if let Err(error) = fs::remove_file(dir.join(&name)) {
            failures.push(format!("could not delete old archive {name}: {error}"));
        }
    }
    failures
}

/// Whether a file of the archive folder is past keeping, by its name alone: `events-<stamp>.jsonl.gz` stamped more than
/// 180 days before `now`, or `events-<stamp>.jsonl.gz.<pid>.tmp` stamped more than a day before it. Any other name never
/// is.
fn expired(name: &str, now: Timestamp) -> bool {
    let Some(rest) = name.strip_prefix("events-") else {
        return false;
    };
    let Some((stamp, tail)) = rest.split_at_checked(STAMP_LENGTH) else {
        return false;
    };
    let kept = match tail.strip_prefix(".jsonl.gz") {
        Some("") => ARCHIVE_KEPT,
        Some(temp) if is_temp_suffix(temp) => TEMP_KEPT,
        _ => return false,
    };
    stamp_time(stamp).is_some_and(|when| when < now - kept)
}

/// The length of a `yyyyMMddTHHmmssZ` stamp.
const STAMP_LENGTH: usize = 16;

/// Whether `suffix` is `.<digits>.tmp`.
fn is_temp_suffix(suffix: &str) -> bool {
    suffix
        .strip_prefix('.')
        .and_then(|rest| rest.strip_suffix(".tmp"))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The moment a `yyyyMMddTHHmmssZ` stamp names, or `None` when it is not one.
fn stamp_time(stamp: &str) -> Option<Timestamp> {
    let shape = stamp.bytes().enumerate().all(|(index, byte)| match index {
        8 => byte == b'T',
        15 => byte == b'Z',
        _ => byte.is_ascii_digit(),
    });
    if !shape {
        return None;
    }
    let civil = DateTime::strptime("%Y%m%dT%H%M%S", &stamp[..15]).ok()?;
    civil.to_zoned(TimeZone::UTC).ok().map(|zoned| zoned.timestamp())
}
