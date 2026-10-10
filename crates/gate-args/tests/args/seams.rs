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
            "--test-interrupt-after-samples",
            "3",
            "--test-interrupt-queued",
            "--test-sampler-fail",
            "--test-diagnostics-fail",
            "--test-remote-client",
            r"steps\client.txt",
            "--test-remote-wait-ms=90",
            "--test-git-limit-ms",
            "95",
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
            interrupt_after_samples: Some(3),
            interrupt_queued: true,
            sampler_fail: true,
            diagnostics_fail: true,
            remote_client: Some(std::path::PathBuf::from(r"steps\client.txt")),
            remote_wait_ms: Some(90),
            git_limit_ms: Some(95),
        }
    );
    assert!(request.print_request);
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-interrupt-after-samples", "0", "--", "cmd"]))),
        ["gate: --test-interrupt-after-samples must be a whole number above 0, got '0'"]
    );
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
    assert_eq!(
        problems(&["stop-tree", "--test-git-limit-ms", "5", "1"]),
        ["gate: unknown option --test-git-limit-ms", "gate: unexpected argument 1"]
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
fn remote_seams_need_a_value_once() {
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-remote-client", "--", "cmd"]))),
        ["gate: --test-remote-client needs a value"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-remote-wait-ms=", "--", "cmd"]))),
        ["gate: --test-remote-wait-ms needs a value"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-remote-wait-ms", "0", "--", "cmd"]))),
        ["gate: --test-remote-wait-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-client", "a", "--test-remote-client", "b", "--", "cmd"]
        ))),
        ["gate: --test-remote-client is given twice"]
    );
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-wait-ms", "1", "--test-remote-wait-ms", "2", "--", "cmd"]
        ))),
        ["gate: --test-remote-wait-ms is given twice"]
    );
    assert_eq!(
        problems(&[
            "-Log",
            "x",
            "-TimeoutSeconds",
            "5",
            "-Slot",
            "heavy",
            "--test-remote-client",
            "a",
            "--",
            "cmd"
        ]),
        ["gate: unknown option --test-remote-client", "gate: unexpected argument a"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-git-limit-ms=", "--", "cmd"]))),
        ["gate: --test-git-limit-ms needs a value"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-git-limit-ms", "0", "--", "cmd"]))),
        ["gate: --test-git-limit-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-git-limit-ms", "1", "--test-git-limit-ms", "2", "--", "cmd"]))),
        ["gate: --test-git-limit-ms is given twice"]
    );
    assert_eq!(
        problems(&[
            "-Log",
            "x",
            "-TimeoutSeconds",
            "5",
            "-Slot",
            "heavy",
            "--test-git-limit-ms",
            "5",
            "--",
            "cmd"
        ]),
        ["gate: unknown option --test-git-limit-ms", "gate: unexpected argument 5"]
    );
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
        problems(&refs(&with(&RUN, &["--test-diagnostics-fail", "--", "cmd"]))),
        ["gate: unknown option --test-diagnostics-fail"]
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
