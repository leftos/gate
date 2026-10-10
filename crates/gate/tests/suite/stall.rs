//! The watch: a command with no output and no CPU for the stall window is killed with the STALLED line, its job's
//! processes terminated and the run failed with status 124, and a run that passes journals its load-adjusted seconds.

use std::fs;

use regex::Regex;
use serde_json::Value;

use crate::support::{Case, FAKE_TOOL, assert_in_order, run_words};

/// The journal's `release` line.
fn release(case: &Case) -> Value {
    fs::read_to_string(case.root().join("events.jsonl"))
        .expect("the journal was written")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a journal line is JSON"))
        .find(|event| event["event"] == "release")
        .expect("the run journalled its release")
}

#[test]
fn a_silent_idle_command_is_killed_as_stalled() {
    let case = Case::new("a_silent_idle_command_is_killed_as_stalled");
    let seams = ["--test-stall-ms", "1500", "--test-sample-ms", "100"];

    // `sleep` blocks without a tick of CPU, where `wait` would poll.
    let outcome = case.run(&run_words("out.log", &seams, &[FAKE_TOOL, "pid", "out", "hi", "sleep", "30000"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let stalled = Regex::new(&format!(
        concat!(
            r"(?m)^gate: STALLED: no output and no CPU for \d+ s \(machine free \d+% on average\); ",
            r"killed {} pid out hi sleep 30000 and its children$"
        ),
        regex::escape(FAKE_TOOL)
    ))
    .expect("the pattern is valid");
    let found = stalled.find(&outcome.stderr);
    assert!(found.is_some(), "no STALLED line on stderr: {outcome:?}");
    let line = found.expect("checked above").as_str();
    let after = ["gate: terminated the job's 1 process(es)", "gate: FAILED (status 124)"];
    assert_in_order(&outcome.stderr, &[&[line][..], &after[..]].concat());
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_in_order(&log, &[&["hi\n", line][..], &after[..]].concat());

    let release = release(&case);
    assert_eq!(
        (&release["verdict"], &release["status"]),
        (&Value::from("stalled"), &Value::from(124)),
        "{release}"
    );
    let pid: Option<u32> = log.lines().find_map(|text| text.strip_prefix("pid ")).and_then(|pid| pid.parse().ok());
    assert!(pid.is_some(), "the command said its pid: {log}");
    let pid = pid.expect("checked above");
    assert!(
        gate_win32::process_start_of(pid).is_ok_and(|start| start.is_none()),
        "the killed command {pid} is gone"
    );
}

#[test]
fn a_passed_run_journals_its_load_adjusted_seconds() {
    let case = Case::new("a_passed_run_journals_its_load_adjusted_seconds");

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "hi"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let release = release(&case);
    assert!(release["adjustedSeconds"].is_number(), "{release}");
    assert_eq!(release["verdict"], "passed", "{release}");
}
