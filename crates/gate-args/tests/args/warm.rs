//! `gate warm`: no words, `--cd <folder>`, `--background` and `--state-dir <path>`, the usage line, and its hidden seams.

use std::path::PathBuf;

use gate_args::{Invocation, TestSeams, USAGE, WarmRequest};

use crate::support::{invocation, problems};

fn warm_request(words: &[&str]) -> WarmRequest {
    let invocation = invocation(words);
    let request = match &invocation {
        Invocation::Warm(request) => Some(request.clone()),
        _ => None,
    };
    assert!(request.is_some(), "{words:?} is a warm, got {invocation:?}");
    request.expect("checked above")
}

#[test]
fn warm_parses_to_a_warm_request() {
    assert_eq!(
        invocation(&["warm"]),
        Invocation::Warm(WarmRequest {
            cd: None,
            background: false,
            state_dir: None,
            test_seams: TestSeams::default(),
        })
    );
}

#[test]
fn warm_reads_each_option() {
    assert_eq!(warm_request(&["warm", "--cd", r"src\app"]).cd, Some(PathBuf::from(r"src\app")));
    assert_eq!(warm_request(&["warm", "--cd=sub"]).cd, Some(PathBuf::from("sub")), "the inline form");
    assert!(warm_request(&["warm", "--background"]).background);
    assert_eq!(
        warm_request(&["warm", "--state-dir", r"C:\state"]).state_dir,
        Some(PathBuf::from(r"C:\state"))
    );
    assert_eq!(
        warm_request(&["warm", "--background", "--state-dir=s", "--cd", "c"]),
        WarmRequest {
            cd: Some(PathBuf::from("c")),
            background: true,
            state_dir: Some(PathBuf::from("s")),
            test_seams: TestSeams::default(),
        }
    );
}

#[test]
fn warm_options_are_given_once() {
    assert_eq!(problems(&["warm", "--cd", "a", "--cd", "b"]), ["gate: --cd is given twice"]);
    assert_eq!(problems(&["warm", "--background", "--background"]), ["gate: --background is given twice"]);
    assert_eq!(
        problems(&["warm", "--state-dir=a", "--state-dir=b"]),
        ["gate: --state-dir is given twice"]
    );
    assert_eq!(problems(&["warm", "--cd"]), ["gate: --cd needs a value"]);
    assert_eq!(problems(&["warm", "--background=yes"]), ["gate: unknown option --background=yes"]);
}

#[test]
fn warm_takes_no_words_and_no_command() {
    assert_eq!(problems(&["warm", "here"]), ["gate: unexpected argument here"]);
    assert_eq!(problems(&["warm", "--"]), ["gate: unexpected argument --"]);
    assert_eq!(
        problems(&["warm", "--", "dotnet", "build"]),
        [
            "gate: unexpected argument --",
            "gate: unexpected argument dotnet",
            "gate: unexpected argument build"
        ],
        "the commands come only from the warm-up file"
    );
    assert_eq!(
        problems(&["warm", "--slot", "heavy"]),
        ["gate: unknown option --slot", "gate: unexpected argument heavy"],
        "run options are not warm options"
    );
    assert_eq!(
        problems(&["warm", "--timeout", "5"]),
        ["gate: unknown option --timeout", "gate: unexpected argument 5"],
        "the ceiling is the file's"
    );
}

#[test]
fn warm_help_prints_the_usage() {
    assert_eq!(invocation(&["warm", "--help"]), Invocation::Help);
    assert_eq!(invocation(&["warm", "-h"]), Invocation::Help);
    assert_eq!(invocation(&["warm", "x", "--help"]), Invocation::Help, "a help word wins over the rest");
}

#[test]
fn the_usage_lists_warm_after_check() {
    let lines: Vec<&str> = USAGE.lines().collect();
    let warm = "       gate warm [--cd <folder>] [--background] [--state-dir <path>]";
    let at = lines.iter().position(|line| *line == warm);
    assert!(at.is_some(), "the usage lists gate warm:\n{USAGE}");
    assert_eq!(
        lines[at.expect("checked above") - 1],
        "       gate check [--builder <host>]",
        "after gate check"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn warm_reads_the_client_seams_and_its_wait_with_the_feature() {
    let request = warm_request(&["warm", "--test-remote-client", r"steps\client.txt", "--test-warm-wait-ms", "250"]);
    assert_eq!(
        request.test_seams,
        TestSeams {
            remote_client: Some(PathBuf::from(r"steps\client.txt")),
            warm_wait_ms: Some(250),
            ..TestSeams::default()
        }
    );
    let transported = warm_request(&[
        "warm",
        "--test-remote-transport",
        r"local:C:\builder",
        "--test-remote-ssh",
        "fake-ssh.exe",
        "--test-warm-wait-ms=40",
    ]);
    assert_eq!(transported.test_seams.remote_transport.as_deref(), Some(r"local:C:\builder"));
    assert_eq!(transported.test_seams.remote_ssh, Some(PathBuf::from("fake-ssh.exe")));
    assert_eq!(transported.test_seams.warm_wait_ms, Some(40));
    assert_eq!(
        problems(&["warm", "--test-warm-wait-ms", "0"]),
        ["gate: --test-warm-wait-ms must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&["warm", "--test-warm-wait-ms", "5", "--test-warm-wait-ms", "6"]),
        ["gate: --test-warm-wait-ms is given twice"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn warm_client_seams_need_the_transport() {
    assert_eq!(
        problems(&["warm", "--test-remote-ssh", "fake-ssh.exe"]),
        ["gate: --test-remote-ssh needs --test-remote-transport"]
    );
    assert_eq!(
        problems(&["warm", "--test-remote-backstop-ms", "5"]),
        ["gate: --test-remote-backstop-ms needs --test-remote-transport"]
    );
    assert_eq!(
        problems(&["warm", "--test-remote-transport", "local:x", "--test-remote-snapshot"]),
        ["gate: --test-remote-snapshot cannot be given with --test-remote-transport"]
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn the_warm_wait_seam_is_warms_alone() {
    use crate::support::{RUN, refs, with};

    assert_eq!(
        problems(&refs(&with(&RUN, &["--test-warm-wait-ms", "5", "--", "cmd"]))),
        ["gate: unknown option --test-warm-wait-ms", "gate: unexpected argument 5"],
        "the warm wait is not a run seam"
    );
    assert_eq!(
        problems(&["warm", "--test-print-request"]),
        ["gate: unknown option --test-print-request"],
        "warm has no request printer"
    );
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn warm_seams_are_unknown_without_the_feature() {
    for seam in ["--test-warm-wait-ms", "--test-remote-client"] {
        assert_eq!(
            problems(&["warm", seam, "5"]),
            [format!("gate: unknown option {seam}"), "gate: unexpected argument 5".to_owned()]
        );
    }
}
