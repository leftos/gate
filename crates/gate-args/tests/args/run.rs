//! `gate run` in the subcommand CLI.

use std::ffi::OsString;
use std::path::PathBuf;

use gate_args::{DEFAULT_STALL_SECONDS, DEFAULT_TAIL_LINES, RunRequest, Slot, Spelling, TestSeams};

use crate::support::{RUN, os, problems, problems_of, refs, run_of, run_request, unreadable_word, with};

fn run_with(extra: &[&str]) -> RunRequest {
    run_request(&refs(&with(&RUN, extra)))
}

fn problems_with(extra: &[&str]) -> Vec<String> {
    problems(&refs(&with(&RUN, extra)))
}

#[test]
fn run_reads_every_option_and_the_defaults() {
    assert_eq!(
        run_with(&["--", "cmd", "/c", "echo hi"]),
        RunRequest {
            slot: Slot::Heavy,
            timeout: 5,
            log: PathBuf::from("out.log"),
            stall: DEFAULT_STALL_SECONDS,
            tail: DEFAULT_TAIL_LINES,
            no_markers: false,
            local: false,
            state_dir: None,
            command: os(&["cmd", "/c", "echo hi"]),
            test_seams: TestSeams::default(),
            print_request: false,
            spelling: Spelling::Cli,
        }
    );
    let request = run_with(&[
        "--stall",
        "30",
        "--tail",
        "7",
        "--no-markers",
        "--local",
        "--state-dir",
        "root",
        "--",
        "cmd",
    ]);
    assert_eq!(
        (request.stall, request.tail, request.no_markers, request.local, request.state_dir),
        (30, 7, true, true, Some(PathBuf::from("root")))
    );
}

#[test]
fn new_cli_refuses_slot_auto_and_timeout_auto() {
    assert_eq!(
        problems(&["run", "--slot", "auto", "--timeout", "5", "--log", "out.log", "--", "cmd"]),
        ["gate: --slot must be heavy, light, critical or none, got 'auto'"]
    );
    assert_eq!(
        problems(&["run", "--slot", "heavy", "--timeout", "auto", "--log", "out.log", "--", "cmd"]),
        ["gate: --timeout must be a whole number above 0, got 'auto'"]
    );
}

#[test]
fn run_reports_every_missing_input_on_its_own_line() {
    assert_eq!(
        problems(&["run"]),
        [
            "gate: missing --slot heavy|light|critical|none",
            "gate: missing --timeout <seconds>",
            "gate: missing --log <path>",
            "gate: missing the command after --"
        ]
    );
    assert_eq!(
        problems(&["-Local"]),
        [
            "gate: missing -Slot heavy|light|critical|none",
            "gate: missing -TimeoutSeconds <seconds>",
            "gate: missing -Log <path>",
            "gate: missing the command after --"
        ]
    );
}

#[test]
fn run_rejects_bad_timeout() {
    for value in ["abc", "0", "-1", "1.5", "+5", " 5"] {
        assert_eq!(
            problems(&["run", "--slot", "heavy", "--timeout", value, "--log", "out.log", "--", "cmd"]),
            [format!("gate: --timeout must be a whole number above 0, got '{value}'")],
            "--timeout {value}"
        );
    }
    assert_eq!(
        problems(&["run", "--slot", "heavy", "--timeout=0", "--log", "out.log", "--", "cmd"]),
        ["gate: --timeout must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems_with(&["--stall", "0", "--", "cmd"]),
        ["gate: --stall must be a whole number above 0, got '0'"]
    );
}

#[test]
fn run_caps_whole_numbers_at_i32_max() {
    let request = run_request(&[
        "run",
        "--slot",
        "heavy",
        "--timeout",
        "2147483647",
        "--log",
        "out.log",
        "--stall",
        "2147483647",
        "--tail",
        "2147483647",
        "--",
        "cmd",
    ]);
    assert_eq!(
        (request.timeout, request.stall, request.tail),
        (2_147_483_647, 2_147_483_647, 2_147_483_647)
    );

    for value in ["2147483648", "4294967296", "99999999999999999999"] {
        assert_eq!(
            problems(&["run", "--slot", "heavy", "--timeout", value, "--log", "out.log", "--", "cmd"]),
            [format!("gate: --timeout must be at most 2147483647, got '{value}'")],
            "--timeout {value}"
        );
        assert_eq!(
            problems_with(&["--stall", value, "--tail", value, "--", "cmd"]),
            [
                format!("gate: --stall must be at most 2147483647, got '{value}'"),
                format!("gate: --tail must be at most 2147483647, got '{value}'"),
            ],
            "--stall and --tail {value}"
        );
    }
    assert_eq!(
        problems(&["-Log", "x", "-TimeoutSeconds", "2147483648", "-Slot", "heavy", "--", "cmd"]),
        ["gate: -TimeoutSeconds must be at most 2147483647, got '2147483648'"]
    );
}

#[test]
fn run_rejects_bad_slot() {
    assert_eq!(
        problems(&["run", "--slot", "medium", "--timeout", "5", "--log", "out.log", "--", "cmd"]),
        ["gate: --slot must be heavy, light, critical or none, got 'medium'"]
    );
    for (value, slot) in [
        ("Light", Slot::Light),
        ("CRITICAL", Slot::Critical),
        ("heavy", Slot::Heavy),
        ("none", Slot::None),
        ("None", Slot::None),
    ] {
        let request = run_request(&["run", "--slot", value, "--timeout", "5", "--log", "out.log", "--", "cmd"]);
        assert_eq!(request.slot, slot, "--slot {value}");
    }
}

#[test]
fn run_rejects_bad_tail() {
    assert_eq!(
        problems_with(&["--tail", "-1", "--", "cmd"]),
        ["gate: --tail must be a whole number 0 or above, got '-1'"]
    );
    assert_eq!(run_with(&["--tail", "0", "--", "cmd"]).tail, 0);
}

#[test]
fn run_flag_missing_value() {
    assert_eq!(
        problems(&["run", "--slot", "heavy", "--timeout", "5", "--log"]),
        ["gate: --log needs a value", "gate: missing the command after --"]
    );
    assert_eq!(
        problems(&["run", "--slot", "heavy", "--timeout", "5", "--log", "--", "cmd"]),
        ["gate: --log needs a value"]
    );
    assert_eq!(
        problems(&["run", "--slot", "heavy", "--timeout", "5", "--log=", "--", "cmd"]),
        ["gate: --log needs a value"]
    );
}

#[test]
fn run_unknown_flag() {
    assert_eq!(problems_with(&["--bogus", "--", "cmd"]), ["gate: unknown option --bogus"]);
    assert_eq!(problems_with(&["-x", "--", "cmd"]), ["gate: unknown option -x"]);
    assert_eq!(
        problems_with(&["--no-markers=true", "--", "cmd"]),
        ["gate: unknown option --no-markers=true"]
    );
    assert_eq!(
        problems_with(&["-Log", "x", "--", "cmd"]),
        ["gate: unknown option -Log", "gate: unexpected argument x"]
    );
}

#[test]
fn run_repeated_option_is_usage_error() {
    assert_eq!(problems_with(&["--slot", "light", "--", "cmd"]), ["gate: --slot is given twice"]);
    assert_eq!(problems_with(&["--local", "--local", "--", "cmd"]), ["gate: --local is given twice"]);
    assert_eq!(
        problems(&["-Log", "a", "-log", "b", "-TimeoutSeconds", "5", "-Slot", "heavy", "--", "cmd"]),
        ["gate: -log is given twice"]
    );
}

#[test]
fn run_accepts_equals_form() {
    let request = run_request(&[
        "run",
        "--slot=heavy",
        "--timeout=5",
        "--log=out.log",
        "--stall=30",
        "--tail=0",
        "--state-dir=root",
        "--",
        "cmd",
    ]);
    assert_eq!(
        request,
        RunRequest {
            stall: 30,
            tail: 0,
            state_dir: Some(PathBuf::from("root")),
            ..run_with(&["--", "cmd"])
        }
    );
    assert_eq!(
        run_request(&["run", "--slot", "heavy", "--timeout", "5", "--log=a=b", "--", "cmd"]).log,
        PathBuf::from("a=b")
    );
}

#[test]
fn run_double_dash_ends_options() {
    assert_eq!(run_with(&["--", "--log", "x"]).command, os(&["--log", "x"]));
    assert_eq!(run_with(&["--", "--", "x"]).command, os(&["--", "x"]));
}

#[test]
fn run_command_words_pass_through_unchanged() {
    assert_eq!(
        run_with(&["--", "dotnet", "-p:Name=Value", "a b", ""]).command,
        os(&["dotnet", "-p:Name=Value", "a b", ""])
    );

    let mut words = os(&refs(&with(&RUN, &["--", "tool"])));
    words.push(unreadable_word());
    assert_eq!(run_of(words).command, vec![OsString::from("tool"), unreadable_word()]);

    let mut words = os(&["run", "--log"]);
    words.push(unreadable_word());
    words.extend(os(&["--slot", "heavy", "--timeout", "5", "--", "cmd"]));
    assert_eq!(problems_of(words), ["gate: cannot read the argument a\u{FFFD}"]);
}
