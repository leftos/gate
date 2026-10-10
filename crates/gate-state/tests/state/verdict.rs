//! The verdict a release line gives for how a command ended.

use gate_state::Verdict;

#[test]
fn stalled_kill_line_is_stalled() {
    assert_eq!(Verdict::of(Some("gate: STALLED: no output for 120 s"), 124, 124), Verdict::Stalled);
    assert_eq!(Verdict::Stalled.as_str(), "stalled");
}

#[test]
fn timed_out_kill_line_is_timed_out() {
    assert_eq!(Verdict::of(Some("gate: TIMED OUT after 900 s"), 124, 124), Verdict::TimedOut);
    assert_eq!(Verdict::TimedOut.as_str(), "timed-out");
}

#[test]
fn backstop_kill_line_is_backstop() {
    assert_eq!(Verdict::of(Some("gate: BACKSTOP at 2700 s"), 124, 124), Verdict::Backstop);
    assert_eq!(Verdict::Backstop.as_str(), "backstop");
}

#[test]
fn exit_0_with_a_failing_gate_status_is_log_failure() {
    assert_eq!(Verdict::of(None, 0, 1), Verdict::LogFailure);
    assert_eq!(Verdict::LogFailure.as_str(), "log-failure");
}

#[test]
fn status_0_is_passed() {
    assert_eq!(Verdict::of(None, 0, 0), Verdict::Passed);
    assert_eq!(Verdict::Passed.as_str(), "passed");
}

#[test]
fn non_zero_status_is_failed() {
    assert_eq!(Verdict::of(None, 3, 3), Verdict::Failed);
    assert_eq!(
        Verdict::of(Some("gate: STALLEDX is not a kill line"), 3, 3),
        Verdict::Failed,
        "the kill word must end at a word boundary"
    );
    assert_eq!(Verdict::of(Some(" gate: STALLED"), 3, 3), Verdict::Failed, "a kill line starts the line");
    assert_eq!(Verdict::Failed.as_str(), "failed");
    assert_eq!(Verdict::Unknown.as_str(), "unknown");
}

#[test]
fn kill_line_case_is_ignored() {
    assert_eq!(Verdict::of(Some("gate: Timed out after 900 s"), 124, 124), Verdict::TimedOut);
    assert_eq!(Verdict::of(Some("GATE: stalled: no output for 120 s"), 124, 124), Verdict::Stalled);
    assert_eq!(Verdict::of(Some("Gate: Backstop at 2700 s"), 124, 124), Verdict::Backstop);
    assert_eq!(
        Verdict::of(Some("gate: Stalledx is not a kill line"), 3, 3),
        Verdict::Failed,
        "the word boundary still holds"
    );
}

#[test]
fn exit_124_without_a_kill_line_is_failed() {
    assert_eq!(Verdict::of(None, 124, 124), Verdict::Failed);
}
