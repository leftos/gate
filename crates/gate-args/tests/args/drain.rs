//! `gate drain`: no words, `--lift` or `--status`, `--builder <host>`, the usage line, and its hidden seams.

use gate_args::{DrainAction, DrainRequest, DrainSeams, Invocation, USAGE};

use crate::support::{invocation, problems};

/// The drain request of `action` on `builder`, with no seams.
fn drain(action: DrainAction, builder: Option<&str>) -> Invocation {
    Invocation::Drain(DrainRequest {
        action,
        builder: builder.map(str::to_owned),
        seams: DrainSeams::default(),
    })
}

#[test]
fn drain_alone_raises() {
    assert_eq!(invocation(&["drain"]), drain(DrainAction::Raise, None));
}

#[test]
fn drain_lift_lifts_and_drain_status_reports() {
    assert_eq!(invocation(&["drain", "--lift"]), drain(DrainAction::Lift, None));
    assert_eq!(invocation(&["drain", "--status"]), drain(DrainAction::Status, None));
}

#[test]
fn drain_builder_takes_one_host_with_each_form() {
    assert_eq!(invocation(&["drain", "--builder", "box-1"]), drain(DrainAction::Raise, Some("box-1")));
    assert_eq!(
        invocation(&["drain", "--builder=box-2"]),
        drain(DrainAction::Raise, Some("box-2")),
        "the inline form"
    );
    assert_eq!(
        invocation(&["drain", "--lift", "--builder", "box-1"]),
        drain(DrainAction::Lift, Some("box-1"))
    );
    assert_eq!(
        invocation(&["drain", "--builder", "box-1", "--status"]),
        drain(DrainAction::Status, Some("box-1"))
    );
    assert_eq!(problems(&["drain", "--builder"]), ["gate: --builder needs a value"]);
    assert_eq!(
        problems(&["drain", "--builder", "box-1", "--builder", "box-2"]),
        ["gate: --builder is given twice"],
        "one builder per drain"
    );
}

#[test]
fn drain_lift_with_status_is_refused() {
    assert_eq!(problems(&["drain", "--lift", "--status"]), ["gate: --lift cannot be given with --status"]);
    assert_eq!(
        problems(&["drain", "--status", "--lift"]),
        ["gate: --lift cannot be given with --status"],
        "in either order"
    );
    assert_eq!(problems(&["drain", "--lift", "--lift"]), ["gate: --lift is given twice"]);
    assert_eq!(
        problems(&["drain", "--lift=yes"]),
        ["gate: unknown option --lift=yes"],
        "a switch takes no value"
    );
}

#[test]
fn drain_takes_no_words() {
    assert_eq!(problems(&["drain", "now"]), ["gate: unexpected argument now"]);
    assert_eq!(problems(&["drain", "--"]), ["gate: unexpected argument --"]);
    assert_eq!(
        problems(&["drain", "--builder", "box-1", "box-2"]),
        ["gate: unexpected argument box-2"],
        "one host"
    );
    assert_eq!(
        problems(&["drain", "--state-dir", "x"]),
        ["gate: unknown option --state-dir", "gate: unexpected argument x"],
        "run options are not drain options"
    );
    assert_eq!(problems(&["drain", "--raise"]), ["gate: unknown option --raise"]);
}

#[test]
fn drain_help_prints_the_usage() {
    assert_eq!(invocation(&["drain", "--help"]), Invocation::Help);
    assert_eq!(invocation(&["drain", "--lift", "-h"]), Invocation::Help);
}

#[test]
fn the_usage_lists_drain_between_push_and_prune() {
    let lines: Vec<&str> = USAGE.lines().collect();
    let at = lines
        .iter()
        .position(|line| *line == "       gate drain [--lift | --status] [--builder <host>]")
        .expect("the usage lists gate drain");
    assert_eq!(lines[at - 1], "       gate push", "{USAGE}");
    assert_eq!(lines[at + 1], "       gate prune --builders", "{USAGE}");
}

#[cfg(feature = "test-seams")]
#[test]
fn drain_seams_parse_with_the_feature() {
    use std::path::PathBuf;

    let words = [
        "drain",
        "--test-poll-ms",
        "20",
        "--test-heartbeat-ms=40",
        "--test-drain-raise-ms",
        "60",
        "--test-drain-interrupt-after-list",
        "--test-drain-interrupt-before-raise",
        "--test-remote-transport",
        r"local-ssh:C:\builder",
        "--test-remote-ssh",
        "fake-ssh.exe",
    ];
    assert_eq!(
        invocation(&words),
        Invocation::Drain(DrainRequest {
            action: DrainAction::Raise,
            builder: None,
            seams: DrainSeams {
                poll_ms: Some(20),
                heartbeat_ms: Some(40),
                raise_ms: Some(60),
                interrupt_after_list: true,
                interrupt_before_raise: true,
                remote_transport: Some(r"local-ssh:C:\builder".to_owned()),
                remote_ssh: Some(PathBuf::from("fake-ssh.exe")),
            },
        })
    );
    for seam in ["--test-poll-ms", "--test-heartbeat-ms", "--test-drain-raise-ms"] {
        assert_eq!(
            problems(&["drain", seam, "0"]),
            [format!("gate: {seam} must be a whole number above 0, got '0'")]
        );
        assert_eq!(problems(&["drain", seam]), [format!("gate: {seam} needs a value")]);
    }
    assert_eq!(
        problems(&["drain", "--test-remote-ssh", "fake-ssh.exe"]),
        ["gate: --test-remote-ssh needs --test-remote-transport"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn the_runs_drain_poll_and_other_seams_are_not_drain_seams() {
    for seam in ["--test-drain-poll-ms", "--test-confirm-ms", "--test-remote-timeout-ms", "--test-push-ssh"] {
        assert_eq!(
            problems(&["drain", seam, "5"]),
            [format!("gate: unknown option {seam}"), "gate: unexpected argument 5".to_owned()],
            "{seam}"
        );
    }
    for seam in ["--test-cutover-interrupt-after-list", "--test-print-request"] {
        assert_eq!(problems(&["drain", seam]), [format!("gate: unknown option {seam}")], "{seam}");
    }
    assert_eq!(
        problems(&[
            "run",
            "--slot",
            "heavy",
            "--timeout",
            "5",
            "--log",
            "x",
            "--test-drain-raise-ms",
            "5",
            "--",
            "cmd"
        ]),
        ["gate: unknown option --test-drain-raise-ms", "gate: unexpected argument 5"],
        "the drain seam is not a run seam"
    );
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn drain_seams_are_unknown_without_the_feature() {
    for seam in [
        "--test-poll-ms",
        "--test-heartbeat-ms",
        "--test-drain-raise-ms",
        "--test-remote-transport",
        "--test-remote-ssh",
    ] {
        assert_eq!(
            problems(&["drain", seam, "5"]),
            [format!("gate: unknown option {seam}"), "gate: unexpected argument 5".to_owned()],
            "{seam}"
        );
    }
    for seam in ["--test-drain-interrupt-after-list", "--test-drain-interrupt-before-raise"] {
        assert_eq!(problems(&["drain", seam]), [format!("gate: unknown option {seam}")], "{seam}");
    }
}
