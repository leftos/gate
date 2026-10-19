//! The ceiling and the backstop: a run whose load-adjusted clock reaches its ceiling is killed with the TIMED OUT line,
//! a run whose sampler failed is watched by wall time alone and killed by the backstop at five ceilings, and a run that
//! passes after its sampler failed says its load-adjusted time is unknown.

use std::fs;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::support::{Case, Escapee, FAKE_TOOL, UNTIL_KILLED, assert_in_order, release, run_words, shown};

/// The line the gate says once `--test-sampler-fail` has failed its first sample.
const SAMPLER_FAILED: &str = "gate: sampler failed: --test-sampler-fail makes every sample fail (test seam only); watching by wall time only";

/// The words of a run in `slot` with a 1 s ceiling, logging to `out.log`, `seams` before `--` and `command` after it.
fn ceiling_words<'a>(slot: &'a str, seams: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec!["run", "--slot", slot, "--timeout", "1", "--log", "out.log"];
    if slot == "critical" {
        words.extend(["--reason", "the suite's critical run"]);
    }
    words.extend(seams);
    words.push("--");
    words.extend(command);
    words
}

/// The first line of `stderr` that `pattern` matches.
fn matched<'a>(stderr: &'a str, pattern: &str) -> &'a str {
    let found = Regex::new(pattern).expect("the pattern is valid").find(stderr);
    assert!(found.is_some(), "no line matches {pattern:?} on stderr:\n{stderr}");
    found.expect("checked above").as_str()
}

/// Asserts the journal released the run with `verdict` and status 124.
fn assert_killed_as(case: &Case, verdict: &str) {
    let release = release(case);
    assert_eq!(
        (&release["verdict"], &release["status"]),
        (&Value::from(verdict), &Value::from(124)),
        "{release}"
    );
}

#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_ceiling_kill_ends_a_child_outside_the_job_whose_launcher_has_ended() {
    let case = Case::new("a_ceiling_kill_ends_a_child_outside_the_job_whose_launcher_has_ended");
    // The launcher lives a second, so a sample sees the escapee under it; then nothing in the job is its parent, and
    // the command idles until the 3 s ceiling, or the backstop past it.
    let escapee = Escapee::new(&case);
    let command = escapee.launched_command();
    let mut words = run_words("out.log", &["--test-sample-ms", "100", "--test-job-breakaway"], &[]);
    words[4] = "3";
    words.extend(command.iter().map(String::as_str));

    let mut running = case.start(&words);
    let (pid, away) = escapee.open(|| !running.is_running());
    let launcher = escapee.launcher_ended();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(
        matches!(away.wait(Duration::ZERO), Ok(true)),
        "the escapee {pid}, whose launcher had ended, was ended by the kill: {outcome:?}"
    );
    assert!(
        matches!((launcher.exited(), away.exited()), (Ok(left), Ok(ended)) if left < ended),
        "the launcher had ended before the kill ended the escapee {pid}: {outcome:?}"
    );
}

#[test]
fn a_run_past_its_ceiling_is_killed_as_timed_out() {
    let case = Case::new("a_run_past_its_ceiling_is_killed_as_timed_out");
    // One process that does nothing it could be late for: the kill comes 1 s in whatever the command has done by then.
    let command = [FAKE_TOOL, "sleep", UNTIL_KILLED];

    // A critical run is High priority, so its free share is 1 and the load-adjusted clock is wall time: the ceiling
    // comes before the 5 s backstop however busy the machine is.
    let outcome = case.run(&ceiling_words("critical", &["--test-sample-ms", "100"], &command));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let pattern = format!(
        concat!(
            r"(?m)^gate: TIMED OUT after 1 s of load-adjusted time \(wall \d+ s, machine free \d+% on average\); ",
            r"killed {} and its children$"
        ),
        regex::escape(&command.join(" "))
    );
    let line = matched(&outcome.stderr, &pattern);
    let order = [line, "gate: terminated the job's 1 process(es)", "gate: FAILED (status 124)"];
    assert_in_order(&outcome.stderr, &order);
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_in_order(&log, &order);

    assert_killed_as(&case, "timed-out");
}

#[test]
fn a_run_whose_clock_never_moves_is_killed_by_the_backstop() {
    let case = Case::new("a_run_whose_clock_never_moves_is_killed_by_the_backstop");
    let command = [FAKE_TOOL, "sleep", UNTIL_KILLED];
    let seams = ["--test-sampler-fail", "--test-sample-ms", "100"];

    let outcome = case.run(&ceiling_words("light", &seams, &command));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let pattern = format!(
        concat!(
            r"(?m)^gate: BACKSTOP: \d+ s of wall time \(5 x 1 s ceiling, machine load unknown: the sampler failed\); ",
            r"killed {} and its children$"
        ),
        regex::escape(&command.join(" "))
    );
    let line = matched(&outcome.stderr, &pattern);
    let order = [
        SAMPLER_FAILED,
        line,
        "gate: terminated the job's 1 process(es)",
        "gate: FAILED (status 124)",
    ];
    assert_in_order(&outcome.stderr, &order);
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_in_order(&log, &order);
    assert_eq!(
        outcome.stderr.matches("gate: sampler failed:").count(),
        1,
        "the failure is said once however many samples followed: {outcome:?}"
    );
    for (name, text) in [("stderr", &outcome.stderr), ("the log", &log)] {
        assert!(
            !text.contains("gate: STALLED") && !text.contains("gate: TIMED OUT"),
            "neither the stall nor the ceiling is read once the sampler failed, yet {name} has one:\n{text}"
        );
    }

    assert_killed_as(&case, "backstop");
}

#[test]
fn a_run_that_passes_after_its_sampler_failed_reads_unknown() {
    let case = Case::new("a_run_that_passes_after_its_sampler_failed_reads_unknown");
    let go = case.folder().join("go");
    let go_shown = shown(&go);
    let seams = ["--test-sampler-fail", "--test-sample-ms", "100"];

    let running = case.start(&run_words("out.log", &seams, &[FAKE_TOOL, "out", "hi", "wait", &go_shown]));
    running.wait_for_stderr(SAMPLER_FAILED);
    fs::write(&go, "").expect("the go file can be written");
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let passed = format!(
        r"(?m)^gate: passed in \d+ s \(load-adjusted unknown: the sampler failed, ceiling 3600 s\)\. Full output: {}$",
        regex::escape(&shown(&case.folder().join("out.log")))
    );
    assert!(
        Regex::new(&passed).expect("the pattern is valid").is_match(&outcome.stdout),
        "{outcome:?}"
    );
    let release = release(&case);
    assert_eq!(release.get("adjustedSeconds"), Some(&Value::Null), "{release}");
    assert_eq!(release["verdict"], "passed", "{release}");
}
