//! `gate check`: no words and no options, the usage line, and its hidden seam.

use gate_args::{CheckRequest, CheckSeams, Invocation, USAGE};

use crate::support::{invocation, problems};

#[test]
fn check_parses_to_a_check_request() {
    assert_eq!(
        invocation(&["check"]),
        Invocation::Check(CheckRequest {
            seams: CheckSeams::default()
        })
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
}

#[test]
fn check_unknown_option_is_refused() {
    assert_eq!(problems(&["check", "--all"]), ["gate: unknown option --all"]);
    assert_eq!(problems(&["check", "-Check"]), ["gate: unknown option -Check"]);
    assert_eq!(
        problems(&["check", "--builder", "box-1"]),
        ["gate: unknown option --builder", "gate: unexpected argument box-1"],
        "this build checks this machine only"
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
        USAGE.lines().any(|line| line == "       gate check"),
        "the usage lists gate check:\n{USAGE}"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn check_seam_probe_limit_ms_parses_with_the_feature() {
    use crate::support::{RUN, refs, with};

    let limited = |probe_limit_ms| {
        Invocation::Check(CheckRequest {
            seams: CheckSeams {
                probe_limit_ms: Some(probe_limit_ms),
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

#[cfg(not(feature = "test-seams"))]
#[test]
fn check_seam_probe_limit_ms_is_unknown_without_the_feature() {
    assert_eq!(
        problems(&["check", "--test-probe-limit-ms", "5"]),
        ["gate: unknown option --test-probe-limit-ms", "gate: unexpected argument 5"]
    );
}
