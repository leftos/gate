//! The hidden `--test-*` flags, read only in a build with the `test-seams` feature.

use crate::support::{RUN, problems, refs, with};

#[cfg(feature = "test-seams")]
#[test]
fn seam_flags_parse_with_the_feature() {
    use gate_args::TestSeams;

    use crate::support::{run_request, stop_request};

    let words = with(
        &RUN,
        &[
            "--test-sample-ms",
            "50",
            "--test-heartbeat-ms=60",
            "--test-poll-ms",
            "70",
            "--test-stall-ms",
            "80",
            "--test-print-request",
            "--",
            "cmd",
        ],
    );
    let request = run_request(&refs(&words));
    assert_eq!(
        request.test_seams,
        TestSeams {
            sample_ms: Some(50),
            heartbeat_ms: Some(60),
            poll_ms: Some(70),
            stall_ms: Some(80),
            journal_run: None,
            hold_until: None,
        }
    );
    assert!(request.print_request);
    assert!(stop_request(&["stop-tree", "--test-print-request", "1"]).print_request);
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-poll-ms", "0", "--", "cmd"]))),
        ["gate: --test-poll-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        run_request(&refs(&with(&RUN, &["--test-stall-ms", "2147483647", "--", "cmd"])))
            .test_seams
            .stall_ms,
        Some(2_147_483_647)
    );
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &[
                "--test-sample-ms",
                "2147483648",
                "--test-heartbeat-ms",
                "99999999999999999999",
                "--",
                "cmd"
            ]
        ))),
        [
            "gate: --test-sample-ms must be at most 2147483647, got '2147483648'",
            "gate: --test-heartbeat-ms must be at most 2147483647, got '99999999999999999999'"
        ]
    );
    assert_eq!(
        problems(&["stop-tree", "--test-poll-ms", "5", "1"]),
        ["gate: unknown option --test-poll-ms", "gate: unexpected argument 1"]
    );

    let compat = run_request(&["-Log", "x", "--test-print-request", "-TimeoutSeconds", "5", "-Slot", "heavy", "--", "cmd"]);
    assert!(compat.print_request, "the compatibility layer reads the hidden switch");
    assert!(stop_request(&["-StopTree", "1", "--test-print-request"]).print_request);
    assert_eq!(
        problems(&["-Log", "x", "-TimeoutSeconds", "5", "-Slot", "heavy", "-TestPrintRequest", "--", "cmd"]),
        ["gate: unknown option -TestPrintRequest"]
    );
    assert_eq!(
        problems(&["-Log", "x", "-TimeoutSeconds", "5", "-Slot", "heavy", "--test-poll-ms", "5", "--", "cmd"]),
        ["gate: unknown option --test-poll-ms", "gate: unexpected argument 5"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn test_journal_run_parses_with_the_feature() {
    use crate::support::run_request;

    for (value, status) in [("0", 0), ("3", 3), ("255", 255)] {
        let request = run_request(&refs(&with(&RUN, &["--test-journal-run", value, "--", "cmd"])));
        assert_eq!(request.test_seams.journal_run, Some(status), "{value}");
        assert!(!request.print_request);
    }
    let inline = run_request(&refs(&with(&RUN, &["--test-journal-run=7", "--", "cmd"])));
    assert_eq!(inline.test_seams.journal_run, Some(7));
    assert_eq!(run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.journal_run, None);
    assert_eq!(
        problems(&[
            "-Log",
            "x",
            "-TimeoutSeconds",
            "5",
            "-Slot",
            "heavy",
            "--test-journal-run",
            "0",
            "--",
            "cmd"
        ]),
        ["gate: unknown option --test-journal-run", "gate: unexpected argument 0"],
        "the compatibility layer does not read it"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn test_journal_run_rejects_out_of_range() {
    for value in ["256", "-1", "1.5", "x", "99999999999999999999"] {
        assert_eq!(
            problems(&refs(&with(&RUN, &["--test-journal-run", value, "--", "cmd"]))),
            [format!("gate: --test-journal-run must be a whole number 0 to 255, got '{value}'")]
        );
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn test_journal_run_and_print_request_cannot_be_combined() {
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-journal-run", "0", "--test-print-request", "--", "cmd"]))),
        ["gate: --test-journal-run and --test-print-request cannot be combined"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn test_hold_until_parses_with_the_feature() {
    use std::path::PathBuf;

    use crate::support::run_request;

    let spaced = run_request(&refs(&with(
        &RUN,
        &["--test-journal-run", "0", "--test-hold-until", r"x\go", "--", "cmd"],
    )));
    assert_eq!(spaced.test_seams.hold_until, Some(PathBuf::from(r"x\go")));
    assert_eq!(spaced.test_seams.journal_run, Some(0));
    let inline = run_request(&refs(&with(&RUN, &["--test-hold-until=go", "--test-journal-run=0", "--", "cmd"])));
    assert_eq!(inline.test_seams.hold_until, Some(PathBuf::from("go")), "the value is taken as given");
    assert_eq!(
        run_request(&refs(&with(&RUN, &["--test-journal-run", "0", "--", "cmd"])))
            .test_seams
            .hold_until,
        None
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn test_hold_until_needs_journal_run() {
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-hold-until", "go", "--", "cmd"]))),
        ["gate: --test-hold-until needs --test-journal-run"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn test_hold_until_with_an_invalid_journal_run_says_only_the_value_problem() {
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-journal-run", "300", "--test-hold-until", "go", "--", "cmd"]))),
        ["gate: --test-journal-run must be a whole number 0 to 255, got '300'"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn test_hold_until_rejects_an_empty_path() {
    for words in [
        ["--test-journal-run", "0", "--test-hold-until=", "--", "cmd"].as_slice(),
        ["--test-journal-run", "0", "--test-hold-until", "--", "cmd"].as_slice(),
    ] {
        assert_eq!(
            problems(&refs(&with(&RUN, words))),
            ["gate: --test-hold-until needs a value"],
            "{words:?}"
        );
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn seam_flags_are_unknown_without_the_feature() {
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-sample-ms", "50", "--", "cmd"]))),
        ["gate: unknown option --test-sample-ms", "gate: unexpected argument 50"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-print-request", "--", "cmd"]))),
        ["gate: unknown option --test-print-request"]
    );
    assert_eq!(
        problems(&["stop-tree", "--test-print-request", "1"]),
        ["gate: unknown option --test-print-request"]
    );
    assert_eq!(
        problems(&["-Log", "x", "-TimeoutSeconds", "5", "-Slot", "heavy", "--test-print-request", "--", "cmd"]),
        ["gate: unknown option --test-print-request"]
    );
}
