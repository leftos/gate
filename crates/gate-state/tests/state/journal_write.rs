//! The journal writer: its lines, the append under the journal lock, the rollover and the torn-line fix.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;

use flate2::read::GzDecoder;
use gate_state::{
    FallbackReason, HeavyLight, Held, JOURNAL_CAP_BYTES, JournalEvent, JournalIdentity, JournalLock, JournalNote, LockError, MachineSnapshot,
    OtherClaim, RemoteTag, RunFacts, RunTrace, TracedChild, Verdict, append_line, journal_line,
};
use serde_json::{Value, json};

use crate::support::{keys, ts};

/// The moment every append archives at.
pub const NOW: &str = "2026-10-03T12:00:00Z";
/// Only guards against a stuck test; nothing asserts how long an append took.
const HANG_GUARD: Duration = Duration::from_secs(600);
/// The name the test locks answer with, the shape the binary's journal mutex name has.
pub const LOCK_NAME: &str = r"Local\gate-test-journal";

/// A journal lock that answers at once.
pub enum FakeLock {
    Free,
    Abandoned,
    Busy,
}

impl JournalLock for FakeLock {
    type Guard<'a> = ();

    fn name(&self) -> &str {
        LOCK_NAME
    }

    fn acquire(&self, _timeout: Duration) -> Result<Held<()>, LockError> {
        match self {
            FakeLock::Free => Ok(Held { guard: (), abandoned: false }),
            FakeLock::Abandoned => Ok(Held { guard: (), abandoned: true }),
            FakeLock::Busy => Err(LockError(format!("{LOCK_NAME} was not free within 2 s"))),
        }
    }
}

/// A journal lock shared by threads of one process, waiting as long as it takes.
#[derive(Default)]
struct ThreadLock(Mutex<()>);

impl JournalLock for ThreadLock {
    type Guard<'a> = MutexGuard<'a, ()>;

    fn name(&self) -> &str {
        LOCK_NAME
    }

    fn acquire(&self, _timeout: Duration) -> Result<Held<MutexGuard<'_, ()>>, LockError> {
        let guard = self.0.lock().map_err(|error| LockError(error.to_string()))?;
        Ok(Held { guard, abandoned: false })
    }
}

/// A fresh, empty folder for a test's state root.
pub fn fresh_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gate-state-writer").join(test);
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
    }
    fs::create_dir_all(&dir).expect("the test's folder can be made");
    dir
}

/// JSON lines of `total` bytes in all, each ending in a line feed, every third a `release`.
pub fn seed_lines(total: u64) -> Vec<String> {
    const LINE: usize = 1000;
    let mut left = usize::try_from(total).expect("a seed size fits in memory");
    let mut lines = Vec::new();
    while left > 0 {
        let size = if left >= 2 * LINE { LINE } else { left };
        lines.push(padded_line(lines.len(), size));
        left -= size;
    }
    lines
}

/// Seed line `index`, padded so that with its line feed it is `size` bytes.
fn padded_line(index: usize, size: usize) -> String {
    let event = if index.is_multiple_of(3) { "release" } else { "queue" };
    let base = format!(r#"{{"v":1,"event":"{event}","seed":{index},"pad":""}}"#);
    let pad = "x".repeat(size - 1 - base.len());
    format!(r#"{{"v":1,"event":"{event}","seed":{index},"pad":"{pad}"}}"#)
}

/// The lines, each ending in a line feed.
pub fn text(lines: &[String]) -> String {
    lines.iter().flat_map(|line| [line.as_str(), "\n"]).collect()
}

/// The lines of a file, without their line feeds.
pub fn lines_of(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .expect("the file can be read")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A `append_line` builder that ignores the lock and returns `line`, for the raw-string tests.
pub fn fixed(line: &str) -> impl FnOnce(Option<&str>) -> String + '_ {
    move |_| line.to_owned()
}

/// The names in the root's `archive` folder, sorted.
pub fn archive_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join("archive"))
        .expect("the archive folder can be listed")
        .map(|entry| entry.expect("an archive entry can be read").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The text of a gzip archive.
pub fn gunzip(path: &Path) -> String {
    let mut text = String::new();
    GzDecoder::new(File::open(path).expect("the archive can be opened"))
        .read_to_string(&mut text)
        .expect("the archive is gzip text");
    text
}

/// The lines of `lines` whose `event` is `release`.
pub fn release_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| serde_json::from_str::<Value>(line).is_ok_and(|value| value["event"] == "release"))
        .cloned()
        .collect()
}

fn identity() -> JournalIdentity {
    JournalIdentity {
        pid: 40268,
        process_start: ts("2026-10-03T05:04:38.7424554Z"),
        kind: "light".to_owned(),
        cwd: r"C:\work\repo".to_owned(),
        command: "cmd /c exit 0".to_owned(),
        commit: false,
    }
}

fn release(facts: Option<RunFacts>) -> JournalEvent {
    JournalEvent::Release {
        slots: vec![r"Local\gate-light-slot-0".to_owned()],
        held_seconds: 0.105,
        status: Some(0),
        facts,
        trace: RunTrace::default(),
        cold: false,
    }
}

fn parsed(line: &str) -> Value {
    serde_json::from_str(line).expect("a journal line is JSON")
}

const COMMON: [&str; 8] = ["v", "time", "event", "pid", "processStart", "kind", "cwd", "command"];

fn with_common(own: &[&'static str]) -> Vec<&'static str> {
    COMMON.iter().chain(own).copied().collect()
}

/// The PowerShell gate's journal of routed runs: refused, fallen back and local.
const REMOTE_FIXTURE: &str = include_str!("fixtures/remote-journal.jsonl");
const REMOTE_HOST: &str = "builder@build-box";
const REMOTE_KEY: &str = "repo-ba4a1013";
const REMOTE_SLOT: &str = r"Local\ops253-gate-remote-slot-0";
const REMOTE_KEYS: [&str; 3] = ["remote", "host", "key"];
/// The `release` fields the Rust gate writes and the PowerShell lines of the fixture do not.
const RUN_FACT_KEYS: [&str; 3] = ["wallSeconds", "adjustedSeconds", "verdict"];

fn tag() -> RemoteTag {
    RemoteTag {
        host: REMOTE_HOST.to_owned(),
        key: REMOTE_KEY.to_owned(),
    }
}

/// The fixture's line `number`, counted from 1 as an editor counts.
fn remote_fixture(number: usize) -> &'static str {
    REMOTE_FIXTURE.lines().nth(number - 1).expect("the fixture has the line")
}

/// A journal line's fields after the identity's last, `command`, in the order the line has them.
fn own_fields(line: &str) -> Vec<(String, Value)> {
    parsed(line)
        .as_object()
        .expect("a journal line is an object")
        .iter()
        .skip_while(|(key, _)| *key != "command")
        .skip(1)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn own_keys(line: &str) -> Vec<String> {
    own_fields(line).into_iter().map(|(key, _)| key).collect()
}

#[test]
fn a_routed_queue_line_ends_with_the_remote_fields_as_the_fixture_does() {
    let queue = JournalEvent::Queue {
        log: r"C:\work\repo\.tmp\remote-ok.log".to_owned(),
        not_routed: None,
        reason: None,
    };
    let line = journal_line(&identity(), ts(NOW), &queue, Some(&tag()), None);

    assert_eq!(own_keys(&line), ["log", "remote", "host", "key"]);
    assert_eq!(own_fields(&line), own_fields(remote_fixture(1)));
}

#[test]
fn a_queue_kept_local_writes_not_routed_after_the_log() {
    let queue = JournalEvent::Queue {
        log: r"C:\work\repo\.tmp\local.log".to_owned(),
        not_routed: Some("--local".to_owned()),
        reason: None,
    };
    let line = journal_line(&identity(), ts(NOW), &queue, None, None);

    assert_eq!(own_keys(&line), ["log", "notRouted"]);
    assert_eq!(parsed(&line)["notRouted"], "--local");
    let mut fields = own_fields(&line);
    fields.retain(|(key, _)| key != "notRouted");
    assert_eq!(fields, own_fields(remote_fixture(37)), "otherwise the fixture's local queue line");
}

#[test]
fn a_queue_with_a_reason_writes_it_after_not_routed() {
    let queue = JournalEvent::Queue {
        log: "x".to_owned(),
        not_routed: Some("--local".to_owned()),
        reason: Some("checking routing".to_owned()),
    };
    let line = journal_line(&identity(), ts(NOW), &queue, None, None);

    assert_eq!(own_keys(&line), ["log", "notRouted", "reason"]);
    assert_eq!(parsed(&line)["reason"], "checking routing");

    let without = JournalEvent::Queue {
        log: "x".to_owned(),
        not_routed: None,
        reason: Some("checking routing".to_owned()),
    };
    assert_eq!(own_keys(&journal_line(&identity(), ts(NOW), &without, None, None)), ["log", "reason"]);
}

#[test]
fn a_routed_take_line_puts_the_remote_fields_after_waited_seconds_and_before_abandoned() {
    let take = |abandoned: Vec<String>| {
        let event = JournalEvent::Take {
            slots: vec![REMOTE_SLOT.to_owned()],
            waited_seconds: 0.135,
            abandoned,
            machine: None,
            parent_pid: None,
        };
        journal_line(&identity(), ts(NOW), &event, Some(&tag()), None)
    };

    let line = take(Vec::new());
    assert_eq!(own_keys(&line), ["slots", "waitedSeconds", "remote", "host", "key"]);
    assert_eq!(own_fields(&line), own_fields(remote_fixture(2)));
    assert_eq!(
        own_keys(&take(vec![REMOTE_SLOT.to_owned()])),
        ["slots", "waitedSeconds", "remote", "host", "key", "abandoned"]
    );
}

#[test]
fn a_critical_take_line_puts_its_snapshot_after_waited_seconds_and_before_abandoned() {
    let snapshot = |other_claims: Result<Vec<OtherClaim>, String>| MachineSnapshot {
        counts: HeavyLight { heavy: 2, light: 3 },
        other_held: HeavyLight { heavy: 0, light: 1 },
        other_claims,
    };
    let take = |machine: MachineSnapshot, abandoned: Vec<String>| {
        let event = JournalEvent::Take {
            slots: vec![r"Local\gate-slot-0".to_owned(), r"Local\gate-slot-1".to_owned()],
            waited_seconds: 1.5,
            abandoned,
            machine: Some(machine),
            parent_pid: None,
        };
        journal_line(&identity(), ts(NOW), &event, None, None)
    };
    let claim = OtherClaim {
        pid: 7,
        kind: "light".to_owned(),
        index: 2,
        command: "cargo test".to_owned(),
        file: "gate-light-slot-2.json".to_owned(),
    };

    let line = take(snapshot(Ok(vec![claim])), Vec::new());
    assert_eq!(own_keys(&line), ["slots", "waitedSeconds", "counts", "otherHeld", "otherClaims"]);
    assert!(
        line.ends_with(concat!(
            r#""waitedSeconds":1.5,"counts":{"heavy":2,"light":3},"otherHeld":{"heavy":0,"light":1},"#,
            r#""otherClaims":[{"pid":7,"kind":"light","index":2,"command":"cargo test","file":"gate-light-slot-2.json"}]}"#
        )),
        "{line}"
    );
    assert_eq!(parsed(&take(snapshot(Ok(Vec::new())), Vec::new()))["otherClaims"], json!([]));

    let failed = take(
        snapshot(Err("cannot read the claims".to_owned())),
        vec![r"Local\gate-critical".to_owned()],
    );
    assert_eq!(
        own_keys(&failed),
        ["slots", "waitedSeconds", "counts", "otherHeld", "snapshotError", "abandoned"]
    );
    assert_eq!(parsed(&failed)["snapshotError"], "cannot read the claims");
}

#[test]
fn a_routed_release_line_puts_the_remote_fields_after_the_verdict() {
    let release = |cold: bool| JournalEvent::Release {
        slots: vec![REMOTE_SLOT.to_owned()],
        held_seconds: 12.558,
        status: Some(0),
        facts: Some(RunFacts {
            wall_seconds: 12.4,
            adjusted_seconds: None,
            avg_cores: None,
            peak_cores: None,
            free_cores: None,
            verdict: Verdict::Passed,
        }),
        trace: RunTrace::default(),
        cold,
    };
    let line = journal_line(&identity(), ts(NOW), &release(false), Some(&tag()), None);
    let cold = journal_line(&identity(), ts(NOW), &release(true), Some(&tag()), None);

    assert_eq!(
        own_keys(&cold),
        [
            "slots",
            "heldSeconds",
            "status",
            "wallSeconds",
            "adjustedSeconds",
            "verdict",
            "cold",
            "remote",
            "host",
            "key"
        ],
        "a cold run's field is the last of the release's own"
    );
    assert_eq!(parsed(&cold)["cold"], true);
    assert_eq!(
        own_keys(&line),
        [
            "slots",
            "heldSeconds",
            "status",
            "wallSeconds",
            "adjustedSeconds",
            "verdict",
            "remote",
            "host",
            "key"
        ]
    );
    let mut fields = own_fields(&line);
    fields.retain(|(key, _)| !RUN_FACT_KEYS.contains(&key.as_str()));
    assert_eq!(
        fields,
        own_fields(remote_fixture(7)),
        "the fixture's line has no run facts; the rest is the same"
    );
}

#[test]
fn a_routed_abandon_line_puts_the_remote_fields_after_waited_seconds() {
    let line = journal_line(&identity(), ts(NOW), &JournalEvent::Abandon { waited_seconds: 1.7 }, Some(&tag()), None);

    assert_eq!(own_keys(&line), ["waitedSeconds", "remote", "host", "key"]);
    let value = parsed(&line);
    assert_eq!(
        (&value["waitedSeconds"], &value["remote"], &value["host"], &value["key"]),
        (&json!(1.7), &json!(true), &json!(REMOTE_HOST), &json!(REMOTE_KEY))
    );
}

#[test]
fn a_fallback_line_is_the_one_the_fixture_holds() {
    let fallback = JournalEvent::Fallback {
        reason: FallbackReason::Full,
    };
    let line = journal_line(&identity(), ts(NOW), &fallback, Some(&tag()), None);

    assert_eq!(parsed(&line)["event"], "fallback");
    assert_eq!(own_keys(&line), ["reason", "remote", "host", "key"]);
    assert_eq!(own_fields(&line), own_fields(remote_fixture(4)));
    let under_abandoned_lock = journal_line(&identity(), ts(NOW), &fallback, Some(&tag()), Some(LOCK_NAME));
    assert_eq!(own_keys(&under_abandoned_lock), ["reason", "remote", "host", "key", "abandoned"]);
}

#[test]
fn a_fallback_line_names_each_reason_by_its_word() {
    let reasons = [
        (FallbackReason::Full, "full"),
        (FallbackReason::Busy, "busy"),
        (FallbackReason::Unreachable, "unreachable"),
        (FallbackReason::Parity, "parity"),
        (FallbackReason::Rebooted, "rebooted"),
        (FallbackReason::Tools, "tools"),
        (FallbackReason::Layout, "layout"),
        (FallbackReason::Version, "version"),
        (FallbackReason::Client, "client"),
        (FallbackReason::Inputs, "inputs"),
        (FallbackReason::Cold, "cold"),
        (FallbackReason::Desktop, "desktop"),
        (FallbackReason::Install, "install"),
        (FallbackReason::Lfs, "lfs"),
        (FallbackReason::Root, "root"),
    ];
    for (reason, word) in reasons {
        let line = journal_line(&identity(), ts(NOW), &JournalEvent::Fallback { reason }, Some(&tag()), None);
        assert_eq!(parsed(&line)["reason"], word, "{line}");
    }
}

#[test]
fn a_parent_line_is_the_one_the_fixture_holds() {
    let line = journal_line(&identity(), ts(NOW), &JournalEvent::Parent { parent_pid: 70000 }, None, None);

    assert_eq!(parsed(&line)["event"], "parent");
    assert_eq!(keys(&parsed(&line)), with_common(&["parentPid"]));
    assert_eq!(own_fields(&line), own_fields(remote_fixture(36)));
}

#[test]
fn a_covered_line_carries_the_parent_pid_after_the_identity() {
    let line = journal_line(&identity(), ts(NOW), &JournalEvent::Covered { parent_pid: 70000 }, None, None);

    assert_eq!(parsed(&line)["event"], "covered");
    assert_eq!(keys(&parsed(&line)), with_common(&["parentPid"]));
    assert_eq!(parsed(&line)["parentPid"], json!(70000));
    let under_abandoned_lock = journal_line(&identity(), ts(NOW), &JournalEvent::Covered { parent_pid: 70000 }, None, Some(LOCK_NAME));
    assert_eq!(own_keys(&under_abandoned_lock), ["parentPid", "abandoned"]);
}

#[test]
fn a_line_without_a_remote_tag_has_no_remote_field() {
    let events = [
        JournalEvent::Queue {
            log: "x".to_owned(),
            not_routed: None,
            reason: None,
        },
        JournalEvent::Take {
            slots: Vec::new(),
            waited_seconds: 0.5,
            abandoned: Vec::new(),
            machine: None,
            parent_pid: None,
        },
        release(None),
        JournalEvent::Abandon { waited_seconds: 1.7 },
        JournalEvent::Fallback {
            reason: FallbackReason::Busy,
        },
    ];
    for event in &events {
        let line = journal_line(&identity(), ts(NOW), event, None, None);
        assert!(own_keys(&line).iter().all(|key| !REMOTE_KEYS.contains(&key.as_str())), "{line}");
    }

    let local_take = JournalEvent::Take {
        slots: vec![r"Local\ops253-gate-slot-0".to_owned()],
        waited_seconds: 3.875,
        abandoned: Vec::new(),
        machine: None,
        parent_pid: None,
    };
    let line = journal_line(&identity(), ts(NOW), &local_take, None, None);
    assert_eq!(own_fields(&line), own_fields(remote_fixture(5)), "the local take after a fallback");
}

#[test]
fn line_starts_with_v_then_the_common_keys_in_order() {
    let time = ts("2026-10-03T05:04:39.1209178Z");
    let queue = journal_line(
        &identity(),
        time,
        &JournalEvent::Queue {
            log: r"C:\work\repo\.tmp\f1.log".to_owned(),
            not_routed: None,
            reason: None,
        },
        None,
        None,
    );
    let take = journal_line(
        &identity(),
        time,
        &JournalEvent::Take {
            slots: Vec::new(),
            waited_seconds: 0.0,
            abandoned: Vec::new(),
            machine: None,
            parent_pid: None,
        },
        None,
        None,
    );
    let facts = RunFacts {
        wall_seconds: 0.1,
        adjusted_seconds: Some(0.2),
        avg_cores: None,
        peak_cores: None,
        free_cores: None,
        verdict: Verdict::Passed,
    };
    let released = journal_line(&identity(), time, &release(Some(facts)), None, None);

    assert!(queue.starts_with(r#"{"v":1,"time":"#), "{queue}");
    assert_eq!(keys(&parsed(&queue)), with_common(&["log"]));
    assert_eq!(keys(&parsed(&take)), with_common(&["slots", "waitedSeconds"]));
    assert_eq!(
        keys(&parsed(&released)),
        with_common(&["slots", "heldSeconds", "status", "wallSeconds", "adjustedSeconds", "verdict"])
    );
    let value = parsed(&released);
    assert_eq!(
        (&value["pid"], &value["kind"], &value["cwd"], &value["command"], &value["verdict"]),
        (
            &json!(40268),
            &json!("light"),
            &json!(r"C:\work\repo"),
            &json!("cmd /c exit 0"),
            &json!("passed")
        )
    );
    assert!(take.contains(r#""slots":[],"waitedSeconds":0.0"#), "{take}");
}

#[test]
fn abandon_line_is_the_one_the_reader_fixture_holds() {
    let gate = JournalIdentity {
        pid: 58200,
        process_start: ts("2026-10-03T05:04:43.9Z"),
        kind: "light".to_owned(),
        cwd: r"C:\work\repo".to_owned(),
        command: "cmd /c exit 0".to_owned(),
        commit: false,
    };
    let line = journal_line(
        &gate,
        ts("2026-10-03T05:04:46Z"),
        &JournalEvent::Abandon { waited_seconds: 1.7 },
        None,
        None,
    );

    assert_eq!(keys(&parsed(&line)), with_common(&["waitedSeconds"]));
    assert_eq!(
        line,
        concat!(
            r#"{"v":1,"time":"2026-10-03T05:04:46.0000000Z","event":"abandon","pid":58200,"#,
            r#""processStart":"2026-10-03T05:04:43.9000000Z","kind":"light","cwd":"C:\\work\\repo","#,
            r#""command":"cmd /c exit 0","waitedSeconds":1.7}"#
        ),
        "the abandon line of fixtures/journal.jsonl, which the reader takes as an abandoned run"
    );
}

#[test]
fn time_has_seven_fractional_digits_and_z() {
    let mut gate = identity();
    gate.process_start = ts("2026-10-03T05:04:38Z");
    let line = journal_line(
        &gate,
        ts("2026-10-03T05:04:39.12Z"),
        &JournalEvent::Queue {
            log: "x".to_owned(),
            not_routed: None,
            reason: None,
        },
        None,
        None,
    );
    let value = parsed(&line);

    assert_eq!(value["time"], "2026-10-03T05:04:39.1200000Z");
    assert_eq!(value["processStart"], "2026-10-03T05:04:38.0000000Z");

    let exact = journal_line(
        &identity(),
        ts("2026-10-03T05:04:39.1209178Z"),
        &JournalEvent::Queue {
            log: "x".to_owned(),
            not_routed: None,
            reason: None,
        },
        None,
        None,
    );
    assert_eq!(parsed(&exact)["time"], "2026-10-03T05:04:39.1209178Z");
    assert_eq!(parsed(&exact)["processStart"], "2026-10-03T05:04:38.7424554Z");
}

#[test]
fn release_without_run_facts_writes_nulls_and_unknown() {
    let mut event = release(None);
    if let JournalEvent::Release { status, .. } = &mut event {
        *status = None;
    }
    let line = journal_line(&identity(), ts(NOW), &event, None, None);

    assert!(
        line.ends_with(r#""heldSeconds":0.105,"status":null,"wallSeconds":null,"adjustedSeconds":null,"verdict":"unknown"}"#),
        "{line}"
    );
}

#[test]
fn seconds_round_to_three_decimals() {
    let take = journal_line(
        &identity(),
        ts(NOW),
        &JournalEvent::Take {
            slots: Vec::new(),
            waited_seconds: 1.234_56,
            abandoned: Vec::new(),
            machine: None,
            parent_pid: None,
        },
        None,
        None,
    );
    let released = journal_line(
        &identity(),
        ts(NOW),
        &JournalEvent::Release {
            slots: Vec::new(),
            held_seconds: 0.000_4,
            status: Some(3),
            facts: Some(RunFacts {
                wall_seconds: 10.308_49,
                adjusted_seconds: Some(2.999_9),
                avg_cores: None,
                peak_cores: None,
                free_cores: None,
                verdict: Verdict::Failed,
            }),
            trace: RunTrace::default(),
            cold: false,
        },
        None,
        None,
    );

    assert!(take.ends_with(r#""waitedSeconds":1.235}"#), "{take}");
    assert!(
        released.ends_with(r#""heldSeconds":0.0,"status":3,"wallSeconds":10.308,"adjustedSeconds":3.0,"verdict":"failed"}"#),
        "{released}"
    );
}

#[test]
fn a_take_under_a_parent_names_it_after_waited_seconds() {
    let take = |parent_pid: Option<u32>| {
        let event = JournalEvent::Take {
            slots: vec![r"Local\gate-slot-0".to_owned()],
            waited_seconds: 0.25,
            abandoned: Vec::new(),
            machine: None,
            parent_pid,
        };
        journal_line(&identity(), ts(NOW), &event, None, None)
    };

    let under = take(Some(4242));
    assert_eq!(own_keys(&under), ["slots", "waitedSeconds", "parentPid"]);
    assert_eq!(parsed(&under)["parentPid"], 4242);
    assert_eq!(own_keys(&take(None)), ["slots", "waitedSeconds"], "a gate under no parent names none");
}

#[test]
fn release_with_cores_writes_them_after_adjusted_seconds_to_one_decimal() {
    let facts = RunFacts {
        wall_seconds: 12.0,
        adjusted_seconds: Some(11.0),
        avg_cores: Some(7.44),
        peak_cores: Some(14.56),
        free_cores: Some(0.25),
        verdict: Verdict::Passed,
    };
    let released = journal_line(&identity(), ts(NOW), &release(Some(facts)), None, None);

    assert_eq!(
        own_keys(&released),
        [
            "slots",
            "heldSeconds",
            "status",
            "wallSeconds",
            "adjustedSeconds",
            "avgCores",
            "peakCores",
            "freeCores",
            "verdict"
        ]
    );
    assert!(
        released.ends_with(r#""adjustedSeconds":11.0,"avgCores":7.4,"peakCores":14.6,"freeCores":0.2,"verdict":"passed"}"#),
        "{released}"
    );
}

#[test]
fn release_without_cores_omits_them() {
    let facts = RunFacts {
        wall_seconds: 12.0,
        adjusted_seconds: Some(11.0),
        avg_cores: None,
        peak_cores: None,
        free_cores: None,
        verdict: Verdict::Passed,
    };
    let measured = journal_line(&identity(), ts(NOW), &release(Some(facts)), None, None);
    let unknown = journal_line(&identity(), ts(NOW), &release(None), None, None);

    for line in [measured, unknown] {
        assert!(
            ["avgCores", "peakCores", "freeCores"].iter().all(|key| parsed(&line).get(key).is_none()),
            "{line}"
        );
    }
}

/// A trace with every field set, as a routed run's would not be: the writer writes whatever it is given.
pub fn full_trace() -> RunTrace {
    RunTrace {
        cpu_seconds: Some(41.26),
        processes: Some(37),
        peak_processes: Some(9),
        priority: Some("belowNormal"),
        load_free_share: Some(0.734),
        children: vec![
            TracedChild {
                cmd: "dotnet build".to_owned(),
                image: Some(r"C:\Program Files\dotnet\dotnet.exe".to_owned()),
                start: 1.04,
                end: Some(30.66),
                cpu: 28.04,
            },
            TracedChild {
                cmd: "node.exe".to_owned(),
                image: None,
                start: 2.0,
                end: None,
                cpu: 1.0,
            },
        ],
        phases: vec![("command", 12.0004), ("push", 1.5)],
    }
}

#[test]
fn a_release_s_trace_is_written_after_the_verdict_and_before_cold() {
    let facts = RunFacts {
        wall_seconds: 12.0,
        adjusted_seconds: Some(11.0),
        avg_cores: None,
        peak_cores: None,
        free_cores: None,
        verdict: Verdict::Passed,
    };
    let event = JournalEvent::Release {
        slots: vec![r"Local\gate-light-slot-0".to_owned()],
        held_seconds: 0.105,
        status: Some(0),
        facts: Some(facts),
        trace: full_trace(),
        cold: true,
    };
    let released = journal_line(&identity(), ts(NOW), &event, None, None);

    assert_eq!(
        own_keys(&released),
        [
            "slots",
            "heldSeconds",
            "status",
            "wallSeconds",
            "adjustedSeconds",
            "verdict",
            "cpuSeconds",
            "processes",
            "peakProcesses",
            "priority",
            "loadFreeShare",
            "children",
            "phases",
            "cold"
        ]
    );
    let line = parsed(&released);
    assert_eq!(
        (
            &line["cpuSeconds"],
            &line["processes"],
            &line["peakProcesses"],
            &line["priority"],
            &line["loadFreeShare"]
        ),
        (&json!(41.3), &json!(37), &json!(9), &json!("belowNormal"), &json!(0.73))
    );
    assert_eq!(
        line["children"],
        json!([
            {"cmd": "dotnet build", "image": r"C:\Program Files\dotnet\dotnet.exe", "start": 1.0, "end": 30.7, "cpu": 28.0},
            {"cmd": "node.exe", "start": 2.0, "end": null, "cpu": 1.0}
        ])
    );
    assert!(
        released.contains(r#"{"cmd":"dotnet build","image":"C:\\Program Files\\dotnet\\dotnet.exe","start":1.0,"end":30.7,"cpu":28.0}"#),
        "a child's fields are cmd, image, start, end and cpu, in that order: {released}"
    );
    assert!(
        released.contains(r#"{"cmd":"node.exe","start":2.0,"#),
        "an unread image is left out: {released}"
    );
    assert_eq!(line["phases"], json!({"command": 12.0, "push": 1.5}));
}

#[test]
fn an_empty_trace_writes_none_of_its_fields() {
    let released = journal_line(&identity(), ts(NOW), &release(None), None, None);

    let trace_keys = [
        "cpuSeconds",
        "processes",
        "peakProcesses",
        "priority",
        "loadFreeShare",
        "children",
        "phases",
    ];
    assert!(trace_keys.iter().all(|key| parsed(&released).get(key).is_none()), "{released}");
}

#[test]
fn seconds_round_a_midpoint_to_even_as_dotnet_does() {
    let waited = |seconds: f64| {
        let line = journal_line(
            &identity(),
            ts(NOW),
            &JournalEvent::Take {
                slots: Vec::new(),
                waited_seconds: seconds,
                abandoned: Vec::new(),
                machine: None,
                parent_pid: None,
            },
            None,
            None,
        );
        parsed(&line)["waitedSeconds"].clone()
    };

    assert_eq!(waited(0.0625), json!(0.062), "a midpoint goes down to the even digit");
    assert_eq!(waited(0.0635), json!(0.064), "a midpoint goes up to the even digit");
    assert_eq!(waited(4.567_89), json!(4.568));
}

#[test]
fn append_ends_every_line_with_lf_and_writes_no_bom() {
    let root = fresh_dir("append_ends_every_line_with_lf_and_writes_no_bom");

    for line in [r#"{"v":1,"event":"queue","note":"é"}"#, r#"{"v":1,"event":"take"}"#] {
        let appended = append_line(&root, &FakeLock::Free, fixed(line), ts(NOW));
        assert!(appended.written && appended.notes.is_empty(), "{appended:?}");
    }

    let bytes = fs::read(root.join("events.jsonl")).expect("the journal was written");
    assert_eq!(
        String::from_utf8(bytes).expect("the journal is UTF-8"),
        "{\"v\":1,\"event\":\"queue\",\"note\":\"é\"}\n{\"v\":1,\"event\":\"take\"}\n",
        "no byte order mark, one LF after each line"
    );
}

#[test]
fn torn_last_line_gets_a_line_feed_first() {
    let root = fresh_dir("torn_last_line_gets_a_line_feed_first");
    fs::write(root.join("events.jsonl"), "{\"event\":\"queue\"}\n{\"event\":\"ta").expect("the journal can be seeded");

    let appended = append_line(&root, &FakeLock::Free, fixed(r#"{"event":"release"}"#), ts(NOW));

    assert!(appended.written, "{appended:?}");
    assert_eq!(
        fs::read_to_string(root.join("events.jsonl")).expect("the journal can be read"),
        "{\"event\":\"queue\"}\n{\"event\":\"ta\n{\"event\":\"release\"}\n"
    );
}

#[test]
fn journal_below_the_cap_does_not_roll() {
    let root = fresh_dir("journal_below_the_cap_does_not_roll");
    let seed = seed_lines(JOURNAL_CAP_BYTES - 1);
    fs::write(root.join("events.jsonl"), text(&seed)).expect("the journal can be seeded");

    let appended = append_line(&root, &FakeLock::Free, fixed(r#"{"event":"take"}"#), ts(NOW));

    assert!(appended.written && appended.notes.is_empty(), "{appended:?}");
    assert!(!root.join("events.1.jsonl").exists(), "a journal under the cap stays where it is");
    assert!(!root.join("archive").exists(), "nothing rolled, so nothing is archived");
    assert_eq!(lines_of(&root.join("events.jsonl")).len(), seed.len() + 1);
}

#[test]
fn journal_at_the_cap_rolls_before_the_append() {
    let root = fresh_dir("journal_at_the_cap_rolls_before_the_append");
    let seed = text(&seed_lines(JOURNAL_CAP_BYTES));
    fs::write(root.join("events.jsonl"), &seed).expect("the journal can be seeded");

    let appended = append_line(&root, &FakeLock::Free, fixed(r#"{"event":"take"}"#), ts(NOW));

    assert!(appended.written && appended.notes.is_empty(), "{appended:?}");
    assert_eq!(fs::read_to_string(root.join("events.1.jsonl")).expect("the journal was rolled"), seed);
    assert_eq!(
        fs::read_to_string(root.join("events.jsonl")).expect("a new journal was begun"),
        "{\"event\":\"take\"}\n"
    );
}

#[test]
fn roll_replaces_an_existing_events_1() {
    let root = fresh_dir("roll_replaces_an_existing_events_1");
    let seed = text(&seed_lines(JOURNAL_CAP_BYTES));
    fs::write(root.join("events.jsonl"), &seed).expect("the journal can be seeded");
    fs::write(root.join("events.1.jsonl"), "{\"event\":\"older\"}\n").expect("an older roll can be seeded");

    let appended = append_line(&root, &FakeLock::Free, fixed(r#"{"event":"take"}"#), ts(NOW));

    assert!(appended.written && appended.notes.is_empty(), "{appended:?}");
    assert_eq!(fs::read_to_string(root.join("events.1.jsonl")).expect("the journal was rolled"), seed);
}

#[test]
fn failed_roll_appends_in_place_and_says_so() {
    let root = fresh_dir("failed_roll_appends_in_place_and_says_so");
    let seed = text(&seed_lines(JOURNAL_CAP_BYTES));
    let journal = root.join("events.jsonl");
    let rolled = root.join("events.1.jsonl");
    fs::write(&journal, &seed).expect("the journal can be seeded");
    // A folder where the rolled journal goes: no rename replaces it on either platform.
    fs::create_dir_all(&rolled).expect("the blocking folder can be made");
    fs::write(rolled.join("keep"), "x").expect("the blocking folder can hold a file");

    let appended = append_line(&root, &FakeLock::Free, fixed(r#"{"event":"take"}"#), ts(NOW));

    assert!(appended.written, "{appended:?}");
    assert_eq!(
        fs::read_to_string(&journal).expect("the journal can be read"),
        format!("{seed}{{\"event\":\"take\"}}\n"),
        "the line is appended to the journal as it is"
    );
    assert_eq!(appended.notes.len(), 1, "{appended:?}");
    assert!(matches!(appended.notes[0], JournalNote::RollFailed { .. }), "{appended:?}");
    let said = appended.notes[0].to_string();
    let prefix = format!("could not move {} over {}: ", journal.display(), rolled.display());
    assert!(said.starts_with(&prefix), "{said}");
    assert!(said.ends_with("; appended to it as it is"), "{said}");
    assert!(!root.join("archive").exists(), "nothing rolled, so nothing is archived");
}

#[test]
fn lock_timeout_skips_the_line_and_reports_it() {
    let root = fresh_dir("lock_timeout_skips_the_line_and_reports_it").join("root");

    let appended = append_line(&root, &FakeLock::Busy, fixed(r#"{"event":"take"}"#), ts(NOW));

    assert!(!appended.written && !appended.abandoned, "{appended:?}");
    assert!(!root.exists(), "nothing is touched without the lock");
    let said: Vec<String> = appended.notes.iter().map(ToString::to_string).collect();
    assert_eq!(
        said,
        [format!(
            r"could not write the journal {} (Local\gate-test-journal was not free within 2 s)",
            root.join("events.jsonl").display()
        )]
    );
}

#[test]
fn abandoned_lock_still_writes() {
    let root = fresh_dir("abandoned_lock_still_writes");

    let appended = append_line(&root, &FakeLock::Abandoned, fixed(r#"{"event":"take"}"#), ts(NOW));

    assert!(appended.written && appended.abandoned && appended.notes.is_empty(), "{appended:?}");
    assert_eq!(lines_of(&root.join("events.jsonl")), [r#"{"event":"take"}"#]);
}

#[test]
fn a_take_with_abandoned_slots_lists_them_after_waited_seconds() {
    let take = journal_line(
        &identity(),
        ts(NOW),
        &JournalEvent::Take {
            slots: vec![r"Local\gate-1a2b3c4d-slot-0".to_owned()],
            waited_seconds: 0.5,
            abandoned: vec![r"Local\gate-1a2b3c4d-slot-0".to_owned()],
            machine: None,
            parent_pid: None,
        },
        None,
        None,
    );

    assert!(
        take.ends_with(r#""waitedSeconds":0.5,"abandoned":["Local\\gate-1a2b3c4d-slot-0"]}"#),
        "{take}"
    );
}

#[test]
fn a_take_with_no_abandoned_slots_has_no_abandoned_key() {
    let take = journal_line(
        &identity(),
        ts(NOW),
        &JournalEvent::Take {
            slots: Vec::new(),
            waited_seconds: 0.5,
            abandoned: Vec::new(),
            machine: None,
            parent_pid: None,
        },
        None,
        None,
    );

    assert!(
        !parsed(&take).as_object().expect("a line is an object").contains_key("abandoned"),
        "{take}"
    );
    assert!(take.ends_with(r#""waitedSeconds":0.5}"#), "{take}");
}

#[test]
fn journal_line_lists_the_lock_on_a_queue_line() {
    let line = journal_line(
        &identity(),
        ts(NOW),
        &JournalEvent::Queue {
            log: "x".to_owned(),
            not_routed: None,
            reason: None,
        },
        None,
        Some(r"Local\gate-test-journal"),
    );

    assert!(line.ends_with(r#""log":"x","abandoned":["Local\\gate-test-journal"]}"#), "{line}");
}

#[test]
fn journal_line_puts_the_lock_after_the_slots_on_a_take() {
    let line = journal_line(
        &identity(),
        ts(NOW),
        &JournalEvent::Take {
            slots: vec![r"Local\gate-1a2b3c4d-slot-0".to_owned()],
            waited_seconds: 0.5,
            abandoned: vec![r"Local\gate-1a2b3c4d-slot-0".to_owned()],
            machine: None,
            parent_pid: None,
        },
        None,
        Some(r"Local\gate-test-journal"),
    );

    assert!(
        line.ends_with(r#""waitedSeconds":0.5,"abandoned":["Local\\gate-1a2b3c4d-slot-0","Local\\gate-test-journal"]}"#),
        "{line}"
    );
}

#[test]
fn a_line_appended_under_an_abandoned_journal_lock_lists_the_lock() {
    let root = fresh_dir("a_line_appended_under_an_abandoned_journal_lock_lists_the_lock");

    let appended = append_line(
        &root,
        &FakeLock::Abandoned,
        |abandoned| {
            journal_line(
                &identity(),
                ts(NOW),
                &JournalEvent::Queue {
                    log: "x".to_owned(),
                    not_routed: None,
                    reason: None,
                },
                None,
                abandoned,
            )
        },
        ts(NOW),
    );

    assert!(appended.written && appended.abandoned && appended.notes.is_empty(), "{appended:?}");
    let written = &lines_of(&root.join("events.jsonl"))[0];
    assert!(written.ends_with(r#""abandoned":["Local\\gate-test-journal"]}"#), "{written}");
}

#[test]
fn an_abandoned_lock_on_a_take_with_abandoned_slots_extends_its_list() {
    let root = fresh_dir("an_abandoned_lock_on_a_take_with_abandoned_slots_extends_its_list");
    let take = |abandoned: Option<&str>| {
        journal_line(
            &identity(),
            ts(NOW),
            &JournalEvent::Take {
                slots: vec![r"Local\gate-1a2b3c4d-slot-0".to_owned()],
                waited_seconds: 0.5,
                abandoned: vec![r"Local\gate-1a2b3c4d-slot-0".to_owned()],
                machine: None,
                parent_pid: None,
            },
            None,
            abandoned,
        )
    };

    let appended = append_line(&root, &FakeLock::Abandoned, take, ts(NOW));

    assert!(appended.written && appended.abandoned, "{appended:?}");
    let written = &lines_of(&root.join("events.jsonl"))[0];
    assert!(
        written.ends_with(r#""abandoned":["Local\\gate-1a2b3c4d-slot-0","Local\\gate-test-journal"]}"#),
        "{written}"
    );
}

#[test]
fn missing_root_is_created() {
    let root = fresh_dir("missing_root_is_created").join("not").join("yet");

    let appended = append_line(&root, &FakeLock::Free, fixed(r#"{"event":"queue"}"#), ts(NOW));

    assert!(appended.written && appended.notes.is_empty(), "{appended:?}");
    assert_eq!(lines_of(&root.join("events.jsonl")), [r#"{"event":"queue"}"#]);
}

#[test]
fn rollover_while_others_append_loses_no_line() {
    const THREADS: usize = 8;
    const LINES: usize = 40;
    let root = fresh_dir("rollover_while_others_append_loses_no_line");
    let seed = seed_lines(JOURNAL_CAP_BYTES - 1);
    fs::write(root.join("events.jsonl"), text(&seed)).expect("the journal can be seeded");
    let lock = Arc::new(ThreadLock::default());
    let (done, finished) = mpsc::channel();
    let threads: Vec<_> = (0..THREADS)
        .map(|thread| {
            let (lock, root, done) = (Arc::clone(&lock), root.clone(), done.clone());
            thread::spawn(move || {
                for number in 0..LINES {
                    let event = if number.is_multiple_of(2) { "release" } else { "queue" };
                    let line = json!({"v": 1, "event": event, "thread": thread, "number": number}).to_string();
                    let appended = append_line(&root, &*lock, fixed(&line), ts(NOW));
                    assert!(appended.written && appended.notes.is_empty(), "{appended:?}");
                }
                done.send(()).expect("the test waits for every thread");
            })
        })
        .collect();
    drop(done);
    for _ in 0..THREADS {
        finished.recv_timeout(HANG_GUARD).expect("every appending thread ends");
    }
    for thread in threads {
        thread.join().expect("no appending thread panicked");
    }

    let rolled = lines_of(&root.join("events.1.jsonl"));
    let current = lines_of(&root.join("events.jsonl"));
    assert_eq!(rolled[..seed.len()], seed[..], "the seed was rolled whole");
    let mut seen: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for line in rolled.iter().chain(&current) {
        let value = parsed(line);
        if let (Some(thread), Some(number)) = (value["thread"].as_u64(), value["number"].as_u64()) {
            seen.entry(thread).or_default().push(number);
        } else {
            assert!(value["seed"].is_u64(), "a line is a seed line or an appended one: {line}");
        }
    }
    assert!(current.iter().all(|line| parsed(line)["seed"].is_null()), "exactly one roll");
    let expected: Vec<u64> = (0..LINES as u64).collect();
    assert_eq!(seen.len(), THREADS);
    for (thread, numbers) in &seen {
        assert_eq!(numbers, &expected, "thread {thread}'s lines, each once and in order");
    }
    let archives = archive_names(&root);
    assert_eq!(archives.len(), 1, "{archives:?}");
    assert_eq!(
        gunzip(&root.join("archive").join(&archives[0])),
        text(&release_lines(&rolled)),
        "the archive holds the rolled file's release lines"
    );
}
