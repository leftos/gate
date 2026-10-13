//! A run held between its take and its release by a command that waits for a file, and the lines a gate says while it
//! is still running.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::support::{Case, FAKE_TOOL, assert_in_order, run_words, shown, start, wait_for_journal_line};

fn let_go(go: &Path) {
    fs::write(go, "").expect("the go file can be made");
}

#[test]
fn a_held_run_stays_up_until_its_file_appears() {
    let case = Case::new("a_held_run_stays_up_until_its_file_appears");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let journal = case.root().join("events.jsonl");
    let mut running = case.start(&run_words("out.log", &[], &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));

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
fn a_gate_says_its_problem_lines_while_it_runs() {
    let case = Case::new("a_gate_says_its_problem_lines_while_it_runs");
    let blocker = case.folder().join("blocker");
    fs::write(&blocker, "").expect("the blocking file can be made");
    let root = blocker.join("root");
    let root_word = shown(&root);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let words = run_words("out.log", &["--state-dir", &root_word], &[FAKE_TOOL, "wait", &go_word, "exit", "0"]);
    let mut running = start(case.gate_without_state_dir(&words));
    let line = format!("gate: could not write the journal {} (", shown(&root.join("events.jsonl")));
    let ticket = format!("gate: cannot write the ticket {}", shown(&root.join("waiting")));
    let claim = format!(
        "gate: cannot write the claim {}: ",
        shown(&root.join("slots").join("gate-light-slot-0.json"))
    );
    let rebuild = format!("gate: cannot rebuild {} (", shown(&root.join("durations.json")));

    running.wait_for_stderr(&claim);
    assert!(running.is_running(), "the lines are said while the gate holds");

    let_go(&go);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("gate: passed in "), "the run passes: {outcome:?}");
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    assert_eq!(lines.len(), 4, "each problem is said once: {outcome:?}");
    assert!(
        lines[0].starts_with(&line) && lines[0].ends_with(')'),
        "the root that cannot be made is said once for the run's three journal lines: {outcome:?}"
    );
    assert!(lines[1].starts_with(&ticket), "then the ticket that cannot be written: {outcome:?}");
    assert!(lines[2].starts_with(&claim), "then the claim that cannot be written: {outcome:?}");
    assert!(
        lines[3].starts_with(&rebuild) && lines[3].ends_with("; expected times are unavailable"),
        "then, once the run has released, the durations store that cannot be rebuilt: {outcome:?}"
    );
}
