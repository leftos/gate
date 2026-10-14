//! The first word, `--version`, `--help`, unknown subcommands and `gate stop-tree`.

use std::path::PathBuf;

use gate_args::{Invocation, StopRequest, parse};

use crate::support::{invocation, os, problems, problems_of, run_request, stop_request, unreadable_word};

#[test]
fn version_and_help_are_matched_before_the_compat_layer() {
    assert_eq!(invocation(&["--version"]), Invocation::Version);
    assert_eq!(invocation(&["--help"]), Invocation::Help);
    assert_eq!(invocation(&["help"]), Invocation::Help);
    for word in ["--Version", "-Help", "-version"] {
        assert_eq!(
            problems(&[word]),
            [
                format!("gate: unknown option {word}"),
                "gate: missing -Slot heavy|light|critical|none".to_owned(),
                "gate: missing -TimeoutSeconds <seconds>".to_owned(),
                "gate: missing -Log <path>".to_owned(),
                "gate: missing the command after --".to_owned(),
            ],
            "{word} goes to the compatibility layer"
        );
    }
    assert_eq!(problems(&["--version", "now"]), ["gate: unexpected argument now"]);
}

#[test]
fn a_help_word_before_the_separator_asks_for_the_usage() {
    for words in [
        &["run", "--help"][..],
        &["run", "--slot", "light", "--help"][..],
        &["push", "-h"][..],
        &["stop-tree", "-h"][..],
        &["cutover", "--help"][..],
        &["burn", "-h"][..],
        &["expect", "--help"][..],
        &["-h"][..],
    ] {
        assert_eq!(invocation(words), Invocation::Help, "{words:?}");
    }
    assert_eq!(
        run_request(&["run", "--slot", "light", "--timeout", "5", "--log", "x.log", "--", "tool", "--help"]).command,
        os(&["tool", "--help"]),
        "a --help after the separator belongs to the command the gate runs"
    );
}

#[test]
fn unknown_subcommand_is_usage_error() {
    assert_eq!(problems(&["build"]), ["gate: unknown subcommand build"]);
    assert_eq!(problems(&["Run"]), ["gate: unknown subcommand Run"]);
    assert_eq!(parse(Vec::new()), Err(Vec::new()), "no arguments is the usage alone");
    assert_eq!(problems_of(vec![unreadable_word()]), ["gate: cannot read the argument a\u{FFFD}"]);
}

#[test]
fn stop_tree_rejects_missing_and_invalid_pid() {
    assert_eq!(problems(&["stop-tree"]), ["gate: missing the process id"]);
    for value in ["abc", "0", "4294967296", "1.5"] {
        assert_eq!(
            problems(&["stop-tree", value]),
            [format!("gate: the process id must be a whole number above 0, got '{value}'")],
            "stop-tree {value}"
        );
    }
    assert_eq!(
        problems(&["stop-tree", "-1"]),
        ["gate: unknown option -1", "gate: missing the process id"]
    );
    assert_eq!(problems(&["stop-tree", "1", "2"]), ["gate: unexpected argument 2"]);
    assert_eq!(
        stop_request(&["stop-tree", "4294967295"]),
        StopRequest {
            pid: u32::MAX,
            state_dir: None,
            print_request: false,
        }
    );
    assert_eq!(
        stop_request(&["stop-tree", "--state-dir", "root", "123"]).state_dir,
        Some(PathBuf::from("root"))
    );
    assert_eq!(stop_request(&["stop-tree", "123", "--state-dir=root"]).pid, 123);
    assert_eq!(stop_request(&["stop-tree", "--", "123"]).pid, 123);
    assert!(
        parse(os(&["stop-tree", "--local", "1"])).is_err(),
        "run options are not stop-tree options"
    );
}
