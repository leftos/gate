//! The first word, the usage and a valid run.

use crate::support::{Case, FAKE_TOOL, USAGE, assert_in_order, shown};

#[test]
fn version_exits_0() {
    let outcome = Case::new("version_exits_0").run(&["--version"]);

    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stdout, format!("gate {}\n", env!("CARGO_PKG_VERSION")));
    assert_eq!(outcome.stderr, "");
}

#[test]
fn help_and_help_subcommand_print_usage_on_stdout_exit_0() {
    let case = Case::new("help_and_help_subcommand");
    for word in ["--help", "help"] {
        let outcome = case.run(&[word]);

        assert_eq!(outcome.code, Some(0), "{word}");
        assert_eq!(outcome.stdout, USAGE, "{word}");
        assert_eq!(outcome.stderr, "", "{word}");
    }
}

#[test]
fn no_arguments_print_usage_and_exit_2() {
    let outcome = Case::new("no_arguments").run(&[]);

    assert_eq!(outcome.code, Some(2));
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, USAGE);
}

#[test]
fn unknown_subcommand_exits_2_with_usage() {
    let outcome = Case::new("unknown_subcommand").run(&["build"]);

    assert_eq!(outcome.code, Some(2));
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, format!("gate: unknown subcommand build\n{USAGE}"));
}

#[test]
fn run_reports_missing_inputs_then_usage_exit_2() {
    let case = Case::new("run_reports_missing_inputs");
    let outcome = case.run(&["run"]);

    assert_eq!(outcome.code, Some(2));
    assert_eq!(outcome.stdout, "");
    assert_in_order(
        &outcome.stderr,
        &[
            "gate: missing --slot heavy|light|critical\n",
            "gate: missing --timeout <seconds>\n",
            "gate: missing --log <path>\n",
            "gate: missing the command after --\n",
            "usage: gate run",
        ],
    );
    assert_eq!(outcome.stderr.lines().filter(|line| line.starts_with("gate: ")).count(), 4);
    assert!(outcome.stderr.ends_with(USAGE), "the usage comes last");
    assert_eq!(case.root_entries(), Vec::<String>::new(), "a refused run writes nothing");
    assert_eq!(case.folder_entries(), Vec::<String>::new(), "a refused run writes nothing");
}

#[test]
fn valid_run_runs_its_command_and_exits_with_its_status() {
    let case = Case::new("valid_run_runs_its_command");
    let outcome = case.run(&[
        "run",
        "--slot",
        "heavy",
        "--timeout",
        "5",
        "--log",
        "out.log",
        "--",
        FAKE_TOOL,
        "exit",
        "0",
    ]);

    let log = shown(&case.folder().join("out.log"));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(outcome.stdout.starts_with("gate: passed in "), "{outcome:?}");
    assert!(
        outcome.stdout.ends_with(&format!(" s (ceiling 5 s). Full output: {log}\n")),
        "{outcome:?}"
    );
    assert!(case.folder().join("out.log").is_file(), "the log is in the working folder");
    let entries = case.root_entries();
    assert!(entries.iter().any(|entry| entry == "events.jsonl"), "events.jsonl in {entries:?}");
}
