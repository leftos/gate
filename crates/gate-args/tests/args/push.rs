//! `gate push`: no words and no options, the usage line, and its hidden seams.

use gate_args::{Invocation, PushRequest, PushSeams, USAGE};

use crate::support::{invocation, problems};

#[test]
fn push_parses_to_a_push_request() {
    assert_eq!(invocation(&["push"]), Invocation::Push(PushRequest { seams: PushSeams::default() }));
}

#[test]
fn push_takes_no_words() {
    assert_eq!(problems(&["push", "builder-1"]), ["gate: unexpected argument builder-1"]);
    assert_eq!(problems(&["push", "--"]), ["gate: unexpected argument --"]);
    assert_eq!(problems(&["push", "--all"]), ["gate: unknown option --all"]);
    assert_eq!(problems(&["push", "-Push"]), ["gate: unknown option -Push"]);
    assert_eq!(
        problems(&["push", "--state-dir", "x"]),
        ["gate: unknown option --state-dir", "gate: unexpected argument x"],
        "run options are not push options"
    );
}

#[test]
fn push_is_in_the_usage() {
    assert!(
        USAGE.lines().any(|line| line == "       gate push"),
        "the usage lists gate push:\n{USAGE}"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn push_seams_parse_with_the_feature() {
    assert_eq!(
        invocation(&["push", "--test-push-ssh", r"C:\suite\fake-ssh.exe", "--test-push-bound-ms=250"]),
        Invocation::Push(PushRequest {
            seams: PushSeams {
                ssh: Some(r"C:\suite\fake-ssh.exe".into()),
                bound_ms: Some(250),
            },
        })
    );
    assert_eq!(
        problems(&["push", "--test-push-bound-ms", "0"]),
        ["gate: --test-push-bound-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(problems(&["push", "--test-push-ssh"]), ["gate: --test-push-ssh needs a value"]);
    assert_eq!(
        problems(&["push", "--test-push-bound-ms", "5", "--test-push-bound-ms", "6"]),
        ["gate: --test-push-bound-ms is given twice"]
    );
    assert_eq!(
        problems(&["push", "--test-poll-ms", "5"]),
        ["gate: unknown option --test-poll-ms", "gate: unexpected argument 5"],
        "a cutover seam is not a push seam"
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
            "--test-push-ssh",
            "y",
            "--",
            "cmd"
        ]),
        ["gate: unknown option --test-push-ssh", "gate: unexpected argument y"],
        "the push seam is not a run seam"
    );
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn push_seams_are_unknown_without_the_feature() {
    for flag in ["--test-push-ssh", "--test-push-bound-ms"] {
        assert_eq!(
            problems(&["push", flag, "5"]),
            [format!("gate: unknown option {flag}"), "gate: unexpected argument 5".to_owned()],
            "{flag}"
        );
    }
}
