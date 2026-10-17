//! A real run: the command started with its environment, its output, `.err` bytes and the gate's own lines in the
//! log, the marker scan, the tail and the verdict line, a start failure, a refused batch word, a log another run's
//! command holds and a log inside the state root, and words reaching the command exactly, through a program and
//! through a `.cmd` file.

use std::fs;
use std::path::PathBuf;

use gate_state::{KernelObject, StateRoot};
use gate_win32::{NamedEvent, StoppableProcess};

use crate::support::{
    Case, Escapee, FAKE_TOOL, GATE, Go, assert_in_order, copy_tool, events, finish, none_words, run_words, said_pid, shown, start, wait_for_file,
    wait_for_journal_line, wait_until, wait_until_closed, write_counts,
};

/// The exit code a process the gate ends at its run's end is given.
const STOPPED: u32 = 124;

/// A light pool of one slot.
const ONE_LIGHT: &str = r#"{"v":1,"light":1}"#;
/// A light pool of two slots.
const TWO_LIGHT: &str = r#"{"v":1,"light":2}"#;

/// cmd.exe's `%VAR%` forms, with `VAR` set in the gate's environment.
const PERCENT_WORDS: [&str; 4] = ["%CD%", "%VAR:~0,3%", "%VAR:a=b%", "%RANDOM%"];

/// Words the C runtime and cmd.exe would split, unquote or misread unless each is quoted for its reader.
const QUOTED_WORDS: [&str; 11] = [
    "a b",
    "",
    r#"x"y"#,
    r"c:\dir with space\",
    r#"back\\"quote"#,
    "h\u{e9}llo w\u{f6}rld",
    "\u{65e5}\u{672c}\u{8a9e}",
    "\u{1f600}",
    "-p:Name=Value",
    "a&b",
    "(x)",
];

fn log_path(case: &Case) -> PathBuf {
    case.folder().join("out.log")
}

fn log_bytes(case: &Case) -> Vec<u8> {
    fs::read(log_path(case)).expect("the log was written")
}

fn log_text(case: &Case) -> String {
    String::from_utf8_lossy(&log_bytes(case)).into_owned()
}

/// A `.cmd` file in the case root that hands its words to `fake-tool argv`, by the fake tool's absolute path.
fn write_shim(case: &Case) -> String {
    let shim = case.folder().join("shim.cmd");
    fs::write(&shim, format!("@\"{FAKE_TOOL}\" argv %*\r\n")).expect("the shim can be written");
    shown(&shim)
}

/// The line the gate refuses a batch target's `word` with.
fn refusal(word: &str) -> String {
    format!("gate: a .cmd or .bat command cannot take a word holding a line break or a %NAME% cmd.exe would expand: {word}\n")
}

/// What `fake-tool argv` writes for `words`: each on its own line.
fn argv_output(words: &[&str]) -> Vec<u8> {
    words.iter().flat_map(|word| format!("{word}\n").into_bytes()).collect()
}

/// The journal's events, in order.

#[test]
fn a_run_launches_its_command_and_passes_its_status() {
    let case = Case::new("a_run_launches_its_command_and_passes_its_status");

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "hello", "err", "oops", "exit", "0"]));

    let log = shown(&log_path(&case));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(
        outcome.stdout.starts_with("hello\noops\ngate: passed in "),
        "the tail, then the verdict: {outcome:?}"
    );
    let passed = format!(
        r"\ngate: passed in \d+ s \(load-adjusted \d+ s, ceiling 3600 s\)\. Full output: {}\n\z",
        regex::escape(&log)
    );
    assert!(
        regex::Regex::new(&passed).expect("the pattern is valid").is_match(&outcome.stdout),
        "{outcome:?}"
    );
    let text = log_text(&case);
    assert!(
        text.starts_with("hello\noops\ngate: passed in "),
        "output, errors, then the gate's lines: {text}"
    );
    assert!(!case.folder().join("out.log.err").exists(), "the .err file went into the log");
}

#[test]
fn exit_3_is_passed_through_with_the_failed_line() {
    let case = Case::new("exit_3_is_passed_through_with_the_failed_line");

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "hi", "exit", "3"]));

    let failed = format!("gate: FAILED (status 3). Full output: {}\n", shown(&log_path(&case)));
    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    assert_eq!(outcome.stdout, "hi\n");
    assert_eq!(outcome.stderr, failed);
    assert_eq!(log_text(&case), format!("hi\n{failed}"));
}

#[test]
fn a_marker_with_exit_0_is_status_1() {
    let case = Case::new("a_marker_with_exit_0_is_status_1");

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "Build FAILED.", "exit", "0"]));

    let log = shown(&log_path(&case));
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "1:Build FAILED.\nBuild FAILED.\n", "the marker lines, then the tail");
    assert_eq!(
        outcome.stderr,
        format!("gate: command exited 0 but its log reports a failure -> {log}\ngate: FAILED (status 1). Full output: {log}\n")
    );
}

#[test]
fn no_markers_keeps_status_0() {
    let case = Case::new("no_markers_keeps_status_0");

    let outcome = case.run(&run_words(
        "out.log",
        &["--no-markers"],
        &[FAKE_TOOL, "out", "Build FAILED.", "exit", "0"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(outcome.stdout.starts_with("Build FAILED.\ngate: passed in "), "{outcome:?}");
}

#[test]
fn a_missing_program_exits_127_and_releases_the_slot() {
    let case = Case::new("a_missing_program_exits_127_and_releases_the_slot");
    write_counts(&case.root(), ONE_LIGHT);

    let outcome = case.run(&run_words("out.log", &[], &["gate-no-such-program", "x"]));

    let start = format!("gate: could not start gate-no-such-program: {}", gate_win32::win32_message(2));
    let failed = format!("gate: FAILED (status 127). Full output: {}", shown(&log_path(&case)));
    assert_eq!(outcome.code, Some(127), "{outcome:?}");
    assert_eq!(outcome.stderr, format!("{start}\n{failed}\n"));
    assert_eq!(outcome.stdout, format!("{start}\n"), "the log's tail holds the gate's line");
    assert_eq!(log_text(&case), format!("{start}\n{failed}\n"));
    let journalled = events(&case);
    let names: Vec<&str> = journalled.iter().filter_map(|event| event["event"].as_str()).collect();
    assert_eq!(names, ["queue", "take", "release"]);
    assert_eq!(journalled[2]["status"], 127);

    let next = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "exit", "0"]));

    assert_eq!(next.code, Some(0), "{next:?}");
    assert_eq!(next.stderr, "", "the slot was released, not left held: {next:?}");
}

/// The suite's own `PATH` with `folders` in front, for a case whose bare word must be found in one of them.
fn path_with(folders: &[PathBuf]) -> std::ffi::OsString {
    let own = std::env::var_os("PATH").expect("PATH is set");
    std::env::join_paths(folders.iter().cloned().chain(std::env::split_paths(&own))).expect("the folders join into a PATH")
}

/// A bare name holding a dot that is not a `PATHEXT` extension, the shape a versioned Godot exe's derived name has, is
/// found on `PATH` as `<name>.exe`.
#[test]
fn a_dotted_bare_name_is_found_with_exe_appended() {
    let case = Case::new("a_dotted_bare_name_is_found_with_exe_appended");
    copy_tool(&case, "fake.tool.v1.exe");
    let mut command = case.gate(&run_words("out.log", &[], &["fake.tool.v1", "out", "ran", "exit", "0"]));
    command.env("PATH", path_with(&[case.folder()]));

    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.starts_with("ran\n"), "{outcome:?}");
}

#[test]
fn a_refused_batch_word_exits_2_before_queueing() {
    let case = Case::new("a_refused_batch_word_exits_2_before_queueing");
    let shim = write_shim(&case);

    let outcome = case.run(&run_words("out.log", &[], &[&shim, "%PATH%"]));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, refusal("%PATH%"));
    assert!(!case.root().join("events.jsonl").exists(), "nothing was queued");
    assert!(!log_path(&case).exists(), "no log was readied");
}

#[test]
fn the_build_server_environment_reaches_the_command() {
    let case = Case::new("the_build_server_environment_reaches_the_command");
    let command = [
        FAKE_TOOL,
        "env",
        "SharedCompilationId",
        "env",
        "MSBUILDNODEHANDSHAKESALT",
        "env",
        "MSBUILDUSESERVER",
        "env",
        "DOTNET_CLI_USE_MSBUILD_SERVER",
        "env",
        "MSBUILDDISABLENODEREUSE",
        "env",
        "GATE_SLOT_HELD",
        "env",
        "GATE_STATE_DIR",
    ];
    let mut gate = case.gate(&run_words("out.log", &[], &command));
    // An inherited MSBuild server switch is overridden; node reuse is turned off by the gate itself, not inherited from a
    // gate above the suite's own run.
    gate.env("MSBUILDUSESERVER", "1").env_remove("MSBUILDDISABLENODEREUSE");
    let running = start(gate);
    let pid = running.id();

    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let text = log_text(&case);
    let lines: Vec<&str> = text.lines().collect();
    let marker: Vec<&str> = lines[5].split(':').collect();
    let root = StateRoot::new(&shown(&case.root()), None);
    let hash = root.hash().expect("a case root is not the default root");
    assert_eq!(marker.len(), 4, "{text}");
    assert_eq!((marker[0], marker[1], marker[3]), ("light", pid.to_string().as_str(), hash), "{text}");
    assert!(marker[2].parse::<u64>().is_ok(), "the gate's start in epoch ms: {text}");
    let run_id = format!("gate-{pid}-{}-{hash}", marker[2]);
    assert_eq!(
        lines[..2],
        [run_id.as_str(), run_id.as_str()],
        "the compiler server and the nodes are the run's: {text}"
    );
    assert_eq!(lines[2..5], ["0", "0", "1"], "the MSBuild server and node reuse are off: {text}");
    assert_eq!(lines[6], shown(&case.root()), "{text}");
}

/// The gate turns shared compilation off for every command it starts, an inherited `true` included: a command that waits
/// for every process in its job never ends while the run's own Roslyn compiler server idles in it (GATE-383; Godot's
/// console wrapper waits for its whole job).
#[test]
fn the_command_gets_shared_compilation_off_over_an_inherited_on() {
    let case = Case::new("the_command_gets_shared_compilation_off_over_an_inherited_on");
    let mut gate = case.gate(&run_words("out.log", &[], &[FAKE_TOOL, "env", "UseSharedCompilation"]));
    gate.env("UseSharedCompilation", "true");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        log_text(&case).lines().next(),
        Some("false"),
        "the command sees the gate's value, not the inherited one"
    );
}

#[test]
fn an_env_pair_reaches_the_command_and_cannot_undo_the_gate_s_own() {
    let case = Case::new("an_env_pair_reaches_the_command_and_cannot_undo_the_gate_s_own");
    let command = [
        FAKE_TOOL,
        "env",
        "ZZ_GATE_ENV_GIVEN",
        "env",
        "ZZ_GATE_ENV_OTHER",
        "env",
        "MSBUILDUSESERVER",
    ];
    let extra = ["--env", "zz_gate_env_given=from --env", "--env", "ZZ_GATE_ENV_OTHER=a=b"];
    let mut gate = case.gate(&run_words("out.log", &extra, &command));
    gate.env("ZZ_GATE_ENV_GIVEN", "inherited").env("MSBUILDUSESERVER", "1");

    let outcome = start(gate).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let text = log_text(&case);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[..3],
        ["from --env", "a=b", "0"],
        "the pair replaces an inherited value under another letter case, and the gate's own setting stays: {text}"
    );

    let refused = case.run(&run_words("refused.log", &["--env", "MSBUILDUSESERVER=1"], &[FAKE_TOOL, "exit", "0"]));

    assert_eq!(refused.code, Some(2), "{refused:?}");
    assert!(
        refused
            .stderr
            .starts_with("gate: --env sets MSBUILDUSESERVER, which the gate sets itself\nusage: gate run"),
        "{refused:?}"
    );
    assert!(!case.folder().join("refused.log").exists(), "a refused run readies no log");
}

#[test]
fn a_forwarded_env_name_reaches_the_command_and_the_gate_does_not_say_its_value() {
    let case = Case::new("a_forwarded_env_name_reaches_the_command_and_the_gate_does_not_say_its_value");
    let command = [FAKE_TOOL, "env", "ZZ_GATE_FORWARDED"];
    let mut gate = case.gate(&run_words("out.log", &["--env", "ZZ_GATE_FORWARDED"], &command));
    gate.env("ZZ_GATE_FORWARDED", "from the caller's environment");

    let outcome = start(gate).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let text = log_text(&case);
    assert!(
        text.lines().any(|line| line == "from the caller's environment"),
        "the command sees the forwarded value: {text}"
    );
    assert!(
        !outcome
            .stdout
            .lines()
            .chain(outcome.stderr.lines())
            .any(|line| line.starts_with("gate: ") && line.contains("from the caller's environment")),
        "no line of the gate's says the value: {outcome:?}"
    );
    assert!(
        !text
            .lines()
            .any(|line| line.starts_with("gate: ") && line.contains("from the caller's environment")),
        "no line of the gate's says the value: {text}"
    );
}

#[test]
fn a_written_and_a_forwarded_value_may_hold_a_line_end() {
    let case = Case::new("a_written_and_a_forwarded_value_may_hold_a_line_end");
    let command = [FAKE_TOOL, "env", "ZZ_GATE_WRITTEN_LINES", "env", "ZZ_GATE_FORWARDED_LINES"];
    let extra = ["--env", "ZZ_GATE_WRITTEN_LINES=one\ntwo", "--env", "ZZ_GATE_FORWARDED_LINES"];
    let mut gate = case.gate(&run_words("out.log", &extra, &command));
    gate.env("ZZ_GATE_FORWARDED_LINES", "one\r\ntwo");

    let outcome = start(gate).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let text = log_text(&case);
    assert!(text.contains("one\ntwo"), "the written value reaches the command as it is: {text:?}");
    assert!(text.contains("one\r\ntwo"), "the forwarded value reaches the command as it is: {text:?}");
}

#[test]
fn two_runs_give_their_commands_different_compiler_server_ids() {
    let case = Case::new("two_runs_give_their_commands_different_compiler_server_ids");
    let first = case.run(&run_words("first.log", &[], &[FAKE_TOOL, "env", "SharedCompilationId"]));
    let second = case.run(&run_words("second.log", &[], &[FAKE_TOOL, "env", "SharedCompilationId"]));

    assert_eq!((first.code, second.code), (Some(0), Some(0)), "{first:?} {second:?}");
    let id = |log: &str| {
        let text = fs::read_to_string(case.folder().join(log)).expect("the log was written");
        text.lines().next().unwrap_or_default().to_owned()
    };
    let (first_id, second_id) = (id("first.log"), id("second.log"));
    assert!(first_id.starts_with("gate-"), "{first_id}");
    assert!(second_id.starts_with("gate-"), "{second_id}");
    assert_ne!(first_id, second_id, "each run has its own id");
}

#[test]
fn percent_forms_reach_a_program_untouched_and_a_batch_file_refuses_them() {
    let case = Case::new("percent_forms_reach_a_program_untouched_and_a_batch_file_refuses_them");
    let shim = write_shim(&case);
    let mut command = vec![FAKE_TOOL, "argv"];
    command.extend(PERCENT_WORDS);
    let mut gate = case.gate(&run_words("out.log", &[], &command));
    gate.env("VAR", "abcdef");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(log_bytes(&case).starts_with(&argv_output(&PERCENT_WORDS)), "{}", log_text(&case));
    for word in PERCENT_WORDS {
        let mut gate = case.gate(&run_words("out.log", &[], &[&shim, word]));
        gate.env("VAR", "abcdef");

        let refused = finish(gate);

        assert_eq!(refused.code, Some(2), "{word}: {refused:?}");
        assert_eq!(refused.stderr, refusal(word), "{word}");
    }
}

#[test]
fn quoted_spaced_and_non_ascii_words_reach_the_command_byte_for_byte() {
    let case = Case::new("quoted_spaced_and_non_ascii_words_reach_the_command_byte_for_byte");
    let shim = write_shim(&case);
    let expected = argv_output(&QUOTED_WORDS);
    let mut direct = vec![FAKE_TOOL, "argv"];
    direct.extend(QUOTED_WORDS);
    let mut through_cmd = vec![shim.as_str()];
    through_cmd.extend(QUOTED_WORDS);

    for command in [direct, through_cmd] {
        let outcome = case.run(&run_words("out.log", &[], &command));

        assert_eq!(outcome.code, Some(0), "{}: {outcome:?}", command[0]);
        let bytes = log_bytes(&case);
        assert!(bytes.starts_with(&expected), "{}: {}", command[0], log_text(&case));
        assert!(
            bytes[expected.len()..].starts_with(b"gate: passed in "),
            "{}: {}",
            command[0],
            log_text(&case)
        );
    }
}

#[test]
fn invalid_utf8_keeps_its_bytes_in_the_log_and_prints_as_replacement_characters() {
    let case = Case::new("invalid_utf8_keeps_its_bytes_in_the_log_and_prints_as_replacement_characters");

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "bytes", "6f6b20fffe206261640a"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(log_bytes(&case).starts_with(b"ok \xff\xfe bad\n"), "{:?}", log_bytes(&case));
    assert!(
        outcome.raw_stdout.starts_with("ok \u{fffd}\u{fffd} bad\n".as_bytes()),
        "{:?}",
        outcome.raw_stdout
    );
}

#[test]
fn a_takeover_line_follows_the_command_output_in_the_log() {
    let case = Case::new("a_takeover_line_follows_the_command_output_in_the_log");
    write_counts(&case.root(), ONE_LIGHT);
    let name = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::LightSlot(0));
    let go = shown(&case.folder().join("go"));
    let held_log = case.folder().join("held.log");
    let holder = case.start(&run_words("held.log", &[], &[FAKE_TOOL, "out", "holding", "wait", &go]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));
    wait_until("the holder's command writing its log", || {
        fs::read(&held_log).is_ok_and(|bytes| bytes.starts_with(b"holding\n"))
    });
    holder.kill();

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "the command's output", "exit", "0"]));
    // The killed gate's command is let go, and every handle on its log seen closed, before any assertion, so none is
    // left waiting.
    fs::write(&go, "").expect("the go file can be written");
    wait_until_closed(&held_log);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = format!("gate: took {name}, which a killed gate had left held\n");
    assert_in_order(&log_text(&case), &["the command's output\n", &line, "gate: passed in "]);
}

#[test]
fn an_exit_code_above_255_and_a_negative_one_are_passed_through_whole() {
    let case = Case::new("an_exit_code_above_255_and_a_negative_one_are_passed_through_whole");

    for (status, code) in [("300", 300), ("-1", -1)] {
        let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "exit", status]));

        assert_eq!(outcome.code, Some(code), "{outcome:?}");
        let failed = format!("gate: FAILED (status {code}). Full output: {}\n", shown(&log_path(&case)));
        assert!(outcome.stderr.ends_with(&failed), "{outcome:?}");
    }
}

#[test]
fn a_marker_after_a_lone_carriage_return_is_a_line_of_its_own() {
    let case = Case::new("a_marker_after_a_lone_carriage_return_is_a_line_of_its_own");

    let outcome = case.run(&run_words(
        "out.log",
        &[],
        &[FAKE_TOOL, "bytes", "3530250d4275696c64204641494c45442e0a", "exit", "0"],
    ));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stdout.starts_with("2:Build FAILED.\n50%\nBuild FAILED.\n"), "{outcome:?}");
}

#[test]
fn a_batch_file_named_through_a_dot_dot_is_still_a_batch_target() {
    let case = Case::new("a_batch_file_named_through_a_dot_dot_is_still_a_batch_target");
    let named = format!(r"{}\x\..", write_shim(&case));

    let outcome = case.run(&run_words("out.log", &[], &[&named, "%CD%"]));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stderr, refusal("%CD%"));
}

#[test]
fn a_second_gate_cannot_remove_a_log_a_running_command_holds() {
    let case = Case::new("a_second_gate_cannot_remove_a_log_a_running_command_holds");
    write_counts(&case.root(), TWO_LIGHT);
    let log = case.folder().join("same.log");
    let go = shown(&case.folder().join("go"));
    // Its go file ends it, so its ceiling sits past the suite's hang guard: a short one could end it before the second gate looks.
    let held = [
        "run",
        "--slot",
        "light",
        "--timeout",
        "900",
        "--log",
        "same.log",
        "--",
        FAKE_TOOL,
        "out",
        "started",
        "wait",
        &go,
    ];
    let first = case.start(&held);
    wait_until("the first command writing its log", || {
        fs::read(&log).is_ok_and(|bytes| bytes.starts_with(b"started\n"))
    });

    let second = case.run(&[
        "run",
        "--slot",
        "light",
        "--timeout",
        "5",
        "--log",
        "same.log",
        "--",
        FAKE_TOOL,
        "exit",
        "0",
    ]);
    // The first command is let go before any assertion, so a failing one leaves no command waiting for ever.
    fs::write(&go, "").expect("the go file can be written");
    let outcome = first.finish();

    assert_eq!(second.code, Some(1), "{second:?}");
    assert_eq!(second.stdout, "", "{second:?}");
    assert_eq!(second.stderr, format!("gate: the log {} is in use by another gate\n", shown(&log)));
    let queued = events(&case).iter().filter(|event| event["event"] == "queue").count();
    assert_eq!(queued, 1, "only the first gate queued");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let text = fs::read_to_string(&log).expect("the first gate's log was kept");
    assert!(text.starts_with("started\ngate: passed in "), "its own output, then its verdict: {text}");
}

#[test]
fn a_log_held_by_a_leftover_process_does_not_block_the_next_run() {
    let case = Case::new("a_log_held_by_a_leftover_process_does_not_block_the_next_run");
    let go = shown(&case.folder().join("go"));

    let first = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "first", "spawn-wait", &go]));
    let second = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "second"]));
    // The leftover is let go, and seen to end, before any assertion and before the case folder goes, so no helper is
    // left waiting.
    fs::write(&go, "").expect("the go file can be written");
    let leftover: Option<u32> = first.stdout.lines().find_map(|line| line.strip_prefix("spawned ")?.parse().ok());
    if let Some(pid) = leftover {
        wait_until("the leftover helper ending", || {
            gate_win32::process_start_of(pid).is_ok_and(|start| start.is_none())
        });
    }

    assert!(leftover.is_some(), "the first run's tail names the leftover: {first:?}");
    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(second.code, Some(0), "the leftover's handles do not keep the log: {second:?}");
    let text = log_text(&case);
    assert!(
        text.starts_with("second\ngate: passed in "),
        "the second run's own output and verdict: {text}"
    );
}

/// Runs a command that starts a child in its job, a `fake-tool wait` on a go file no one writes until the case ends,
/// then exits with `status` once the case lets it; the gate's outcome and the child's exit code once it has ended,
/// `None` while it still runs. The child is opened while the command still runs, so its pid is pinned.
fn run_leaving_a_child(case: &Case, status: &str) -> (crate::support::Outcome, Option<u32>) {
    let (go, proceed) = (case.folder().join("go"), case.folder().join("proceed"));
    let (go_text, proceed_text) = (shown(&go), shown(&proceed));
    let command = [FAKE_TOOL, "spawn-wait", &go_text, "wait", &proceed_text, "exit", status];
    let running = case.start(&run_words("out.log", &[], &command));
    wait_until("the child's pid in the log", || {
        fs::read_to_string(log_path(case)).is_ok_and(|text| text.contains("spawned "))
    });
    let pid = said_pid(&log_text(case), "spawned ");
    let child = StoppableProcess::open(pid)
        .expect("the child is opened")
        .expect("the child runs: it waits for a go file no one has written");

    fs::write(&proceed, "").expect("the proceed file can be written");
    let outcome = running.finish();
    let ended = Escapee::exit_code_once_ended(&child);
    // The child is let go before any assertion, so a failing case leaves no helper waiting.
    fs::write(&go, "").expect("the go file can be written");
    (outcome, ended)
}

#[test]
fn a_process_left_in_the_job_ends_with_the_run() {
    let case = Case::new("a_process_left_in_the_job_ends_with_the_run");

    let (outcome, child) = run_leaving_a_child(&case, "0");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(child, Some(STOPPED), "the gate ended the child at the run's end: {outcome:?}");
    assert!(!outcome.stderr.contains("cannot end the run's job"), "{outcome:?}");
}

/// The name of the job the gate `gate` runs its command in under the case's root.
fn job_name(case: &Case, gate: u32) -> String {
    StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::Job { gate_pid: gate })
}

/// The run's end looks at what its command left before the verdict is reached, so all it says comes before the verdict
/// line. A gate in the job is spared, and one whose job cannot be listed, as when an event holds the name its job would
/// have, is said on a `cannot end` line, on stderr and in the log, where this case reads it before the verdict line. The
/// gate is fake-tool copied as `gate.exe`, since the run's end judges a gate by its image name.
#[test]
fn a_spared_gate_whose_job_cannot_be_listed_is_said_before_the_verdict() {
    let case = Case::new("a_spared_gate_whose_job_cannot_be_listed_is_said_before_the_verdict");
    copy_tool(&case, "gate.exe");
    let gate = shown(&case.folder().join("gate.exe"));
    let (go, release) = (Go(case.folder().join("go")), case.folder().join("release"));
    let (go_text, release_text, pid_text) = (shown(go.path()), shown(&release), shown(&case.folder().join("pid")));
    let command = [FAKE_TOOL, "spawn-pid", &pid_text, &release_text, &gate, "wait", &go_text];
    let running = case.start(&run_words("out.log", &[], &command));
    wait_until("the spared gate's pid in the log", || {
        fs::read_to_string(log_path(&case)).is_ok_and(|text| text.contains("spawned "))
    });
    let pid = said_pid(&log_text(&case), "spawned ");
    let spared = StoppableProcess::open(pid)
        .expect("the spared gate is looked for")
        .expect("it runs: it waits for a go file no one has written");
    let _squatter = NamedEvent::create(&job_name(&case, pid)).expect("the event holding its job's name is created");

    fs::write(&release, "").expect("the release file can be written");
    let outcome = running.finish();
    // The spared gate is let go before any assertion, so a failing case leaves no helper waiting.
    drop(go);
    let ended = Escapee::exit_code_once_ended(&spared);

    let cannot = format!("gate: cannot end the run's job: listing the job of gate {pid} failed");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(&cannot), "{outcome:?}");
    assert_in_order(&log_text(&case), &[&cannot, "gate: passed in "]);
    assert_eq!(ended, Some(0), "the spared gate was left running, then ended on its go file: {outcome:?}");
}

/// The verdict's scan of the log sees what a leftover wrote there before its command exited: a leftover's `error CS1`
/// line fails a command that exits 0, and the run's end still ends the leftover. The line is written before the command
/// exits, so this shows the scan reads a leftover's output, not the order of the run's end and the scan.
#[test]
fn a_leftover_s_failure_output_fails_a_passing_command() {
    let case = Case::new("a_leftover_s_failure_output_fails_a_passing_command");
    let (go, release) = (Go(case.folder().join("go")), case.folder().join("release"));
    let (go_text, release_text, pid_text) = (shown(go.path()), shown(&release), shown(&case.folder().join("pid")));
    let command = [
        FAKE_TOOL,
        "spawn-pid",
        &pid_text,
        &release_text,
        FAKE_TOOL,
        "out",
        "error CS1",
        "wait",
        &go_text,
    ];
    let running = case.start(&run_words("out.log", &[], &command));
    wait_until("the leftover's pid and its failure line in the log", || {
        fs::read_to_string(log_path(&case)).is_ok_and(|text| text.contains("spawned ") && text.contains("error CS1"))
    });
    let pid = said_pid(&log_text(&case), "spawned ");
    let leftover = StoppableProcess::open(pid)
        .expect("the leftover is looked for")
        .expect("it runs: it waits for a go file no one has written");

    fs::write(&release, "").expect("the release file can be written");
    let outcome = running.finish();
    // The leftover is let go before any assertion, so a failing case leaves no helper waiting.
    drop(go);
    let ended = Escapee::exit_code_once_ended(&leftover);

    assert_eq!(outcome.code, Some(1), "the leftover's failure line fails the run: {outcome:?}");
    assert!(
        outcome.stderr.contains("gate: command exited 0 but its log reports a failure"),
        "{outcome:?}"
    );
    assert_eq!(ended, Some(STOPPED), "the run's end ended the leftover: {outcome:?}");
}

/// A real gate the command starts through `cmd /c` is spared at the run's end with its launcher, the processes above
/// it in the run's job: the launcher is left to end when the nested gate does, with its status, rather than ended
/// with the run's end's own.
#[test]
fn a_nested_gate_s_launcher_is_not_ended_by_the_run_end() {
    let case = Case::new("a_nested_gate_s_launcher_is_not_ended_by_the_run_end");
    let (started, go) = (case.folder().join("started"), Go(case.folder().join("go")));
    let (release, pid_file) = (case.folder().join("release"), case.folder().join("launcher.pid"));
    let texts = [&started, go.path(), &release, &pid_file].map(shown);
    let mut command = vec![FAKE_TOOL, "spawn-pid", &texts[3], &texts[2], "cmd.exe", "/c", GATE];
    command.extend(none_words("inner.log", &[], &[FAKE_TOOL, "mark", &texts[0], "wait", &texts[1]]));
    let running = case.start(&run_words("out.log", &[], &command));
    wait_for_file(&started);
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("the launcher's pid is written before the nested gate starts")
        .trim()
        .parse()
        .expect("the pid file holds a pid");
    let launcher = StoppableProcess::open(pid)
        .expect("the launcher is looked for")
        .expect("it runs: its nested gate's command waits for a go file no one has written");

    fs::write(&release, "").expect("the release file can be written");
    let outcome = running.finish();
    // The nested gate is let go, and seen to close its log as it ends, before any assertion, so a failing case leaves no
    // helper waiting.
    drop(go);
    let ended = Escapee::exit_code_once_ended(&launcher);
    wait_until_closed(&case.folder().join("inner.log"));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("cannot end the run's job"), "{outcome:?}");
    assert_eq!(
        ended,
        Some(0),
        "the launcher was left running, then ended with its nested gate's status: {outcome:?}"
    );
}

#[test]
fn a_failed_run_ends_what_it_left_in_its_job_and_keeps_the_command_s_status() {
    let case = Case::new("a_failed_run_ends_what_it_left_in_its_job_and_keeps_the_command_s_status");

    let (outcome, child) = run_leaving_a_child(&case, "3");

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert_eq!(child, Some(STOPPED), "the gate ended the child at the run's end: {outcome:?}");
}

#[test]
fn a_log_inside_the_state_root_is_refused_before_queueing() {
    let case = Case::new("a_log_inside_the_state_root_is_refused_before_queueing");
    let inside = shown(&case.root().join("x.log"));
    let upper = shown(&case.folder().join("STATE").join("x.log"));

    for (given, shown_log) in [(inside.as_str(), inside.as_str()), (r"STATE\sub\..\x.log", upper.as_str())] {
        let outcome = case.run(&["run", "--slot", "light", "--timeout", "5", "--log", given, "--", FAKE_TOOL, "exit", "0"]);

        assert_eq!(outcome.code, Some(2), "{given}: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{given}");
        assert_eq!(
            outcome.stderr,
            format!("gate: the log {shown_log} is inside the gate's state folder {}\n", shown(&case.root())),
            "{given}"
        );
        assert_eq!(case.root_entries(), Vec::<String>::new(), "{given}: nothing was journalled or logged");
    }

    // It runs a command that must pass, so its ceiling sits past the suite's hang guard: a short one could end it on a busy PC.
    let beside = case.run(&[
        "run",
        "--slot",
        "light",
        "--timeout",
        "900",
        "--log",
        "stateful.log",
        "--",
        FAKE_TOOL,
        "exit",
        "0",
    ]);

    assert_eq!(
        beside.code,
        Some(0),
        "a name that only starts like the state folder is outside it: {beside:?}"
    );
}

#[test]
fn tail_3_prints_the_last_3_lines_before_the_verdict() {
    let case = Case::new("tail_3_prints_the_last_3_lines_before_the_verdict");

    let outcome = case.run(&run_words("out.log", &["--tail", "3"], &[FAKE_TOOL, "lines", "10", "line", "exit", "0"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert_eq!(lines.len(), 4, "{outcome:?}");
    assert_eq!(lines[..3], ["line8", "line9", "line10"], "{outcome:?}");
    assert!(lines[3].starts_with("gate: passed in "), "{outcome:?}");
}

#[test]
fn a_failed_run_prints_only_the_first_40_marker_lines() {
    let case = Case::new("a_failed_run_prints_only_the_first_40_marker_lines");

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "lines", "45", "error CS", "exit", "3"]));

    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    let mut expected: Vec<String> = (1..=40).map(|number| format!("{number}:error CS{number}")).collect();
    expected.extend((26..=45).map(|number| format!("error CS{number}")));
    assert_eq!(
        outcome.stdout,
        format!("{}\n", expected.join("\n")),
        "the first 40 marker lines, then the default 20-line tail"
    );
}
