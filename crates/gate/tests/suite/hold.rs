//! The `--test-hold-until` seam: a seam run held between its take and its release, and the lines a gate says while it
//! is still running.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::support::{Case, assert_in_order, shown, start, wait_for_journal_line};

/// The words of a light seam run that exits 0 and holds until `go` exists, `extra` before `--`.
fn held_run(go: &Path, extra: &[String]) -> Vec<String> {
    let mut words: Vec<String> = ["run", "--slot", "light", "--timeout", "5", "--log", "out.log"]
        .iter()
        .map(|word| (*word).to_owned())
        .collect();
    words.extend_from_slice(extra);
    words.extend(["--test-journal-run".to_owned(), "0".to_owned(), "--test-hold-until".to_owned(), shown(go)]);
    words.extend(["--", "cmd", "/c", "exit", "0"].iter().map(|word| (*word).to_owned()));
    words
}

fn refs(words: &[String]) -> Vec<&str> {
    words.iter().map(String::as_str).collect()
}

fn let_go(go: &Path) {
    fs::write(go, "").expect("the go file can be made");
}

#[test]
fn a_held_seam_run_stays_up_until_its_file_appears() {
    let case = Case::new("a_held_seam_run_stays_up_until_its_file_appears");
    let go = case.root().join("go");
    let journal = case.root().join("events.jsonl");
    let mut running = case.start(&refs(&held_run(&go, &[])));

    wait_for_journal_line(&case.root(), r#""event":"take""#);
    assert!(running.is_running(), "the gate holds after its take");
    let held = fs::read_to_string(&journal).expect("the journal was written");
    assert!(!held.contains(r#""event":"release""#), "no release before the file appears:\n{held}");

    let_go(&go);
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    let released = fs::read_to_string(&journal).expect("the journal was written");
    assert_in_order(&released, &[r#""event":"take""#, r#""event":"release""#]);
    let release: Value = serde_json::from_str(released.lines().last().expect("the journal has lines")).expect("a journal line is JSON");
    let held_seconds = release["heldSeconds"].as_f64().expect("the release has its held seconds");
    assert!(held_seconds > 0.0, "the observed hold counts as held time: {release}");
}

#[test]
fn a_hold_whose_path_cannot_be_checked_ends_with_a_line() {
    let case = Case::new("a_hold_whose_path_cannot_be_checked_ends_with_a_line");
    let go = case.root().join("go*");

    let outcome = case.run(&refs(&held_run(&go, &[])));

    assert_eq!(
        (outcome.code, outcome.stdout.as_str()),
        (Some(0), ""),
        "the run goes on to its release: {outcome:?}"
    );
    let line = format!("gate: cannot check {}: ", shown(&go));
    assert!(
        outcome.stderr.starts_with(&line) && outcome.stderr.lines().count() == 1,
        "the unreadable path is said once: {outcome:?}"
    );
    let journal = fs::read_to_string(case.root().join("events.jsonl")).expect("the journal was written");
    assert_in_order(&journal, &[r#""event":"take""#, r#""event":"release""#]);
}

#[test]
fn a_gate_says_its_problem_lines_while_it_runs() {
    let case = Case::new("a_gate_says_its_problem_lines_while_it_runs");
    let blocker = case.root().join("blocker");
    fs::write(&blocker, "").expect("the blocking file can be made");
    let root = blocker.join("root");
    let go = case.root().join("go");
    let words = held_run(&go, &["--state-dir".to_owned(), shown(&root)]);
    let mut running = start(case.gate_without_state_dir(&refs(&words)));
    let line = format!("gate: could not write the journal {} (", shown(&root.join("events.jsonl")));

    running.wait_for_stderr(&line);
    assert!(running.is_running(), "the line is said while the gate holds");

    let_go(&go);
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stdout.as_str()), (Some(0), ""), "{outcome:?}");
    assert!(
        outcome.stderr.starts_with(&line) && outcome.stderr.ends_with(")\n") && outcome.stderr.lines().count() == 1,
        "the root that cannot be made is said once for the run's three lines: {outcome:?}"
    );
}
