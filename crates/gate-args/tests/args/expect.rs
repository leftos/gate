//! `gate expect`: the command's words after `--`, `--state-dir` and `--slot`, and nothing else.

use std::ffi::OsString;
use std::path::PathBuf;

use gate_args::{ExpectRequest, Invocation, Slot, USAGE};

use crate::support::{invocation, problems};

fn expect_request(words: &[&str]) -> ExpectRequest {
    let invocation = invocation(words);
    let request = match &invocation {
        Invocation::Expect(request) => Some(request.clone()),
        _ => None,
    };
    assert!(request.is_some(), "{words:?} is a gate expect, got {invocation:?}");
    request.expect("checked above")
}

fn command(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

#[test]
fn expect_reads_the_command_after_the_double_dash() {
    assert_eq!(
        expect_request(&["expect", "--", "cargo", "test", "--", "-q"]),
        ExpectRequest {
            slot: Slot::Heavy,
            state_dir: None,
            command: command(&["cargo", "test", "--", "-q"]),
        },
        "the slot is heavy when none is given, and every word after the first -- is the command's"
    );
    assert_eq!(expect_request(&["expect", "--", "--slot"]).command, command(&["--slot"]));
}

#[test]
fn expect_takes_a_state_dir() {
    assert_eq!(
        expect_request(&["expect", "--state-dir", "root", "--", "cmd"]).state_dir,
        Some(PathBuf::from("root"))
    );
    assert_eq!(
        expect_request(&["expect", "--state-dir=root", "--", "cmd"]).state_dir,
        Some(PathBuf::from("root"))
    );
}

#[test]
fn expect_takes_the_run_s_slot_spellings() {
    assert_eq!(expect_request(&["expect", "--slot", "critical", "--", "cmd"]).slot, Slot::Critical);
    assert_eq!(expect_request(&["expect", "--slot=Light", "--", "cmd"]).slot, Slot::Light);
    assert_eq!(
        problems(&["expect", "--slot", "auto", "--", "cmd"]),
        ["gate: --slot must be heavy, light or critical, got 'auto'"]
    );
}

#[test]
fn expect_refuses_the_none_slot() {
    for words in [&["expect", "--slot", "none", "--", "cmd"][..], &["expect", "--slot=None", "--", "cmd"]] {
        assert_eq!(
            problems(words),
            ["gate: --slot must be heavy, light or critical, got 'none'"],
            "a none-slot run has no expected time: {words:?}"
        );
    }
}

#[test]
fn expect_without_a_command_is_refused() {
    for words in [&["expect"][..], &["expect", "--"], &["expect", "--slot", "light"]] {
        assert_eq!(problems(words), ["gate: missing the command after --"], "{words:?}");
    }
    assert_eq!(
        problems(&["expect", "cmd"]),
        ["gate: unexpected argument cmd", "gate: missing the command after --"],
        "the command comes after --"
    );
}

#[test]
fn expect_refuses_an_option_given_twice() {
    assert_eq!(
        problems(&["expect", "--slot", "light", "--slot=heavy", "--", "cmd"]),
        ["gate: --slot is given twice"]
    );
    assert_eq!(
        problems(&["expect", "--state-dir", "a", "--state-dir", "b", "--", "cmd"]),
        ["gate: --state-dir is given twice"]
    );
}

#[test]
fn expect_refuses_any_other_option() {
    for option in ["--local", "--timeout=5", "--log=out.log", "--test-print-request", "-Slot"] {
        assert_eq!(
            problems(&["expect", option, "--", "cmd"]),
            [format!("gate: unknown option {option}")],
            "{option}"
        );
    }
}

#[test]
fn expect_has_no_compatibility_spelling() {
    assert_eq!(
        problems(&["-Expect", "--", "cmd"]),
        [
            "gate: unknown option -Expect",
            "gate: missing -Slot heavy|light|critical|none",
            "gate: missing -TimeoutSeconds <seconds>",
            "gate: missing -Log <path>",
        ]
    );
}

#[test]
fn the_usage_lists_gate_expect() {
    assert!(
        USAGE
            .lines()
            .any(|line| line == "       gate expect [--state-dir <path>] [--slot heavy|light|critical] -- <command> [args...]"),
        "the usage lists gate expect:\n{USAGE}"
    );
}
