//! The diagnostics block a kill writes: the facts of the killed job's processes and the last samples go into the log
//! before the kill line, one pointer line on stderr says where the block went, the failed run's tail leaves the block
//! out, and a gathering that failed ends the run as the kill it was gathering for.

use std::fs;

use regex::Regex;

use crate::support::{Case, FAKE_TOOL, assert_in_order, release, run_words, said_pid, shown};

/// The failure `--test-diagnostics-fail` makes at the start of a gathering.
const DIAGNOSTICS_FAIL: &str = "--test-diagnostics-fail makes the diagnostics fail (test seam only)";

/// The words of a run in `slot` with a 1 s ceiling, logging to `out.log`, `seams` before `--` and `command` after it.
fn ceiling_words<'a>(slot: &'a str, seams: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec!["run", "--slot", slot, "--timeout", "1", "--log", "out.log"];
    words.extend(seams);
    words.push("--");
    words.extend(command);
    words
}

/// The lines of `log`'s diagnostics block after its heading, up to the first line that is not one of the block's own.
fn block_lines(log: &str) -> Vec<&str> {
    log.lines()
        .skip_while(|line| *line != "gate: diagnostics")
        .skip(1)
        .take_while(|line| line.starts_with("gate:   "))
        .collect()
}

/// Whether the gathering's 2000 ms budget ran out, so the block counts what it did not read instead of listing it.
fn cut(log: &str) -> bool {
    log.contains("gate:   ... diagnostics cut at ")
}

/// Asserts that `log`'s block falls before `kill`, that it says at least one thing after its heading and that every
/// line of it is one of the block's own; and, unless the gathering hit its budget, that it holds `lines` in order. The
/// budget is the gate's, and a machine under another build's load reaches it, so a case pins the block's contents only
/// while the block was not cut: no case waits on the clock.
fn assert_block(log: &str, kill: &str, lines: &[&str]) {
    assert_in_order(log, &["gate: diagnostics\n", kill]);
    let block = block_lines(log);
    assert!(!block.is_empty(), "the block says at least one thing after its heading:\n{log}");
    for line in block {
        assert!(
            line.starts_with("gate:   "),
            "every line of the block is one of its own, got {line:?} in:\n{log}"
        );
    }
    if !cut(log) {
        assert_in_order(log, lines);
    }
}

#[test]
fn a_kill_writes_its_diagnostics_to_the_log_before_the_kill_line() {
    let case = Case::new("a_kill_writes_its_diagnostics_to_the_log_before_the_kill_line");
    // The server scan is confined to this case's own folder, which holds none, so a build server another case started
    // can never take the `build servers: none` line this case pins.
    let under = shown(&case.folder());
    let seams = [
        "--test-stall-ms",
        "1500",
        "--test-sample-ms",
        "100",
        "--test-servers-under",
        under.as_str(),
    ];
    let log_path = case.folder().join("out.log");

    let outcome = case.run(&run_words("out.log", &seams, &[FAKE_TOOL, "pid", "out", "hi", "sleep", "30000"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let log = fs::read_to_string(&log_path).expect("the log was written");
    let pid = said_pid(&log, "pid ");
    let stalled = Regex::new(r"(?m)^gate: STALLED: .*$").expect("the pattern is valid");
    let kill = stalled.find(&log).expect("the log has its STALLED line").as_str();
    let job = format!("gate:   job {pid} parent ");
    let pointer = format!("gate: diagnostics written to {}", shown(&log_path));
    let terminated = "gate: terminated the job's 1 process(es)";

    assert_in_order(&log, &["hi\n", "gate: diagnostics\n"]);
    assert_block(&log, kill, &[&job, "gate:   build servers: none\n", "gate:   sample at "]);
    assert_in_order(&log, &[kill, &format!("{terminated}\n"), &pointer]);

    assert_in_order(&outcome.stderr, &[kill, terminated, &pointer]);
    for line in outcome.stderr.lines() {
        assert!(!line.starts_with("gate:   "), "no block line belongs on stderr, yet stderr has {line:?}");
        assert!(
            line == pointer || !line.starts_with("gate: diagnostics"),
            "only the pointer names the diagnostics on stderr, yet stderr has {line:?}"
        );
    }

    // The verdict's tail is said on stdout: the command's last output, then the kill, the terminated count and the
    // pointer, with the block's own lines left out.
    let tail: Vec<&str> = outcome.stdout.lines().collect();
    assert!(tail.contains(&"hi"), "the failed tail shows the command's last output: {outcome:?}");
    assert!(
        tail.iter().all(|line| !line.starts_with("gate:   ")),
        "the failed tail leaves the block's lines out: {outcome:?}"
    );
    assert_in_order(&outcome.stdout, &[kill, terminated, &pointer]);
}

#[test]
fn a_failing_diagnostics_gathering_still_kills() {
    let case = Case::new("a_failing_diagnostics_gathering_still_kills");
    let seams = ["--test-stall-ms", "1500", "--test-sample-ms", "100", "--test-diagnostics-fail"];

    let outcome = case.run(&run_words("out.log", &seams, &[FAKE_TOOL, "sleep", "30000"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_in_order(
        &log,
        &[
            "gate: diagnostics\n",
            &format!("gate: diagnostics failed: {DIAGNOSTICS_FAIL}\n"),
            "gate: STALLED: ",
        ],
    );
    assert_eq!(release(&case)["verdict"], "stalled", "{outcome:?}");
}

#[test]
fn the_failed_tail_holds_the_last_lines_around_a_big_block() {
    let case = Case::new("the_failed_tail_holds_the_last_lines_around_a_big_block");
    let seams = ["--test-stall-ms", "1500", "--test-sample-ms", "100"];
    let log_path = case.folder().join("out.log");

    let outcome = case.run(&run_words("out.log", &seams, &[FAKE_TOOL, "lines", "30", "n", "sleep", "30000"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let log = fs::read_to_string(&log_path).expect("the log was written");
    assert!(log.contains("gate: diagnostics\n"), "the log keeps the whole block:\n{log}");
    let stalled = Regex::new(r"(?m)^gate: STALLED: .*$").expect("the pattern is valid");
    let kill = stalled.find(&log).expect("the log has its STALLED line").as_str();
    let terminated = "gate: terminated the job's 1 process(es)";
    let pointer = format!("gate: diagnostics written to {}", shown(&log_path));

    // The tail is the last 20 lines of the log once the block is dropped by position: the command's last 17 numbered
    // lines, then the kill, the terminated count and the pointer. Nothing else is dropped, so a command line equal to a
    // block line would still be shown.
    let tail: Vec<&str> = outcome.stdout.lines().collect();
    let numbered: Vec<String> = (14..=30).map(|number| format!("n{number}")).collect();
    let expected: Vec<&str> = numbered.iter().map(String::as_str).collect();
    assert_eq!(tail.len(), 20, "{outcome:?}");
    assert_eq!(tail[..17], expected[..], "{outcome:?}");
    assert_eq!(tail[17], kill);
    assert_eq!(tail[18], terminated);
    assert_eq!(tail[19], pointer);
}

#[test]
fn a_timed_out_kill_writes_its_diagnostics_too() {
    let case = Case::new("a_timed_out_kill_writes_its_diagnostics_too");
    let command = [FAKE_TOOL, "sleep", "30000"];

    // A critical run's free share is 1, so its load-adjusted clock is wall time and the ceiling comes before the
    // backstop however busy the machine is.
    let outcome = case.run(&ceiling_words("critical", &["--test-sample-ms", "100"], &command));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_block(&log, "gate: TIMED OUT", &["gate:   job "]);
    assert_eq!(release(&case)["verdict"], "timed-out", "{outcome:?}");
}

#[test]
fn a_backstop_kill_writes_its_diagnostics_too() {
    let case = Case::new("a_backstop_kill_writes_its_diagnostics_too");
    let command = [FAKE_TOOL, "sleep", "30000"];
    let seams = ["--test-sampler-fail", "--test-sample-ms", "100"];

    // Once the sampler has failed there are no samples to show, so the block says so.
    let outcome = case.run(&ceiling_words("light", &seams, &command));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_in_order(&log, &["gate: diagnostics\n", "gate:   samples: none\n", "gate: BACKSTOP"]);
    assert_eq!(release(&case)["verdict"], "backstop", "{outcome:?}");
}
