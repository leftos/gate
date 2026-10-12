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
fn the_server_image_seam_needs_the_servers_under_seam_and_a_value_once() {
    use crate::support::run_request;

    let given = ["--test-servers-under", ".", "--test-server-image", "gate-test-server.exe", "--", "cmd"];
    assert_eq!(
        run_request(&refs(&with(&RUN, &given))).test_seams.server_image,
        Some("gate-test-server.exe".to_owned())
    );
    assert_eq!(
        run_request(&refs(&with(&RUN, &["--test-servers-under", ".", "--", "cmd"])))
            .test_seams
            .server_image,
        None,
        "none unless given"
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-server-image", "gate-test-server.exe", "--", "cmd"]))),
        ["gate: --test-server-image needs --test-servers-under"]
    );
    assert_eq!(
        problems(&refs(&with(
            &RUN,
            &[
                "--test-servers-under",
                ".",
                "--test-server-image",
                "a.exe",
                "--test-server-image",
                "b.exe",
                "--",
                "cmd"
            ]
        ))),
        ["gate: --test-server-image is given twice"]
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-servers-under", ".", "--test-server-image=", "--", "cmd"]))),
        ["gate: --test-server-image needs a value"]
    );
    assert_eq!(
        problems(&["stop-tree", "--test-server-image", "5", "1"]),
        ["gate: unknown option --test-server-image", "gate: unexpected argument 1"]
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
    for call in ["preflight", "run", "outputs"] {
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
        ["gate: --test-remote-interrupt-at must be preflight, run or outputs, got 'push'"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn the_remote_watch_seams_name_one_call_and_a_ceiling() {
    use crate::support::run_request;

    for call in ["preflight", "push", "run", "ls-remote", "fetch", "outputs"] {
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
        ["gate: --test-remote-watch-call must be preflight, push, run, ls-remote, fetch or outputs, got 'stop'"]
    );
    let words = with(
        &RUN,
        &["--test-remote-transport", "local:b", "--test-remote-timeout-ms", "300", "--", "cmd"],
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
fn the_bridge_seams_need_a_value_once() {
    use crate::support::run_request;

    let seams = run_request(&refs(&with(&RUN, &["--", "cmd"]))).test_seams;
    assert_eq!((seams.remote_script, seams.bridge_pwsh), (None, None), "none unless given");
    let given = ["--test-remote-script", r"case\remote.ps1", "--test-bridge-pwsh=no-pwsh.exe", "--", "cmd"];
    let seams = run_request(&refs(&with(&RUN, &given))).test_seams;
    assert_eq!(
        (seams.remote_script, seams.bridge_pwsh),
        (
            Some(std::path::PathBuf::from(r"case\remote.ps1")),
            Some(std::path::PathBuf::from("no-pwsh.exe"))
        )
    );
    for flag in ["--test-remote-script", "--test-bridge-pwsh"] {
        assert_eq!(
            problems(&refs(&with(&RUN, &[flag, "--", "cmd"]))),
            [format!("gate: {flag} needs a value")],
            "{flag} without its value"
        );
        assert_eq!(
            problems(&refs(&with(&RUN, &[flag, "a", flag, "b", "--", "cmd"]))),
            [format!("gate: {flag} is given twice")],
            "{flag} twice"
        );
        assert_eq!(
            problems(&["stop-tree", flag, "5", "1"]),
            [format!("gate: unknown option {flag}"), "gate: unexpected argument 1".to_owned()],
            "{flag} on stop-tree"
        );
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn the_bridge_seams_are_unknown_without_the_feature() {
    for flag in ["--test-remote-script", "--test-bridge-pwsh"] {
        assert_eq!(
            problems(&refs(&with(&RUN, &[flag, "5", "--", "cmd"]))),
            [format!("gate: unknown option {flag}"), "gate: unexpected argument 5".to_owned()],
            "{flag}"
        );
    }
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
            seams: BoxSeams {
                heartbeat_ms,
                ..BoxSeams::default()
            },
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

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_outputs_budget_bytes_is_read_before_the_verb() {
    use std::ffi::OsString;

    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let budgeted = |outputs_budget_bytes| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            payload: OsString::from("e30="),
            seams: BoxSeams {
                outputs_budget_bytes,
                ..BoxSeams::default()
            },
        })
    };
    assert_eq!(
        invocation(&["box", "--test-outputs-budget-bytes", "1024", "run", "e30="]),
        budgeted(Some(1024))
    );
    assert_eq!(
        invocation(&["box", "--test-outputs-budget-bytes=1024", "run", "e30="]),
        budgeted(Some(1024))
    );
    assert_eq!(invocation(&["box", "run", "e30="]), budgeted(None), "no seam given, none set");
    assert_eq!(
        invocation(&["box", "--test-outputs-budget-bytes", "2147483647", "run", "e30="]),
        budgeted(Some(2_147_483_647))
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_outputs_budget_bytes_refuses_what_the_run_seams_refuse() {
    let rows: [(&[&str], &[&str]); 7] = [
        (
            &["box", "--test-outputs-budget-bytes", "--test-outputs-budget-bytes=2", "run", "e30="],
            &[
                "gate: --test-outputs-budget-bytes needs a value",
                "gate: --test-outputs-budget-bytes is given twice",
            ],
        ),
        (
            &["box", "--test-outputs-budget-bytes=", "run", "e30="],
            &["gate: --test-outputs-budget-bytes needs a value"],
        ),
        (
            &["box", "--test-outputs-budget-bytes", "0", "run", "e30="],
            &["gate: --test-outputs-budget-bytes must be a whole number above 0, got '0'"],
        ),
        (
            &["box", "--test-outputs-budget-bytes", "fast", "run", "e30="],
            &["gate: --test-outputs-budget-bytes must be a whole number above 0, got 'fast'"],
        ),
        (
            &["box", "--test-outputs-budget-bytes", "2147483648", "run", "e30="],
            &["gate: --test-outputs-budget-bytes must be at most 2147483647, got '2147483648'"],
        ),
        (
            &["box", "--test-outputs-budget-bytes", "1", "--test-outputs-budget-bytes=2", "run", "e30="],
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

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_outputs_budget_bytes_is_an_unknown_word_without_the_feature() {
    assert_eq!(problems(&["box", "--test-outputs-budget-bytes", "30", "run", "e30="]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-outputs-budget-bytes=30", "run", "e30="]), [BOX_USAGE]);
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_console_and_park_launch_are_read_before_the_verb() {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation};

    use crate::support::invocation;

    let running = |seams| {
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Run,
            payload: OsString::from("e30="),
            seams,
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
        invocation(&["box", "--test-box-console", "fail-before", "run", "e30="]),
        running(failing())
    );
    assert_eq!(invocation(&["box", "--test-box-console=fail-before", "run", "e30="]), running(failing()));
    assert_eq!(
        invocation(&["box", "--test-box-park-launch", r"X:\case\park", "run", "e30="]),
        running(parked())
    );
    assert_eq!(
        invocation(&["box", "--test-box-park-launch=X:\\case\\park", "run", "e30="]),
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
            "e30="
        ]),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Stop,
            payload: OsString::from("e30="),
            seams: BoxSeams {
                heartbeat_ms: Some(30),
                fail_console: true,
                park_launch: Some(PathBuf::from(r"X:\case\park")),
                outputs_budget_bytes: None,
            },
        }),
        "the seams come in any order before the verb"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn box_seam_console_and_park_launch_refuse_a_bad_missing_or_repeated_value() {
    let rows: [(&[&str], &[&str]); 12] = [
        (
            &["box", "--test-box-console", "fail-sideways", "run", "e30="],
            &["gate: --test-box-console must be fail-before, got 'fail-sideways'"],
        ),
        (
            &["box", "--test-box-console", "fail-after", "run", "e30="],
            &["gate: --test-box-console must be fail-before, got 'fail-after'"],
        ),
        (
            &["box", "--test-box-console", "FAIL-BEFORE", "run", "e30="],
            &["gate: --test-box-console must be fail-before, got 'FAIL-BEFORE'"],
        ),
        (
            &["box", "--test-box-console=", "run", "e30="],
            &["gate: --test-box-console needs a value"],
        ),
        (
            &["box", "--test-box-console", "--test-box-park-launch", "p", "run", "e30="],
            &["gate: --test-box-console needs a value"],
        ),
        (
            &[
                "box",
                "--test-box-console",
                "fail-before",
                "--test-box-console=fail-before",
                "run",
                "e30=",
            ],
            &["gate: --test-box-console is given twice"],
        ),
        (
            &["box", "--test-box-park-launch=", "run", "e30="],
            &["gate: --test-box-park-launch needs a value"],
        ),
        (
            &["box", "--test-box-park-launch", "--test-heartbeat-ms", "30", "run", "e30="],
            &["gate: --test-box-park-launch needs a value"],
        ),
        (
            &["box", "--test-box-park-launch", "a", "--test-box-park-launch=b", "run", "e30="],
            &["gate: --test-box-park-launch is given twice"],
        ),
        (&["box", "--test-box-park-restart", "p", "run", "e30="], &[BOX_USAGE]),
        (&["box", "run", "--test-box-console", "fail-before", "e30="], &[BOX_USAGE]),
        (&["box", "--test-box-consolex", "fail-before", "run", "e30="], &[BOX_USAGE]),
    ];
    for (words, want) in rows {
        assert_eq!(problems(words), want, "{words:?}");
    }
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn box_seam_console_and_park_launch_are_unknown_words_without_the_feature() {
    assert_eq!(problems(&["box", "--test-box-console", "fail-before", "run", "e30="]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-box-console=fail-before", "run", "e30="]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-box-park-launch", "park", "run", "e30="]), [BOX_USAGE]);
    assert_eq!(problems(&["box", "--test-box-park-launch=park", "run", "e30="]), [BOX_USAGE]);
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
        "--test-local-sdks",
        "--test-server-image",
        "--test-diagnostics-budget-ms",
    ] {
        assert_eq!(
            problems(&refs(&with(&RUN, &[flag, "5", "--", "cmd"]))),
            [format!("gate: unknown option {flag}"), "gate: unexpected argument 5".to_owned()],
            "{flag}"
        );
    }
    assert_eq!(
        problems(&["stop-tree", "--test-print-request", "1"]),
        ["gate: unknown option --test-print-request"]
    );
    assert_eq!(
        problems(&["-Log", "x", "-TimeoutSeconds", "5", "-Slot", "heavy", "--test-print-request", "--", "cmd"]),
        ["gate: unknown option --test-print-request"]
    );
}
