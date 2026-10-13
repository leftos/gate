//! `gate remote-cutover`: `--state-dir` and no other word, the usage line, and its hidden seams.

use std::path::PathBuf;

use gate_args::{Invocation, RemoteCutoverRequest, RemoteCutoverSeams, USAGE};

use crate::support::{invocation, problems};

#[test]
fn remote_cutover_alone_parses() {
    assert_eq!(
        invocation(&["remote-cutover"]),
        Invocation::RemoteCutover(RemoteCutoverRequest {
            state_dir: None,
            seams: RemoteCutoverSeams::default(),
        })
    );
}

#[test]
fn remote_cutover_takes_a_state_dir_in_either_spelling() {
    for words in [
        &["remote-cutover", "--state-dir", r"C:\scratch"][..],
        &["remote-cutover", r"--state-dir=C:\scratch"],
    ] {
        assert_eq!(
            invocation(words),
            Invocation::RemoteCutover(RemoteCutoverRequest {
                state_dir: Some(PathBuf::from(r"C:\scratch")),
                seams: RemoteCutoverSeams::default(),
            }),
            "{words:?}"
        );
    }
}

#[test]
fn remote_cutover_refuses_every_other_word_and_option() {
    assert_eq!(problems(&["remote-cutover", "now"]), ["gate: unexpected argument now"]);
    assert_eq!(problems(&["remote-cutover", "--"]), ["gate: unexpected argument --"]);
    assert_eq!(problems(&["remote-cutover", "--undo"]), ["gate: unknown option --undo"]);
    assert_eq!(
        problems(&["remote-cutover", "-StateDir", "x"]),
        ["gate: unknown option -StateDir", "gate: unexpected argument x"]
    );
    assert_eq!(problems(&["remote-cutover", "--state-dir"]), ["gate: --state-dir needs a value"]);
    assert_eq!(
        problems(&["remote-cutover", "--state-dir", "a", "--state-dir", "b"]),
        ["gate: --state-dir is given twice"]
    );
    assert_eq!(
        problems(&["remote-cutover", "--local"]),
        ["gate: unknown option --local"],
        "run options are not remote-cutover options"
    );
}

#[test]
fn the_usage_lists_remote_cutover() {
    assert!(
        USAGE.lines().any(|line| line == "       gate remote-cutover [--state-dir <path>]"),
        "the usage lists gate remote-cutover:\n{USAGE}"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn remote_cutover_seams_parse_with_the_feature() {
    let words = [
        "remote-cutover",
        "--test-poll-ms",
        "20",
        "--test-confirm-ms=60",
        "--test-cutover-interrupt-after-list",
        "--test-cutover-interrupt-after-round",
        "--test-remote-bound-ms",
        "1000",
        "--test-remote-transport",
        r"local:C:\builder",
        "--test-remote-ssh",
        "fake-ssh.exe",
        "--state-dir",
        "root",
    ];
    assert_eq!(
        invocation(&words),
        Invocation::RemoteCutover(RemoteCutoverRequest {
            state_dir: Some(PathBuf::from("root")),
            seams: RemoteCutoverSeams {
                poll_ms: Some(20),
                confirm_ms: Some(60),
                interrupt_after_list: true,
                interrupt_after_round: true,
                bound_ms: Some(1000),
                transport: Some(r"local:C:\builder".to_owned()),
                ssh: Some(PathBuf::from("fake-ssh.exe")),
            },
        })
    );
    assert_eq!(
        problems(&["remote-cutover", "--test-poll-ms", "0"]),
        ["gate: --test-poll-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&["remote-cutover", "--test-heartbeat-ms", "5"]),
        ["gate: unknown option --test-heartbeat-ms", "gate: unexpected argument 5"],
        "the cutover's still-waiting period is not a remote-cutover seam"
    );
    assert_eq!(
        problems(&["remote-cutover", "--test-print-request"]),
        ["gate: unknown option --test-print-request"]
    );
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn remote_cutover_seams_are_unknown_without_the_feature() {
    for flag in ["--test-poll-ms", "--test-confirm-ms", "--test-remote-transport", "--test-remote-ssh"] {
        assert_eq!(
            problems(&["remote-cutover", flag, "5"]),
            [format!("gate: unknown option {flag}"), "gate: unexpected argument 5".to_owned()],
            "{flag}"
        );
    }
    assert_eq!(
        problems(&["remote-cutover", "--test-cutover-interrupt-after-list"]),
        ["gate: unknown option --test-cutover-interrupt-after-list"]
    );
}
