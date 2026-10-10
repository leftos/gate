//! The archive of a rolled journal's release lines, its prune, and its place inside the rolling append's lock hold.

use std::cell::RefCell;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gate_state::{Held, JOURNAL_CAP_BYTES, JournalLock, LockError, append_line, archive_rolled_journal};

use crate::journal_write::{NOW, archive_names, fresh_dir, gunzip, lines_of, release_lines, seed_lines, text};
use crate::support::ts;

const RELEASE: &str = r#"{"v":1,"time":"2026-10-03T05:04:39.2910745Z","event":"release","pid":40268,"status":0}"#;
const QUEUE: &str = r#"{"v":1,"time":"2026-10-03T05:04:39.1209178Z","event":"queue","pid":40268}"#;
const TAKE: &str = r#"{"v":1,"time":"2026-10-03T05:04:39.1866333Z","event":"take","pid":40268}"#;
const OTHER_RELEASE: &str = r#"{"time":"2026-10-03T05:04:51.5529399Z","event":"release","pid":58188,"status":3}"#;

/// A state root whose rolled journal holds `lines`.
fn rolled_root(test: &str, lines: &[&str]) -> PathBuf {
    let root = fresh_dir(test);
    let owned: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    fs::write(root.join("events.1.jsonl"), text(&owned)).expect("the rolled journal can be seeded");
    root
}

/// Empty files named `names` in the root's archive folder.
fn seed_archive(root: &Path, names: &[&str]) {
    let dir = root.join("archive");
    fs::create_dir_all(&dir).expect("the archive folder can be made");
    for name in names {
        fs::write(dir.join(name), "").expect("an archive file can be seeded");
    }
}

#[test]
fn archive_holds_only_release_lines() {
    let root = rolled_root("archive_holds_only_release_lines", &[QUEUE, TAKE, RELEASE, OTHER_RELEASE]);

    let notes = archive_rolled_journal(&root, ts(NOW));

    assert_eq!(notes, Vec::<String>::new());
    let names = archive_names(&root);
    assert_eq!(names.len(), 1, "{names:?}");
    assert_eq!(
        gunzip(&root.join("archive").join(&names[0])),
        format!("{RELEASE}\n{OTHER_RELEASE}\n"),
        "the release lines as written, each ending in a line feed"
    );
}

#[test]
fn archive_name_is_the_utc_stamp() {
    let root = rolled_root("archive_name_is_the_utc_stamp", &[RELEASE]);

    let notes = archive_rolled_journal(&root, ts("2026-10-03T05:04:39.9876543Z"));

    assert_eq!(notes, Vec::<String>::new());
    assert_eq!(archive_names(&root), ["events-20261003T050439Z.jsonl.gz"]);
}

#[test]
fn unparseable_release_lines_are_counted_and_skipped() {
    let torn = r#"{"v":1,"event":"release","pid":"#;
    let not_json = r#"not json "event":"release" at all"#;
    let root = rolled_root(
        "unparseable_release_lines_are_counted_and_skipped",
        &[RELEASE, torn, QUEUE, not_json, "{broken"],
    );

    let notes = archive_rolled_journal(&root, ts(NOW));

    assert_eq!(
        notes,
        [format!(
            "archived the release lines of {}; skipped 2 line(s) that do not parse",
            root.join("events.1.jsonl").display()
        )],
        "a broken line without the release text is no release line and is not counted"
    );
    let names = archive_names(&root);
    assert_eq!(gunzip(&root.join("archive").join(&names[0])), format!("{RELEASE}\n"));
}

#[test]
fn archives_older_than_180_days_are_pruned_by_name() {
    let root = rolled_root("archives_older_than_180_days_are_pruned_by_name", &[RELEASE]);
    seed_archive(
        &root,
        &[
            "events-20260406T115959Z.jsonl.gz",
            "events-20260406T120000Z.jsonl.gz",
            "events-20260406T120001Z.jsonl.gz",
        ],
    );
    let kept = root.join("archive").join("events-20260406T120001Z.jsonl.gz");
    let long_ago = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    File::options()
        .write(true)
        .open(&kept)
        .and_then(|file| file.set_modified(long_ago))
        .expect("the kept archive's write time can be set");

    let notes = archive_rolled_journal(&root, ts(NOW));

    assert_eq!(notes, Vec::<String>::new());
    assert_eq!(
        archive_names(&root),
        [
            "events-20260406T120000Z.jsonl.gz",
            "events-20260406T120001Z.jsonl.gz",
            "events-20261003T120000Z.jsonl.gz",
        ],
        "only the archive stamped more than 180 days ago goes; a write time long ago does not count"
    );
}

#[test]
fn temp_files_older_than_a_day_are_pruned_by_name() {
    let root = rolled_root("temp_files_older_than_a_day_are_pruned_by_name", &[RELEASE]);
    seed_archive(
        &root,
        &["events-20261002T115959Z.jsonl.gz.1234.tmp", "events-20261002T120001Z.jsonl.gz.1234.tmp"],
    );

    let notes = archive_rolled_journal(&root, ts(NOW));

    assert_eq!(notes, Vec::<String>::new());
    assert_eq!(
        archive_names(&root),
        ["events-20261002T120001Z.jsonl.gz.1234.tmp", "events-20261003T120000Z.jsonl.gz"]
    );
}

#[test]
fn other_names_are_never_pruned() {
    let root = rolled_root("other_names_are_never_pruned", &[RELEASE]);
    let others = [
        "events-20200101T000000Z.jsonl",
        "events-20200101T000000Z.jsonl.gz.tmp",
        "events-20200101T000000Z.jsonl.gz.12a.tmp",
        "events-20200101T000000.jsonl.gz",
        "events-2020.jsonl.gz",
        "notes.txt",
    ];
    seed_archive(&root, &others);

    let notes = archive_rolled_journal(&root, ts(NOW));

    assert_eq!(notes, Vec::<String>::new());
    let mut expected: Vec<&str> = others.to_vec();
    expected.push("events-20261003T120000Z.jsonl.gz");
    expected.sort_unstable();
    assert_eq!(archive_names(&root), expected);
}

#[test]
fn same_second_archive_fails_without_overwriting() {
    let root = rolled_root("same_second_archive_fails_without_overwriting", &[RELEASE]);
    assert_eq!(archive_rolled_journal(&root, ts(NOW)), Vec::<String>::new());
    let first = root.join("archive").join("events-20261003T120000Z.jsonl.gz");
    let before = fs::read(&first).expect("the first archive can be read");
    fs::write(root.join("events.1.jsonl"), format!("{OTHER_RELEASE}\n")).expect("a second roll can be seeded");

    let notes = archive_rolled_journal(&root, ts("2026-10-03T12:00:00.5Z"));

    assert_eq!(
        fs::read(&first).expect("the first archive can still be read"),
        before,
        "the archive already there is left byte for byte"
    );

    assert_eq!(notes.len(), 1, "{notes:?}");
    let prefix = format!(
        "could not archive the release lines of {} in {}: ",
        root.join("events.1.jsonl").display(),
        root.join("archive").display()
    );
    assert!(notes[0].starts_with(&prefix), "{notes:?}");
    assert_eq!(archive_names(&root), ["events-20261003T120000Z.jsonl.gz"], "no temporary file is left");
    assert_eq!(
        gunzip(&root.join("archive").join("events-20261003T120000Z.jsonl.gz")),
        format!("{RELEASE}\n"),
        "the first archive is kept"
    );
}

/// A journal lock that records each take and release with the number of archives at that moment.
struct RecordingLock {
    root: PathBuf,
    events: RefCell<Vec<String>>,
}

impl RecordingLock {
    fn record(&self, what: &str) {
        let archives = fs::read_dir(self.root.join("archive")).map_or(0, Iterator::count);
        self.events.borrow_mut().push(format!("{what} with {archives} archive(s)"));
    }
}

struct Recorder<'a>(&'a RecordingLock);

impl Drop for Recorder<'_> {
    fn drop(&mut self) {
        self.0.record("release");
    }
}

impl JournalLock for RecordingLock {
    type Guard<'a> = Recorder<'a>;

    fn acquire(&self, _timeout: Duration) -> Result<Held<Recorder<'_>>, LockError> {
        self.record("acquire");
        Ok(Held {
            guard: Recorder(self),
            abandoned: false,
        })
    }
}

#[test]
fn roll_archives_under_the_same_lock_hold() {
    let root = fresh_dir("roll_archives_under_the_same_lock_hold");
    let seed = seed_lines(JOURNAL_CAP_BYTES);
    fs::write(root.join("events.jsonl"), text(&seed)).expect("the journal can be seeded");
    let lock = RecordingLock {
        root: root.clone(),
        events: RefCell::new(Vec::new()),
    };

    let appended = append_line(&root, &lock, RELEASE, ts(NOW));

    assert!(appended.written && appended.notes.is_empty(), "{appended:?}");
    assert_eq!(
        lock.events.into_inner(),
        ["acquire with 0 archive(s)", "release with 1 archive(s)"],
        "one hold: the archive is written after the take and before the release"
    );
    let names = archive_names(&root);
    assert_eq!(
        gunzip(&root.join("archive").join(&names[0])),
        text(&release_lines(&seed)),
        "the archive holds the rolled journal's release lines, not the line appended after the roll"
    );
    assert_eq!(lines_of(&root.join("events.jsonl")), [RELEASE]);
}
