//! The gate journal read into runs and per-command held times, and the cache that reads only what was appended.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use gate_state::{CommandSummary, HeavyLight, History, HistoryRun, JournalCache, MachineSnapshot, OtherClaim, RunState, normalize_command};
use jiff::SignedDuration;
use proptest::prelude::*;
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

/// The repository a test folder names: its last segment, `None` for an empty one.
fn last_segment(cwd: &str) -> Option<String> {
    cwd.rsplit(['\\', '/']).next().filter(|segment| !segment.is_empty()).map(str::to_owned)
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
    JournalCache::default()
        .read(dir, "", ts(NOW), not_live, last_segment)
        .expect("the journal can be read")
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

/// A run's command, state, verdict, wall seconds and adjusted seconds.
type RunFacts<'a> = (&'a str, RunState, Option<&'a str>, Option<f64>, Option<f64>);

/// `line` with `"v": v` put first, as the Rust gate writes it.
fn versioned(line: &str, v: u64) -> String {
    let fields: serde_json::Map<String, Value> = serde_json::from_str(line).expect("a test line is a JSON object");
    let mut versioned = serde_json::Map::new();
    versioned.insert("v".to_owned(), json!(v));
    versioned.extend(fields);
    Value::Object(versioned).to_string()
}

/// `line` with `fields` added after its own.
fn with_fields(line: &str, fields: &Value) -> String {
    let mut value: Value = serde_json::from_str(line).expect("a test line is JSON");
    let extra = fields.as_object().expect("the fields are an object").clone();
    value.as_object_mut().expect("a test line is an object").extend(extra);
    value.to_string()
}

#[test]
fn release_with_and_without_run_facts_parses() {
    let facts = Gate::new(1, START, "cargo build");
    let bare = Gate::new(2, START, "cargo test");
    let early = Gate::new(3, START, "cargo check");
    let lines = vec![
        versioned(&facts.queue(&ago(30)), 1),
        versioned(&facts.take(&ago(29), 0.5), 1),
        versioned(
            &with_fields(
                &facts.release(&ago(28), 1.5, json!(0)),
                &json!({"wallSeconds": 1.25, "adjustedSeconds": 1.125, "verdict": "passed"}),
            ),
            1,
        ),
        bare.queue(&ago(20)),
        bare.take(&ago(19), 0.5),
        bare.release(&ago(18), 2.0, json!(3)),
        versioned(&early.queue(&ago(10)), 1),
        versioned(&early.take(&ago(9), 0.5), 1),
        versioned(
            &with_fields(
                &early.release(&ago(8), 0.25, Value::Null),
                &json!({"wallSeconds": null, "adjustedSeconds": null, "verdict": "unknown"}),
            ),
            1,
        ),
    ];

    let history = history_of("release_with_and_without_run_facts_parses", &lines);

    let facts_of: Vec<RunFacts<'_>> = history
        .runs
        .iter()
        .map(|run| {
            (
                run.command.as_str(),
                run.state,
                run.verdict.as_deref(),
                run.wall_seconds,
                run.adjusted_seconds,
            )
        })
        .collect();
    assert_eq!(
        facts_of,
        [
            ("cargo check", RunState::Failed, Some("unknown"), None, None),
            ("cargo test", RunState::Failed, None, None, None),
            ("cargo build", RunState::Ok, Some("passed"), Some(1.25), Some(1.125)),
        ]
    );
}

#[test]
fn reserved_release_fields_are_accepted() {
    let gate = Gate::new(1, START, COMMAND);
    let reserved = json!({
        "wallSeconds": 3.5, "adjustedSeconds": 2.0, "verdict": "passed", "avgCores": 3.25, "peakCores": 8.0, "freeCores": 12.5,
    });
    let lines = vec![
        versioned(&gate.queue(&ago(30)), 1),
        versioned(&gate.take(&ago(29), 0.5), 1),
        versioned(&with_fields(&gate.release(&ago(28), 4.0, json!(0)), &reserved), 1),
    ];

    let history = history_of("reserved_release_fields_are_accepted", &lines);

    assert_eq!(states(&history), [RunState::Ok]);
    assert_eq!((history.runs[0].held_seconds, history.runs[0].status), (Some(4.0), Some(0)));
}

#[test]
fn reserved_take_fields_are_accepted() {
    let gate = Gate::new(1, START, COMMAND);
    let reserved = json!({
        "abandoned": [r"Local\gate-slot-1"], "counts": {"heavy": 4, "light": 7}, "otherHeld": [r"Local\gate-light-slot-2"],
        "otherClaims": [], "snapshotError": "the process snapshot failed",
    });
    let lines = vec![
        versioned(&gate.queue(&ago(30)), 1),
        versioned(&with_fields(&gate.take(&ago(29), 0.5), &reserved), 1),
    ];

    let history = history_of("reserved_take_fields_are_accepted", &lines);

    assert_eq!(states(&history), [RunState::Unrecorded]);
    let run = &history.runs[0];
    assert_eq!(
        (run.started.as_deref(), run.slot, run.waited_seconds),
        (Some(ago(29).as_str()), Some(1), Some(0.5))
    );
}

#[test]
fn a_no_slot_run_folds_to_a_finished_run_with_no_slot() {
    let gate = Gate::new(1, START, COMMAND);
    let none = json!({"kind": "none"});
    let none_slots = json!({"kind": "none", "slots": []});
    let lines = vec![
        versioned(&with_fields(&gate.queue(&ago(30)), &none), 1),
        versioned(&with_fields(&gate.take(&ago(29), 0.0), &none_slots), 1),
        versioned(
            &with_fields(
                &gate.release(&ago(28), 1.0, json!(0)),
                &json!({"kind": "none", "slots": [], "verdict": "passed"}),
            ),
            1,
        ),
    ];

    let history = history_of("a_no_slot_run_folds_to_a_finished_run_with_no_slot", &lines);

    assert_eq!(states(&history), [RunState::Ok]);
    let run = &history.runs[0];
    assert_eq!(
        (run.kind.as_str(), run.started.as_deref(), run.slot, run.slots.len()),
        ("none", Some(ago(29).as_str()), None, 0)
    );
}

#[test]
fn a_take_naming_its_parent_is_accepted_and_runs_on_its_own_slot() {
    let gate = Gate::new(1, START, COMMAND);
    let lines = vec![
        versioned(&gate.queue(&ago(30)), 1),
        versioned(&with_fields(&gate.take(&ago(29), 0.5), &json!({"parentPid": 4242})), 1),
    ];

    let history = history_of("a_take_naming_its_parent_is_accepted_and_runs_on_its_own_slot", &lines);

    assert_eq!(states(&history), [RunState::Unrecorded]);
    let run = &history.runs[0];
    assert_eq!(
        (run.started.as_deref(), run.slot, run.parent_pid),
        (Some(ago(29).as_str()), Some(1), None),
        "a take's parentPid does not put the run on that parent's slot"
    );
}

#[test]
fn line_with_unknown_v_is_skipped() {
    let future = Gate::new(1, START, "cargo build");
    let current = Gate::new(2, START, COMMAND);
    let mut lines: Vec<String> = future.run(30, 1.0, json!(0)).iter().map(|line| versioned(line, 2)).collect();
    lines.extend(current.run(20, 1.0, json!(0)).iter().map(|line| versioned(line, 1)));

    let history = history_of("line_with_unknown_v_is_skipped", &lines);

    assert_eq!(commands(&history), [COMMAND], "a line of a format this reader does not know is skipped");
}

#[test]
fn line_without_v_is_read_as_powershell_format() {
    let gate = Gate::new(1, START, COMMAND);
    let with_facts = Gate::new(2, START, "cargo build");
    let mut lines = gate.run(30, 2.0, json!(0));
    lines.extend([
        with_facts.queue(&ago(20)),
        with_facts.take(&ago(19), 0.5),
        with_fields(
            &with_facts.release(&ago(18), 3.25, json!(3)),
            &json!({"wallSeconds": 3.2, "adjustedSeconds": 2.9, "verdict": "failed"}),
        ),
    ]);

    let history = history_of("line_without_v_is_read_as_powershell_format", &lines);

    assert_eq!(states(&history), [RunState::Failed, RunState::Ok]);
    let facts = &history.runs[0];
    assert_eq!(
        (facts.held_seconds, facts.verdict.as_deref(), facts.wall_seconds, facts.adjusted_seconds),
        (Some(3.25), Some("failed"), Some(3.2), Some(2.9)),
        "a PowerShell release that carries the run's facts, wall seconds to one decimal"
    );
    let bare = &history.runs[1];
    assert_eq!(
        (bare.held_seconds, bare.verdict.as_deref(), bare.wall_seconds),
        (Some(2.0), None, None),
        "a release line without them"
    );
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
            concat!(
                "pwsh -NoProfile -File C:/Users/someone/.claude.wt/cc32/tools/gate/gate.selftest.ps1 ",
                "-Case Test-JournalRolloverBlocked,Test-JournalTornLine",
            ),
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

/// A word quoted as the C runtime reads it back: the quotes and backslash escapes of `split_command_line`, the word
/// rules on the unquoted word, and the re-quoting on output.
#[test]
fn normalize_command_quoted_words() {
    let cases = [
        (r#"cmd /c echo "x\"y" .tmp/out.txt"#, r#"cmd /c echo "x\"y" …"#),
        (
            r#"cargo test --manifest-path "X:/dev/my repo.wt/a/Cargo.toml" --release"#,
            "cargo test --release",
        ),
        (r#"git commit -F ".tmp/my msg.txt""#, r"git commit -F …"),
        (r#"echo "a\\\"b" .tmp/x"#, r#"echo "a\\\"b" …"#),
        (r#"tool "" "X:/dev/r.wt/a/b""#, r#"tool "" b"#),
        (r#"tool "C:\dir with space\\" .tmp/x"#, r#"tool "C:\dir with space\\" …"#),
        (r#"cargo test "--manifest-path" X:/dev/r.wt/a/Cargo.toml"#, "cargo test"),
        (r#"tool "X:/dev/my repo.wt/a/sub dir/f.txt""#, r#"tool "sub dir/f.txt""#),
        (r#"tool "" .tmp/x"#, r#"tool "" …"#),
        (
            r#"uv run pytest tests/test_skill_injection_rules.py tests/test_permission_rules.py -k "not_allowed or permission""#,
            r#"uv run pytest tests/test_skill_injection_rules.py tests/test_permission_rules.py -k "not_allowed or permission""#,
        ),
        (r#"tool "a""b""#, r#"tool "a\"b""#),
        ("tool\ta", "tool a"),
        ("  tool   a  ", "tool a"),
        (r#"echo "abc"#, "echo abc"),
        (r#"a"b c"d"#, r#""ab cd""#),
    ];
    for (input, normalized) in cases {
        assert_eq!(normalize_command(input), normalized, "input {input}");
    }
}

/// The flags whose next word names the tree a command runs in.
const TREE_FLAGS: [&str; 4] = ["--manifest-path", "--directory", "--dir", "-C"];

/// Whether a word rule rewrites `word` or drops it: an absolute path, a `.tmp/` or `.tmp\` path, or a tree flag.
fn is_rewritten(word: &str) -> bool {
    let bytes = word.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
    drive
        || word.starts_with('/')
        || word.starts_with(r"\\")
        || word.starts_with('~')
        || word.starts_with(".tmp/")
        || word.starts_with(r".tmp\")
        || TREE_FLAGS.contains(&word)
}

/// A word no rule rewrites: a string of separators, quotes, backslash runs, whitespace and path punctuation that is
/// not absolute, not a `.tmp` path and not a tree flag.
fn rewrite_free_word() -> impl Strategy<Value = String> {
    let character = prop_oneof![
        Just('x'),
        Just('a'),
        Just(' '),
        Just('\t'),
        Just('\n'),
        Just('"'),
        Just('\\'),
        Just('\u{a0}'),
        Just('\u{85}'),
        Just('\u{3000}'),
        Just('\u{200b}'),
        Just('\u{feff}'),
        Just('.'),
        Just('/'),
        Just('-'),
    ];
    prop::collection::vec(character, 0..6)
        .prop_map(|characters| characters.into_iter().collect::<String>())
        .prop_filter("no word rule rewrites it", |word| !is_rewritten(word))
}

proptest! {
    // Splitting a written command text gives the words back: the identity holds for words no rule rewrites, so it
    // round-trips through both the quoter and the splitter.
    #[test]
    fn normalize_split_round_trips(words in prop::collection::vec(rewrite_free_word(), 0..6)) {
        let args: Vec<OsString> = words.into_iter().map(OsString::from).collect();
        let text = gate_args::command_text(&args);
        prop_assert_eq!(normalize_command(&text), text);
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
                cwd: Some(r"C:\work\repo".to_owned()),
                repo: Some("repo".to_owned()),
                kind: "heavy".to_owned(),
                slot: Some(1),
                slots: vec![r"Local\gate-slot-1".to_owned()],
                machine: None,
                remote: false,
                host: None,
                fallback: None,
                pid: 200,
                process_start: START.to_owned(),
                parent_pid: None,
                state: RunState::Failed,
                running: false,
                queued: Some(ago(29)),
                started: Some(ago(27)),
                waited_seconds: Some(1.0),
                held_seconds: Some(60.0),
                status: Some(4),
                verdict: None,
                wall_seconds: None,
                adjusted_seconds: None,
                abandoned: Vec::new(),
            },
            HistoryRun {
                command: "cargo build".to_owned(),
                cwd: Some(r"C:\work\repo".to_owned()),
                repo: Some("repo".to_owned()),
                kind: "heavy".to_owned(),
                slot: Some(1),
                slots: vec![r"Local\gate-slot-1".to_owned()],
                machine: None,
                remote: false,
                host: None,
                fallback: None,
                pid: 100,
                process_start: START.to_owned(),
                parent_pid: None,
                state: RunState::Ok,
                running: false,
                queued: Some(ago(30)),
                started: Some(ago(28)),
                waited_seconds: Some(2.0),
                held_seconds: Some(120.0),
                status: Some(0),
                verdict: None,
                wall_seconds: None,
                adjusted_seconds: None,
                abandoned: Vec::new(),
            },
        ]
    );
}

#[test]
fn a_runs_cwd_is_its_first_lines() {
    let mut first = Gate::new(100, START, COMMAND);
    first.cwd = r"X:\dev\ops-tower.wt\ops-92";
    let mut moved = Gate::new(100, START, COMMAND);
    moved.cwd = r"X:\dev\elsewhere";
    let mut blank = Gate::new(200, START, COMMAND);
    blank.cwd = "";
    let lines = [
        first.queue(&ago(30)),
        moved.take(&ago(29), 1.0),
        moved.release(&ago(28), 60.0, json!(0)),
        blank.queue(&ago(20)),
    ];
    let history = history_of("a_runs_cwd_is_its_first_lines", &lines);
    let cwds: Vec<Option<&str>> = history.runs.iter().map(|run| run.cwd.as_deref()).collect();
    assert_eq!(
        cwds,
        [None, Some(r"X:\dev\ops-tower.wt\ops-92")],
        "an empty cwd is none, and a run keeps its first line's cwd"
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
            repo: Some("repo".to_owned()),
            remote: false,
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
    let history = JournalCache::default()
        .read(&dir, "", ts(NOW), is_live, last_segment)
        .expect("the journal can be read");

    assert_eq!(states(&history), [RunState::Running, RunState::Unrecorded, RunState::Running]);
    assert_eq!(history.runs[0].started, None, "a run still queued has not started");
    assert_eq!(history.runs[2].held_seconds, None);
    assert!(history.summary.is_empty(), "open runs are not counted: {:?}", history.summary);
}

#[test]
fn selftest_lines_and_a_parent_line_naming_no_parent_are_ignored() {
    let mut selftest = Gate::new(1, START, COMMAND);
    selftest.cwd = r"C:\Users\me\AppData\Local\Temp\gate-selftest-12\work";
    let covered = Gate::new(2, START, COMMAND);
    let parent = Gate::new(3, START, COMMAND);
    let mut lines = selftest.run(30, 5.0, json!(0));
    lines.extend([
        covered.line("covered", &ago(20), json!({"parentPid": 9})),
        parent.line("parent", &ago(10), json!({})),
    ]);
    let history = history_of("selftest_lines_and_a_parent_line_naming_no_parent_are_ignored", &lines);
    let runs: Vec<(RunState, Option<u32>)> = history.runs.iter().map(|run| (run.state, run.parent_pid)).collect();
    assert_eq!(runs, [(RunState::Covered, Some(9))], "only the covered line makes a run");
    assert!(history.summary.is_empty(), "{:?}", history.summary);
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
            repo: Some("repo".to_owned()),
            remote: false,
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
                repo: Some("repo".to_owned()),
                remote: false,
                count: 10,
                p50_seconds: 5.0,
                p90_seconds: 9.0,
                max_seconds: 10.0,
            },
            CommandSummary {
                command: "cargo build".to_owned(),
                repo: Some("repo".to_owned()),
                remote: false,
                count: 1,
                p50_seconds: 1.0,
                p90_seconds: 1.0,
                max_seconds: 1.0,
            },
        ]
    );
}

#[test]
fn run_repo_comes_from_its_folder_and_is_none_without_one() {
    let mut nowhere = Gate::new(2, START, COMMAND);
    nowhere.cwd = "";
    let mut lines = Gate::new(1, START, COMMAND).run(30, 5.0, json!(0));
    lines.extend(nowhere.run(20, 5.0, json!(0)));
    let history = history_of("run_repo_comes_from_its_folder_and_is_none_without_one", &lines);
    let repos: Vec<Option<&str>> = history.runs.iter().map(|run| run.repo.as_deref()).collect();
    assert_eq!(
        repos,
        [None, Some("repo")],
        "newest first: the run with no folder, then the one in C:\\work\\repo"
    );
    assert_eq!(history.runs[0].cwd, None);
}

#[test]
fn summary_groups_by_repo_and_command_most_run_first_then_by_command_then_by_repo() {
    let in_folder = |pid: u32, command: &'static str, cwd: &'static str| {
        let mut gate = Gate::new(pid, START, command);
        gate.cwd = cwd;
        gate
    };
    let alpha = r"C:\work\alpha";
    let beta = r"C:\work\beta";
    let mut lines = in_folder(1, "zz check", alpha).run(60, 2.0, json!(0));
    lines.extend(in_folder(2, "zz check", alpha).run(55, 4.0, json!(0)));
    lines.extend(in_folder(3, "cargo build", beta).run(50, 1.0, json!(0)));
    lines.extend(in_folder(4, COMMAND, beta).run(45, 1.0, json!(0)));
    lines.extend(in_folder(5, COMMAND, "").run(40, 1.0, json!(0)));
    lines.extend(in_folder(6, COMMAND, alpha).run(35, 1.0, json!(0)));
    let history = history_of("summary_groups_by_repo_and_command_most_run_first_then_by_command_then_by_repo", &lines);

    let rows: Vec<(&str, Option<&str>, usize)> = history
        .summary
        .iter()
        .map(|row| (row.command.as_str(), row.repo.as_deref(), row.count))
        .collect();
    assert_eq!(
        rows,
        [
            ("zz check", Some("alpha"), 2),
            ("cargo build", Some("beta"), 1),
            (COMMAND, None, 1),
            (COMMAND, Some("alpha"), 1),
            (COMMAND, Some("beta"), 1),
        ],
        "one row per repository and command: the most run first, then by command, then by repository with none first"
    );
    assert_eq!(
        history.summary[0],
        CommandSummary {
            command: "zz check".to_owned(),
            repo: Some("alpha".to_owned()),
            remote: false,
            count: 2,
            p50_seconds: 2.0,
            p90_seconds: 4.0,
            max_seconds: 4.0,
        }
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

/// The pids of the history's runs, newest first: the covered runs' when `covered`, else every other run's.
fn pids_where_covered(history: &History, covered: bool) -> Vec<u32> {
    history
        .runs
        .iter()
        .filter(|run| (run.state == RunState::Covered) == covered)
        .map(|run| run.pid)
        .collect()
}

/// `count` covered lines from pids `first_pid` on, under parent gate `parent_pid`, the first `minutes` before [`NOW`] and
/// each next one a minute later.
fn covered_flood(first_pid: u32, count: u32, parent_pid: u32, minutes: i64) -> Vec<String> {
    (0..count)
        .map(|index| {
            Gate::new(first_pid + index, START, "cargo fmt --check").line(
                "covered",
                &ago(minutes - i64::from(index)),
                json!({"parentPid": parent_pid}),
            )
        })
        .collect()
}

#[test]
fn covered_runs_do_not_crowd_the_history() {
    let mut lines = Vec::new();
    for pid in 1..=49_u32 {
        lines.extend(Gate::new(pid, START, COMMAND).run(1000 - 3 * i64::from(pid), 1.0, json!(0)));
    }
    let parent = Gate::new(100, START, "git commit -F .tmp/msg.txt");
    lines.extend([parent.queue(&ago(500)), parent.take(&ago(499), 0.5)]);
    lines.extend(covered_flood(200, 40, 100, 400));
    lines.push(parent.release(&ago(300), 99.0, json!(0)));

    let history = history_of("covered_runs_do_not_crowd_the_history", &lines);

    let counted: Vec<u32> = [100].into_iter().chain((1..=49).rev()).collect();
    assert_eq!(
        pids_where_covered(&history, false),
        counted,
        "the parent and the 49 older plain runs are the 50 runs that are not covered"
    );
    assert_eq!(
        pids_where_covered(&history, true),
        (200..240).rev().collect::<Vec<u32>>(),
        "every covered run newer than the 50th counted run is listed"
    );
    assert_eq!(history.runs.len(), 90);
}

#[test]
fn covered_rows_are_capped_at_50() {
    let mut lines = Vec::new();
    for pid in 1..=49_u32 {
        lines.extend(Gate::new(pid, START, COMMAND).run(1000 - 3 * i64::from(pid), 1.0, json!(0)));
    }
    let parent = Gate::new(100, START, "git commit -F .tmp/msg.txt");
    lines.extend([parent.queue(&ago(500)), parent.take(&ago(499), 0.5)]);
    lines.extend(covered_flood(200, 120, 100, 400));
    lines.push(parent.release(&ago(250), 99.0, json!(0)));

    let history = history_of("covered_rows_are_capped_at_50", &lines);

    let counted: Vec<u32> = [100].into_iter().chain((1..=49).rev()).collect();
    assert_eq!(pids_where_covered(&history, false), counted, "the 50 runs that are not covered");
    assert_eq!(
        pids_where_covered(&history, true),
        (270..320).rev().collect::<Vec<u32>>(),
        "the 50 newest covered runs are listed, the 70 older ones are not"
    );
    assert_eq!(history.runs.len(), 100);
}

#[test]
fn a_parent_run_counts_toward_the_cap() {
    let mut lines = Vec::new();
    for pid in 1..=5_u32 {
        lines.extend(Gate::new(pid, START, COMMAND).run(1000 - 3 * i64::from(pid), 1.0, json!(0)));
    }
    let mut counted = Vec::new();
    for index in 0..50_u32 {
        let minutes = 900 - 10 * i64::from(index);
        if index % 5 == 0 {
            let lent = Gate::new(300 + index, START, COMMAND);
            lines.extend([lent.queue(&ago(minutes)), lent.line("parent", &ago(minutes - 1), json!({"parentPid": 9}))]);
            counted.push(300 + index);
        } else {
            lines.extend(Gate::new(100 + index, START, COMMAND).run(minutes, 1.0, json!(0)));
            counted.push(100 + index);
        }
        if index % 2 == 1 {
            lines.extend(covered_flood(500 + index, 1, 9, minutes + 5));
        }
    }
    counted.reverse();

    let history = history_of("a_parent_run_counts_toward_the_cap", &lines);

    assert_eq!(
        pids_where_covered(&history, false),
        counted,
        "the 10 runs on their parent's slot count among the 50, so the 5 oldest plain runs are not listed"
    );
    assert_eq!(history.runs.iter().filter(|run| run.state == RunState::Parent).count(), 10);
    assert_eq!(pids_where_covered(&history, true).len(), 25);
}

#[test]
fn covered_runs_between_counted_runs_are_listed() {
    let mut lines = covered_flood(1000, 1, 9, 50);
    for pid in 1..=51_u32 {
        let minutes = 100 + 10 * i64::from(pid);
        lines.extend(Gate::new(pid, START, COMMAND).run(minutes, 1.0, json!(0)));
        if pid <= 10 {
            lines.extend(covered_flood(1100 + pid, 1, 9, minutes + 5));
        }
    }
    lines.extend(covered_flood(1001, 1, 9, 595));
    lines.extend(covered_flood(1002, 1, 9, 605));

    let history = history_of("covered_runs_between_counted_runs_are_listed", &lines);

    let mut expected = vec![1000];
    for pid in 1..=49_u32 {
        expected.push(pid);
        if pid <= 10 {
            expected.push(1100 + pid);
        }
    }
    expected.extend([1001, 50]);
    let listed: Vec<u32> = history.runs.iter().map(|run| run.pid).collect();
    assert_eq!(
        listed, expected,
        "the covered runs newer than the 50th counted run, pid 50, are listed in their place; the one just older than it and \
         pid 51 are not"
    );
}

#[test]
fn a_covered_flood_older_than_the_cap_is_dropped() {
    let mut lines = covered_flood(200, 10, 9, 900);
    for pid in 1..=50_u32 {
        lines.extend(Gate::new(pid, START, COMMAND).run(800 - 3 * i64::from(pid), 1.0, json!(0)));
    }

    let history = history_of("a_covered_flood_older_than_the_cap_is_dropped", &lines);

    assert_eq!(
        pids_where_covered(&history, true),
        Vec::<u32>::new(),
        "covered runs older than the 50th are not listed"
    );
    assert_eq!(history.runs.len(), 50);
}

#[test]
fn prune_keeps_the_runs_a_covered_flood_would_push_out() {
    let week = 7 * 24 * 60;
    let mut lines = Vec::new();
    for pid in 1..=50_u32 {
        lines.extend(Gate::new(pid, START, COMMAND).run(week + 200 - 3 * i64::from(pid), 1.0, json!(0)));
    }
    lines.extend(covered_flood(200, 40, 9, 100));

    let history = history_of("prune_keeps_the_runs_a_covered_flood_would_push_out", &lines);

    assert_eq!(
        pids_where_covered(&history, false),
        (1..=50).rev().collect::<Vec<u32>>(),
        "runs older than 7 days stay while they are among the 50 newest that are not covered"
    );
    assert_eq!(history.runs.len(), 90);
}

#[test]
fn prune_keeps_a_listed_covered_runs_parent() {
    let week = 7 * 24 * 60;
    let mut lines = Vec::new();
    let parent = Gate::new(100, START, "git commit -F .tmp/msg.txt");
    lines.extend([parent.queue(&ago(week + 300)), parent.take(&ago(week + 299), 0.5)]);
    for pid in 1..=50_u32 {
        lines.extend(Gate::new(pid, START, COMMAND).run(week + 200 - 3 * i64::from(pid), 1.0, json!(0)));
    }
    lines.extend(covered_flood(200, 1, 100, 40));
    lines.push(parent.release(&ago(30), 99.0, json!(0)));

    let history = history_of("prune_keeps_a_listed_covered_runs_parent", &lines);

    assert!(
        !history.runs.iter().any(|run| run.pid == 100),
        "the parent queued behind the 50 newest counted runs, so it is not listed"
    );
    let covered = history.runs.iter().find(|run| run.pid == 200).expect("the covered run is listed");
    assert_eq!(covered.state, RunState::Covered);
    assert_eq!(
        covered.status,
        Some(0),
        "the covered row's status comes from its parent's run, kept past the 7-day prune"
    );
    assert_eq!(
        covered.held_seconds,
        Some(99.0),
        "the covered row's held time comes from its parent's run, kept past the 7-day prune"
    );
}

#[test]
fn cache_parses_only_appended_lines() {
    let dir = journal_dir("cache_parses_only_appended_lines");
    let journal = dir.join("events.jsonl");
    let old = text(&Gate::new(1, START, "cargo build").run(30, 5.0, json!(0)));
    fs::write(&journal, &old).expect("the journal can be written");
    let mut cache = JournalCache::default();
    let first = cache.read(&dir, "", ts(NOW), not_live, last_segment).expect("the journal can be read");
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

    let second = cache
        .read(&dir, "", ts(NOW), not_live, last_segment)
        .expect("the journal can be read again");
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

    let partial = cache.read(&dir, "", ts(NOW), not_live, last_segment).expect("the journal can be read");
    assert_eq!(states(&partial), [RunState::Unrecorded], "the release is not read until its line ends");

    append(&journal, &format!("{tail}\n"));
    let complete = cache
        .read(&dir, "", ts(NOW), not_live, last_segment)
        .expect("the journal can be read again");
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
    let before = cache.read(&dir, "", ts(NOW), not_live, last_segment).expect("the journal can be read");
    assert_eq!(commands(&before), ["cargo clippy", "cargo build"]);

    fs::rename(&journal, dir.join("events.1.jsonl")).expect("the journal can be rolled");
    fs::write(&journal, text(&Gate::new(3, START, COMMAND).run(10, 7.0, json!(0)))).expect("the new journal can be written");

    let after = cache
        .read(&dir, "", ts(NOW), not_live, last_segment)
        .expect("the rolled journal can be read");
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

    let again = cache
        .read(&dir, "", ts(NOW), not_live, last_segment)
        .expect("the twice-rolled journal can be read");
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
    let before = cache.read(&dir, "", ts(NOW), not_live, last_segment).expect("the journal can be read");
    assert_eq!(commands(&before), ["cargo build"]);

    // The roll as a tunnelled creation time leaves it: the old content moves to the rolled file, and the journal is
    // rewritten in place (so it keeps its creation time) with more bytes than were read, the first run starting at 0.
    fs::write(dir.join("events.1.jsonl"), &old).expect("the rolled journal can be written");
    let mut new = Gate::new(2, START, "cargo doc").run(30, 1.0, json!(0));
    new.extend(Gate::new(3, START, COMMAND).run(20, 2.0, json!(0)));
    let new = text(&new);
    assert!(new.len() > old.len(), "the length check alone would not see the roll");
    fs::write(&journal, &new).expect("the new journal can be written");

    let after = cache
        .read(&dir, "", ts(NOW), not_live, last_segment)
        .expect("the rolled journal can be read");
    assert_eq!(
        commands(&after),
        [COMMAND, "cargo doc", "cargo build"],
        "the new journal is read from its start"
    );
    assert_eq!(after, read_fresh(&dir), "a cold read agrees");
}

/// The ssh destination the remote lines name.
const HOST: &str = "builder@build-box";

impl Gate<'_> {
    /// The queue line of a gate routed to the remote pool.
    fn remote_queue(&self, time: &str) -> String {
        self.line(
            "queue",
            time,
            json!({"log": r"C:\work\repo\.tmp\run.log", "remote": true, "host": HOST, "key": "repo-1"}),
        )
    }

    /// The take line of a gate that took remote slot `index`.
    fn remote_take(&self, time: &str, index: u32) -> String {
        let slot = format!(r"Local\gate-remote-slot-{index}");
        self.line(
            "take",
            time,
            json!({"slots": [slot], "waitedSeconds": 0.5, "remote": true, "host": HOST, "key": "repo-1"}),
        )
    }

    /// The fallback line of a remote run sent back to the local queue, with `reason` when there is one.
    fn fallback(&self, time: &str, reason: Option<&str>) -> String {
        let mut fields = json!({"remote": true, "host": HOST, "key": "repo-1"});
        if let Some(reason) = reason {
            fields["reason"] = json!(reason);
        }
        self.line("fallback", time, fields)
    }
}

#[test]
fn remote_take_marks_the_run_remote_with_its_host() {
    let gate = Gate::new(1, START, COMMAND);
    let lines = [
        gate.remote_queue(&ago(30)),
        gate.remote_take(&ago(29), 1),
        gate.release(&ago(28), 60.0, json!(0)),
    ];
    let history = history_of("remote_take_marks_the_run_remote_with_its_host", &lines);
    let run = &history.runs[0];
    assert_eq!(
        (run.slot, run.remote, run.host.as_deref(), run.fallback.as_deref()),
        (Some(1), true, Some(HOST), None)
    );
    assert_eq!(run.kind, "heavy", "a remote run's kind stays heavy");
}

#[test]
fn queued_remote_run_is_remote_while_it_waits() {
    let gate = Gate::new(1, START, COMMAND);
    let history = history_of("queued_remote_run_is_remote_while_it_waits", &[gate.remote_queue(&ago(5))]);
    let run = &history.runs[0];
    assert_eq!((run.slot, run.remote, run.host.as_deref()), (None, true, Some(HOST)));
}

#[test]
fn fallback_clears_remote_and_the_local_take_holds_the_slot() {
    let gate = Gate::new(1, START, COMMAND);
    let lines = [
        gate.remote_queue(&ago(30)),
        gate.remote_take(&ago(29), 0),
        gate.fallback(&ago(28), Some("rebooted")),
        gate.take(&ago(27), 0.2),
        gate.release(&ago(26), 60.0, json!(0)),
    ];
    let history = history_of("fallback_clears_remote_and_the_local_take_holds_the_slot", &lines);
    let run = &history.runs[0];
    assert_eq!(history.runs.len(), 1, "a fallback is no run of its own");
    assert_eq!(
        (run.slot, run.remote, run.host.as_deref(), run.fallback.as_deref()),
        (Some(1), false, None, Some("rebooted"))
    );
    assert_eq!(run.started.as_deref(), Some(ago(27).as_str()), "the local take is the run's start");
    assert_eq!(run.state, RunState::Ok);
}

#[test]
fn fallback_after_a_remote_take_holds_nothing_until_a_local_take() {
    let gate = Gate::new(1, START, COMMAND);
    let lines = [
        gate.remote_queue(&ago(30)),
        gate.remote_take(&ago(29), 1),
        gate.fallback(&ago(28), Some("rebooted")),
    ];
    let history = history_of("fallback_after_a_remote_take_holds_nothing_until_a_local_take", &lines);
    let run = &history.runs[0];
    assert_eq!((run.slot, run.started.as_deref(), run.waited_seconds), (None, None, None));
    assert_eq!((run.remote, run.fallback.as_deref()), (false, Some("rebooted")));
}

#[test]
fn remote_take_after_a_fallback_is_a_plain_remote_run() {
    let gate = Gate::new(1, START, COMMAND);
    let lines = [
        gate.remote_queue(&ago(30)),
        gate.fallback(&ago(29), Some("busy")),
        gate.remote_take(&ago(28), 0),
    ];
    let history = history_of("remote_take_after_a_fallback_is_a_plain_remote_run", &lines);
    let run = &history.runs[0];
    assert_eq!((run.slot, run.remote, run.fallback.as_deref()), (Some(0), true, None));
}

#[test]
fn remote_release_marks_a_run_whose_take_rolled_out_remote() {
    let gate = Gate::new(1, START, COMMAND);
    let release = gate.line(
        "release",
        &ago(10),
        json!({"slots": [r"Local\gate-remote-slot-0"], "heldSeconds": 30.0, "status": 0, "remote": true, "host": HOST}),
    );
    let history = history_of("remote_release_marks_a_run_whose_take_rolled_out_remote", &[release]);
    let run = &history.runs[0];
    assert_eq!((run.remote, run.host.as_deref()), (true, Some(HOST)));
    assert_eq!(history.summary, Vec::<CommandSummary>::new(), "a run with no take is not counted");
}

#[test]
fn unknown_fallback_reason_is_kept_and_a_missing_one_reads_unknown() {
    let odd = Gate::new(1, START, COMMAND);
    let bare = Gate::new(2, START, COMMAND);
    let lines = [
        odd.remote_queue(&ago(30)),
        odd.fallback(&ago(29), Some("solar flare")),
        bare.remote_queue(&ago(20)),
        bare.fallback(&ago(19), None),
    ];
    let history = history_of("unknown_fallback_reason_is_kept_and_a_missing_one_reads_unknown", &lines);
    let reasons: Vec<Option<&str>> = history.runs.iter().map(|run| run.fallback.as_deref()).collect();
    assert_eq!(reasons, [Some("unknown"), Some("solar flare")]);
}

/// The gate writes one queue line per run; a second one, should a journal hold it, is tolerated.
#[test]
fn a_tolerated_second_queue_line_keeps_the_first_queued_time() {
    let gate = Gate::new(1, START, COMMAND);
    let lines = [
        gate.remote_queue(&ago(30)),
        gate.fallback(&ago(29), Some("busy")),
        gate.queue(&ago(29)),
        gate.take(&ago(28), 60.0),
    ];
    let history = history_of("a_tolerated_second_queue_line_keeps_the_first_queued_time", &lines);
    let run = &history.runs[0];
    assert_eq!(run.queued.as_deref(), Some(ago(30).as_str()));
    assert!(!run.remote);
}

#[test]
fn a_remote_take_ended_by_a_fallback_reads_as_the_local_run() {
    let gate = Gate::new(1, START, COMMAND);
    let lines = [
        gate.remote_queue(&ago(30)),
        gate.remote_take(&ago(29), 0),
        gate.fallback(&ago(28), Some("unreachable")),
        gate.take(&ago(27), 60.0),
        gate.release(&ago(26), 45.0, json!(0)),
    ];
    let history = history_of("a_remote_take_ended_by_a_fallback_reads_as_the_local_run", &lines);
    let run = &history.runs[0];
    assert_eq!(
        (run.state, run.slot, run.remote, run.host.as_deref(), run.fallback.as_deref()),
        (RunState::Ok, Some(1), false, None, Some("unreachable")),
        "no release for the remote slot: the fallback ended its take, and the local slot is the run's"
    );
    assert_eq!(run.started.as_deref(), Some(ago(27).as_str()));
    assert_eq!(run.waited_seconds, Some(60.0), "the local take's wait, counted from the fallback");
    assert_eq!(run.held_seconds, Some(45.0));
}

/// A gate that fell back onto its parent's slot: remote queue, remote take, fallback (`busy`), then `parent` naming pid 9.
fn on_parent_lines(child: &Gate<'_>) -> Vec<String> {
    vec![
        child.remote_queue(&ago(30)),
        child.remote_take(&ago(29), 0),
        child.fallback(&ago(28), Some("busy")),
        child.line("parent", &ago(27), json!({"parentPid": 9})),
    ]
}

#[test]
fn a_parent_line_after_a_fallback_reads_on_parent() {
    let child = Gate::new(1, START, COMMAND);
    let parent = Gate::new(9, START, COMMAND);
    let reused = Gate::new(9, OTHER_START, COMMAND);
    let mut ended = vec![parent.queue(&ago(40)), parent.take(&ago(39), 0.5)];
    ended.extend(on_parent_lines(&child));
    ended.push(parent.release(&ago(20), 1140.0, json!(0)));
    ended.extend(reused.run(12, 30.0, json!(1)));
    let history = history_of("a_parent_line_after_a_fallback_reads_on_parent", &ended);
    let run = history.runs.iter().find(|run| run.parent_pid.is_some()).expect("the child is a run");
    assert_eq!(
        (run.state, run.parent_pid, run.slot, run.started.as_deref(), run.fallback.as_deref()),
        (RunState::Parent, Some(9), None, None, Some("busy"))
    );
    assert_eq!(
        (run.status, run.held_seconds),
        (Some(0), Some(1140.0)),
        "the end of the parent's run whose take came before the parent line, not of a later run of the reused pid"
    );
    let counted: Vec<(usize, f64)> = history.summary.iter().map(|row| (row.count, row.max_seconds)).collect();
    assert_eq!(counted, [(1, 1140.0)], "only the parent's run counts its time");

    let mut open = vec![parent.queue(&ago(40)), parent.take(&ago(39), 0.5)];
    open.extend(on_parent_lines(&child));
    let dir = journal_dir("a_parent_line_after_a_fallback_reads_on_parent_open");
    fs::write(dir.join("events.jsonl"), text(&open)).expect("the journal can be written");
    for live in [false, true] {
        let history = JournalCache::default()
            .read(&dir, "", ts(NOW), |_pid: u32, _start: &str| live, last_segment)
            .expect("the journal can be read");
        let run = history.runs.iter().find(|run| run.parent_pid.is_some()).expect("the child is a run");
        assert_eq!(
            (run.state, run.status, run.held_seconds),
            (RunState::Parent, None, None),
            "with the parent's run not ended, the run stays on parent whether its gate is live ({live}) or gone"
        );
        assert!(history.summary.is_empty(), "nothing has passed: {:?}", history.summary);
    }
}

#[test]
fn the_parent_is_the_latest_run_of_its_pid_to_take_a_slot_before_the_parent_line() {
    let child = Gate::new(1, START, COMMAND);
    let earlier = Gate::new(9, OTHER_START, COMMAND);
    let parent = Gate::new(9, START, COMMAND);
    let mut lines = earlier.run(60, 30.0, json!(1));
    lines.extend([parent.queue(&ago(40)), parent.take(&ago(39), 0.5)]);
    lines.extend(on_parent_lines(&child));
    lines.push(parent.release(&ago(20), 1140.0, json!(0)));
    let history = history_of("the_parent_is_the_latest_run_of_its_pid_to_take_a_slot_before_the_parent_line", &lines);
    let run = history.runs.iter().find(|run| run.parent_pid.is_some()).expect("the child is a run");
    assert_eq!(
        (run.state, run.status, run.held_seconds),
        (RunState::Parent, Some(0), Some(1140.0)),
        "the run of pid 9 that took its slot last before the parent line, not the earlier one released before it"
    );
}

#[test]
fn apply_parent_reads_a_lent_childs_parent_line_on_parent_with_or_without_a_fallback() {
    let child = Gate::new(1, START, COMMAND);
    let parent = Gate::new(9, START, COMMAND);
    let queued = vec![child.queue(&ago(30)), child.line("parent", &ago(27), json!({"parentPid": 9}))];
    for (name, lent) in [("queued", queued), ("fell_back", on_parent_lines(&child))] {
        let mut lines = vec![parent.queue(&ago(40)), parent.take(&ago(39), 0.5)];
        lines.extend(lent);
        lines.push(parent.release(&ago(20), 1140.0, json!(0)));
        let history = history_of(&format!("a_lent_childs_parent_line_reads_on_parent_{name}"), &lines);
        let run = history.runs.iter().find(|run| run.parent_pid.is_some());
        assert!(run.is_some(), "{name}: the child reads on its parent: {:?}", history.runs);
        let run = run.expect("checked above");
        assert_eq!(
            (run.state, run.parent_pid, run.status, run.held_seconds),
            (RunState::Parent, Some(9), Some(0), Some(1140.0)),
            "{name}: the child ends as its parent's run ended"
        );
    }
}

#[test]
fn a_gate_lent_before_it_queued_shows_as_a_parent_run() {
    let child = Gate::new(1, START, COMMAND);
    let parent = Gate::new(9, START, COMMAND);
    let lines = vec![
        parent.queue(&ago(40)),
        parent.take(&ago(39), 0.5),
        child.line("parent", &ago(27), json!({"parentPid": 9})),
        parent.release(&ago(20), 1140.0, json!(0)),
    ];

    let history = history_of("a_gate_lent_before_it_queued_shows_as_a_parent_run", &lines);

    let run = history.runs.iter().find(|run| run.parent_pid.is_some());
    assert!(run.is_some(), "the lone parent line makes a run: {:?}", history.runs);
    let run = run.expect("checked above");
    assert_eq!(
        (run.state, run.parent_pid, run.status, run.held_seconds),
        (RunState::Parent, Some(9), Some(0), Some(1140.0)),
        "the child ends as its parent's run ended"
    );
    assert_eq!((&run.queued, &run.started), (&None, &None), "the child neither queued nor took a slot");
    assert_eq!(history.runs.len(), 2, "{:?}", history.runs);
}

#[test]
fn summary_splits_remote_runs_from_local_runs_of_one_command() {
    let remote = Gate::new(1, START, COMMAND);
    let mut lines = vec![
        remote.remote_queue(&ago(60)),
        remote.remote_take(&ago(59), 0),
        remote.release(&ago(58), 90.0, json!(0)),
    ];
    lines.extend(Gate::new(2, START, COMMAND).run(50, 30.0, json!(0)));
    let history = history_of("summary_splits_remote_runs_from_local_runs_of_one_command", &lines);
    let rows: Vec<(&str, bool, usize, f64)> = history
        .summary
        .iter()
        .map(|row| (row.command.as_str(), row.remote, row.count, row.max_seconds))
        .collect();
    assert_eq!(
        rows,
        [(COMMAND, false, 1, 30.0), (COMMAND, true, 1, 90.0)],
        "one row each, the local row first when the counts tie"
    );
}

#[test]
fn a_run_lists_the_slots_its_take_took_abandoned() {
    let gate = Gate::new(1, START, COMMAND);
    let slot = r"Local\gate-slot-1";
    let lines = vec![
        versioned(&gate.queue(&ago(30)), 1),
        versioned(&with_fields(&gate.take(&ago(29), 0.5), &json!({"abandoned": [slot]})), 1),
        versioned(&gate.release(&ago(28), 2.0, json!(0)), 1),
    ];

    let history = history_of("a_run_lists_the_slots_its_take_took_abandoned", &lines);

    assert_eq!(history.runs[0].abandoned, [slot]);
}

#[test]
fn a_run_lists_an_abandoned_journal_mutex_from_any_of_its_lines_once() {
    let gate = Gate::new(1, START, COMMAND);
    let journal = r"Local\gate-1a2b3c4d-journal";
    let slot = r"Local\gate-slot-1";
    let lines = vec![
        versioned(&with_fields(&gate.queue(&ago(30)), &json!({"abandoned": [journal]})), 1),
        versioned(&with_fields(&gate.take(&ago(29), 0.5), &json!({"abandoned": [journal, slot]})), 1),
        versioned(&gate.release(&ago(28), 2.0, json!(0)), 1),
    ];

    let history = history_of("a_run_lists_an_abandoned_journal_mutex_from_any_of_its_lines_once", &lines);

    assert_eq!(
        history.runs[0].abandoned,
        [journal, slot],
        "the journal mutex the queue and take lines both name is listed once, then the slot"
    );
}

#[test]
fn a_run_without_abandoned_lines_has_an_empty_list() {
    let gate = Gate::new(1, START, COMMAND);

    let history = history_of("a_run_without_abandoned_lines_has_an_empty_list", &gate.run(30, 2.0, json!(0)));

    let run = &history.runs[0];
    assert!(run.abandoned.is_empty(), "{run:?}");
    let serialized = serde_json::to_string(run).expect("a run serializes");
    assert!(serialized.contains(r#""abandoned":[]"#), "{serialized}");
}

#[test]
fn a_run_lists_an_abandoned_journal_mutex_from_its_parent_line() {
    let gate = Gate::new(1, START, COMMAND);
    let journal = r"Local\gate-1a2b3c4d-journal";
    let lines = vec![
        gate.queue(&ago(30)),
        gate.take(&ago(29), 0.5),
        gate.line("parent", &ago(28), json!({"parentPid": 9, "abandoned": [journal]})),
    ];

    let history = history_of("a_run_lists_an_abandoned_journal_mutex_from_its_parent_line", &lines);

    assert_eq!(history.runs[0].abandoned, [journal], "the run's parent line names the abandoned mutex");
}

#[test]
fn a_run_lists_an_abandoned_journal_mutex_from_its_covered_line() {
    let gate = Gate::new(1, START, COMMAND);
    let journal = r"Local\gate-1a2b3c4d-journal";
    let lines = vec![
        gate.queue(&ago(30)),
        gate.take(&ago(29), 0.5),
        gate.line("covered", &ago(28), json!({"parentPid": 9, "abandoned": [journal]})),
    ];

    let history = history_of("a_run_lists_an_abandoned_journal_mutex_from_its_covered_line", &lines);

    assert_eq!(history.runs[0].abandoned, [journal], "the run's covered line names the abandoned mutex");
}

/// A hand-written v1 journal: a light gate (56000) whose slot is lent to 55000 before it queued, a critical gate (52000)
/// whose take carries its snapshot and under which 54000 runs covered, and a critical gate (52100) whose snapshot failed.
const V1_JOURNAL: &str = include_str!("fixtures/v1-journal.jsonl");
/// Why 52100's take could not read the other gates' claims.
const SNAPSHOT_ERROR: &str = r"cannot read the claims in C:\Users\me\AppData\Local\gate\v1\slots: Access is denied. (os error 5)";

/// The history of `v1-journal.jsonl`.
fn v1_history(test: &str) -> History {
    let dir = journal_dir(test);
    fs::write(dir.join("events.jsonl"), V1_JOURNAL).expect("the journal can be written");
    read_fresh(&dir)
}

/// The slots a critical take lists: every heavy slot and the light slots below the two it keeps free.
fn critical_slots() -> Vec<String> {
    let heavy = (0..3).map(|index| format!(r"Local\gate-slot-{index}"));
    heavy.chain((0..5).map(|index| format!(r"Local\gate-light-slot-{index}"))).collect()
}

#[test]
fn a_critical_take_carries_its_slots_and_machine() {
    let history = v1_history("a_critical_take_carries_its_slots_and_machine");

    let critical: Vec<&HistoryRun> = history.runs.iter().filter(|run| run.kind == "critical").collect();
    assert_eq!(critical.len(), 2, "two critical runs, newest first: {critical:?}");
    let (failed, passed) = (critical[0], critical[1]);
    assert_eq!((passed.state, passed.slot, &passed.slots), (RunState::Ok, Some(0), &critical_slots()));
    assert_eq!(
        passed.machine,
        Some(MachineSnapshot {
            counts: HeavyLight { heavy: 3, light: 7 },
            other_held: HeavyLight { heavy: 0, light: 1 },
            other_claims: Ok(vec![OtherClaim {
                pid: 57000,
                kind: "light".to_owned(),
                index: 5,
                command: "cargo check".to_owned(),
                file: "gate-light-slot-5.json".to_owned(),
            }]),
        })
    );
    assert_eq!((failed.state, &failed.slots), (RunState::Failed, &critical_slots()));
    assert_eq!(
        failed.machine,
        Some(MachineSnapshot {
            counts: HeavyLight { heavy: 3, light: 7 },
            other_held: HeavyLight { heavy: 0, light: 0 },
            other_claims: Err(SNAPSHOT_ERROR.to_owned()),
        })
    );
    let shape = |run: &HistoryRun| serde_json::to_value(run).expect("a run serializes");
    assert_eq!(shape(passed)["slots"], json!(critical_slots()));
    assert_eq!(
        shape(passed)["machine"],
        json!({
            "counts": {"heavy": 3, "light": 7},
            "otherHeld": {"heavy": 0, "light": 1},
            "otherClaims": [{"pid": 57000, "kind": "light", "index": 5, "command": "cargo check", "file": "gate-light-slot-5.json"}],
        })
    );
    assert_eq!(
        shape(failed)["machine"],
        json!({"counts": {"heavy": 3, "light": 7}, "otherHeld": {"heavy": 0, "light": 0}, "snapshotError": SNAPSHOT_ERROR})
    );
    let light = history
        .runs
        .iter()
        .find(|run| run.command == "cargo fmt --all")
        .expect("the light gate is a run");
    assert_eq!(
        (light.slot, &light.slots, &light.machine),
        (Some(0), &vec![r"Local\gate-light-slot-0".to_owned()], &None),
        "a take without a snapshot has no machine"
    );
    assert_eq!(shape(light)["machine"], Value::Null);
}

#[test]
fn a_fallback_clears_slots_and_machine() {
    let gate = Gate::new(1, START, COMMAND);
    let snapshot = json!({"counts": {"heavy": 3, "light": 7}, "otherHeld": {"heavy": 0, "light": 0}, "otherClaims": []});
    let mut lines = vec![gate.remote_queue(&ago(30)), with_fields(&gate.remote_take(&ago(29), 1), &snapshot)];
    let taken = history_of("a_fallback_clears_slots_and_machine_taken", &lines);
    let run = &taken.runs[0];
    assert_eq!(
        (&run.slots, run.machine.is_some()),
        (&vec![r"Local\gate-remote-slot-1".to_owned()], true),
        "{run:?}"
    );

    lines.push(gate.fallback(&ago(28), Some("busy")));
    let fell_back = history_of("a_fallback_clears_slots_and_machine_fell_back", &lines);

    let run = &fell_back.runs[0];
    assert_eq!((run.slot, run.slots.is_empty(), &run.machine), (None, true, &None), "{run:?}");
}

#[test]
fn a_covered_line_is_a_covered_run_with_its_parent_pid() {
    let history = v1_history("a_covered_line_is_a_covered_run_with_its_parent_pid");

    let covered = history
        .runs
        .iter()
        .find(|run| run.state == RunState::Covered)
        .expect("the covered line is a run");
    assert_eq!(
        (
            covered.command.as_str(),
            covered.parent_pid,
            covered.status,
            covered.held_seconds,
            covered.verdict.as_deref()
        ),
        ("cargo fmt --check", Some(52000), Some(0), Some(300.0), Some("passed")),
        "a covered run ends as the run of its parent's slot ended"
    );
    let shape = serde_json::to_value(covered).expect("a run serializes");
    assert_eq!((&shape["state"], &shape["parentPid"]), (&json!("covered"), &json!(52000)));
    let lent = history
        .runs
        .iter()
        .find(|run| run.state == RunState::Parent)
        .expect("the parent line is a run");
    assert_eq!(
        (lent.command.as_str(), lent.parent_pid, lent.held_seconds),
        ("cargo clippy", Some(56000), Some(659.8)),
        "a gate lent its parent's slot stays a parent run"
    );
    let counted: Vec<&str> = history.summary.iter().map(|row| row.command.as_str()).collect();
    assert_eq!(
        counted,
        ["cargo fmt --all", "cargo test --workspace"],
        "only the parents' runs count their time"
    );
}

#[test]
fn a_covered_run_carries_its_gates_pid_start_and_liveness() {
    let dir = journal_dir("a_covered_run_carries_its_gates_pid_start_and_liveness");
    fs::write(dir.join("events.jsonl"), V1_JOURNAL).expect("the journal can be written");
    let covered_start = "2026-10-03T11:01:59.8000000Z";

    for live in [false, true] {
        let is_live = |pid: u32, process_start: &str| live && (pid, process_start) == (54000, covered_start);
        let history = JournalCache::default()
            .read(&dir, "", ts(NOW), is_live, last_segment)
            .expect("the journal can be read");
        let covered = history
            .runs
            .iter()
            .find(|run| run.state == RunState::Covered)
            .expect("the covered line is a run");

        assert_eq!(
            (covered.pid, covered.process_start.as_str(), covered.running),
            (54000, covered_start, live),
            "a covered run stays covered, live or not"
        );
        let shape = serde_json::to_value(covered).expect("a run serializes");
        assert_eq!(
            (&shape["pid"], &shape["processStart"], &shape["running"], &shape["state"]),
            (&json!(54000), &json!(covered_start), &json!(live), &json!("covered"))
        );
        let others: Vec<bool> = history.runs.iter().filter(|run| run.pid != 54000).map(|run| run.running).collect();
        assert!(others.iter().all(|running| !running), "no other gate is live: {others:?}");
    }
}

#[test]
fn a_covered_line_alone_takes_its_time_and_kind_from_itself() {
    let history = v1_history("a_covered_line_alone_takes_its_time_and_kind_from_itself");

    assert_eq!(
        commands(&history),
        [
            "cargo test --workspace",
            "cargo fmt --check",
            "cargo clippy",
            "cargo test --workspace",
            "cargo fmt --all",
        ],
        "newest first by each run's first line: the covered run at its covered line's time"
    );
    let covered = &history.runs[1];
    assert_eq!(
        (covered.state, covered.kind.as_str(), &covered.queued, &covered.started, covered.slot),
        (RunState::Covered, "light", &None, &None, None),
        "the covered gate neither queued nor took a slot"
    );
}
