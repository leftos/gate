//! The journal a run writes through `--test-journal-run`: its v1 lines in order with their verdict, the state root's own
//! journal mutex, and the rollover and archive while other gates append.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::thread;
use std::time::Duration;

use flate2::read::GzDecoder;
use gate_state::{JOURNAL_CAP_BYTES, KernelObject, StateRoot};
use gate_win32::NamedMutex;
use serde_json::Value;

use crate::support::{Case, Outcome, assert_in_order, finish, shown};

/// The words of a light run that journals itself and exits with `status`, the command after `--` last.
fn journal_run(status: &'static str) -> Vec<&'static str> {
    vec![
        "run",
        "--slot",
        "light",
        "--timeout",
        "5",
        "--log",
        "out.log",
        "--test-journal-run",
        status,
        "--",
        "cmd",
        "/c",
        "exit",
        status,
    ]
}

/// JSON lines of `total` bytes in all, each ending in a line feed, every thousandth a `release` and the rest `queue`: a
/// handful of release lines near the cap, so the archive written inside the journal mutex's hold is little work.
fn seed_lines(total: u64) -> Vec<String> {
    const LINE: usize = 1000;
    const RELEASE_EVERY: usize = 1000;
    let mut left = usize::try_from(total).expect("a seed size fits in memory");
    let mut lines = Vec::new();
    while left > 0 {
        let size = if left >= 2 * LINE { LINE } else { left };
        let event = if lines.len().is_multiple_of(RELEASE_EVERY) { "release" } else { "queue" };
        let base = format!(r#"{{"v":1,"event":"{event}","seed":{},"pad":""}}"#, lines.len());
        let pad = "x".repeat(size - 1 - base.len());
        lines.push(format!(r#"{{"v":1,"event":"{event}","seed":{},"pad":"{pad}"}}"#, lines.len()));
        left -= size;
    }
    lines
}

fn text(lines: &[String]) -> String {
    lines.iter().flat_map(|line| [line.as_str(), "\n"]).collect()
}

fn lines_of(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .expect("the file can be read")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn parsed(line: &str) -> Value {
    serde_json::from_str(line).expect("a journal line is JSON")
}

fn release_lines(lines: &[String]) -> Vec<String> {
    lines.iter().filter(|line| parsed(line)["event"] == "release").cloned().collect()
}

fn gunzip(path: &Path) -> String {
    let mut text = String::new();
    GzDecoder::new(File::open(path).expect("the archive can be opened"))
        .read_to_string(&mut text)
        .expect("the archive is gzip text");
    text
}

/// Whether `name` is `events-<yyyyMMddTHHmmssZ>.jsonl.gz`.
fn is_archive_name(name: &str) -> bool {
    name.strip_prefix("events-")
        .and_then(|rest| rest.strip_suffix(".jsonl.gz"))
        .is_some_and(|stamp| {
            stamp.len() == 16
                && stamp.bytes().enumerate().all(|(index, byte)| match index {
                    8 => byte == b'T',
                    15 => byte == b'Z',
                    _ => byte.is_ascii_digit(),
                })
        })
}

/// The one archive in the case root, by name.
fn only_archive(case: &Case) -> String {
    let names: Vec<String> = fs::read_dir(case.root().join("archive"))
        .expect("the archive folder exists")
        .map(|entry| entry.expect("an archive entry can be read").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    names[0].clone()
}

/// A journal at one byte under the cap, in the case root.
fn seed_near_cap(case: &Case) -> Vec<String> {
    let seed = seed_lines(JOURNAL_CAP_BYTES - 1);
    fs::write(case.root().join("events.jsonl"), text(&seed)).expect("the journal can be seeded");
    seed
}

fn assert_quiet(outcome: &Outcome, code: i32) {
    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(code), ""), "{outcome:?}");
}

#[test]
fn journal_run_writes_queue_take_release_in_order() {
    let case = Case::new("journal_run_writes_queue_take_release_in_order");

    let outcome = case.run(&journal_run("0"));

    assert_quiet(&outcome, 0);
    let journal = fs::read_to_string(case.root().join("events.jsonl")).expect("the journal was written");
    assert_in_order(
        &journal,
        &[
            r#"{"v":1,"time":""#,
            r#""event":"queue""#,
            "\n",
            r#"{"v":1,"time":""#,
            r#""event":"take""#,
            r#""slots":[],"waitedSeconds":0.0}"#,
            "\n",
            r#"{"v":1,"time":""#,
            r#""event":"release""#,
            r#""status":0,"#,
            r#""adjustedSeconds":null,"verdict":"passed"}"#,
            "\n",
        ],
    );
    let lines: Vec<Value> = journal.lines().map(parsed).collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        assert_eq!(
            (&line["pid"], line["kind"].as_str(), line["cwd"].as_str(), line["command"].as_str()),
            (&lines[0]["pid"], Some("light"), Some(shown(&case.root()).as_str()), Some("cmd /c exit 0"))
        );
    }
    assert_eq!(lines[0]["log"], shown(&case.root().join("out.log")));
}

#[test]
fn journal_run_with_status_3_is_failed_and_exits_3() {
    let case = Case::new("journal_run_with_status_3_is_failed_and_exits_3");

    let outcome = case.run(&journal_run("3"));

    assert_quiet(&outcome, 3);
    let lines = lines_of(&case.root().join("events.jsonl"));
    let release = parsed(lines.last().expect("the journal has lines"));
    assert_eq!(
        (&release["event"], &release["status"], &release["verdict"]),
        (&Value::from("release"), &Value::from(3), &Value::from("failed"))
    );
}

#[test]
fn journal_lines_carry_the_case_root_mutex_not_the_machine_s() {
    let case = Case::new("journal_lines_carry_the_case_root_mutex");
    let name = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::Journal);
    assert_ne!(name, r"Local\gate-journal");
    let mutex = NamedMutex::open(&name).expect("the case root's journal mutex opens");
    let held = mutex.acquire(Duration::from_secs(5)).expect("the case root's journal mutex is free");

    let blocked = case.run(&journal_run("0"));
    drop(held);

    assert_eq!(blocked.code, Some(0), "the gate runs on without its journal: {blocked:?}");
    assert_eq!(
        blocked.stderr,
        format!(
            "gate: could not write the journal {} ({name} was not free within 2 s)\n",
            shown(&case.root().join("events.jsonl"))
        ),
        "the gate waits on the case root's mutex, and says so once for its three lines"
    );
    assert!(!case.root().join("events.jsonl").exists());
    assert_quiet(&case.run(&journal_run("0")), 0);
    assert_eq!(lines_of(&case.root().join("events.jsonl")).len(), 3);
}

#[test]
fn rollover_while_another_gate_appends_loses_no_line() {
    const GATES: usize = 6;
    let case = Case::new("rollover_while_another_gate_appends_loses_no_line");
    let seed = seed_near_cap(&case);
    let seeded_releases = release_lines(&seed);
    assert!(
        (1..=10).contains(&seeded_releases.len()),
        "a handful of seeded release lines: {}",
        seeded_releases.len()
    );
    let commands: Vec<_> = (0..GATES).map(|_| case.gate(&journal_run("0"))).collect();

    let runs: Vec<_> = commands.into_iter().map(|command| thread::spawn(move || finish(command))).collect();
    let outcomes: Vec<Outcome> = runs.into_iter().map(|run| run.join().expect("a gate's runner ends")).collect();

    for outcome in &outcomes {
        assert_quiet(outcome, 0);
    }
    let rolled = lines_of(&case.root().join("events.1.jsonl"));
    let current = lines_of(&case.root().join("events.jsonl"));
    assert_eq!(rolled[..seed.len()], seed[..], "the seed was rolled whole");
    assert!(current.iter().all(|line| parsed(line)["seed"].is_null()), "exactly one roll");
    let mut events: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for line in rolled[seed.len()..].iter().chain(&current) {
        let value = parsed(line);
        let pid = value["pid"].as_u64().expect("a gate's line has its pid");
        events
            .entry(pid)
            .or_default()
            .push(value["event"].as_str().expect("a line names its event").to_owned());
    }
    assert_eq!(events.len(), GATES, "{events:?}");
    for (pid, lines) in &events {
        assert_eq!(lines, &["queue", "take", "release"], "gate {pid}'s lines, each once and in order");
    }
    let archive = only_archive(&case);
    assert!(is_archive_name(&archive), "{archive}");
    let mut archived = seeded_releases;
    archived.extend(release_lines(&rolled[seed.len()..]));
    assert_eq!(
        gunzip(&case.root().join("archive").join(&archive)),
        text(&archived),
        "the archive holds the seeded release lines and any run's release that went in before the roll"
    );
}

#[test]
fn archive_is_written_on_roll() {
    let case = Case::new("archive_is_written_on_roll");
    let seed = seed_near_cap(&case);

    let outcome = case.run(&journal_run("0"));

    assert_quiet(&outcome, 0);
    let rolled = lines_of(&case.root().join("events.1.jsonl"));
    assert_eq!(
        rolled.len(),
        seed.len() + 1,
        "the queue line went in under the cap; the take rolled the journal"
    );
    let archive = only_archive(&case);
    assert!(is_archive_name(&archive), "{archive}");
    let archived = gunzip(&case.root().join("archive").join(&archive));
    assert!(archived.lines().all(|line| parsed(line)["event"] == "release"), "release lines only");
    assert_eq!(archived, text(&release_lines(&seed)));
    assert_eq!(lines_of(&case.root().join("events.jsonl")).len(), 2, "the take and the release");
}
