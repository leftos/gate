//! `gate cutover`: no words and no options, the usage line, and its hidden seams.

use gate_args::{CutoverRequest, CutoverSeams, Invocation, USAGE};

use crate::support::{invocation, problems};

#[test]
fn cutover_alone_parses() {
    assert_eq!(
        invocation(&["cutover"]),
        Invocation::Cutover(CutoverRequest {
            print_request: false,
            seams: CutoverSeams::default(),
        })
    );
}

#[test]
fn cutover_refuses_every_word_and_option() {
    assert_eq!(problems(&["cutover", "now"]), ["gate: unexpected argument now"]);
    assert_eq!(
        problems(&["cutover", "--state-dir", "x"]),
        ["gate: unknown option --state-dir", "gate: unexpected argument x"]
    );
    assert_eq!(problems(&["cutover", "--state-dir=x"]), ["gate: unknown option --state-dir=x"]);
    assert_eq!(problems(&["cutover", "--undo"]), ["gate: unknown option --undo"]);
    assert_eq!(problems(&["cutover", "-Cutover"]), ["gate: unknown option -Cutover"]);
    assert_eq!(problems(&["cutover", "--"]), ["gate: unexpected argument --"]);
    assert_eq!(
        problems(&["cutover", "--local", "1"]),
        ["gate: unknown option --local", "gate: unexpected argument 1"],
        "run options are not cutover options"
    );
}

#[test]
fn the_usage_lists_cutover() {
    assert!(
        USAGE.lines().any(|line| line == "       gate cutover"),
        "the usage lists gate cutover:\n{USAGE}"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn cutover_seams_parse_with_the_feature() {
    let words = [
        "cutover",
        "--test-poll-ms",
        "20",
        "--test-heartbeat-ms=40",
        "--test-confirm-ms",
        "60",
        "--test-cutover-interrupt-after-list",
        "--test-print-request",
    ];
    assert_eq!(
        invocation(&words),
        Invocation::Cutover(CutoverRequest {
            print_request: true,
            seams: CutoverSeams {
                poll_ms: Some(20),
                heartbeat_ms: Some(40),
                confirm_ms: Some(60),
                interrupt_after_list: true,
            },
        })
    );
    assert_eq!(
        problems(&["cutover", "--test-poll-ms", "0"]),
        ["gate: --test-poll-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&["cutover", "--test-confirm-ms", "0"]),
        ["gate: --test-confirm-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(problems(&["cutover", "--test-heartbeat-ms"]), ["gate: --test-heartbeat-ms needs a value"]);
    assert_eq!(
        problems(&["cutover", "--test-poll-ms", "5", "--test-poll-ms", "6"]),
        ["gate: --test-poll-ms is given twice"]
    );
    assert_eq!(
        problems(&["cutover", "--test-sample-ms", "5"]),
        ["gate: unknown option --test-sample-ms", "gate: unexpected argument 5"],
        "a run seam is not a cutover seam"
    );
    assert_eq!(
        problems(&[
            "run",
            "--slot",
            "heavy",
            "--timeout",
            "5",
            "--log",
            "x",
            "--test-cutover-interrupt-after-list",
            "--",
            "cmd"
        ]),
        ["gate: unknown option --test-cutover-interrupt-after-list"],
        "the cutover seam is not a run seam"
    );
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn cutover_seams_are_unknown_without_the_feature() {
    for flag in ["--test-poll-ms", "--test-heartbeat-ms", "--test-confirm-ms"] {
        assert_eq!(
            problems(&["cutover", flag, "5"]),
            [format!("gate: unknown option {flag}"), "gate: unexpected argument 5".to_owned()],
            "{flag}"
        );
    }
    for flag in ["--test-cutover-interrupt-after-list", "--test-print-request"] {
        assert_eq!(problems(&["cutover", flag]), [format!("gate: unknown option {flag}")], "{flag}");
    }
}
