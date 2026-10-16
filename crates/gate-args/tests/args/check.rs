//! `gate check`: no words, one option, `--builder <host>`, the usage line, and its hidden seams.

use gate_args::{CheckRequest, CheckSeams, Invocation, USAGE};

use crate::support::{invocation, problems};

#[test]
fn check_parses_to_a_check_request() {
    assert_eq!(
        invocation(&["check"]),
        Invocation::Check(CheckRequest {
            builder: None,
            seams: CheckSeams::default()
        })
    );
}

#[test]
fn check_builder_takes_one_host() {
    let of = |host: &str| {
        Invocation::Check(CheckRequest {
            builder: Some(host.to_owned()),
            seams: CheckSeams::default(),
        })
    };
    assert_eq!(invocation(&["check", "--builder", "box-1"]), of("box-1"));
    assert_eq!(invocation(&["check", "--builder=box-2"]), of("box-2"), "the inline form");
}

#[test]
fn check_builder_needs_a_value_and_is_given_once() {
    assert_eq!(problems(&["check", "--builder"]), ["gate: --builder needs a value"]);
    assert_eq!(problems(&["check", "--builder="]), ["gate: --builder needs a value"]);
    assert_eq!(
        problems(&["check", "--builder", "--"]),
        ["gate: --builder needs a value", "gate: unexpected argument --"]
    );
    assert_eq!(
        problems(&["check", "--builder", "box-1", "--builder", "box-2"]),
        ["gate: --builder is given twice"],
        "one host"
    );
    assert_eq!(
        problems(&["check", "--builder", "box-1", "box-2"]),
        ["gate: unexpected argument box-2"],
        "one host"
    );
}

#[test]
fn check_takes_no_words() {
    assert_eq!(problems(&["check", "here"]), ["gate: unexpected argument here"]);
    assert_eq!(problems(&["check", "--"]), ["gate: unexpected argument --"]);
    assert_eq!(
        problems(&["check", "--state-dir", "x"]),
        ["gate: unknown option --state-dir", "gate: unexpected argument x"],
        "run options are not check options"
    );
    assert_eq!(
        problems(&["check", "--cd", "x"]),
        ["gate: unknown option --cd", "gate: unexpected argument x"],
        "run options are not check options"
    );
}

#[test]
fn check_unknown_option_is_refused() {
    assert_eq!(problems(&["check", "--all"]), ["gate: unknown option --all"]);
    assert_eq!(problems(&["check", "-Check"]), ["gate: unknown option -Check"]);
    assert_eq!(
        problems(&["check", "--hosts", "box-1"]),
        ["gate: unknown option --hosts", "gate: unexpected argument box-1"]
    );
}

#[test]
fn check_help_prints_the_usage() {
    assert_eq!(invocation(&["check", "--help"]), Invocation::Help);
    assert_eq!(invocation(&["check", "-h"]), Invocation::Help);
    assert_eq!(invocation(&["check", "x", "--help"]), Invocation::Help, "a help word wins over the rest");
}

#[test]
fn the_usage_lists_check() {
    assert!(
        USAGE.lines().any(|line| line == "       gate check [--builder <host>]"),
        "the usage lists gate check:\n{USAGE}"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn check_seam_probe_limit_ms_parses_with_the_feature() {
    use crate::support::{RUN, refs, with};

    let limited = |probe_limit_ms| {
        Invocation::Check(CheckRequest {
            builder: None,
            seams: CheckSeams {
                probe_limit_ms: Some(probe_limit_ms),
                ..CheckSeams::default()
            },
        })
    };
    assert_eq!(invocation(&["check", "--test-probe-limit-ms", "200"]), limited(200));
    assert_eq!(invocation(&["check", "--test-probe-limit-ms=1500"]), limited(1500));
    assert_eq!(
        problems(&["check", "--test-probe-limit-ms", "0"]),
        ["gate: --test-probe-limit-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&["check", "--test-probe-limit-ms"]),
        ["gate: --test-probe-limit-ms needs a value"]
    );
    assert_eq!(
        problems(&["check", "--test-probe-limit-ms", "5", "--test-probe-limit-ms", "6"]),
        ["gate: --test-probe-limit-ms is given twice"]
    );
    assert_eq!(
        problems(&["check", "--test-push-bound-ms", "5"]),
        ["gate: unknown option --test-push-bound-ms", "gate: unexpected argument 5"],
        "a push seam is not a check seam"
    );
    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-probe-limit-ms", "5", "--", "cmd"]))),
        ["gate: unknown option --test-probe-limit-ms", "gate: unexpected argument 5"],
        "the check seam is not a run seam"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn check_builder_seams_parse_with_the_feature() {
    use std::path::PathBuf;

    assert_eq!(
        invocation(&[
            "check",
            "--builder",
            "box-1",
            "--test-remote-transport",
            r"local:C:\builder",
            "--test-remote-ssh",
            "fake-ssh.exe",
            "--test-remote-timeout-ms",
            "250",
        ]),
        Invocation::Check(CheckRequest {
            builder: Some("box-1".to_owned()),
            seams: CheckSeams {
                probe_limit_ms: None,
                remote_transport: Some(r"local:C:\builder".to_owned()),
                remote_ssh: Some(PathBuf::from("fake-ssh.exe")),
                remote_timeout_ms: Some(250),
            },
        })
    );
    assert_eq!(
        problems(&["check", "--builder", "box-1", "--test-remote-timeout-ms", "0"]),
        ["gate: --test-remote-timeout-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&["check", "--builder", "box-1", "--test-remote-ssh", "fake-ssh.exe"]),
        ["gate: --test-remote-ssh needs --test-remote-transport"]
    );
    assert_eq!(
        problems(&[
            "check",
            "--test-remote-transport",
            "local:x",
            "--test-remote-ssh",
            "s.exe",
            "--test-remote-timeout-ms",
            "5"
        ]),
        [
            "gate: --test-remote-transport needs --builder",
            "gate: --test-remote-ssh needs --builder",
            "gate: --test-remote-timeout-ms needs --builder",
        ],
        "the builder seams are the builder check's alone"
    );
    assert_eq!(
        problems(&["check", "--builder", "b", "--test-remote-bound-ms", "5"]),
        ["gate: unknown option --test-remote-bound-ms", "gate: unexpected argument 5"],
        "a run seam the check does not read is not a check seam"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn check_probe_limit_is_not_used_with_builder() {
    assert_eq!(
        problems(&["check", "--builder", "box-1", "--test-probe-limit-ms", "5"]),
        ["gate: --test-probe-limit-ms is not used with --builder"],
        "a builder's probes run under the box's own limit"
    );
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn check_seam_probe_limit_ms_is_unknown_without_the_feature() {
    assert_eq!(
        problems(&["check", "--test-probe-limit-ms", "5"]),
        ["gate: unknown option --test-probe-limit-ms", "gate: unexpected argument 5"]
    );
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn check_builder_seams_are_unknown_without_the_feature() {
    for seam in ["--test-remote-transport", "--test-remote-ssh", "--test-remote-timeout-ms"] {
        assert_eq!(
            problems(&["check", "--builder", "box-1", seam, "5"]),
            [format!("gate: unknown option {seam}"), "gate: unexpected argument 5".to_owned()]
        );
    }
}
