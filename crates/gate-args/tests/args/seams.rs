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
            "--test-servers-under",
            r"target\case",
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
            servers_under: Some(std::path::PathBuf::from(r"target\case")),
            remote_snapshot: false,
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

#[cfg(feature = "test-seams")]
#[test]
fn the_servers_under_seam_needs_a_value_once() {
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-servers-under=", "--", "cmd"]))),
        ["gate: --test-servers-under needs a value"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-servers-under", "--", "cmd"]))),
        ["gate: --test-servers-under needs a value"]
    );
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-servers-under", "a", "--test-servers-under", "b", "--", "cmd"]
        ))),
        ["gate: --test-servers-under is given twice"]
    );
    assert_eq!(
        problems(&["stop-tree", "--test-servers-under", "5", "1"]),
        ["gate: unknown option --test-servers-under", "gate: unexpected argument 1"]
    );
    assert_eq!(
        problems(&[
            "-Log",
            "x",
            "-TimeoutSeconds",
            "5",
            "-Slot",
            "heavy",
            "-TestServersUnder",
            "5",
            "--",
            "cmd"
        ]),
        ["gate: unknown option -TestServersUnder", "gate: unexpected argument 5"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn the_remote_snapshot_seam_is_a_switch_given_once() {
    use crate::support::run_request;

    assert!(
        run_request(&refs(&with(&RUN, &["--test-remote-snapshot", "--", "cmd"])))
            .test_seams
            .remote_snapshot
    );
    assert!(
        !run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.remote_snapshot,
        "off unless given"
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-remote-snapshot", "--test-remote-snapshot", "--", "cmd"]))),
        ["gate: --test-remote-snapshot is given twice"]
    );
    assert_eq!(
        problems(&["stop-tree", "--test-remote-snapshot", "1"]),
        ["gate: unknown option --test-remote-snapshot"]
    );
}

/// The one line a malformed `gate box` prints.
const BOX_USAGE: &str = "usage: gate box preflight|run|stop|boot|outputs <base64 JSON payload>";

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_heartbeat_ms_is_read_before_the_verb() {
    use std::ffi::OsString;

    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let beating = |heartbeat_ms| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            payload: OsString::from("e30="),
            seams: BoxSeams { heartbeat_ms },
        })
    };
    assert_eq!(invocation(&["box", "--test-heartbeat-ms", "30", "run", "e30="]), beating(Some(30)));
    assert_eq!(invocation(&["box", "--test-heartbeat-ms=45", "run", "e30="]), beating(Some(45)));
    assert_eq!(invocation(&["box", "run", "e30="]), beating(None), "no seam given, none set");
    assert_eq!(
        invocation(&["box", "--test-heartbeat-ms", "2147483647", "run", "e30="]),
        beating(Some(2_147_483_647))
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_heartbeat_ms_refuses_what_the_run_seams_refuse() {
    let rows: [(&[&str], &[&str]); 10] = [
        (
            &["box", "--test-heartbeat-ms", "--test-heartbeat-ms=2", "run", "e30="],
            &["gate: --test-heartbeat-ms needs a value", "gate: --test-heartbeat-ms is given twice"],
        ),
        (
            &["box", "--test-heartbeat-ms=", "run", "e30="],
            &["gate: --test-heartbeat-ms needs a value"],
        ),
        (
            &["box", "--test-heartbeat-ms", "0", "run", "e30="],
            &["gate: --test-heartbeat-ms must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-heartbeat-ms", "fast", "run", "e30="],
            &["gate: --test-heartbeat-ms must be a whole number above 0, got 'fast'"],
        ),
        (
            &["box", "--test-heartbeat-ms", "2147483648", "run", "e30="],
            &["gate: --test-heartbeat-ms must be at most 2147483647, got '2147483648'"],
        ),
        (
            &["box", "--test-heartbeat-ms", "1", "--test-heartbeat-ms=2", "run", "e30="],
            &["gate: --test-heartbeat-ms is given twice"],
        ),
        (&["box", "--test-heartbeat-ms"], &["gate: --test-heartbeat-ms needs a value", BOX_USAGE]),
        (&["box", "run", "--test-heartbeat-ms", "30", "e30="], &[BOX_USAGE]),
        (&["box", "--test-heartbeat-msx", "30", "run", "e30="], &[BOX_USAGE]),
        (&["box", "--test-sample-ms", "30", "run", "e30="], &[BOX_USAGE]),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_heartbeat_ms_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-heartbeat-ms", "30", "run", "e30="]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-heartbeat-ms=30", "run", "e30="]), [BOX_USAGE]);
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
        problems(&refs(&with(&RUN, &["--test-remote-snapshot", "--", "cmd"]))),
        ["gate: unknown option --test-remote-snapshot"]
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
