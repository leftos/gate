//! The cores hint and the release line's cores fields: a run that kept more cores busy than its light slot's budget,
//! or fewer than a heavy slot is for, is told so before its passed line, and every sampled run journals its cores.
//!
//! How many cores a run really gets depends on the machine's load, so each case reads the cores the gate journalled
//! and asserts the line from those, never from what the command asked for. A gate starved until its command ended may
//! take no sample, journal no cores and say no line; each case then checks only that no line was said.

use serde_json::{Value, json};

use crate::nesting::{first_event, let_go};
use crate::routing::{HOST, repo, routed_case, routed_words, stub};
use crate::support::{
    CORE_SECOND, Case, Escapee, FAKE_TOOL, GATE, Outcome, copy_tool, cpu_ticks, events, heavy_words, none_words, run_words, shown, start, wait_until,
    write_remote_json,
};

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
    let pattern = format!(
        r"\Agate: used {avg:.1} cores on average \(peak (\d+), {free:.1} free\) on a {slot} slot; {}\z",
        regex::escape(advice)
    );
    assert_matched_before_passed(outcome, &pattern, peak);
}

/// Asserts that `outcome` passed with a commit run's cores line of `cores` on `slot` with `advice` just before the
/// passed line, as [`assert_said_before_passed`] asserts any other run's.
fn assert_commit_said_before_passed(outcome: &Outcome, cores: (f64, f64, f64), slot: &str, advice: &str) {
    let (avg, peak, free) = cores;
    let pattern = format!(
        r"\Agate: the commit's hooks used {avg:.1} cores on average \(peak (\d+), {free:.1} free\) on a {slot} slot; {}\z",
        regex::escape(advice)
    );
    assert_matched_before_passed(outcome, &pattern, peak);
}

/// Asserts that `outcome`'s commit run said no cores line: the line before its passed line is not one.
fn assert_no_commit_cores_line(outcome: &Outcome) {
    assert_no_cores_line(outcome);
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert!(
        lines.len() < 2 || !lines[lines.len() - 2].starts_with("gate: the commit's hooks used "),
        "{outcome:?}"
    );
}

/// Asserts that `outcome` passed with the no-slot cores line of `cores` just before the passed line, as
/// [`assert_said_before_passed`] asserts a slot's.
fn assert_no_slot_line_before_passed(outcome: &Outcome, cores: (f64, f64, f64)) {
    let (avg, peak, _) = cores;
    let pattern =
        format!(r"\Agate: used {avg:.1} cores on average \(peak (\d+)\) on no slot; work that keeps 1\.5 or more busy belongs on light or heavy\z");
    assert_matched_before_passed(outcome, &pattern, peak);
}

/// Asserts that `outcome` passed with a line matching `pattern` just before the passed line, which stays last, its
/// captured peak within a rounding of the journalled `peak`.
fn assert_matched_before_passed(outcome: &Outcome, pattern: &str, peak: f64) {
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert!(lines.len() >= 2, "{outcome:?}");
    assert!(lines[lines.len() - 1].starts_with("gate: passed in "), "{outcome:?}");
    let said = regex::Regex::new(pattern).expect("the pattern is valid").captures(lines[lines.len() - 2]);
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

#[test]
fn a_busy_no_slot_run_says_it_belongs_on_light_or_heavy() {
    let case = Case::new("a_busy_no_slot_run_says_it_belongs_on_light_or_heavy");
    let command = [FAKE_TOOL, "burn-threads", "4", "12000"];

    let running = case.start(&none_words("out.log", &["--test-sample-ms", "1000"], &command));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    match journalled_cores(&release_of(&case, pid)) {
        Some(cores) if cores.0 >= 1.5 => assert_no_slot_line_before_passed(&outcome, cores),
        _ => assert_no_cores_line(&outcome),
    }
}

#[test]
fn a_quiet_no_slot_run_prints_no_hint() {
    let case = Case::new("a_quiet_no_slot_run_prints_no_hint");
    let command = [FAKE_TOOL, "sleep", "11000"];

    let running = case.start(&none_words("out.log", &["--test-sample-ms", "1000"], &command));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    if let Some(cores) = journalled_cores(&release_of(&case, pid)) {
        assert!(cores.0 < 1.5, "a sleeping command keeps no core busy: {cores:?}");
    }
    assert_no_cores_line(&outcome);
}

#[test]
fn a_no_slot_run_whose_nested_gate_took_no_slot_keeps_its_hint() {
    let case = Case::new("a_no_slot_run_whose_nested_gate_took_no_slot_keeps_its_hint");
    // The nested no-slot gate burns four threads for 12 s inside the no-slot parent's job and takes no slot, so the
    // parent's CPU is its own to judge.
    let mut nested = vec![GATE];
    nested.extend(none_words("nested.log", &[], &[FAKE_TOOL, "burn-threads", "4", "12000"]));

    let running = case.start(&none_words("out.log", &["--test-sample-ms", "1000"], &nested));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    let lines = events(&case);
    let named = lines
        .iter()
        .find(|line| line["event"] == "take" && line["pid"] != pid)
        .map(|take| (take["parentPid"].clone(), take["slots"].clone()));
    assert_eq!(
        named,
        Some((Value::from(pid), Value::Array(Vec::new()))),
        "the nested gate's take names its parent and no slot: {lines:?}"
    );
    let release = release_of(&case, pid);
    let cores = journalled_cores(&release);
    assert!(cores.is_some(), "the parent journals its cores, as no nested gate took a slot: {release}");
    match cores {
        Some(cores) if cores.0 >= 1.5 => assert_no_slot_line_before_passed(&outcome, cores),
        _ => assert_no_cores_line(&outcome),
    }
}

#[test]
fn a_command_whose_child_left_its_job_counts_that_child_s_cores() {
    // The core-seconds the child outside the job burns before the case stops it: on two threads, at least 10 s, so the
    // run is long enough to be judged.
    const BURNED: u32 = 20;
    let case = Case::new("a_command_whose_child_left_its_job_counts_that_child_s_cores");
    let escapee = Escapee::new(&case);
    let command = escapee.command("2");
    let words: Vec<&str> = command.iter().map(String::as_str).collect();

    let mut running = case.start(&none_words("out.log", &["--test-sample-ms", "100", "--test-job-breakaway"], &words));
    let gate = running.id();
    let (pid, _held) = escapee.open(|| !running.is_running());
    wait_until(&format!("the escapee using {BURNED} core-seconds, or the gate ending"), || {
        cpu_ticks(pid) >= u64::from(BURNED) * CORE_SECOND || !running.is_running()
    });
    escapee.stop();
    let outcome = running.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    let release = release_of(&case, gate);
    let wall = release["wallSeconds"].as_f64();
    assert!(wall.is_some(), "the release has its wall seconds: {release}");
    let wall = wall.expect("checked above");
    match journalled_cores(&release) {
        Some(cores) => {
            // The gate counts the child's CPU up to its last sample of it, short of what it burned by at most the
            // stretch after that sample, so half of it is a floor no load lowers: the wall a load stretches divides
            // both. The average is kept to a tenth, so it can read up to 0.05 under the CPU over the wall. Were the
            // child not counted, the average would be the waiting command's own, 0.0.
            assert!(
                (cores.0 + 0.05) * wall >= f64::from(BURNED) / 2.0,
                "{} core-seconds counted over {wall} s, under half the {BURNED} the child burned: {release}",
                cores.0 * wall
            );
            if cores.0 >= 1.5 {
                assert_no_slot_line_before_passed(&outcome, cores);
            } else {
                assert_no_cores_line(&outcome);
            }
        }
        None => assert_no_cores_line(&outcome),
    }
}

#[test]
fn a_no_slot_run_whose_nested_gate_took_a_slot_prints_no_hint() {
    let case = Case::new("a_no_slot_run_whose_nested_gate_took_a_slot_prints_no_hint");
    // The nested heavy gate burns four threads for 12 s inside the no-slot parent's job: alone it would read busy.
    let mut nested = vec![GATE];
    nested.extend(heavy_words("nested.log", &[], &[FAKE_TOOL, "burn-threads", "4", "12000"]));

    let running = case.start(&none_words("out.log", &["--test-sample-ms", "1000"], &nested));
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

#[test]
fn a_heavy_commit_whose_hook_gate_routes_keeps_its_cores_hint() {
    let case = routed_case("a_heavy_commit_whose_hook_gate_routes_keeps_its_cores_hint");
    let routing = json!({ "host": HOST, "repos": { repo(&case): ["*tool*"] }, "hooks": [repo(&case)] });
    write_remote_json(&case, &routing.to_string());
    copy_tool(&case, "git.exe");
    let steps = shown(&stub(&case, &["status 0"]));
    let (pid_file, release) = (case.folder().join("inner.pid"), case.folder().join("outer.release"));
    let (pid_text, release_text) = (shown(&pid_file), shown(&release));
    // The commit sleeps 11 s, long enough to be judged, then its hook starts a gate that routes and waits for it.
    let mut command = vec![r".\git.exe", "commit", "sleep", "11000", "spawn-pid", &pid_text, &release_text, GATE];
    command.extend(routed_words("inner.log", &steps, &[], &[r".\tool.exe", "out", "ran there"]));
    let mut outer = case.gate(&heavy_words("outer.log", &["--test-sample-ms", "1000"], &command));
    // Inside a pre-commit hook, as git sets it for the hook and every gate the hook starts.
    outer.env("GIT_INDEX_FILE", "x");
    let outer = start(outer);
    let pid = outer.id();

    let (inner, first) = first_event(&case, &pid_file);
    assert!(first["event"] == "queue" && first["remote"] == true, "the hook gate routes: {first}");
    assert_eq!(inner.ended(), 0);
    let_go(&release);
    let outcome = outer.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines = events(&case);
    let nested_take = lines.iter().find(|line| line["event"] == "take" && line["pid"] == inner.pid);
    assert!(
        nested_take.is_some_and(|take| take["parentPid"] == pid && take["remote"] == true),
        "the hook gate's take is of a remote slot, naming the commit: {lines:?}"
    );
    let release = release_of(&case, pid);
    let cores = journalled_cores(&release);
    assert!(
        cores.is_some(),
        "the commit journals its cores, as its hook gate took no slot here: {release}"
    );
    match cores {
        Some(cores) if cores.0 < 1.5 && cores.2 >= 4.0 => {
            assert_commit_said_before_passed(&outcome, cores, "heavy", "hooks that keep under 1.5 busy fit on --slot light");
        }
        _ => assert_no_commit_cores_line(&outcome),
    }
}
