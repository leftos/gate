//! `gate burn`: `--cores <n>`, `--seconds <s>`, the usage line, and the words it refuses.

use gate_args::{BurnRequest, Invocation, USAGE};

use crate::support::{invocation, problems};

#[test]
fn burn_takes_cores_and_seconds() {
    assert_eq!(
        invocation(&["burn", "--cores", "4", "--seconds", "2"]),
        Invocation::Burn(BurnRequest { cores: Some(4), seconds: 2 })
    );
    assert_eq!(
        invocation(&["burn", "--seconds=180"]),
        Invocation::Burn(BurnRequest { cores: None, seconds: 180 }),
        "the cores are the machine's logical processors when --cores is not given"
    );
    assert!(
        USAGE.lines().any(|line| line == "       gate burn [--cores <n>] --seconds <s>"),
        "the usage lists gate burn:\n{USAGE}"
    );
}

#[test]
fn burn_needs_seconds() {
    assert_eq!(problems(&["burn"]), ["gate: missing --seconds <seconds>"]);
    assert_eq!(problems(&["burn", "--cores", "2"]), ["gate: missing --seconds <seconds>"]);
    assert_eq!(problems(&["burn", "--seconds"]), ["gate: --seconds needs a value"]);
}

#[test]
fn burn_refuses_seconds_over_180() {
    assert_eq!(
        problems(&["burn", "--seconds", "181"]),
        ["gate: --seconds must be a whole number from 1 to 180, got '181'"]
    );
    assert_eq!(
        problems(&["burn", "--seconds", "999999999999"]),
        ["gate: --seconds must be a whole number from 1 to 180, got '999999999999'"]
    );
}

#[test]
fn burn_refuses_zero_seconds_and_zero_cores() {
    assert_eq!(
        problems(&["burn", "--seconds", "0"]),
        ["gate: --seconds must be a whole number from 1 to 180, got '0'"]
    );
    assert_eq!(
        problems(&["burn", "--seconds", "abc"]),
        ["gate: --seconds must be a whole number from 1 to 180, got 'abc'"]
    );
    assert_eq!(
        problems(&["burn", "--seconds", "2", "--cores", "0"]),
        ["gate: --cores must be a whole number above 0, got '0'"]
    );
    assert_eq!(
        problems(&["burn", "--seconds", "2", "--cores", "many"]),
        ["gate: --cores must be a whole number above 0, got 'many'"]
    );
}

#[test]
fn burn_refuses_another_word() {
    assert_eq!(problems(&["burn", "now", "--seconds", "2"]), ["gate: unexpected argument now"]);
    assert_eq!(
        problems(&["burn", "--"]),
        ["gate: unexpected argument --", "gate: missing --seconds <seconds>"]
    );
    assert_eq!(
        problems(&["burn", "--state-dir", "x", "--seconds", "2"]),
        ["gate: unknown option --state-dir", "gate: unexpected argument x"]
    );
    assert_eq!(
        problems(&["burn", "--seconds", "2", "--seconds", "3"]),
        ["gate: --seconds is given twice"]
    );
}
