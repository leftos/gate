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
            ..TestSeams::default()
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

/// The seam that starts the local watchdog's stall window at the command's first output byte is a switch of its own,
/// off in a run that does not give it.
#[cfg(feature = "test-seams")]
#[test]
fn the_stall_from_output_seam_is_a_switch() {
    use crate::support::run_request;

    let words = with(&RUN, &["--test-stall-from-output", "--", "cmd"]);
    assert!(run_request(&refs(&words)).test_seams.stall_from_output);
    assert!(
        !run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.stall_from_output,
        "off unless given"
    );
}

/// The seam that ends a run Ctrl+C during the local wait after a `full` fallback is a switch of its own, off in a run
/// that does not give it.
#[cfg(feature = "test-seams")]
#[test]
fn the_interrupt_after_fallback_seam_is_a_switch() {
    use crate::support::run_request;

    let words = with(&RUN, &["--test-interrupt-queued-after-fallback", "--", "cmd"]);
    let seams = run_request(&refs(&words)).test_seams;
    assert!(seams.interrupt_after_fallback);
    assert!(
        !seams.interrupt_queued,
        "it is its own switch, not the one that fires on any waiting line"
    );
    assert!(
        !run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.interrupt_after_fallback,
        "off unless given"
    );
}

/// The seam that raises Ctrl+C once a fallback walk has built the next builder is a switch of its own, off unless given.
#[cfg(feature = "test-seams")]
#[test]
fn the_interrupt_before_later_seam_parses_as_its_own_switch() {
    use crate::support::run_request;

    let words = with(&RUN, &["--test-interrupt-before-later", "--", "cmd"]);
    let seams = run_request(&refs(&words)).test_seams;
    assert!(seams.interrupt_before_later);
    assert!(!seams.interrupt_before_key, "it is its own switch");
    assert!(
        !run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.interrupt_before_later,
        "off unless given"
    );
}

/// The seams of the key wait: its bound is a whole number above zero, and the raise before the take is a switch of its
/// own, off unless given.
#[cfg(feature = "test-seams")]
#[test]
fn the_key_wait_seams_parse() {
    use crate::support::run_request;

    let words = with(&RUN, &["--test-key-wait-ms", "91", "--test-interrupt-before-key", "--", "cmd"]);
    let seams = run_request(&refs(&words)).test_seams;
    assert_eq!(seams.key_wait_ms, Some(91));
    assert!(seams.interrupt_before_key);
    assert!(!seams.interrupt_queued, "it is its own switch");
    assert!(
        !run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.interrupt_before_key,
        "off unless given"
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-key-wait-ms", "0", "--", "cmd"]))),
        ["gate: --test-key-wait-ms must be a whole number above 0, got '0'"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn the_diagnostics_budget_seam_is_read_once_and_refuses_zero() {
    use crate::support::run_request;

    let words = with(&RUN, &["--test-diagnostics-budget-ms", "1", "--", "cmd"]);
    assert_eq!(run_request(&refs(&words)).test_seams.diagnostics_budget_ms, Some(1));
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-diagnostics-budget-ms", "--", "cmd"]))),
        ["gate: --test-diagnostics-budget-ms needs a value"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-diagnostics-budget-ms", "0", "--", "cmd"]))),
        ["gate: --test-diagnostics-budget-ms must be a whole number above 0, got '0'"]
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
fn the_dotnet_limit_seam_is_read_once_and_refuses_zero() {
    use crate::support::run_request;

    let words = with(&RUN, &["--test-dotnet-limit-ms", "96", "--", "cmd"]);
    assert_eq!(run_request(&refs(&words)).test_seams.dotnet_limit_ms, Some(96));
    assert_eq!(run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.dotnet_limit_ms, None);
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-dotnet-limit-ms=", "--", "cmd"]))),
        ["gate: --test-dotnet-limit-ms needs a value"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-dotnet-limit-ms", "0", "--", "cmd"]))),
        ["gate: --test-dotnet-limit-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-dotnet-limit-ms", "1", "--test-dotnet-limit-ms", "2", "--", "cmd"]
        ))),
        ["gate: --test-dotnet-limit-ms is given twice"]
    );
    assert_eq!(
        problems(&[
            "-Log",
            "x",
            "-TimeoutSeconds",
            "5",
            "-Slot",
            "heavy",
            "--test-dotnet-limit-ms",
            "5",
            "--",
            "cmd"
        ]),
        ["gate: unknown option --test-dotnet-limit-ms", "gate: unexpected argument 5"]
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

#[cfg(feature = "test-seams")]
#[test]
fn the_own_start_seam_is_a_switch_given_once() {
    use crate::support::run_request;

    assert!(
        run_request(&refs(&with(&RUN, &["--test-own-start-fails", "--", "cmd"])))
            .test_seams
            .own_start_fails
    );
    assert!(
        !run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.own_start_fails,
        "off unless given"
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-own-start-fails", "--test-own-start-fails", "--", "cmd"]))),
        ["gate: --test-own-start-fails is given twice"]
    );
    assert_eq!(
        problems(&["stop-tree", "--test-own-start-fails", "1"]),
        ["gate: unknown option --test-own-start-fails"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn the_parent_job_seam_is_a_switch_given_once_in_either_form() {
    use crate::support::run_request;

    assert!(
        run_request(&refs(&with(&RUN, &["--test-parent-job-trusted", "--", "cmd"])))
            .test_seams
            .parent_job_trusted
    );
    assert!(
        !run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.parent_job_trusted,
        "off unless given"
    );
    let compat = run_request(&[
        "-Log",
        "x",
        "-TimeoutSeconds",
        "5",
        "-Slot",
        "critical",
        "-Reason",
        "a seam test",
        "--test-parent-job-trusted",
        "--",
        "cmd",
    ]);
    assert!(compat.test_seams.parent_job_trusted, "the compatibility layer reads the hidden switch");
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-parent-job-trusted", "--test-parent-job-trusted", "--", "cmd"]
        ))),
        ["gate: --test-parent-job-trusted is given twice"]
    );
    assert_eq!(
        problems(&["stop-tree", "--test-parent-job-trusted", "1"]),
        ["gate: unknown option --test-parent-job-trusted"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn the_remote_client_seams_need_a_value_once() {
    use crate::support::run_request;

    let seams = run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams;
    assert_eq!(
        (
            seams.remote_transport,
            seams.remote_ssh,
            seams.remote_retry_pause_ms,
            seams.remote_bound_ms
        ),
        (None, None, None, None),
        "none unless given"
    );
    let given = [
        "--test-remote-transport",
        r"local:target\builder",
        "--test-remote-ssh=fake-ssh.exe",
        "--test-remote-retry-pause-ms",
        "15",
        "--test-remote-bound-ms",
        "200",
        "--",
        "cmd",
    ];
    let seams = run_request(&refs(&with(&RUN, &given))).test_seams;
    assert_eq!(
        (
            seams.remote_transport,
            seams.remote_ssh,
            seams.remote_retry_pause_ms,
            seams.remote_bound_ms
        ),
        (
            Some(r"local:target\builder".to_owned()),
            Some(std::path::PathBuf::from("fake-ssh.exe")),
            Some(15),
            Some(200)
        )
    );
    for flag in [
        "--test-remote-transport",
        "--test-remote-ssh",
        "--test-remote-retry-pause-ms",
        "--test-remote-bound-ms",
        "--test-local-sdks",
    ] {
        assert_eq!(
            problems(&refs(&with(&RUN, &[flag, "--", "cmd"]))),
            [format!("gate: {flag} needs a value")],
            "{flag} without its value"
        );
        assert_eq!(
            problems(&refs(&with(&RUN, &[&format!("{flag}="), "--", "cmd"]))),
            [format!("gate: {flag} needs a value")],
            "{flag} with an empty value"
        );
        let transport: &[&str] = if flag == "--test-remote-transport" {
            &[]
        } else {
            &["--test-remote-transport", "local:b"]
        };
        let twice: Vec<&str> = transport.iter().copied().chain([flag, "1", flag, "2", "--", "cmd"]).collect();
        assert_eq!(
            problems(&refs(&with(&RUN, &twice))),
            [format!("gate: {flag} is given twice")],
            "{flag} twice"
        );
        assert_eq!(
            problems(&["stop-tree", flag, "5", "1"]),
            [format!("gate: unknown option {flag}"), "gate: unexpected argument 1".to_owned()],
            "{flag} on stop-tree"
        );
    }
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-remote-retry-pause-ms", "0", "--", "cmd"]))),
        ["gate: --test-remote-retry-pause-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-transport", "local:b", "--test-remote-bound-ms", "0", "--", "cmd"]
        ))),
        ["gate: --test-remote-bound-ms must be a whole number above 0, got '0'"]
    );
    for call in ["preflight", "lfs", "run", "outputs", "install", "sdks", "stop", "manifest", "apply"] {
        let words = with(
            &RUN,
            &["--test-remote-transport", "local:b", "--test-remote-interrupt-at", call, "--", "cmd"],
        );
        assert_eq!(run_request(&refs(&words)).test_seams.remote_interrupt_at.as_deref(), Some(call));
    }
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-transport", "local:b", "--test-remote-interrupt-at", "push", "--", "cmd"]
        ))),
        ["gate: --test-remote-interrupt-at must be preflight, inputs, lfs, run, outputs, install, sdks, stop, manifest or apply, got 'push'"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn the_remote_watch_seams_name_one_call_and_a_ceiling() {
    use crate::support::run_request;

    for call in [
        "preflight",
        "push",
        "inputs-diff",
        "inputs",
        "lfs-diff",
        "lfs",
        "run",
        "ls-remote",
        "fetch",
        "outputs",
    ] {
        let words = with(
            &RUN,
            &["--test-remote-transport", "local:b", "--test-remote-watch-call", call, "--", "cmd"],
        );
        assert_eq!(run_request(&refs(&words)).test_seams.remote_watch_call.as_deref(), Some(call));
    }
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-transport", "local:b", "--test-remote-watch-call", "stop", "--", "cmd"]
        ))),
        ["gate: --test-remote-watch-call must be preflight, push, inputs-diff, inputs, lfs-diff, lfs, run, ls-remote, fetch or outputs, got 'stop'"]
    );
    let words = with(
        &RUN,
        &[
            "--test-remote-transport",
            "local:b",
            "--test-remote-watch-call",
            "fetch",
            "--test-remote-timeout-ms",
            "300",
            "--",
            "cmd",
        ],
    );
    assert_eq!(run_request(&refs(&words)).test_seams.remote_timeout_ms, Some(300));
    let words = with(
        &RUN,
        &["--test-local-sdks", "999.0.100", "--test-remote-transport", "local:b", "--", "cmd"],
    );
    assert_eq!(run_request(&refs(&words)).test_seams.local_sdks.as_deref(), Some("999.0.100"));
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-transport", "local:b", "--test-remote-timeout-ms", "0", "--", "cmd"]
        ))),
        ["gate: --test-remote-timeout-ms must be a whole number above 0, got '0'"]
    );
    let none = run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams;
    assert_eq!((none.remote_watch_call, none.remote_timeout_ms), (None, None), "none unless given");
}

#[cfg(feature = "test-seams")]
#[test]
fn the_fetch_cap_seam_takes_a_whole_number_of_bytes_above_0() {
    use crate::support::run_request;

    let words = with(
        &RUN,
        &[
            "--test-remote-transport",
            "local:b",
            "--test-remote-fetch-cap-bytes",
            "262144",
            "--",
            "cmd",
        ],
    );
    assert_eq!(run_request(&refs(&words)).test_seams.remote_fetch_cap_bytes, Some(262_144));
    for value in ["0", "4GiB"] {
        assert_eq!(
            problems(&refs(&with(
                &RUN,
                &["--test-remote-transport", "local:b", "--test-remote-fetch-cap-bytes", value, "--", "cmd"]
            ))),
            [format!(
                "gate: --test-remote-fetch-cap-bytes must be a whole number above 0, got '{value}'"
            )]
        );
    }
    let none = run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams;
    assert_eq!(none.remote_fetch_cap_bytes, None, "none unless given");
}

#[cfg(feature = "test-seams")]
#[test]
fn the_outputs_budget_seam_takes_a_whole_number_of_bytes_above_0() {
    use crate::support::run_request;

    const FLAG: &str = "--test-remote-outputs-budget-bytes";
    let words = with(&RUN, &["--test-remote-transport", "local:b", FLAG, "1048576", "--", "cmd"]);
    assert_eq!(run_request(&refs(&words)).test_seams.remote_outputs_budget_bytes, Some(1_048_576));
    for value in ["0", "4GiB"] {
        assert_eq!(
            problems(&refs(&with(&RUN, &["--test-remote-transport", "local:b", FLAG, value, "--", "cmd"]))),
            [format!("gate: {FLAG} must be a whole number above 0, got '{value}'")]
        );
    }
    let none = run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams;
    assert_eq!(none.remote_outputs_budget_bytes, None, "none unless given");
}

#[cfg(feature = "test-seams")]
#[test]
fn the_inputs_budget_seam_takes_a_whole_number_of_bytes_above_0_once() {
    use crate::support::run_request;

    const FLAG: &str = "--test-remote-inputs-budget-bytes";
    let transport = ["--test-remote-transport", "local:b"];
    let words = with(&RUN, &[&transport[..], &[FLAG, "1048576", "--", "cmd"]].concat());
    assert_eq!(run_request(&refs(&words)).test_seams.remote_inputs_budget_bytes, Some(1_048_576));
    for (value, said) in [
        ("0", format!("gate: {FLAG} must be a whole number above 0, got '0'")),
        ("4GiB", format!("gate: {FLAG} must be a whole number above 0, got '4GiB'")),
        (
            "18446744073709551616",
            format!("gate: {FLAG} must be at most 2147483647, got '18446744073709551616'"),
        ),
    ] {
        let words = with(&RUN, &[&transport[..], &[FLAG, value, "--", "cmd"]].concat());
        assert_eq!(problems(&refs(&words)), [said], "{value}");
    }
    let twice = with(&RUN, &[&transport[..], &[FLAG, "1", FLAG, "2", "--", "cmd"]].concat());
    assert_eq!(problems(&refs(&twice)), [format!("gate: {FLAG} is given twice")]);
    let empty = format!("{FLAG}=");
    for missing in [&[FLAG][..], &[empty.as_str()][..]] {
        let words = with(&RUN, &[&transport[..], missing, &["--", "cmd"]].concat());
        assert_eq!(problems(&refs(&words)), [format!("gate: {FLAG} needs a value")], "{missing:?}");
    }
    let none = run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams;
    assert_eq!(none.remote_inputs_budget_bytes, None, "none unless given");
}

#[cfg(feature = "test-seams")]
#[test]
fn the_log_cap_seam_takes_a_whole_number_of_bytes_above_0_once() {
    use crate::support::run_request;

    const FLAG: &str = "--test-remote-log-cap-bytes";
    let words = with(&RUN, &["--test-remote-transport", "local:b", FLAG, "65536", "--", "cmd"]);
    assert_eq!(run_request(&refs(&words)).test_seams.remote_log_cap_bytes, Some(65_536));
    let spelled_with = with(
        &RUN,
        &["--test-remote-transport", "local:b", "--test-remote-log-cap-bytes=1", "--", "cmd"],
    );
    assert_eq!(run_request(&refs(&spelled_with)).test_seams.remote_log_cap_bytes, Some(1));
    for value in ["0", "256MiB"] {
        assert_eq!(
            problems(&refs(&with(&RUN, &["--test-remote-transport", "local:b", FLAG, value, "--", "cmd"]))),
            [format!("gate: {FLAG} must be a whole number above 0, got '{value}'")],
            "{value:?}"
        );
    }
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-transport", "local:b", FLAG, "1", FLAG, "2", "--", "cmd"]
        ))),
        [format!("gate: {FLAG} is given twice")]
    );
    let none = run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams;
    assert_eq!(none.remote_log_cap_bytes, None, "none unless given");
}

#[cfg(feature = "test-seams")]
#[test]
fn the_tamper_seam_names_one_point_and_step_and_needs_the_transport() {
    use crate::support::run_request;

    const FLAG: &str = "--test-remote-tamper";
    for tamper in [
        "before-hold:case",
        "before-hold:replace",
        "before-hold:replace-root",
        "after-first-snapshot:worktree",
    ] {
        let words = with(&RUN, &["--test-remote-transport", "local:b", FLAG, tamper, "--", "cmd"]);
        assert_eq!(run_request(&refs(&words)).test_seams.remote_tamper.as_deref(), Some(tamper));
    }
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-transport", "local:b", FLAG, "after-first-snapshot:case", "--", "cmd"]
        ))),
        [format!(
            "gate: {FLAG} must be before-hold:case, before-hold:replace, before-hold:replace-root or \
             after-first-snapshot:worktree, got 'after-first-snapshot:case'"
        )]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &[FLAG, "before-hold:case", "--", "cmd"]))),
        [format!("gate: {FLAG} needs --test-remote-transport")]
    );
    let none = run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams;
    assert_eq!(none.remote_tamper, None, "none unless given");
}

#[cfg(feature = "test-seams")]
#[test]
fn the_apply_cap_seam_takes_a_whole_number_of_bytes_above_0_once() {
    use crate::support::run_request;

    const FLAG: &str = "--test-remote-apply-cap-bytes";
    let words = with(&RUN, &["--test-remote-transport", "local:b", FLAG, "1048576", "--", "cmd"]);
    assert_eq!(run_request(&refs(&words)).test_seams.remote_apply_cap_bytes, Some(1_048_576));
    for value in ["0", "4GiB"] {
        assert_eq!(
            problems(&refs(&with(&RUN, &["--test-remote-transport", "local:b", FLAG, value, "--", "cmd"]))),
            [format!("gate: {FLAG} must be a whole number above 0, got '{value}'")]
        );
    }
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-transport", "local:b", FLAG, "1", FLAG, "2", "--", "cmd"]
        ))),
        [format!("gate: {FLAG} is given twice")]
    );
    let none = run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams;
    assert_eq!(none.remote_apply_cap_bytes, None, "none unless given");
}

#[cfg(feature = "test-seams")]
#[test]
fn the_remote_client_seams_that_conflict_are_refused() {
    for (words, problem) in [
        (
            &["--test-remote-ssh", "fake-ssh.exe"][..],
            "gate: --test-remote-ssh needs --test-remote-transport",
        ),
        (
            &["--test-remote-retry-pause-ms", "5"][..],
            "gate: --test-remote-retry-pause-ms needs --test-remote-transport",
        ),
        (
            &["--test-remote-bound-ms", "5"][..],
            "gate: --test-remote-bound-ms needs --test-remote-transport",
        ),
        (
            &["--test-remote-interrupt-at", "run"][..],
            "gate: --test-remote-interrupt-at needs --test-remote-transport",
        ),
        (
            &["--test-remote-watch-call", "run"][..],
            "gate: --test-remote-watch-call needs --test-remote-transport",
        ),
        (
            &["--test-remote-timeout-ms", "300"][..],
            "gate: --test-remote-timeout-ms needs --test-remote-transport",
        ),
        (
            &["--test-local-sdks", "999.0.100"][..],
            "gate: --test-local-sdks needs --test-remote-transport",
        ),
        (
            &["--test-remote-fetch-cap-bytes", "262144"][..],
            "gate: --test-remote-fetch-cap-bytes needs --test-remote-transport",
        ),
        (
            &["--test-remote-apply-cap-bytes", "1048576"][..],
            "gate: --test-remote-apply-cap-bytes needs --test-remote-transport",
        ),
        (
            &["--test-remote-outputs-budget-bytes", "1048576"][..],
            "gate: --test-remote-outputs-budget-bytes needs --test-remote-transport",
        ),
        (
            &["--test-remote-inputs-budget-bytes", "1048576"][..],
            "gate: --test-remote-inputs-budget-bytes needs --test-remote-transport",
        ),
        (
            &["--test-remote-log-cap-bytes", "65536"][..],
            "gate: --test-remote-log-cap-bytes needs --test-remote-transport",
        ),
        (
            &["--test-remote-backstop-ms", "300"][..],
            "gate: --test-remote-backstop-ms needs --test-remote-transport",
        ),
        (
            &["--test-remote-snapshot", "--test-remote-transport", "local:b"][..],
            "gate: --test-remote-snapshot cannot be given with --test-remote-transport",
        ),
    ] {
        let given: Vec<&str> = words.iter().copied().chain(["--", "cmd"]).collect();
        assert_eq!(problems(&refs(&with(&RUN, &given))), [problem], "{words:?}");
    }
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &["--test-remote-ssh", "fake-ssh.exe", "--test-remote-retry-pause-ms", "5", "--", "cmd"]
        ))),
        [
            "gate: --test-remote-ssh needs --test-remote-transport",
            "gate: --test-remote-retry-pause-ms needs --test-remote-transport"
        ],
        "each seam that needs the transport is named"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn the_remote_timeout_seam_needs_a_watched_call_other_than_run() {
    use crate::support::run_request;

    const NEEDS: &str = "gate: --test-remote-timeout-ms needs --test-remote-watch-call naming a call other than run";
    let given = |words: &[&'static str]| -> Vec<String> {
        let words: Vec<&str> = ["--test-remote-transport", "local:b"]
            .into_iter()
            .chain(words.iter().copied())
            .chain(["--", "cmd"])
            .collect();
        with(&RUN, &words)
    };
    for words in [
        &["--test-remote-timeout-ms", "300"][..],
        &["--test-remote-watch-call", "run", "--test-remote-timeout-ms", "300"][..],
    ] {
        assert_eq!(problems(&refs(&given(words))), [NEEDS], "{words:?}");
    }
    assert_eq!(
        problems(&refs(&given(&["--test-remote-watch-call", "stop", "--test-remote-timeout-ms", "300"]))),
        ["gate: --test-remote-watch-call must be preflight, push, inputs-diff, inputs, lfs-diff, lfs, run, ls-remote, fetch or outputs, got 'stop'"],
        "a refused watched call says its own problem alone"
    );
    let fetch = run_request(&refs(&given(&["--test-remote-watch-call", "fetch", "--test-remote-timeout-ms", "300"])));
    assert_eq!(fetch.test_seams.remote_timeout_ms, Some(300), "another call takes the seam");
    let stall = run_request(&refs(&given(&["--test-remote-watch-call", "run", "--test-stall-ms", "300"])));
    assert_eq!(stall.test_seams.stall_ms, Some(300), "the run call still takes the stall seam");
    let given = with(
        &RUN,
        &["--test-remote-transport", "local:b", "--test-remote-backstop-ms", "300", "--", "cmd"],
    );
    assert_eq!(
        run_request(&refs(&given)).test_seams.remote_backstop_ms,
        Some(300),
        "the backstop seam is read"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seams_of_the_ceiling_refuse_a_bad_missing_or_repeated_value() {
    let rows: [(&[&str], &[&str]); 14] = [
        (
            &["box", "--test-box-kill-leaves", "0", "run", "--stdin"],
            &["gate: --test-box-kill-leaves must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-box-kill-leaves=", "run", "--stdin"],
            &["gate: --test-box-kill-leaves needs a value"],
        ),
        (
            &["box", "--test-box-kill-leaves"],
            &["gate: --test-box-kill-leaves needs a value", BOX_USAGE],
        ),
        (
            &["box", "--test-box-kill-leaves", "1", "--test-box-kill-leaves=2", "run", "--stdin"],
            &["gate: --test-box-kill-leaves is given twice"],
        ),
        (&["box", "run", "--test-box-kill-leaves", "1", "--stdin"], &[BOX_USAGE]),
        (
            &["box", "--test-box-sample-ms", "0", "run", "--stdin"],
            &["gate: --test-box-sample-ms must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-box-sample-ms=", "run", "--stdin"],
            &["gate: --test-box-sample-ms needs a value"],
        ),
        (&["box", "--test-box-sample-ms"], &["gate: --test-box-sample-ms needs a value", BOX_USAGE]),
        (
            &["box", "--test-box-sample-ms", "1", "--test-box-sample-ms=2", "run", "--stdin"],
            &["gate: --test-box-sample-ms is given twice"],
        ),
        (
            &["box", "--test-box-sampler-fail", "maybe", "run", "--stdin"],
            &["gate: --test-box-sampler-fail takes true or false, got 'maybe'"],
        ),
        (
            &["box", "--test-box-free-percent", "0", "run", "--stdin"],
            &["gate: --test-box-free-percent must be a whole number from 1 to 100, got '0'"],
        ),
        (
            &["box", "--test-box-free-percent", "101", "run", "--stdin"],
            &["gate: --test-box-free-percent must be a whole number from 1 to 100, got '101'"],
        ),
        (
            &["box", "--test-box-free-percent", "5", "--test-box-free-percent=6", "run", "--stdin"],
            &["gate: --test-box-free-percent is given twice"],
        ),
        (
            &["box", "--test-box-free-percent=", "run", "--stdin"],
            &["gate: --test-box-free-percent needs a value"],
        ),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seams_of_the_ceiling_are_read_before_the_verb() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    assert_eq!(
        invocation(&[
            "box",
            "--test-box-sample-ms",
            "20",
            "--test-box-sampler-fail=true",
            "--test-box-free-percent",
            "5",
            "--test-box-kill-leaves=3",
            "run",
            "--stdin"
        ]),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            seams: BoxSeams {
                sample_ms: Some(20),
                sampler_fail: true,
                free_percent: Some(5),
                kill_leaves: Some(3),
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    );
}

#[test]
fn the_bridge_seams_are_unknown_options() {
    for flag in ["--test-remote-script", "--test-bridge-pwsh"] {
        assert_eq!(
            problems(&refs(&with(&RUN, &[flag, "5", "--", "cmd"]))),
            [format!("gate: unknown option {flag}"), "gate: unexpected argument 5".to_owned()],
            "{flag}"
        );
    }
}

/// The usage a malformed `gate box` prints.
const BOX_USAGE: &str = concat!(
    "usage: gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs --stdin (the base64 JSON request as stdin's first line)\n",
    "       gate box prune --root <folder> [--keys <key>...]",
);

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_heartbeat_ms_is_read_before_the_verb() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let beating = |heartbeat_ms| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            seams: BoxSeams {
                heartbeat_ms,
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    };
    assert_eq!(invocation(&["box", "--test-heartbeat-ms", "30", "run", "--stdin"]), beating(Some(30)));
    assert_eq!(invocation(&["box", "--test-heartbeat-ms=45", "run", "--stdin"]), beating(Some(45)));
    assert_eq!(invocation(&["box", "run", "--stdin"]), beating(None), "no seam given, none set");
    assert_eq!(
        invocation(&["box", "--test-heartbeat-ms", "2147483647", "run", "--stdin"]),
        beating(Some(2_147_483_647))
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_heartbeat_ms_refuses_what_the_run_seams_refuse() {
    let rows: [(&[&str], &[&str]); 10] = [
        (
            &["box", "--test-heartbeat-ms", "--test-heartbeat-ms=2", "run", "--stdin"],
            &["gate: --test-heartbeat-ms needs a value", "gate: --test-heartbeat-ms is given twice"],
        ),
        (
            &["box", "--test-heartbeat-ms=", "run", "--stdin"],
            &["gate: --test-heartbeat-ms needs a value"],
        ),
        (
            &["box", "--test-heartbeat-ms", "0", "run", "--stdin"],
            &["gate: --test-heartbeat-ms must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-heartbeat-ms", "fast", "run", "--stdin"],
            &["gate: --test-heartbeat-ms must be a whole number above 0, got 'fast'"],
        ),
        (
            &["box", "--test-heartbeat-ms", "2147483648", "run", "--stdin"],
            &["gate: --test-heartbeat-ms must be at most 2147483647, got '2147483648'"],
        ),
        (
            &["box", "--test-heartbeat-ms", "1", "--test-heartbeat-ms=2", "run", "--stdin"],
            &["gate: --test-heartbeat-ms is given twice"],
        ),
        (&["box", "--test-heartbeat-ms"], &["gate: --test-heartbeat-ms needs a value", BOX_USAGE]),
        (&["box", "run", "--test-heartbeat-ms", "30", "--stdin"], &[BOX_USAGE]),
        (&["box", "--test-heartbeat-msx", "30", "run", "--stdin"], &[BOX_USAGE]),
        (&["box", "--test-sample-ms", "30", "run", "--stdin"], &[BOX_USAGE]),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_heartbeat_ms_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-heartbeat-ms", "30", "run", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-heartbeat-ms=30", "run", "--stdin"]), [BOX_USAGE]);
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_outputs_budget_bytes_is_read_before_the_verb() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let budgeted = |outputs_budget_bytes| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            seams: BoxSeams {
                outputs_budget_bytes,
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    };
    assert_eq!(
        invocation(&["box", "--test-outputs-budget-bytes", "1024", "run", "--stdin"]),
        budgeted(Some(1024))
    );
    assert_eq!(
        invocation(&["box", "--test-outputs-budget-bytes=1024", "run", "--stdin"]),
        budgeted(Some(1024))
    );
    assert_eq!(invocation(&["box", "run", "--stdin"]), budgeted(None), "no seam given, none set");
    assert_eq!(
        invocation(&["box", "--test-outputs-budget-bytes", "2147483647", "run", "--stdin"]),
        budgeted(Some(2_147_483_647))
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_outputs_budget_bytes_refuses_what_the_run_seams_refuse() {
    let rows: [(&[&str], &[&str]); 7] = [
        (
            &["box", "--test-outputs-budget-bytes", "--test-outputs-budget-bytes=2", "run", "--stdin"],
            &[
                "gate: --test-outputs-budget-bytes needs a value",
                "gate: --test-outputs-budget-bytes is given twice",
            ],
        ),
        (
            &["box", "--test-outputs-budget-bytes=", "run", "--stdin"],
            &["gate: --test-outputs-budget-bytes needs a value"],
        ),
        (
            &["box", "--test-outputs-budget-bytes", "0", "run", "--stdin"],
            &["gate: --test-outputs-budget-bytes must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-outputs-budget-bytes", "fast", "run", "--stdin"],
            &["gate: --test-outputs-budget-bytes must be a whole number above 0, got 'fast'"],
        ),
        (
            &["box", "--test-outputs-budget-bytes", "2147483648", "run", "--stdin"],
            &["gate: --test-outputs-budget-bytes must be at most 2147483647, got '2147483648'"],
        ),
        (
            &[
                "box",
                "--test-outputs-budget-bytes",
                "1",
                "--test-outputs-budget-bytes=2",
                "run",
                "--stdin",
            ],
            &["gate: --test-outputs-budget-bytes is given twice"],
        ),
        (
            &["box", "--test-outputs-budget-bytes"],
            &["gate: --test-outputs-budget-bytes needs a value", BOX_USAGE],
        ),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_inputs_budget_bytes_is_read_before_the_verb() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let budgeted = |inputs_budget_bytes| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Inputs,
            seams: BoxSeams {
                inputs_budget_bytes,
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    };
    assert_eq!(
        invocation(&["box", "--test-inputs-budget-bytes", "1024", "inputs", "--stdin"]),
        budgeted(Some(1024))
    );
    assert_eq!(
        invocation(&["box", "--test-inputs-budget-bytes=1024", "inputs", "--stdin"]),
        budgeted(Some(1024))
    );
    assert_eq!(invocation(&["box", "inputs", "--stdin"]), budgeted(None), "no seam given, none set");
    assert_eq!(
        invocation(&["box", "--test-inputs-budget-bytes", "2147483647", "inputs", "--stdin"]),
        budgeted(Some(2_147_483_647))
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_inputs_budget_bytes_refuses_what_the_outputs_budget_refuses() {
    let rows: [(&[&str], &[&str]); 7] = [
        (
            &["box", "--test-inputs-budget-bytes", "--test-inputs-budget-bytes=2", "inputs", "--stdin"],
            &[
                "gate: --test-inputs-budget-bytes needs a value",
                "gate: --test-inputs-budget-bytes is given twice",
            ],
        ),
        (
            &["box", "--test-inputs-budget-bytes=", "inputs", "--stdin"],
            &["gate: --test-inputs-budget-bytes needs a value"],
        ),
        (
            &["box", "--test-inputs-budget-bytes", "0", "inputs", "--stdin"],
            &["gate: --test-inputs-budget-bytes must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-inputs-budget-bytes", "fast", "inputs", "--stdin"],
            &["gate: --test-inputs-budget-bytes must be a whole number above 0, got 'fast'"],
        ),
        (
            &["box", "--test-inputs-budget-bytes", "2147483648", "inputs", "--stdin"],
            &["gate: --test-inputs-budget-bytes must be at most 2147483647, got '2147483648'"],
        ),
        (
            &[
                "box",
                "--test-inputs-budget-bytes",
                "1",
                "--test-inputs-budget-bytes=2",
                "inputs",
                "--stdin",
            ],
            &["gate: --test-inputs-budget-bytes is given twice"],
        ),
        (
            &["box", "--test-inputs-budget-bytes"],
            &["gate: --test-inputs-budget-bytes needs a value", BOX_USAGE],
        ),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_inputs_budget_bytes_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-inputs-budget-bytes", "30", "inputs", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-inputs-budget-bytes=30", "inputs", "--stdin"]), [BOX_USAGE]);
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_outputs_budget_bytes_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-outputs-budget-bytes", "30", "run", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-outputs-budget-bytes=30", "run", "--stdin"]), [BOX_USAGE]);
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_dotnet_limit_ms_is_read_before_the_verb() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let limited = |dotnet_limit_ms| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Preflight,
            seams: BoxSeams {
                dotnet_limit_ms,
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    };
    assert_eq!(
        invocation(&["box", "--test-dotnet-limit-ms", "1000", "preflight", "--stdin"]),
        limited(Some(1000))
    );
    assert_eq!(
        invocation(&["box", "--test-dotnet-limit-ms=1500", "preflight", "--stdin"]),
        limited(Some(1500))
    );
    assert_eq!(invocation(&["box", "preflight", "--stdin"]), limited(None), "no seam given, none set");
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_dotnet_limit_ms_refuses_what_the_run_seams_refuse() {
    let rows: [(&[&str], &[&str]); 5] = [
        (
            &["box", "--test-dotnet-limit-ms", "1", "--test-dotnet-limit-ms=2", "preflight", "--stdin"],
            &["gate: --test-dotnet-limit-ms is given twice"],
        ),
        (
            &["box", "--test-dotnet-limit-ms", "0", "preflight", "--stdin"],
            &["gate: --test-dotnet-limit-ms must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-dotnet-limit-ms", "fast", "preflight", "--stdin"],
            &["gate: --test-dotnet-limit-ms must be a whole number above 0, got 'fast'"],
        ),
        (
            &["box", "--test-dotnet-limit-ms=", "preflight", "--stdin"],
            &["gate: --test-dotnet-limit-ms needs a value"],
        ),
        (
            &["box", "--test-dotnet-limit-ms"],
            &["gate: --test-dotnet-limit-ms needs a value", BOX_USAGE],
        ),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_dotnet_limit_ms_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-dotnet-limit-ms", "1000", "preflight", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-dotnet-limit-ms=1000", "preflight", "--stdin"]), [BOX_USAGE]);
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_probe_limit_ms_is_read_before_the_verb() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let limited = |probe_limit_ms| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Preflight,
            seams: BoxSeams {
                probe_limit_ms,
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    };
    assert_eq!(
        invocation(&["box", "--test-probe-limit-ms", "200", "preflight", "--stdin"]),
        limited(Some(200))
    );
    assert_eq!(
        invocation(&["box", "--test-probe-limit-ms=1500", "preflight", "--stdin"]),
        limited(Some(1500))
    );
    assert_eq!(invocation(&["box", "preflight", "--stdin"]), limited(None), "no seam given, none set");
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_probe_limit_ms_refuses_what_the_dotnet_limit_refuses() {
    let rows: [(&[&str], &[&str]); 5] = [
        (
            &["box", "--test-probe-limit-ms", "1", "--test-probe-limit-ms=2", "preflight", "--stdin"],
            &["gate: --test-probe-limit-ms is given twice"],
        ),
        (
            &["box", "--test-probe-limit-ms", "0", "preflight", "--stdin"],
            &["gate: --test-probe-limit-ms must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-probe-limit-ms", "fast", "preflight", "--stdin"],
            &["gate: --test-probe-limit-ms must be a whole number above 0, got 'fast'"],
        ),
        (
            &["box", "--test-probe-limit-ms=", "preflight", "--stdin"],
            &["gate: --test-probe-limit-ms needs a value"],
        ),
        (
            &["box", "--test-probe-limit-ms"],
            &["gate: --test-probe-limit-ms needs a value", BOX_USAGE],
        ),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_git_limit_names_a_step_and_its_bound_before_the_verb() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let limited = |git_limit: Option<(&str, u64)>| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            seams: BoxSeams {
                git_limit: git_limit.map(|(step, ms)| (step.to_owned(), ms)),
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    };
    assert_eq!(
        invocation(&["box", "--test-box-git-limit", "checkout:2000", "run", "--stdin"]),
        limited(Some(("checkout", 2000)))
    );
    assert_eq!(
        invocation(&["box", "--test-box-git-limit=git init --bare:1500", "run", "--stdin"]),
        limited(Some(("git init --bare", 1500))),
        "a step of several words"
    );
    assert_eq!(
        invocation(&["box", "--test-box-git-limit", "a:b:30", "run", "--stdin"]),
        limited(Some(("a:b", 30))),
        "split at the last colon"
    );
    assert_eq!(invocation(&["box", "run", "--stdin"]), limited(None), "no seam given, none set");
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_git_limit_refuses_a_value_without_a_step_or_whole_milliseconds() {
    let rows: [(&[&str], &[&str]); 6] = [
        (
            &["box", "--test-box-git-limit", "checkout", "run", "--stdin"],
            &["gate: --test-box-git-limit must be <step>:<ms>, got 'checkout'"],
        ),
        (
            &["box", "--test-box-git-limit", ":2000", "run", "--stdin"],
            &["gate: --test-box-git-limit must be <step>:<ms>, got ':2000'"],
        ),
        (
            &["box", "--test-box-git-limit", "checkout:0", "run", "--stdin"],
            &["gate: --test-box-git-limit must be a whole number above 0, got 'checkout:0'"],
        ),
        (
            &["box", "--test-box-git-limit", "checkout:fast", "run", "--stdin"],
            &["gate: --test-box-git-limit must be a whole number above 0, got 'checkout:fast'"],
        ),
        (
            &["box", "--test-box-git-limit", "a:1", "--test-box-git-limit=b:2", "run", "--stdin"],
            &["gate: --test-box-git-limit is given twice"],
        ),
        (&["box", "--test-box-git-limit"], &["gate: --test-box-git-limit needs a value", BOX_USAGE]),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_git_armed_by_names_the_file_that_arms_a_bound() {
    use std::path::PathBuf;

    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let armed = |git_armed_by: Option<&str>| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            seams: BoxSeams {
                git_limit: Some(("checkout".to_owned(), 2000)),
                git_armed_by: git_armed_by.map(PathBuf::from),
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    };
    assert_eq!(
        invocation(&[
            "box",
            "--test-box-git-limit",
            "checkout:2000",
            "--test-box-git-armed-by",
            "C:\\case\\parked",
            "run",
            "--stdin",
        ]),
        armed(Some("C:\\case\\parked"))
    );
    assert_eq!(
        invocation(&[
            "box",
            "--test-box-git-limit=checkout:2000",
            "--test-box-git-armed-by=C:\\case\\parked",
            "run",
            "--stdin",
        ]),
        armed(Some("C:\\case\\parked")),
        "each seam in its = spelling"
    );
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_git_armed_by_is_an_unknown_word_without_the_feature() {
    let words = [
        "box",
        "--test-box-git-limit",
        "checkout:2000",
        "--test-box-git-armed-by",
        "parked",
        "run",
        "--stdin",
    ];
    assert_eq!(problems(&words), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-box-git-armed-by=parked", "run", "--stdin"]), [BOX_USAGE]);
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_git_armed_by_needs_the_git_limit_it_arms() {
    let rows: [(&[&str], &[&str]); 3] = [
        (
            &["box", "--test-box-git-armed-by", "C:\\case\\parked", "run", "--stdin"],
            &["gate: --test-box-git-armed-by needs --test-box-git-limit"],
        ),
        (
            &[
                "box",
                "--test-box-git-limit",
                "checkout",
                "--test-box-git-armed-by",
                "C:\\case\\parked",
                "run",
                "--stdin",
            ],
            &["gate: --test-box-git-limit must be <step>:<ms>, got 'checkout'"],
        ),
        (
            &[
                "box",
                "--test-box-git-limit",
                "checkout:2000",
                "--test-box-git-armed-by",
                "C:\\case\\parked",
                "--test-box-git-armed-by=C:\\case\\other",
                "run",
                "--stdin",
            ],
            &["gate: --test-box-git-armed-by is given twice"],
        ),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn the_probe_limit_seam_is_refused_outside_gate_box() {
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-probe-limit-ms", "5", "--", "cmd"]))),
        ["gate: unknown option --test-probe-limit-ms", "gate: unexpected argument 5"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_probe_drain_ms_is_read_before_the_verb() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let drained = |probe_drain_ms| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Preflight,
            seams: BoxSeams {
                probe_drain_ms,
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    };
    assert_eq!(
        invocation(&["box", "--test-probe-drain-ms", "200", "preflight", "--stdin"]),
        drained(Some(200))
    );
    assert_eq!(
        invocation(&["box", "--test-probe-drain-ms=1500", "preflight", "--stdin"]),
        drained(Some(1500))
    );
    assert_eq!(invocation(&["box", "preflight", "--stdin"]), drained(None), "no seam given, none set");
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_probe_drain_ms_refuses_what_the_probe_limit_refuses() {
    let rows: [(&[&str], &[&str]); 5] = [
        (
            &["box", "--test-probe-drain-ms", "1", "--test-probe-drain-ms=2", "preflight", "--stdin"],
            &["gate: --test-probe-drain-ms is given twice"],
        ),
        (
            &["box", "--test-probe-drain-ms", "0", "preflight", "--stdin"],
            &["gate: --test-probe-drain-ms must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-probe-drain-ms", "fast", "preflight", "--stdin"],
            &["gate: --test-probe-drain-ms must be a whole number above 0, got 'fast'"],
        ),
        (
            &["box", "--test-probe-drain-ms=", "preflight", "--stdin"],
            &["gate: --test-probe-drain-ms needs a value"],
        ),
        (
            &["box", "--test-probe-drain-ms"],
            &["gate: --test-probe-drain-ms needs a value", BOX_USAGE],
        ),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_probe_drain_ms_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-probe-drain-ms", "1000", "preflight", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-probe-drain-ms=1000", "preflight", "--stdin"]), [BOX_USAGE]);
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_probe_limit_ms_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-probe-limit-ms", "1000", "preflight", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-probe-limit-ms=1000", "preflight", "--stdin"]), [BOX_USAGE]);
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_console_and_park_launch_are_read_before_the_verb() {
    use std::path::PathBuf;

    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let running = |seams| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            seams,
            root: None,
            keys: Vec::new(),
        })
    };
    let failing = || BoxSeams {
        fail_console: true,
        ..BoxSeams::default()
    };
    let parked = || BoxSeams {
        park_launch: Some(PathBuf::from(r"X:\case\park")),
        ..BoxSeams::default()
    };
    assert_eq!(
        invocation(&["box", "--test-box-console", "fail-before", "run", "--stdin"]),
        running(failing())
    );
    assert_eq!(
        invocation(&["box", "--test-box-console=fail-before", "run", "--stdin"]),
        running(failing())
    );
    assert_eq!(
        invocation(&["box", "--test-box-park-launch", r"X:\case\park", "run", "--stdin"]),
        running(parked())
    );
    assert_eq!(
        invocation(&["box", "--test-box-park-launch=X:\\case\\park", "run", "--stdin"]),
        running(parked())
    );
    assert_eq!(
        invocation(&[
            "box",
            "--test-box-park-launch",
            r"X:\case\park",
            "--test-heartbeat-ms",
            "30",
            "--test-box-console",
            "fail-before",
            "stop",
            "--stdin"
        ]),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Stop,
            seams: BoxSeams {
                heartbeat_ms: Some(30),
                fail_console: true,
                refuse_token: false,
                park_launch: Some(PathBuf::from(r"X:\case\park")),
                outputs_budget_bytes: None,
                dotnet_limit_ms: None,
                probe_limit_ms: None,
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        }),
        "the seams come in any order before the verb"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_console_and_park_launch_refuse_a_bad_missing_or_repeated_value() {
    let rows: [(&[&str], &[&str]); 12] = [
        (
            &["box", "--test-box-console", "fail-sideways", "run", "--stdin"],
            &["gate: --test-box-console must be fail-before, got 'fail-sideways'"],
        ),
        (
            &["box", "--test-box-console", "fail-after", "run", "--stdin"],
            &["gate: --test-box-console must be fail-before, got 'fail-after'"],
        ),
        (
            &["box", "--test-box-console", "FAIL-BEFORE", "run", "--stdin"],
            &["gate: --test-box-console must be fail-before, got 'FAIL-BEFORE'"],
        ),
        (
            &["box", "--test-box-console=", "run", "--stdin"],
            &["gate: --test-box-console needs a value"],
        ),
        (
            &["box", "--test-box-console", "--test-box-park-launch", "p", "run", "--stdin"],
            &["gate: --test-box-console needs a value"],
        ),
        (
            &[
                "box",
                "--test-box-console",
                "fail-before",
                "--test-box-console=fail-before",
                "run",
                "--stdin",
            ],
            &["gate: --test-box-console is given twice"],
        ),
        (
            &["box", "--test-box-park-launch=", "run", "--stdin"],
            &["gate: --test-box-park-launch needs a value"],
        ),
        (
            &["box", "--test-box-park-launch", "--test-heartbeat-ms", "30", "run", "--stdin"],
            &["gate: --test-box-park-launch needs a value"],
        ),
        (
            &["box", "--test-box-park-launch", "a", "--test-box-park-launch=b", "run", "--stdin"],
            &["gate: --test-box-park-launch is given twice"],
        ),
        (&["box", "--test-box-park-restart", "p", "run", "--stdin"], &[BOX_USAGE]),
        (&["box", "run", "--test-box-console", "fail-before", "--stdin"], &[BOX_USAGE]),
        (&["box", "--test-box-consolex", "fail-before", "run", "--stdin"], &[BOX_USAGE]),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_console_and_park_launch_are_unknown_words_without_the_feature() {
    assert_eq!(problems(&["box", "--test-box-console", "fail-before", "run", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-box-console=fail-before", "run", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-box-park-launch", "park", "run", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-box-park-launch=park", "run", "--stdin"]), [BOX_USAGE]);
    for (seam, value) in [
        ("--test-box-sample-ms", "20"),
        ("--test-box-sampler-fail", "true"),
        ("--test-box-free-percent", "5"),
        ("--test-box-kill-leaves", "1"),
    ] {
        assert_eq!(problems(&["box", seam, value, "run", "--stdin"]), [BOX_USAGE], "{seam}");
        let joined = format!("{seam}={value}");
        assert_eq!(problems(&["box", joined.as_str(), "run", "--stdin"]), [BOX_USAGE], "{joined}");
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_root_walk_stop_is_read_before_the_verb() {
    use std::path::PathBuf;

    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let stopped = Invocation::Box(BoxRequest {
        verb: BoxVerb::Preflight,
        seams: BoxSeams {
            root_walk_stop: Some(PathBuf::from(r"X:\case")),
            ..BoxSeams::default()
        },
        root: None,
        keys: Vec::new(),
    });
    assert_eq!(
        invocation(&["box", "--test-box-root-walk-stop", r"X:\case", "preflight", "--stdin"]),
        stopped
    );
    assert_eq!(
        invocation(&["box", "--test-box-root-walk-stop=X:\\case", "preflight", "--stdin"]),
        stopped
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_root_walk_stop_refuses_a_missing_or_repeated_value() {
    let rows: [(&[&str], &[&str]); 3] = [
        (
            &["box", "--test-box-root-walk-stop=", "preflight", "--stdin"],
            &["gate: --test-box-root-walk-stop needs a value"],
        ),
        (
            &["box", "--test-box-root-walk-stop", "--test-heartbeat-ms", "30", "preflight", "--stdin"],
            &["gate: --test-box-root-walk-stop needs a value"],
        ),
        (
            &[
                "box",
                "--test-box-root-walk-stop",
                "a",
                "--test-box-root-walk-stop=b",
                "preflight",
                "--stdin",
            ],
            &["gate: --test-box-root-walk-stop is given twice"],
        ),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_root_walk_stop_is_an_unknown_word_without_the_feature() {
    assert_eq!(
        problems(&["box", "--test-box-root-walk-stop", "case", "preflight", "--stdin"]),
        [BOX_USAGE]
    );
    assert_eq!(problems(&["box", "--test-box-root-walk-stop=case", "preflight", "--stdin"]), [BOX_USAGE]);
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_token_is_read_before_the_verb() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let refused = Invocation::Box(BoxRequest {
        verb: BoxVerb::Run,
        seams: BoxSeams {
            refuse_token: true,
            ..BoxSeams::default()
        },
        root: None,
        keys: Vec::new(),
    });
    assert_eq!(invocation(&["box", "--test-box-token", "refused", "run", "--stdin"]), refused);
    assert_eq!(invocation(&["box", "--test-box-token=refused", "run", "--stdin"]), refused);
    assert_eq!(
        invocation(&[
            "box",
            "--test-box-console",
            "fail-before",
            "--test-box-token",
            "refused",
            "run",
            "--stdin"
        ]),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            seams: BoxSeams {
                fail_console: true,
                refuse_token: true,
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        }),
        "the token seam comes with the others, in any order"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_token_refuses_a_bad_missing_or_repeated_value() {
    let rows: [(&[&str], &[&str]); 6] = [
        (
            &["box", "--test-box-token", "accepted", "run", "--stdin"],
            &["gate: --test-box-token must be refused, got 'accepted'"],
        ),
        (
            &["box", "--test-box-token", "REFUSED", "run", "--stdin"],
            &["gate: --test-box-token must be refused, got 'REFUSED'"],
        ),
        (&["box", "--test-box-token=", "run", "--stdin"], &["gate: --test-box-token needs a value"]),
        (
            &["box", "--test-box-token", "--test-box-console", "fail-before", "run", "--stdin"],
            &["gate: --test-box-token needs a value"],
        ),
        (
            &["box", "--test-box-token", "refused", "--test-box-token=refused", "run", "--stdin"],
            &["gate: --test-box-token is given twice"],
        ),
        (&["box", "run", "--test-box-token", "refused", "--stdin"], &[BOX_USAGE]),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

/// The registration seam: `ok` or `fail` in place of the Task Scheduler's work, before the verb, given once.
#[cfg(feature = "test-seams")]
#[test]
fn the_register_seam_parses_ok_or_fail_and_refuses_another_value() {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let registering = |register_ok| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::RegisterDesktop,
            seams: BoxSeams {
                register_ok: Some(register_ok),
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        })
    };
    for (word, register_ok) in [("ok", true), ("fail", false)] {
        assert_eq!(
            invocation(&["box", "--test-box-register", word, "register-desktop"]),
            registering(register_ok),
            "{word}"
        );
        let joined = format!("--test-box-register={word}");
        assert_eq!(
            invocation(&["box", joined.as_str(), "register-desktop"]),
            registering(register_ok),
            "{joined}"
        );
    }
    let rows: [(&[&str], &str); 5] = [
        (
            &["box", "--test-box-register", "OK", "register-desktop"],
            "gate: --test-box-register must be ok or fail, got 'OK'",
        ),
        (
            &["box", "--test-box-register", "refused", "register-desktop"],
            "gate: --test-box-register must be ok or fail, got 'refused'",
        ),
        (
            &["box", "--test-box-register=", "register-desktop"],
            "gate: --test-box-register needs a value",
        ),
        (
            &["box", "--test-box-register", "ok", "--test-box-register=fail", "register-desktop"],
            "gate: --test-box-register is given twice",
        ),
        (&["box", "register-desktop", "--test-box-register", "ok"], BOX_USAGE),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), [want], "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_register_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-box-register", "ok", "register-desktop"]), [BOX_USAGE]);
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_token_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-box-token", "refused", "run", "--stdin"]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-box-token=refused", "run", "--stdin"]), [BOX_USAGE]);
}

/// Checks the mutex seam takes a name of exactly the most it allows, `longest` being one of that many characters.
#[cfg(feature = "test-seams")]
fn desktop_mutex_takes_the_longest_name(longest: &str) {
    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    assert_eq!(
        invocation(&["box", "--test-box-desktop-mutex", longest, "run", "--stdin"]),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            seams: BoxSeams {
                desktop_mutex: Some(longest.to_owned()),
                ..BoxSeams::default()
            },
            root: None,
            keys: Vec::new(),
        }),
        "a name of the most the seam takes"
    );
}

/// Checks a bad value of each of the box's desktop seams against the line it takes, `too_long` being a mutex name
/// longer than the seam takes.
#[cfg(feature = "test-seams")]
fn desktop_seam_refusals_hold(too_long: &str) {
    // The faults and the states a refusal line lists, as the seams' own words give them.
    let modes = "ok, exit-before-hello, hang, wrong-version, lie-handle, exit-after-create, task-not-registered, \
                 task-set-up-wrong, task-failed or task-no-pid";
    let sessions = "active, none, locked, other-user or disconnected";
    let mutex = "1 to 64 ASCII letters, digits and '-'";
    let refused = |words: &[&str], want: String| assert_eq!(problems(words), [want], "{words:?}");
    refused(
        &["box", "--test-box-desktop", "restart", "run", "--stdin"],
        format!("gate: --test-box-desktop must be {modes}, got 'restart'"),
    );
    refused(
        &["box", "--test-box-desktop", "OK", "run", "--stdin"],
        format!("gate: --test-box-desktop must be {modes}, got 'OK'"),
    );
    refused(
        &["box", "--test-box-desktop-session", "signed-in", "run", "--stdin"],
        format!("gate: --test-box-desktop-session must be {sessions}, got 'signed-in'"),
    );
    refused(
        &["box", "--test-box-desktop-session", "Active", "run", "--stdin"],
        format!("gate: --test-box-desktop-session must be {sessions}, got 'Active'"),
    );
    for bad in ["a b", "a_b", too_long] {
        refused(
            &["box", "--test-box-desktop-mutex", bad, "run", "--stdin"],
            format!("gate: --test-box-desktop-mutex must be {mutex}, got '{bad}'"),
        );
    }
    for bad in ["0", "fast"] {
        refused(
            &["box", "--test-box-desktop-ms", bad, "run", "--stdin"],
            format!("gate: --test-box-desktop-ms must be a whole number above 0, got '{bad}'"),
        );
        refused(
            &["box", "--test-box-desktop-message-ms", bad, "run", "--stdin"],
            format!("gate: --test-box-desktop-message-ms must be a whole number above 0, got '{bad}'"),
        );
    }
    refused(
        &["box", "--test-box-desktop=", "run", "--stdin"],
        "gate: --test-box-desktop needs a value".to_owned(),
    );
}

/// The `gate box run --stdin` that `seams` are given before.
#[cfg(feature = "test-seams")]
fn box_run(seams: gate_args::BoxSeams) -> gate_args::Invocation {
    gate_args::Invocation::Box(gate_args::BoxRequest {
        verb: gate_args::BoxVerb::Run,
        seams,
        root: None,
        keys: Vec::new(),
    })
}

/// The box's desktop seams: the fault its desktop run takes, the state it finds the console's session in, the unique
/// part of the private namespace and mutex it takes, how long its waits may take, and its message limit.
#[cfg(feature = "test-seams")]
#[test]
fn the_desktop_seams_parse_and_refuse_bad_values() {
    use gate_args::{BoxSeams, DesktopMode, DesktopSession};

    use crate::support::invocation;

    let taking = |desktop| {
        box_run(BoxSeams {
            desktop: Some(desktop),
            ..BoxSeams::default()
        })
    };
    for (word, desktop) in [
        ("ok", DesktopMode::Ok),
        ("exit-before-hello", DesktopMode::ExitBeforeHello),
        ("hang", DesktopMode::Hang),
        ("wrong-version", DesktopMode::WrongVersion),
        ("lie-handle", DesktopMode::LieHandle),
        ("exit-after-create", DesktopMode::ExitAfterCreate),
        ("task-not-registered", DesktopMode::TaskNotRegistered),
        ("task-set-up-wrong", DesktopMode::TaskSetUpWrong),
        ("task-failed", DesktopMode::TaskFailed),
        ("task-no-pid", DesktopMode::TaskNoPid),
    ] {
        assert_eq!(
            invocation(&["box", "--test-box-desktop", word, "run", "--stdin"]),
            taking(desktop),
            "{word}"
        );
        let joined = format!("--test-box-desktop={word}");
        assert_eq!(invocation(&["box", joined.as_str(), "run", "--stdin"]), taking(desktop), "{joined}");
    }
    for (word, session) in [
        ("active", DesktopSession::Active),
        ("none", DesktopSession::NoUser),
        ("locked", DesktopSession::Locked),
        ("other-user", DesktopSession::OtherUser),
        ("disconnected", DesktopSession::Disconnected),
    ] {
        let finding = box_run(BoxSeams {
            desktop_session: Some(session),
            ..BoxSeams::default()
        });
        assert_eq!(
            invocation(&["box", "--test-box-desktop-session", word, "run", "--stdin"]),
            finding,
            "{word}"
        );
        let joined = format!("--test-box-desktop-session={word}");
        assert_eq!(invocation(&["box", joined.as_str(), "run", "--stdin"]), finding, "{joined}");
    }
    assert_eq!(
        invocation(&["box", "--test-box-desktop-mutex", "case-1-AB", "run", "--stdin"]),
        box_run(BoxSeams {
            desktop_mutex: Some("case-1-AB".to_owned()),
            ..BoxSeams::default()
        })
    );
    desktop_mutex_takes_the_longest_name(&"a".repeat(64));
    assert_eq!(
        invocation(&["box", "--test-box-desktop-ms", "250", "run", "--stdin"]),
        box_run(BoxSeams {
            desktop_ms: Some(250),
            ..BoxSeams::default()
        })
    );
    assert_eq!(
        invocation(&[
            "box",
            "--test-box-desktop",
            "task-no-pid",
            "--test-box-desktop-session",
            "none",
            "--test-box-desktop-mutex",
            "case-2",
            "--test-box-desktop-ms",
            "100",
            "--test-box-desktop-message-ms",
            "40",
            "run",
            "--stdin"
        ]),
        box_run(BoxSeams {
            desktop: Some(DesktopMode::TaskNoPid),
            desktop_session: Some(DesktopSession::NoUser),
            desktop_mutex: Some("case-2".to_owned()),
            desktop_ms: Some(100),
            desktop_message_ms: Some(40),
            ..BoxSeams::default()
        }),
        "the desktop seams come with the others, in any order"
    );

    desktop_seam_refusals_hold(&"a".repeat(65));
    assert_eq!(
        problems(&["box", "--test-box-desktop", "--test-box-desktop-session", "active", "run", "--stdin"]),
        ["gate: --test-box-desktop needs a value"],
        "another seam in the value's place is no value"
    );
    assert_eq!(
        problems(&["box", "run", "--test-box-desktop", "ok", "--stdin"]),
        [BOX_USAGE],
        "a desktop seam after the verb is no verb"
    );
}

/// The box's desktop message seam, alone, in either form; its refusals are with the other desktop seams'.
#[cfg(feature = "test-seams")]
#[test]
fn the_desktop_message_seam_parses() {
    use gate_args::BoxSeams;

    use crate::support::invocation;

    let taking = box_run(BoxSeams {
        desktop_message_ms: Some(5000),
        ..BoxSeams::default()
    });
    assert_eq!(invocation(&["box", "--test-box-desktop-message-ms", "5000", "run", "--stdin"]), taking);
    assert_eq!(invocation(&["box", "--test-box-desktop-message-ms=5000", "run", "--stdin"]), taking);
}

#[cfg(feature = "test-seams")]
#[test]
fn the_desktop_seams_are_each_given_once() {
    let rows: [(&[&str], &[&str]); 5] = [
        (
            &["box", "--test-box-desktop", "ok", "--test-box-desktop=hang", "run", "--stdin"],
            &["gate: --test-box-desktop is given twice"],
        ),
        (
            &[
                "box",
                "--test-box-desktop-session",
                "active",
                "--test-box-desktop-session=locked",
                "run",
                "--stdin",
            ],
            &["gate: --test-box-desktop-session is given twice"],
        ),
        (
            &[
                "box",
                "--test-box-desktop-mutex",
                "case-1",
                "--test-box-desktop-mutex=case-2",
                "run",
                "--stdin",
            ],
            &["gate: --test-box-desktop-mutex is given twice"],
        ),
        (
            &["box", "--test-box-desktop-ms", "1", "--test-box-desktop-ms=2", "run", "--stdin"],
            &["gate: --test-box-desktop-ms is given twice"],
        ),
        (
            &[
                "box",
                "--test-box-desktop-message-ms",
                "1",
                "--test-box-desktop-message-ms=2",
                "run",
                "--stdin",
            ],
            &["gate: --test-box-desktop-message-ms is given twice"],
        ),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn the_desktop_seams_without_the_feature_are_no_verb() {
    for (seam, value) in [
        ("--test-box-desktop", "ok"),
        ("--test-box-desktop-session", "active"),
        ("--test-box-desktop-mutex", "case-1"),
        ("--test-box-desktop-ms", "250"),
        ("--test-box-desktop-message-ms", "5000"),
        ("--test-box-git-limit", "checkout:2000"),
    ] {
        assert_eq!(problems(&["box", seam, value, "run", "--stdin"]), [BOX_USAGE], "{seam}");
        let joined = format!("{seam}={value}");
        assert_eq!(problems(&["box", joined.as_str(), "run", "--stdin"]), [BOX_USAGE], "{joined}");
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn the_job_breakaway_seam_parses_with_the_feature() {
    use crate::support::run_request;

    assert!(
        run_request(&refs(&with(&RUN, &["--test-job-breakaway", "--", "cmd"])))
            .test_seams
            .job_breakaway
    );
    assert!(
        !run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams.job_breakaway,
        "the job refuses breakaway unless the seam is given"
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
        problems(&refs(&with(&RUN, &["--test-own-start-fails", "--", "cmd"]))),
        ["gate: unknown option --test-own-start-fails"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-parent-job-trusted", "--", "cmd"]))),
        ["gate: unknown option --test-parent-job-trusted"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-job-breakaway", "--", "cmd"]))),
        ["gate: unknown option --test-job-breakaway"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-remote-snapshot", "--", "cmd"]))),
        ["gate: unknown option --test-remote-snapshot"]
    );
    for flag in [
        "--test-remote-transport",
        "--test-remote-ssh",
        "--test-remote-retry-pause-ms",
        "--test-remote-bound-ms",
        "--test-remote-interrupt-at",
        "--test-remote-watch-call",
        "--test-remote-timeout-ms",
        "--test-remote-backstop-ms",
        "--test-local-sdks",
        "--test-dotnet-limit-ms",
        "--test-remote-fetch-cap-bytes",
        "--test-remote-apply-cap-bytes",
        "--test-remote-outputs-budget-bytes",
        "--test-remote-inputs-budget-bytes",
        "--test-remote-log-cap-bytes",
        "--test-server-image",
        "--test-servers-under",
        "--test-diagnostics-budget-ms",
    ] {
        assert_eq!(
            problems(&refs(&with(&RUN, &[flag, "5", "--", "cmd"]))),
            [format!("gate: unknown option {flag}"), "gate: unexpected argument 5".to_owned()],
            "{flag}"
        );
    }
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-remote-interrupt-at", "stop", "--", "cmd"]))),
        ["gate: unknown option --test-remote-interrupt-at", "gate: unexpected argument stop"]
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
