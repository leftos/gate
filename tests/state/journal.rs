//! The gate journal read into runs and per-command held times, and the cache that reads only what was appended.

use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use gate_state::{CommandSummary, History, HistoryRun, JournalCache, RunState, normalize_command};
use jiff::SignedDuration;
use serde_json::{Value, json};

use crate::support::ts;

/// The moment every history describes.
const NOW: &str = "2026-10-03T12:00:00Z";
/// A gate process's start time.
const START: &str = "2026-10-03T10:00:00.1234567Z";
/// The start time of another process that reused a pid.
const OTHER_START: &str = "2026-10-03T11:00:00.7654321Z";
/// The command most tests run.
const COMMAND: &str = "cargo test --release";

fn not_live(_pid: u32, _process_start: &str) -> bool {
    false
}

/// `minutes` before [`NOW`], as the journal writes a time.
fn ago(minutes: i64) -> String {
    (ts(NOW) - SignedDuration::from_mins(minutes)).to_string()
}

/// One gate process, and the journal lines it writes.
struct Gate<'a> {
    pid: u32,
    start: &'a str,
    command: &'a str,
    cwd: &'a str,
}

impl<'a> Gate<'a> {
    fn new(pid: u32, start: &'a str, command: &'a str) -> Self {
        Self {
            pid,
            start,
            command,
            cwd: r"C:\work\repo",
        }
    }

    fn line(&self, event: &str, time: &str, fields: Value) -> String {
        let mut line = json!({
            "time": time, "event": event, "pid": self.pid, "processStart": self.start, "kind": "heavy", "cwd": self.cwd,
            "command": self.command,
        });
        let extra = match fields {
            Value::Object(extra) => Some(extra),
            _ => None,
        };
        line.as_object_mut()
            .expect("a line is an object")
            .extend(extra.expect("an event's fields are an object"));
        line.to_string()
    }

    fn queue(&self, time: &str) -> String {
        self.line("queue", time, json!({"log": r"C:\work\repo\.tmp\run.log"}))
    }

    fn take(&self, time: &str, waited: f64) -> String {
        self.line("take", time, json!({"slots": [r"Local\gate-slot-1"], "waitedSeconds": waited}))
    }

    fn release(&self, time: &str, held: f64, status: Value) -> String {
        let mut fields = json!({"slots": [r"Local\gate-slot-1"], "heldSeconds": held});
        fields["status"] = status;
        self.line("release", time, fields)
    }

    fn abandon(&self, time: &str, waited: f64) -> String {
        self.line("abandon", time, json!({"waitedSeconds": waited}))
    }

    /// A whole run that queued `minutes` before [`NOW`], took its slot a minute later and released it with `status`.
    fn run(&self, minutes: i64, held: f64, status: Value) -> Vec<String> {
        vec![
            self.queue(&ago(minutes)),
            self.take(&ago(minutes - 1), 0.5),
            self.release(&ago(minutes - 2), held, status),
        ]
    }
}

/// A fresh folder for a test's journal.
fn journal_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gate-state-journal").join(test);
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
    }
    fs::create_dir_all(&dir).expect("the test's folder can be made");
    dir
}

/// The lines, each ending in a line feed.
fn text(lines: &[String]) -> String {
    lines.iter().flat_map(|line| [line.as_str(), "\n"]).collect()
}

fn append(path: &Path, text: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("the journal can be opened");
    file.write_all(text.as_bytes()).expect("the journal can be appended to");
}

/// The history of `dir`'s unprefixed journal as a fresh cache reads it, no gate live.
fn read_fresh(dir: &Path) -> History {
    JournalCache::default().read(dir, "", ts(NOW), not_live).expect("the journal can be read")
}

/// The history of a journal holding just `lines`.
fn history_of(test: &str, lines: &[String]) -> History {
    let dir = journal_dir(test);
    fs::write(dir.join("events.jsonl"), text(lines)).expect("the journal can be written");
    read_fresh(&dir)
}

fn states(history: &History) -> Vec<RunState> {
    history.runs.iter().map(|run| run.state).collect()
}

fn commands(history: &History) -> Vec<&str> {
    history.runs.iter().map(|run| run.command.as_str()).collect()
}

#[test]
fn normalize_command_examples() {
    let examples = [
        (
            "cargo test --manifest-path X:/dev/ops-tower.wt/ops-61/Cargo.toml --release --test http pair",
            "cargo test --release --test http pair",
        ),
        (
            "cargo clippy --manifest-path X:/dev/ops-tower.wt/ops-18/Cargo.toml --workspace --all-targets --all-features -- -D warnings",
            "cargo clippy --workspace --all-targets --all-features -- -D warnings",
        ),
        ("cargo fmt --all --manifest-path X:/dev/ops-tower.wt/ops-18/Cargo.toml", "cargo fmt --all"),
        ("uv run --directory X:/dev/rigcheck.wt/m4i-a-mcp-config ty check", "uv run ty check"),
        ("git commit -F .tmp/msg-sync-gate-launcher.txt", "git commit -F \u{2026}"),
        (
            "pwsh -NoProfile -File C:/Users/someone/.claude.wt/cc32/tools/gate/gate.selftest.ps1 -Case Test-JournalRolloverBlocked,Test-JournalTornLine",
            "pwsh -NoProfile -File tools/gate/gate.selftest.ps1 -Case Test-JournalRolloverBlocked,Test-JournalTornLine",
        ),
    ];
    for (input, normalized) in examples {
        assert_eq!(normalize_command(input), normalized, "input {input}");
    }
}

#[test]
fn normalize_command_non_examples() {
    for unchanged in [
        "uv run --directory tools/linear ty check",
        "cargo test --release --test briefing",
        r#"uv run pytest tests/test_skill_injection_rules.py tests/test_permission_rules.py -k "not_allowed or permission""#,
        "cargo test --workspace --release",
        "git push origin main",
    ] {
        assert_eq!(normalize_command(unchanged), unchanged);
    }
}

#[test]
fn runs_pair_queue_take_release_by_pid_and_process_start() {
    let first = Gate::new(100, START, "cargo build");
    let second = Gate::new(200, START, "cargo test");
    let lines = [
        first.queue(&ago(30)),
        second.queue(&ago(29)),
        first.take(&ago(28), 2.0),
        second.take(&ago(27), 1.0),
        second.release(&ago(26), 60.0, json!(4)),
        first.release(&ago(25), 120.0, json!(0)),
    ];
    let history = history_of("runs_pair_queue_take_release_by_pid_and_process_start", &lines);
    assert_eq!(
        history.runs,
        [
            HistoryRun {
                command: "cargo test".to_owned(),
                kind: "heavy".to_owned(),
                slot: Some(1),
                state: RunState::Failed,
                queued: Some(ago(29)),
                started: Some(ago(27)),
                waited_seconds: Some(1.0),
                held_seconds: Some(60.0),
                status: Some(4),
            },
            HistoryRun {
                command: "cargo build".to_owned(),
                kind: "heavy".to_owned(),
                slot: Some(1),
                state: RunState::Ok,
                queued: Some(ago(30)),
                started: Some(ago(28)),
                waited_seconds: Some(2.0),
                held_seconds: Some(120.0),
                status: Some(0),
            },
        ]
    );
}

#[test]
fn reused_pid_with_a_new_process_start_is_a_new_run() {
    let mut lines = Gate::new(100, START, COMMAND).run(60, 10.0, json!(0));
    lines.extend(Gate::new(100, OTHER_START, COMMAND).run(30, 20.0, json!(1)));
    let history = history_of("reused_pid_with_a_new_process_start_is_a_new_run", &lines);
    assert_eq!(states(&history), [RunState::Failed, RunState::Ok]);
    assert_eq!(history.runs[0].queued.as_deref(), Some(ago(30).as_str()));
    assert_eq!(history.runs[1].queued.as_deref(), Some(ago(60).as_str()));
}

#[test]
fn failed_and_abandoned_runs_are_listed_but_not_counted() {
    let mut lines = Gate::new(1, START, COMMAND).run(40, 10.0, json!(0));
    lines.extend(Gate::new(2, START, COMMAND).run(30, 20.0, json!(101)));
    lines.extend(Gate::new(3, START, COMMAND).run(20, 30.0, Value::Null));
    let abandoned = Gate::new(4, START, COMMAND);
    lines.extend([abandoned.queue(&ago(10)), abandoned.abandon(&ago(9), 60.0)]);
    let history = history_of("failed_and_abandoned_runs_are_listed_but_not_counted", &lines);

    assert_eq!(states(&history), [RunState::Abandoned, RunState::Failed, RunState::Failed, RunState::Ok]);
    assert_eq!(history.runs[0].waited_seconds, Some(60.0));
    assert_eq!(history.runs[0].slot, None);
    assert_eq!(history.runs[0].started, None);
    assert_eq!(history.runs[1].status, None, "a gate that failed before its command ran has no status");
    assert_eq!(
        history.summary,
        [CommandSummary {
            command: COMMAND.to_owned(),
            count: 1,
            p50_seconds: 10.0,
            p90_seconds: 10.0,
            max_seconds: 10.0,
        }]
    );
}

#[test]
fn open_run_is_running_when_live_else_unrecorded() {
    let live = Gate::new(1, START, COMMAND);
    let gone = Gate::new(2, START, COMMAND);
    let queued = Gate::new(3, OTHER_START, COMMAND);
    let lines = [
        live.queue(&ago(30)),
        live.take(&ago(29), 1.0),
        gone.queue(&ago(20)),
        gone.take(&ago(19), 1.0),
        queued.queue(&ago(10)),
    ];
    let dir = journal_dir("open_run_is_running_when_live_else_unrecorded");
    fs::write(dir.join("events.jsonl"), text(&lines)).expect("the journal can be written");
    let is_live = |pid: u32, process_start: &str| (pid, process_start) == (1, START) || (pid, process_start) == (3, OTHER_START);
    let history = JournalCache::default().read(&dir, "", ts(NOW), is_live).expect("the journal can be read");

    assert_eq!(states(&history), [RunState::Running, RunState::Unrecorded, RunState::Running]);
    assert_eq!(history.runs[0].started, None, "a run still queued has not started");
    assert_eq!(history.runs[2].held_seconds, None);
    assert!(history.summary.is_empty(), "open runs are not counted: {:?}", history.summary);
}

#[test]
fn selftest_and_covered_lines_are_ignored() {
    let mut selftest = Gate::new(1, START, COMMAND);
    selftest.cwd = r"C:\Users\me\AppData\Local\Temp\gate-selftest-12\work";
    let covered = Gate::new(2, START, COMMAND);
    let parent = Gate::new(3, START, COMMAND);
    let mut lines = selftest.run(30, 5.0, json!(0));
    lines.extend([
        covered.line("covered", &ago(20), json!({"parentPid": 9})),
        parent.line("parent", &ago(10), json!({"parentPid": 9})),
    ]);
    let history = history_of("selftest_and_covered_lines_are_ignored", &lines);
    assert_eq!(history, History::default());
}

#[test]
fn torn_and_unparseable_lines_are_skipped() {
    let gate = Gate::new(1, START, COMMAND);
    let mut lines = gate.run(30, 5.0, json!(0));
    let torn = gate.release(&ago(5), 1.0, json!(1));
    lines.insert(1, torn[..torn.len() / 2].to_owned());
    lines.insert(2, "not json".to_owned());
    lines.insert(3, "{}".to_owned());
    lines.insert(4, gate.line("release", "yesterday", json!({"heldSeconds": 1.0, "status": 1})));
    let history = history_of("torn_and_unparseable_lines_are_skipped", &lines);
    assert_eq!(states(&history), [RunState::Ok]);
    assert_eq!(history.runs[0].held_seconds, Some(5.0));
}

#[test]
fn summary_counts_only_runs_taken_in_the_last_7_days() {
    let week = 7 * 24 * 60;
    let mut lines = Gate::new(1, START, COMMAND).run(week + 2, 100.0, json!(0));
    lines.extend(Gate::new(2, START, COMMAND).run(week, 30.0, json!(0)));
    let history = history_of("summary_counts_only_runs_taken_in_the_last_7_days", &lines);
    assert_eq!(history.runs.len(), 2, "the older run is still among the newest 50");
    assert_eq!(
        history.summary,
        [CommandSummary {
            command: COMMAND.to_owned(),
            count: 1,
            p50_seconds: 30.0,
            p90_seconds: 30.0,
            max_seconds: 30.0,
        }],
        "the run taken a minute inside the week counts, the one taken a minute before it does not"
    );
}

#[test]
fn percentiles_use_nearest_rank() {
    let mut lines = Vec::new();
    for (index, held) in [7, 3, 10, 1, 9, 5, 2, 8, 4, 6].into_iter().enumerate() {
        let index = u32::try_from(index).expect("ten runs");
        lines.extend(Gate::new(10 + index, START, COMMAND).run(100 - i64::from(index), f64::from(held), json!(0)));
    }
    lines.extend(Gate::new(50, START, "cargo build").run(5, 1.0, json!(0)));
    let history = history_of("percentiles_use_nearest_rank", &lines);
    assert_eq!(
        history.summary,
        [
            CommandSummary {
                command: COMMAND.to_owned(),
                count: 10,
                p50_seconds: 5.0,
                p90_seconds: 9.0,
                max_seconds: 10.0,
            },
            CommandSummary {
                command: "cargo build".to_owned(),
                count: 1,
                p50_seconds: 1.0,
                p90_seconds: 1.0,
                max_seconds: 1.0,
            },
        ]
    );
}

#[test]
fn recent_runs_cap_at_50_newest_first() {
    let mut lines = Vec::new();
    for pid in 1..=60_u32 {
        lines.extend(Gate::new(pid, START, COMMAND).run(200 - i64::from(pid), 1.0, json!(0)));
    }
    let history = history_of("recent_runs_cap_at_50_newest_first", &lines);
    assert_eq!(history.runs.len(), 50);
    assert_eq!(
        history.runs[0].queued.as_deref(),
        Some(ago(140).as_str()),
        "the newest, pid 60, comes first"
    );
    assert_eq!(history.runs[49].queued.as_deref(), Some(ago(189).as_str()), "pid 11 is the 50th newest");
    assert_eq!(
        history.summary[0].count, 60,
        "the summary counts every run of the week, not only the listed ones"
    );
}

#[test]
fn cache_parses_only_appended_lines() {
    let dir = journal_dir("cache_parses_only_appended_lines");
    let journal = dir.join("events.jsonl");
    let old = text(&Gate::new(1, START, "cargo build").run(30, 5.0, json!(0)));
    fs::write(&journal, &old).expect("the journal can be written");
    let mut cache = JournalCache::default();
    let first = cache.read(&dir, "", ts(NOW), not_live).expect("the journal can be read");
    assert_eq!(commands(&first), ["cargo build"]);

    // Overwrite the bytes already read in place with another valid run of the same length, keeping the file: a cache
    // that parsed them again would list `cargo doc`.
    let other = Gate::new(9, START, "cargo doc");
    let replacement = |padding: usize| {
        let queue = other.line("queue", &ago(30), json!({ "log": "x".repeat(padding) }));
        text(&[queue, other.take(&ago(29), 0.5), other.release(&ago(28), 5.0, json!(0))])
    };
    let padding = old
        .len()
        .checked_sub(replacement(0).len())
        .expect("the other run is no longer than the first");
    let replacement = replacement(padding);
    assert_eq!(replacement.len(), old.len());
    let mut file = OpenOptions::new().write(true).open(&journal).expect("the journal can be opened");
    file.seek(SeekFrom::Start(0)).expect("the journal can be rewound");
    file.write_all(replacement.as_bytes()).expect("the journal can be overwritten");
    drop(file);
    append(&journal, &text(&Gate::new(2, START, COMMAND).run(10, 7.0, json!(0))));

    let second = cache.read(&dir, "", ts(NOW), not_live).expect("the journal can be read again");
    assert_eq!(commands(&second), [COMMAND, "cargo build"], "only the appended run is new");
    assert_eq!(states(&second), [RunState::Ok, RunState::Ok]);
}

#[test]
fn cache_waits_for_a_partial_last_line() {
    let dir = journal_dir("cache_waits_for_a_partial_last_line");
    let journal = dir.join("events.jsonl");
    let gate = Gate::new(1, START, COMMAND);
    let release = gate.release(&ago(10), 5.0, json!(0));
    let (head, tail) = release.split_at(release.len() / 2);
    fs::write(&journal, format!("{}{head}", text(&[gate.queue(&ago(20)), gate.take(&ago(19), 1.0)]))).expect("the journal can be written");
    let mut cache = JournalCache::default();

    let partial = cache.read(&dir, "", ts(NOW), not_live).expect("the journal can be read");
    assert_eq!(states(&partial), [RunState::Unrecorded], "the release is not read until its line ends");

    append(&journal, &format!("{tail}\n"));
    let complete = cache.read(&dir, "", ts(NOW), not_live).expect("the journal can be read again");
    assert_eq!(states(&complete), [RunState::Ok]);
    assert_eq!(complete.runs[0].held_seconds, Some(5.0));
    assert_eq!(complete, read_fresh(&dir), "a cold read agrees");
}

#[test]
fn cache_rereads_after_a_roll() {
    let dir = journal_dir("cache_rereads_after_a_roll");
    let journal = dir.join("events.jsonl");
    let mut old = Gate::new(1, START, "cargo build").run(60, 5.0, json!(0));
    old.extend(Gate::new(2, START, "cargo clippy").run(50, 6.0, json!(0)));
    fs::write(&journal, text(&old)).expect("the journal can be written");
    let mut cache = JournalCache::default();
    let before = cache.read(&dir, "", ts(NOW), not_live).expect("the journal can be read");
    assert_eq!(commands(&before), ["cargo clippy", "cargo build"]);

    fs::rename(&journal, dir.join("events.1.jsonl")).expect("the journal can be rolled");
    fs::write(&journal, text(&Gate::new(3, START, COMMAND).run(10, 7.0, json!(0)))).expect("the new journal can be written");

    let after = cache.read(&dir, "", ts(NOW), not_live).expect("the rolled journal can be read");
    assert_eq!(commands(&after), [COMMAND, "cargo clippy", "cargo build"]);
    assert_eq!(after, read_fresh(&dir), "a cold read agrees");

    // A second roll replaces the rolled file, and the new journal is longer than the bytes read so far, so only the
    // rolled file's change (and the creation time, when tunnelling does not keep it) tells the cache.
    let offset = fs::metadata(&journal).expect("the journal is there").len();
    fs::rename(&journal, dir.join("events.1.jsonl")).expect("the journal can be rolled again");
    let mut newer = Gate::new(4, START, "cargo doc").run(8, 1.0, json!(0));
    newer.extend(Gate::new(5, START, "cargo fmt").run(6, 2.0, json!(0)));
    fs::write(&journal, text(&newer)).expect("the newer journal can be written");
    assert!(fs::metadata(&journal).expect("the journal is there").len() > offset);

    let again = cache.read(&dir, "", ts(NOW), not_live).expect("the twice-rolled journal can be read");
    assert_eq!(commands(&again), ["cargo fmt", "cargo doc", COMMAND]);
    assert_eq!(again, read_fresh(&dir), "a cold read agrees");
}

#[test]
fn cache_rereads_when_the_rolled_file_changes_even_if_the_journal_grew_past_the_offset() {
    let dir = journal_dir("cache_rereads_when_the_rolled_file_changes_even_if_the_journal_grew_past_the_offset");
    let journal = dir.join("events.jsonl");
    let old = text(&Gate::new(1, START, "cargo build").run(60, 5.0, json!(0)));
    fs::write(&journal, &old).expect("the journal can be written");
    let mut cache = JournalCache::default();
    let before = cache.read(&dir, "", ts(NOW), not_live).expect("the journal can be read");
    assert_eq!(commands(&before), ["cargo build"]);

    // The roll as a tunnelled creation time leaves it: the old content moves to the rolled file, and the journal is
    // rewritten in place (so it keeps its creation time) with more bytes than were read, the first run starting at 0.
    fs::write(dir.join("events.1.jsonl"), &old).expect("the rolled journal can be written");
    let mut new = Gate::new(2, START, "cargo doc").run(30, 1.0, json!(0));
    new.extend(Gate::new(3, START, COMMAND).run(20, 2.0, json!(0)));
    let new = text(&new);
    assert!(new.len() > old.len(), "the length check alone would not see the roll");
    fs::write(&journal, &new).expect("the new journal can be written");

    let after = cache.read(&dir, "", ts(NOW), not_live).expect("the rolled journal can be read");
    assert_eq!(
        commands(&after),
        [COMMAND, "cargo doc", "cargo build"],
        "the new journal is read from its start"
    );
    assert_eq!(after, read_fresh(&dir), "a cold read agrees");
}
