//! The PowerShell form through the binary. The compatibility layer has no `--state-dir`, so these cases give the root
//! through `GATE_STATE_DIR`.

use std::fs;
use std::process::Command;

use serde_json::Value;

use crate::support::{Case, FAKE_TOOL, USAGE, finish, print_run};

/// The PowerShell form of a run that prints its request, with `extra` before `--`.
fn compat_run(extra: &[&'static str]) -> Vec<&'static str> {
    let mut words = vec!["-Log", "out.log", "-TimeoutSeconds", "5", "-Slot", "heavy", "--test-print-request"];
    words.extend(extra);
    words.extend(["--", "cmd", "/c", "echo hi"]);
    words
}

fn with_env_root(case: &Case, args: &[&str]) -> Command {
    let mut command = case.gate_without_state_dir(args);
    command.env("GATE_STATE_DIR", case.root());
    command
}

fn compat_request(case: &Case, extra: &[&'static str]) -> Value {
    finish(with_env_root(case, &compat_run(extra))).request()
}

#[test]
fn compat_form_prints_the_same_request() {
    let case = Case::new("compat_form_prints_the_same_request");
    let new = finish(with_env_root(&case, &print_run(&["cmd", "/c", "echo hi"])));
    let old = finish(with_env_root(&case, &compat_run(&[])));

    assert_eq!(old.request(), new.request());
    assert_eq!(old.stdout, new.stdout);
}

#[test]
fn compat_no_markers_dollar_false_keeps_markers_on() {
    let case = Case::new("compat_no_markers_dollar_false");

    assert_eq!(compat_request(&case, &["-NoMarkers:$false"])["noMarkers"], false);
    assert_eq!(compat_request(&case, &["-NoMarkers"])["noMarkers"], true);
}

#[test]
fn compat_local_true_forces_local() {
    let case = Case::new("compat_local_true_forces_local");

    assert_eq!(compat_request(&case, &["-Local:true"])["local"], true);
    assert_eq!(compat_request(&case, &["-Local:TRUE"])["local"], true);
    assert_eq!(compat_request(&case, &[])["local"], false);
}

#[test]
fn compat_refuses_slot_auto_exit_2() {
    let case = Case::new("compat_refuses_slot_auto");
    let outcome = finish(with_env_root(
        &case,
        &["-Log", "out.log", "-TimeoutSeconds", "5", "-Slot", "auto", "--", "cmd"],
    ));

    assert_eq!(outcome.code, Some(2));
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        format!("gate: -Slot must be heavy, light, critical or none, got 'auto'\n{USAGE}")
    );
}

#[test]
fn compat_env_pair_reaches_the_command() {
    let case = Case::new("compat_env_pair_reaches_the_command");
    let words = [
        "-Log",
        "out.log",
        "-TimeoutSeconds",
        "5",
        "-Slot",
        "heavy",
        "-Env",
        "ZZ_COMPAT_ENV=from -Env",
        "--",
        FAKE_TOOL,
        "env",
        "ZZ_COMPAT_ENV",
    ];

    let outcome = finish(with_env_root(&case, &words));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.contains("from -Env"), "the command sees the pair: {log}");
}

#[test]
fn compat_env_name_forwards_the_caller_s_value() {
    let case = Case::new("compat_env_name_forwards_the_caller_s_value");
    let words = [
        "-Log",
        "out.log",
        "-TimeoutSeconds",
        "5",
        "-Slot",
        "heavy",
        "-Env",
        "ZZ_COMPAT_FORWARDED",
        "--",
        FAKE_TOOL,
        "env",
        "ZZ_COMPAT_FORWARDED",
    ];
    let mut command = with_env_root(&case, &words);
    command.env("ZZ_COMPAT_FORWARDED", "from the caller's environment");

    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(
        log.lines().any(|line| line == "from the caller's environment"),
        "the command sees the forwarded value: {log}"
    );
    assert!(
        !outcome
            .stdout
            .lines()
            .chain(outcome.stderr.lines())
            .any(|line| line.starts_with("gate: ") && line.contains("from the caller's environment")),
        "no line of the gate's says the value: {outcome:?}"
    );
}

#[test]
fn compat_slot_none_prints_in_the_request() {
    let case = Case::new("compat_slot_none_prints_in_the_request");
    let mut words = compat_run(&[]);
    words[5] = "none";

    assert_eq!(finish(with_env_root(&case, &words)).request()["slot"], "none");
}
