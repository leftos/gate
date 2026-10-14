//! The first word, the usage and a valid run.

use crate::support::{Case, FAKE_TOOL, USAGE, assert_in_order, shown};

#[test]
fn version_exits_0() {
    let outcome = Case::new("version_exits_0").run(&["--version"]);

    assert_eq!(outcome.code, Some(0));
    assert!(outcome.stdout.starts_with("gate "), "{outcome:?}");
    assert_eq!(outcome.stderr, "");
}

#[test]
fn version_names_the_package_version_and_the_build() {
    let outcome = Case::new("version_names_the_package_version_and_the_build").run(&["--version"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    // A test build carries no `GATE_BUILD_ID`, so this is the package version and the `+dev` build, never a literal id.
    assert!(
        outcome.stdout.starts_with(&format!("gate {}+", env!("CARGO_PKG_VERSION"))),
        "the version is the package version and the build: {outcome:?}"
    );
}

#[test]
fn help_and_help_subcommand_print_usage_on_stdout_exit_0() {
    let case = Case::new("help_and_help_subcommand");
    for words in [&["--help"][..], &["help"][..], &["-h"][..], &["run", "--help"][..], &["run", "-h"][..]] {
        let outcome = case.run(words);

        assert_eq!(outcome.code, Some(0), "{words:?}");
        assert_eq!(outcome.stdout, USAGE, "{words:?}");
        assert_eq!(outcome.stderr, "", "{words:?}");
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
fn remote_cutover_is_an_unknown_subcommand() {
    let outcome = Case::new("remote_cutover_unknown").run(&["remote-cutover"]);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, format!("gate: unknown subcommand remote-cutover\n{USAGE}"));
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
            "gate: missing --slot heavy|light|critical|none\n",
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
    let passed = format!(
        r"\Agate: passed in \d+ s \(load-adjusted \d+ s, ceiling 5 s\)\. Full output: {}\n\z",
        regex::escape(&log)
    );
    assert!(
        regex::Regex::new(&passed).expect("the pattern is valid").is_match(&outcome.stdout),
        "{outcome:?}"
    );
    assert!(case.folder().join("out.log").is_file(), "the log is in the working folder");
    let entries = case.root_entries();
    assert!(entries.iter().any(|entry| entry == "events.jsonl"), "events.jsonl in {entries:?}");
    assert!(entries.iter().any(|entry| entry == "durations.json"), "durations.json in {entries:?}");
}
