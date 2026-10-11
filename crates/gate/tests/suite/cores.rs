//! The cores hint and the release line's cores fields: a run that kept more cores busy than its light slot's budget,
//! or fewer than a heavy slot is for, is told so before its passed line, and every sampled run journals its cores.
//!
//! How many cores a run really gets depends on the machine's load, so each case reads the cores the gate journalled
//! and asserts the line from those, never from what the command asked for. A gate starved until its command ended may
//! take no sample, journal no cores and say no line; each case then checks only that no line was said.

use serde_json::Value;

use crate::support::{Case, FAKE_TOOL, GATE, Outcome, events, heavy_words, run_words};

/// The fields the release line carries the cores in.
const CORES_FIELDS: [&str; 3] = ["avgCores", "peakCores", "freeCores"];

/// The one release line the gate `pid` journalled in `case`.
fn release_of(case: &Case, pid: u32) -> Value {
    let lines = events(case);
    let releases: Vec<&Value> = lines.iter().filter(|line| line["event"] == "release" && line["pid"] == pid).collect();
    assert_eq!(releases.len(), 1, "one release of gate {pid}: {lines:?}");
    releases[0].clone()
}

/// The average, peak and free cores `release` journalled, all numbers, or `None` when it carries none of them.
fn journalled_cores(release: &Value) -> Option<(f64, f64, f64)> {
    if CORES_FIELDS.iter().all(|field| release.get(field).is_none()) {
        return None;
    }
    let [avg, peak, free] = CORES_FIELDS.map(|field| {
        let value = release[field].as_f64();
        assert!(value.is_some(), "the release's {field} is a number: {release}");
        value.expect("checked above")
    });
    Some((avg, peak, free))
}

/// Asserts that `outcome` passed with the cores line of `cores` on `slot` with `advice` as its stdout's line just
/// before the passed line, which stays last. The average and free cores are the journal's, one decimal each; the peak
/// is rounded from the unrounded figure, so it is only checked to lie within a rounding of the journal's tenths.
fn assert_said_before_passed(outcome: &Outcome, cores: (f64, f64, f64), slot: &str, advice: &str) {
    let (avg, peak, free) = cores;
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert!(lines.len() >= 2, "{outcome:?}");
    assert!(lines[lines.len() - 1].starts_with("gate: passed in "), "{outcome:?}");
    let pattern = format!(
        r"\Agate: used {avg:.1} cores on average \(peak (\d+), {free:.1} free\) on a {slot} slot; {}\z",
        regex::escape(advice)
    );
    let said = regex::Regex::new(&pattern)
        .expect("the pattern is valid")
        .captures(lines[lines.len() - 2]);
    assert!(said.is_some(), "the line before the passed one is {pattern}: {outcome:?}");
    let said_peak: f64 = said.expect("checked above")[1].parse().expect("the peak is a whole number");
    assert!(
        (said_peak - peak).abs() <= 0.55,
        "peak {said_peak} said for {peak} journalled: {outcome:?}"
    );
}

/// Asserts that `outcome`'s gate said no cores line: the line before its passed line is not one.
fn assert_no_cores_line(outcome: &Outcome) {
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert!(lines.last().is_some_and(|last| last.starts_with("gate: passed in ")), "{outcome:?}");
    assert!(lines.len() < 2 || !lines[lines.len() - 2].starts_with("gate: used "), "{outcome:?}");
}

#[test]
fn a_light_run_that_keeps_more_than_2_cores_busy_is_told_heavy_would_fit() {
    let case = Case::new("a_light_run_that_keeps_more_than_2_cores_busy_is_told_heavy_would_fit");
    // Four threads spin for 12 s of wall time, so the run lasts at least that long however little CPU it is given.
    let command = [FAKE_TOOL, "burn-threads", "4", "12000"];

    let running = case.start(&run_words("out.log", &["--test-sample-ms", "1000"], &command));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    // The peak is one sample interval's and the average takes in the stretch after the last sample too, so the
    // average can lie above the peak; nothing orders the two.
    match journalled_cores(&release_of(&case, pid)) {
        Some(cores) if cores.0 > 2.0 => {
            assert_said_before_passed(&outcome, cores, "light", "work that keeps more than 2 busy belongs on heavy");
        }
        _ => assert_no_cores_line(&outcome),
    }
}

#[test]
fn a_heavy_run_that_keeps_no_core_busy_is_told_light_would_fit_when_4_were_free() {
    let case = Case::new("a_heavy_run_that_keeps_no_core_busy_is_told_light_would_fit_when_4_were_free");
    let command = [FAKE_TOOL, "sleep", "11000"];

    let running = case.start(&heavy_words("out.log", &["--test-sample-ms", "1000"], &command));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    match journalled_cores(&release_of(&case, pid)) {
        Some(cores) => {
            assert!(cores.0 < 1.5, "a sleeping command keeps no core busy: {cores:?}");
            if cores.2 >= 4.0 {
                assert_said_before_passed(&outcome, cores, "heavy", "work that keeps under 1.5 busy fits on light");
            } else {
                assert_no_cores_line(&outcome);
            }
        }
        None => assert_no_cores_line(&outcome),
    }
}

#[test]
fn a_run_under_10_s_gets_no_cores_line() {
    let case = Case::new("a_run_under_10_s_gets_no_cores_line");
    let command = [FAKE_TOOL, "burn-threads", "4", "5000"];

    let running = case.start(&run_words("out.log", &["--test-sample-ms", "200"], &command));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    // A machine that stalled the run past 10 s makes it long enough to judge, and then a line may be said.
    let release = release_of(&case, pid);
    let wall = release["wallSeconds"].as_f64();
    assert!(wall.is_some(), "the release has its wall seconds: {release}");
    if wall.expect("checked above") < 10.0 {
        assert_no_cores_line(&outcome);
    }
}

#[test]
fn a_light_parent_whose_nested_gate_took_a_heavy_slot_is_not_judged() {
    let case = Case::new("a_light_parent_whose_nested_gate_took_a_heavy_slot_is_not_judged");
    // The nested heavy gate burns four threads for 12 s inside the light parent's job: alone it would read hot.
    let mut nested = vec![GATE];
    nested.extend(heavy_words("nested.log", &[], &[FAKE_TOOL, "burn-threads", "4", "12000"]));

    let running = case.start(&run_words("out.log", &["--test-sample-ms", "1000"], &nested));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    let lines = events(&case);
    let named = lines
        .iter()
        .find(|line| line["event"] == "take" && line["pid"] != pid)
        .map(|take| take["parentPid"].clone());
    assert_eq!(named, Some(Value::from(pid)), "the nested gate's take names its parent: {lines:?}");
    let release = release_of(&case, pid);
    assert!(
        CORES_FIELDS.iter().all(|field| release.get(field).is_none()),
        "the parent journals no cores: {release}"
    );
    assert_no_cores_line(&outcome);
}
