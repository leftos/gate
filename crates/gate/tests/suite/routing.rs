//! Routing: which heavy runs the gate sends to the builder, what it says and journals for each decision, and the
//! fallback here of a routed run while no remote client is built. With no client the builder's words are not seen, so
//! a routed case shows the words through the fallback's own run of fake-tool `argv`.

use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStringExt;

use gate_router::{DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE, repo_name};
use serde_json::{Value, json};

use crate::support::{Case, FAKE_TOOL, assert_in_order, copy_tool, events, finish, git, git_init, heavy_words, run_words, write_remote_json};

const HOST: &str = "builder";
const CLIENT_LINE: &str = "gate: remote builder client could not start: the bridge to remote.ps1 is not built yet; running here";
const NOTHING_SENT_LINE: &str = "gate: not routed: sub holds nothing the snapshot sends";

/// A case folder made a checkout of its own, holding fake-tool as `tool.exe`, with a routing file that sends its
/// `*tool*` and `*dotnet.exe*` commands to the builder.
fn routed_case(name: &str) -> Case {
    let case = Case::new(name);
    git_init(&case);
    copy_tool(&case, "tool.exe");
    let routing = json!({ "host": HOST, "repos": { repo(&case): ["*tool*", "*dotnet.exe*"] } });
    write_remote_json(&case, &routing.to_string());
    case
}

/// The repo name of the case folder's checkout.
fn repo(case: &Case) -> String {
    repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy())
}

/// The case's folder `name` holding fake-tool as `tool.exe`, which `.gitignore` leaves out with the logs, and, when
/// `with_file`, a text file the snapshot sends.
fn sub_folder(case: &Case, name: &str, with_file: bool) {
    fs::write(case.folder().join(".gitignore"), "*.exe\n*.log\n").expect("the ignore file can be written");
    let folder = case.folder().join(name);
    fs::create_dir_all(&folder).expect("the sub folder can be made");
    copy_tool(case, &format!(r"{name}\tool.exe"));
    if with_file {
        fs::write(folder.join("a.txt"), "a\n").expect("a file can be written in the sub folder");
    }
}

fn log_path(case: &Case, name: &str) -> String {
    case.folder().join(name).to_str().expect("the case folder is Unicode").to_owned()
}

fn names(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["event"].as_str().expect("a line names its event"))
        .collect()
}

fn has_no_remote_fields(event: &Value) -> bool {
    ["remote", "host", "key"].iter().all(|field| event.get(field).is_none())
}

/// Asserts one gate's journal lines of a routed run that fell back at once: `queue` and `fallback` (reason `client`)
/// with one remote tag, then a `take` and a `release` with none.
fn assert_fell_back(case: &Case, events: &[Value]) {
    assert_eq!(names(events), ["queue", "fallback", "take", "release"], "{events:#?}");
    let (queue, fallback) = (&events[0], &events[1]);
    let key = queue["key"].as_str().expect("the queue line has a key");
    assert!(key.starts_with(&format!("{}-", repo(case))), "the key names the repo: {key}");
    assert_eq!((&queue["remote"], &queue["host"]), (&json!(true), &json!(HOST)), "{queue}");
    assert!(queue.get("notRouted").is_none(), "{queue}");
    assert_eq!(fallback["reason"], "client", "{fallback}");
    assert_eq!(
        (&fallback["remote"], &fallback["host"], &fallback["key"]),
        (&json!(true), &json!(HOST), &json!(key))
    );
    assert!(events[2..].iter().all(has_no_remote_fields), "{events:#?}");
}

/// Asserts one gate's journal lines of a run kept here: `queue`, `take` and `release`, none with a remote field, the
/// `queue` line's `notRouted` as given.
fn assert_local(events: &[Value], not_routed: Option<&str>) {
    assert_eq!(names(events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_no_remote_fields), "{events:#?}");
    assert_eq!(events[0].get("notRouted").and_then(Value::as_str), not_routed, "{}", events[0]);
}

fn stdout_lines(stdout: &str) -> Vec<&str> {
    stdout.lines().collect()
}

#[test]
fn with_no_test_client_a_routed_run_falls_back_with_reason_client() {
    let case = routed_case("routing-no-client");
    let outcome = finish(case.gate(&heavy_words("out.log", &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(CLIENT_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_fell_back(&case, &events(&case));
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_in_order(&log, &["ran here", CLIENT_LINE]);
}

#[test]
fn local_runs_here_saying_why_and_journals_not_routed() {
    let case = routed_case("routing-local");
    let heavy = finish(case.gate(&heavy_words("heavy.log", &["--local"], &[r".\tool.exe", "out", "hi"])));
    let light = finish(case.gate(&run_words("light.log", &["--local"], &[r".\tool.exe", "out", "hi"])));

    assert_eq!((heavy.code, light.code), (Some(0), Some(0)), "{heavy:?} {light:?}");
    assert!(heavy.stderr.contains("gate: not routed: --local\n"), "{heavy:?}");
    assert!(!light.stderr.contains("not routed"), "a light gate says nothing: {light:?}");
    let events = events(&case);
    assert_local(&events[..3], Some("--local"));
    assert_local(&events[3..], None);
    let log = fs::read_to_string(case.folder().join("heavy.log")).expect("the log was written");
    assert!(log.contains("gate: not routed: --local"), "{log}");
}

#[test]
fn without_a_routing_file_nothing_is_said_and_nothing_is_written() {
    let case = Case::new("routing-no-file");
    git_init(&case);
    copy_tool(&case, "tool.exe");
    let outcome = finish(case.gate(&heavy_words("out.log", &[], &[r".\tool.exe", "out", "hi"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_local(&events(&case), None);
    assert!(!case.root().join("remote").exists(), "no remote folder in the state root");
}

#[test]
fn inside_a_git_hook_a_heavy_gate_stays_here_silently() {
    let case = routed_case("routing-hook");
    let mut command = case.gate(&heavy_words("out.log", &[], &[r".\tool.exe", "out", "hi"]));
    command.env("GIT_INDEX_FILE", "x");
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_local(&events(&case), None);
}

#[test]
fn an_unusable_routing_file_is_ignored_with_one_line() {
    let case = Case::new("routing-unusable");
    let path = write_remote_json(&case, "{ not json");
    let heavy = finish(case.gate(&heavy_words("heavy.log", &[], &[FAKE_TOOL, "out", "hi"])));
    let light = finish(case.gate(&run_words("light.log", &[], &[FAKE_TOOL, "out", "hi"])));

    assert_eq!((heavy.code, light.code), (Some(0), Some(0)), "{heavy:?} {light:?}");
    let said: Vec<&str> = heavy.stderr.lines().filter(|line| line.contains("ignored")).collect();
    assert_eq!(said.len(), 1, "one line: {heavy:?}");
    assert!(said[0].starts_with(&format!("gate: ignored {}: ", path.display())), "{heavy:?}");
    assert!(said[0].ends_with("; running locally"), "{heavy:?}");
    let log = fs::read_to_string(case.folder().join("heavy.log")).expect("the log was written");
    assert!(log.contains(said[0]), "{log}");
    assert!(!light.stderr.contains("ignored"), "a light gate reads no routing file: {light:?}");
}

#[test]
fn a_matched_command_outside_the_worktree_runs_here_saying_so() {
    let case = routed_case("routing-outside");
    let outcome = finish(case.gate(&heavy_words("out.log", &[], &[FAKE_TOOL, "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains(&format!("gate: not routed: {FAKE_TOOL} is outside the worktree")),
        "{outcome:?}"
    );
    assert_local(&events(&case), None);
}

#[test]
fn a_folder_that_holds_nothing_the_snapshot_sends_runs_here_saying_so() {
    let case = routed_case("routing-nothing-sent");
    sub_folder(&case, "sub", false);
    let log = log_path(&case, "out.log");
    let outcome = finish(case.gate_in("sub", &heavy_words(&log, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(NOTHING_SENT_LINE), "{outcome:?}");
    assert_local(&events(&case), None);
}

#[test]
fn a_folder_named_with_spaces_and_brackets_holding_a_file_routes() {
    let case = routed_case("routing-brackets");
    sub_folder(&case, "my dir [1]", true);
    let empty = case.folder().join("empty.gitconfig");
    fs::write(&empty, "").expect("an empty git config can be written");
    let log = log_path(&case, "out.log");
    let mut command = case.gate_in("my dir [1]", &heavy_words(&log, &[], &[r".\tool.exe", "out", "ran here"]));
    command.env("GIT_CONFIG_GLOBAL", &empty).env("GIT_CONFIG_NOSYSTEM", "1");
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(CLIENT_LINE), "the folder's name is a literal path: {outcome:?}");
    assert_fell_back(&case, &events(&case));
}

#[test]
fn with_git_dir_and_work_tree_inherited_a_gate_in_a_subfolder_still_routes() {
    let case = routed_case("routing-git-dir");
    sub_folder(&case, "sub", true);
    let at_root = finish(case.gate(&heavy_words("root.log", &[], &[r".\tool.exe", "out", "hi"])));
    let log = log_path(&case, "sub.log");
    let mut command = case.gate_in("sub", &heavy_words(&log, &[], &[r".\tool.exe", "out", "hi"]));
    // As a hook run at the worktree's root exports them: relative, so they name the case's repo only from its root.
    command.env("GIT_DIR", ".git").env("GIT_WORK_TREE", ".");
    let in_sub = finish(command);

    assert_eq!((at_root.code, in_sub.code), (Some(0), Some(0)), "{at_root:?} {in_sub:?}");
    assert!(in_sub.stderr.contains(CLIENT_LINE), "{in_sub:?}");
    let events = events(&case);
    assert_fell_back(&case, &events[..4]);
    assert_fell_back(&case, &events[4..]);
    assert_eq!(events[4]["key"], events[0]["key"], "the subfolder's gate is placed in the same worktree");
}

#[test]
fn a_worktree_with_unstaged_deletes_does_not_count_a_deleted_folder_as_holding_files() {
    let case = routed_case("routing-unstaged-delete");
    sub_folder(&case, "sub", true);
    git(&case, &["add", "sub/a.txt"]);
    git(&case, &["commit", "--quiet", "-m", "a"]);
    fs::remove_file(case.folder().join("sub").join("a.txt")).expect("the tracked file can be deleted");
    let log = log_path(&case, "out.log");
    let outcome = finish(case.gate_in("sub", &heavy_words(&log, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(NOTHING_SENT_LINE), "{outcome:?}");
    assert_local(&events(&case), None);
}

#[test]
fn a_path_argument_with_spaces_routes_with_the_word_intact() {
    let case = routed_case("routing-spaces");
    let word = r"dir with spaces\file.txt";
    let outcome = finish(case.gate(&heavy_words("out.log", &[], &[r".\tool.exe", "argv", word])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(CLIENT_LINE), "{outcome:?}");
    assert!(stdout_lines(&outcome.stdout).contains(&word), "{outcome:?}");
    assert_fell_back(&case, &events(&case));
}

#[test]
fn a_glob_argument_reaches_the_box_literally() {
    let case = routed_case("routing-glob");
    let outcome = finish(case.gate(&heavy_words("out.log", &[], &[r".\tool.exe", "argv", "*.cs"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(CLIENT_LINE), "{outcome:?}");
    assert!(stdout_lines(&outcome.stdout).contains(&"*.cs"), "{outcome:?}");
    assert_fell_back(&case, &events(&case));
}

#[test]
fn no_build_and_other_flags_that_are_not_paths_are_left_alone() {
    let case = routed_case("routing-flags");
    copy_tool(&case, "dotnet.exe");
    let flags = ["--configuration=Release", "-p:Foo=1", "--no-build"];
    let mut tool = vec![r".\tool.exe", "argv"];
    tool.extend(flags);
    let other = finish(case.gate(&heavy_words("tool.log", &[], &tool)));
    let dotnet = finish(case.gate(&heavy_words("dotnet.log", &[], &[r".\dotnet.exe", "argv", "build", "--no-build"])));

    assert_eq!((other.code, dotnet.code), (Some(0), Some(0)), "{other:?} {dotnet:?}");
    assert!(
        other.stderr.contains(CLIENT_LINE) && !other.stderr.contains(DROPPED_NO_BUILD_LINE),
        "{other:?}"
    );
    assert!(stdout_lines(&other.stdout).windows(3).any(|words| words == flags), "{other:?}");
    assert_in_order(&dotnet.stderr, &[DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE, CLIENT_LINE]);
    assert!(
        stdout_lines(&dotnet.stdout).contains(&"--no-build"),
        "a fallback before the builder ran keeps --no-build: {dotnet:?}"
    );
    let events = events(&case);
    assert_fell_back(&case, &events[..4]);
    assert_fell_back(&case, &events[4..]);
}

#[test]
fn a_bare_first_word_stays_bare_unless_it_names_a_file_of_the_folder() {
    let case = routed_case("routing-bare");
    let file = finish(case.gate(&heavy_words("file.log", &[], &["tool.exe", "argv", "here"])));
    let missing = finish(case.gate(&heavy_words("missing.log", &[], &["nonexistent-tool", "argv", "here"])));

    assert!(
        file.stderr.contains(CLIENT_LINE),
        "a bare word naming a file of the folder routes: {file:?}"
    );
    assert!(
        missing.stderr.contains(CLIENT_LINE),
        "a bare word naming no file routes as given: {missing:?}"
    );
    assert!(missing.stderr.contains("gate: could not start nonexistent-tool"), "{missing:?}");
    let events = events(&case);
    assert_fell_back(&case, &events[..4]);
    assert_fell_back(&case, &events[4..]);
}

#[test]
fn a_word_that_is_not_valid_unicode_runs_here_saying_so() {
    let case = routed_case("routing-not-unicode");
    let mut command = case.gate(&heavy_words("out.log", &[], &[r".\tool.exe", "argv"]));
    command.arg(OsString::from_wide(&[u16::from(b'x'), 0xD800]));
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: not routed: a word of the command is not valid Unicode\n"),
        "{outcome:?}"
    );
    assert_local(&events(&case), None);
}
