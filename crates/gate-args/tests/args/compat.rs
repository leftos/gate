//! The compatibility layer: the PowerShell gate's argument form.

use gate_args::{Invocation, RunRequest, Slot, Spelling, StopRequest, parse};

use crate::support::{RUN, invocation, os, problems, refs, run_request, stop_request, with};

const COMPAT: [&str; 6] = ["-Log", "out.log", "-TimeoutSeconds", "5", "-Slot", "heavy"];
const COMMAND: [&str; 4] = ["--", "cmd", "/c", "echo hi"];

fn compat(extra: &[&str]) -> Vec<String> {
    with(&COMPAT, extra)
}

/// The request the CLI words `new` give, as the compatibility layer's form gives it: the same, in its own spelling.
fn as_compat(new: &[String]) -> RunRequest {
    RunRequest {
        spelling: Spelling::Compat,
        ..run_request(&refs(new))
    }
}

#[test]
fn compat_accepts_the_powershell_form() {
    let old = compat(&COMMAND);
    let new = with(&RUN, &COMMAND);
    assert_eq!(run_request(&refs(&old)), as_compat(&new));

    let old = compat(&["-StallSeconds", "30", "-Tail", "5", "-NoMarkers", "-Local", "--", "cmd"]);
    let new = with(&RUN, &["--stall", "30", "--tail", "5", "--no-markers", "--local", "--", "cmd"]);
    assert_eq!(run_request(&refs(&old)), as_compat(&new));
}

#[test]
fn compat_options_are_case_insensitive() {
    let old = [
        "-log",
        "out.log",
        "-TIMEOUTSECONDS",
        "5",
        "-sLoT",
        "Heavy",
        "-stallseconds",
        "30",
        "-TAIL",
        "5",
        "-nomarkers",
        "-LOCAL",
        "--",
        "cmd",
    ];
    let new = with(&RUN, &["--stall", "30", "--tail", "5", "--no-markers", "--local", "--", "cmd"]);
    assert_eq!(run_request(&old), as_compat(&new));
}

#[test]
fn compat_reads_colon_forms() {
    let old = [
        "-Log:out.log",
        "-TimeoutSeconds:5",
        "-Slot:heavy",
        "-StallSeconds:30",
        "-Tail:5",
        "--",
        "cmd",
    ];
    let new = with(&RUN, &["--stall", "30", "--tail", "5", "--", "cmd"]);
    assert_eq!(run_request(&old), as_compat(&new));
}

#[test]
fn compat_no_markers_colon_true_sets_it() {
    for switch in ["-NoMarkers:true", "-NoMarkers:$true", "-nomarkers:True"] {
        assert!(run_request(&refs(&compat(&[switch, "--", "cmd"]))).no_markers, "{switch} sets it");
    }
}

#[test]
fn compat_no_markers_colon_dollar_false_keeps_markers_on() {
    for switch in ["-NoMarkers:$false", "-NoMarkers:$FALSE", "-NoMarkers:false"] {
        assert!(
            !run_request(&refs(&compat(&[switch, "--", "cmd"]))).no_markers,
            "{switch} keeps markers on"
        );
    }
}

#[test]
fn compat_local_colon_true_forces_local() {
    for switch in ["-Local:true", "-Local:TRUE", "-Local:$true"] {
        assert!(run_request(&refs(&compat(&[switch, "--", "cmd"]))).local, "{switch} forces local");
    }
}

#[test]
fn compat_local_colon_false_clears_it() {
    for switch in ["-Local:false", "-Local:$false", "-Local:$False"] {
        assert!(!run_request(&refs(&compat(&[switch, "--", "cmd"]))).local, "{switch} clears it");
    }
}

#[test]
fn compat_switch_colon_other_value_is_usage_error() {
    assert_eq!(
        problems(&refs(&compat(&["-Local:maybe", "--", "cmd"]))),
        ["gate: -Local takes true or false, got 'maybe'"]
    );
    assert_eq!(
        problems(&refs(&compat(&["-NoMarkers:1", "--", "cmd"]))),
        ["gate: -NoMarkers takes true or false, got '1'"]
    );
    assert_eq!(
        problems(&refs(&compat(&["-Local:", "--", "cmd"]))),
        ["gate: -Local takes true or false, got ''"]
    );
}

#[test]
fn compat_refuses_slot_auto() {
    let words = ["-Log", "out.log", "-TimeoutSeconds", "5", "-Slot", "auto", "--", "cmd"];
    assert_eq!(problems(&words), ["gate: -Slot must be heavy, light, critical or none, got 'auto'"]);
}

#[test]
fn compat_accepts_slot_none_in_any_case() {
    for words in [
        ["-Log", "out.log", "-TimeoutSeconds", "5", "-Slot", "none", "--", "cmd"].as_slice(),
        ["-Log", "out.log", "-TimeoutSeconds", "5", "-slot:NONE", "--", "cmd"].as_slice(),
    ] {
        assert_eq!(run_request(words).slot, Slot::None, "{words:?}");
    }
}

#[test]
fn compat_refuses_new_cli_flags_in_old_form() {
    assert_eq!(
        problems(&refs(&compat(&["-StateDir", "x", "--", "cmd"]))),
        ["gate: unknown option -StateDir", "gate: unexpected argument x"]
    );
    assert_eq!(
        problems(&["-Log", "out.log", "-TimeoutSeconds", "5", "--slot", "heavy", "--", "cmd"]),
        [
            "gate: unknown option --slot",
            "gate: unexpected argument heavy",
            "gate: missing -Slot heavy|light|critical|none"
        ]
    );
    assert_eq!(problems(&refs(&compat(&["-Expect", "--", "cmd"]))), ["gate: unknown option -Expect"]);
    assert_eq!(
        problems(&refs(&compat(&["--state-dir=x", "--", "cmd"]))),
        ["gate: unknown option --state-dir=x"]
    );
}

#[test]
fn compat_refuses_unknown_option() {
    assert_eq!(problems(&refs(&compat(&["-Bogus", "--", "cmd"]))), ["gate: unknown option -Bogus"]);
    assert_eq!(problems(&refs(&compat(&["-Bogus:1", "--", "cmd"]))), ["gate: unknown option -Bogus:1"]);
}

#[test]
fn compat_requires_double_dash() {
    assert_eq!(
        problems(&refs(&compat(&["cmd", "/c", "echo hi"]))),
        [
            "gate: unexpected argument cmd",
            "gate: unexpected argument /c",
            "gate: unexpected argument echo hi",
            "gate: missing the command after --"
        ]
    );
    assert_eq!(problems(&refs(&compat(&["--"]))), ["gate: missing the command after --"]);
}

#[test]
fn compat_stop_tree_maps_to_stop_tree() {
    let expected = StopRequest {
        pid: 123,
        state_dir: None,
        print_request: false,
    };
    assert_eq!(stop_request(&["-StopTree", "123"]), expected);
    assert_eq!(stop_request(&["-stoptree:123"]), expected);
    assert_eq!(invocation(&["-StopTree", "123"]), invocation(&["stop-tree", "123"]));
    assert_eq!(problems(&["-StopTree"]), ["gate: missing the process id"]);
    assert_eq!(problems(&["-StopTree:"]), ["gate: missing the process id"]);
    assert_eq!(
        problems(&["-StopTree", "abc"]),
        ["gate: the process id must be a whole number above 0, got 'abc'"]
    );
}

#[test]
fn compat_value_word_that_is_an_option_is_not_a_value() {
    assert_eq!(
        problems(&["-Log", "-NoMarkers", "-TimeoutSeconds", "5", "-Slot", "heavy", "--", "cmd"]),
        ["gate: -Log needs a value"]
    );
    assert_eq!(
        problems(&["-Log", "x", "-TimeoutSeconds", "-slot:heavy", "--", "cmd"]),
        ["gate: -TimeoutSeconds needs a value"]
    );
    assert_eq!(
        problems(&["-StopTree", "-Local"]),
        ["gate: -StopTree is used alone", "gate: missing the process id"]
    );
    assert_eq!(
        problems(&refs(&compat(&["-Tail", "-1", "--", "cmd"]))),
        ["gate: -Tail must be a whole number 0 or above, got '-1'"],
        "a dashed word that is not an option name is still a value"
    );
}

#[test]
fn compat_stop_tree_is_used_alone() {
    assert_eq!(
        problems(&["-StopTree", "abc", "-Log", "x"]),
        [
            "gate: -StopTree is used alone",
            "gate: the process id must be a whole number above 0, got 'abc'"
        ]
    );
    assert_eq!(problems(&["-StopTree", "1", "-StopTree", "2"]), ["gate: -StopTree is given twice"]);
    assert_eq!(problems(&["-StopTree", "123", "-Log", "x"]), ["gate: -StopTree is used alone"]);
    assert_eq!(
        problems(&["-Log", "x", "-StopTree", "123", "--", "cmd"]),
        ["gate: -StopTree is used alone"]
    );
    assert_eq!(problems(&["-StopTree", "123", "--", "cmd"]), ["gate: -StopTree is used alone"]);
    assert!(matches!(parse(os(&["-StopTree", "1"])), Ok(Invocation::StopTree(_))));
}
