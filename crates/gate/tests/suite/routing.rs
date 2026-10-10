//! Routing: which heavy runs the gate sends to the builder, what it says and journals for each decision, the run on
//! the builder through the scripted remote client (`--test-remote-client`), the remote pool and its wait, and the
//! fallback here of a routed run that the pool, the builder or the client turned away.

use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gate_router::{DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE, repo_name};
use serde_json::{Value, json};

use crate::support::{
    Case, FAKE_TOOL, Running, assert_in_order, copy_tool, events, finish, git, git_init, heavy_words, release, run_words, shown,
    wait_for_journal_line, wait_until, write_counts, write_remote_json,
};

const HOST: &str = "builder";
const CLIENT_LINE: &str = "gate: remote builder client could not start: the bridge to remote.ps1 is not built yet; running here";
const BUSY_LINE: &str = "gate: remote builder busy; running here";
const NOTHING_SENT_LINE: &str = "gate: not routed: sub holds nothing the snapshot sends";
const REMOTE_WAITING: &str = "gate: waiting for a remote slot on builder (1 busy)";
/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

static NEXT_STUB: AtomicU64 = AtomicU64::new(0);

/// Writes `steps` as a step file of the scripted remote client in the case folder, one step a line; its path.
fn stub(case: &Case, steps: &[&str]) -> PathBuf {
    let path = case.folder().join(format!("stub-{}.txt", NEXT_STUB.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, steps.join("\n")).expect("the step file can be written");
    path
}

/// The words of a heavy run logging to `log` that calls the scripted client of the step file `stub`, `extra` before
/// `--` and `command` after it, with a routing git limit no machine reaches.
fn routed_words<'a>(log: &'a str, stub: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut before = vec!["--test-remote-client", stub, "--test-git-limit-ms", "3600000"];
    before.extend(extra);
    heavy_words(log, &before, command)
}

/// The words of a heavy run of this suite, with a routing git limit of an hour: a case holds however long git takes
/// on a loaded machine, not only within the gate's default limit.
fn heavy_words_with_limit<'a>(log: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut before = vec!["--test-git-limit-ms", "3600000"];
    before.extend(extra);
    heavy_words(log, &before, command)
}

/// The path of a file `name` in the case folder, as a step's or a command's word.
fn word(case: &Case, name: &str) -> String {
    shown(&case.folder().join(name))
}

/// The call the scripted client recorded at `path`.
fn recorded(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("the client recorded its call")).expect("the record is JSON")
}

/// The words of a recorded call.
fn recorded_words(call: &Value) -> Vec<&str> {
    call["words"]
        .as_array()
        .expect("the call has words")
        .iter()
        .map(|word| word.as_str().expect("a word is text"))
        .collect()
}

/// The journal lines of the gate `pid`.
fn own(case: &Case, pid: u32) -> Vec<Value> {
    events(case).into_iter().filter(|event| event["pid"] == pid).collect()
}

fn has_remote_tag(event: &Value) -> bool {
    event["remote"] == true && event["host"] == HOST && event["key"].is_string()
}

/// Whether the gate `pid` has a ticket in the case root.
fn has_ticket(case: &Case, pid: u32) -> bool {
    fs::read_dir(case.root().join("waiting")).is_ok_and(|entries| {
        entries
            .map(|entry| entry.expect("a ticket entry can be read").file_name().to_string_lossy().into_owned())
            .any(|name| name.starts_with(&format!("{pid}-")))
    })
}

fn remote_claim(case: &Case) -> PathBuf {
    case.root().join("slots").join("gate-remote-slot-0.json")
}

/// Starts a routed run logging to `log` whose client takes the remote slot and holds it until `go` exists, and returns
/// once it has taken it.
fn remote_holder(case: &Case, log: &str, go: &Path) -> Running {
    let steps = stub(case, &[&format!("wait {}", shown(go))]);
    let running = case.start(&routed_words(log, &shown(&steps), &[], &[r".\tool.exe", "out", "hi"]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, running.id()));
    running
}

fn let_go(go: &Path) {
    fs::write(go, "").expect("the go file can be made");
}

/// Asserts one gate's journal lines of a routed run that took a remote slot and fell back for `reason`: `queue`,
/// `take` and `fallback` with the remote tag, then a `take` and a `release` with none.
fn assert_refused(events: &[Value], reason: &str) {
    assert_eq!(names(events), ["queue", "take", "fallback", "take", "release"], "{events:#?}");
    assert!(events[..3].iter().all(has_remote_tag), "{events:#?}");
    assert_eq!(events[2]["reason"], reason, "{events:#?}");
    assert!(events[3..].iter().all(has_no_remote_fields), "{events:#?}");
}

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
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "ran here"])));

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
    let heavy = finish(case.gate(&heavy_words_with_limit("heavy.log", &["--local"], &[r".\tool.exe", "out", "hi"])));
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
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "hi"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_local(&events(&case), None);
    assert!(!case.root().join("remote").exists(), "no remote folder in the state root");
}

#[test]
fn inside_a_git_hook_a_heavy_gate_stays_here_silently() {
    let case = routed_case("routing-hook");
    let mut command = case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "hi"]));
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
    let heavy = finish(case.gate(&heavy_words_with_limit("heavy.log", &[], &[FAKE_TOOL, "out", "hi"])));
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
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[FAKE_TOOL, "out", "ran here"])));

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
    let outcome = finish(case.gate_in("sub", &heavy_words_with_limit(&log, &[], &[r".\tool.exe", "out", "ran here"])));

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
    let mut command = case.gate_in("my dir [1]", &heavy_words_with_limit(&log, &[], &[r".\tool.exe", "out", "ran here"]));
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
    let at_root = finish(case.gate(&heavy_words_with_limit("root.log", &[], &[r".\tool.exe", "out", "hi"])));
    let log = log_path(&case, "sub.log");
    let mut command = case.gate_in("sub", &heavy_words_with_limit(&log, &[], &[r".\tool.exe", "out", "hi"]));
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
    let outcome = finish(case.gate_in("sub", &heavy_words_with_limit(&log, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(NOTHING_SENT_LINE), "{outcome:?}");
    assert_local(&events(&case), None);
}

#[test]
fn a_path_argument_with_spaces_routes_with_the_word_intact() {
    let case = routed_case("routing-spaces");
    let word = r"dir with spaces\file.txt";
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "argv", word])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(CLIENT_LINE), "{outcome:?}");
    assert!(stdout_lines(&outcome.stdout).contains(&word), "{outcome:?}");
    assert_fell_back(&case, &events(&case));
}

#[test]
fn a_glob_argument_reaches_the_box_literally() {
    let case = routed_case("routing-glob");
    let call = case.folder().join("call.json");
    let steps = stub(&case, &[&format!("record {}", shown(&call)), "refuse busy"]);
    let outcome = finish(case.gate(&routed_words("out.log", &shown(&steps), &[], &[r".\tool.exe", "argv", "*.cs"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "{outcome:?}");
    assert_eq!(recorded_words(&recorded(&call))[1..], ["argv", "*.cs"]);
    assert!(stdout_lines(&outcome.stdout).contains(&"*.cs"), "{outcome:?}");
    assert_refused(&events(&case), "busy");
}

#[test]
fn no_build_and_other_flags_that_are_not_paths_are_left_alone() {
    let case = routed_case("routing-flags");
    copy_tool(&case, "dotnet.exe");
    let (other_call, dotnet_call) = (case.folder().join("other.json"), case.folder().join("dotnet.json"));
    let other_steps = shown(&stub(&case, &[&format!("record {}", shown(&other_call)), "refuse busy"]));
    let dotnet_steps = shown(&stub(&case, &[&format!("record {}", shown(&dotnet_call)), "refuse busy"]));
    let flags = ["--configuration=Release", "-p:Foo=1", "--no-build"];
    let mut tool = vec![r".\tool.exe", "argv"];
    tool.extend(flags);
    let other = finish(case.gate(&routed_words("tool.log", &other_steps, &[], &tool)));
    let dotnet_command = [r".\dotnet.exe", "argv", "build", "--no-build"];
    let dotnet = finish(case.gate(&routed_words("dotnet.log", &dotnet_steps, &[], &dotnet_command)));

    assert_eq!((other.code, dotnet.code), (Some(0), Some(0)), "{other:?} {dotnet:?}");
    assert!(
        other.stderr.contains(BUSY_LINE) && !other.stderr.contains(DROPPED_NO_BUILD_LINE),
        "{other:?}"
    );
    assert_eq!(recorded_words(&recorded(&other_call))[2..], flags, "the box gets the flags as given");
    assert_eq!(
        recorded_words(&recorded(&dotnet_call))[1..],
        ["argv", "build"],
        "the box's dotnet drops --no-build"
    );
    assert!(stdout_lines(&other.stdout).windows(3).any(|words| words == flags), "{other:?}");
    assert_in_order(&dotnet.stderr, &[DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE, BUSY_LINE]);
    assert!(
        stdout_lines(&dotnet.stdout).contains(&"--no-build"),
        "a fallback before the builder ran keeps --no-build: {dotnet:?}"
    );
    let events = events(&case);
    assert_refused(&events[..5], "busy");
    assert_refused(&events[5..], "busy");
}

#[test]
fn a_bare_first_word_stays_bare_unless_it_names_a_file_of_the_folder() {
    let case = routed_case("routing-bare");
    let (file_call, missing_call) = (case.folder().join("file.json"), case.folder().join("missing.json"));
    let file_steps = shown(&stub(&case, &[&format!("record {}", shown(&file_call)), "refuse busy"]));
    let missing_steps = shown(&stub(&case, &[&format!("record {}", shown(&missing_call)), "refuse busy"]));
    let file = finish(case.gate(&routed_words("file.log", &file_steps, &[], &["tool.exe", "argv", "here"])));
    let missing = finish(case.gate(&routed_words("missing.log", &missing_steps, &[], &["nonexistent-tool", "argv", "here"])));

    assert!(
        file.stderr.contains(BUSY_LINE),
        "a bare word naming a file of the folder routes: {file:?}"
    );
    assert!(
        missing.stderr.contains(BUSY_LINE),
        "a bare word naming no file routes as given: {missing:?}"
    );
    assert_eq!(recorded_words(&recorded(&file_call)), ["./tool.exe", "argv", "here"]);
    assert_eq!(recorded_words(&recorded(&missing_call)), ["nonexistent-tool", "argv", "here"]);
    assert!(missing.stderr.contains("gate: could not start nonexistent-tool"), "{missing:?}");
    let events = events(&case);
    assert_refused(&events[..5], "busy");
    assert_refused(&events[5..], "busy");
}

#[test]
fn a_routed_run_goes_to_the_client_and_passes_its_output_and_status_through() {
    let case = routed_case("routing-through");
    let steps = shown(&stub(&case, &["out ran there", "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stdout,
        &["ran there", "gate: passed in ", " s on builder (wall clock, ceiling 3600 s)."],
    );
    assert!(!outcome.stdout.contains("ran here"), "the command never ran here: {outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.contains("ran there") && !log.contains("ran here"), "{log}");
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
    let slot = events[1]["slots"][0].as_str().expect("the take names its slot");
    assert!(slot.ends_with("remote-slot-0"), "{slot}");
    assert_eq!((&events[2]["verdict"], &events[2]["adjustedSeconds"]), (&json!("passed"), &Value::Null));
    assert!(!remote_claim(&case).exists(), "the remote claim is gone after the release");
}

#[test]
fn the_client_is_called_with_the_rewritten_words_and_the_route() {
    let case = routed_case("routing-call");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "argv", "a"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let call = recorded(&call);
    let queue = &events(&case)[0];
    assert_eq!((&call["host"], &call["key"]), (&json!(HOST), &queue["key"]), "{call}");
    assert_eq!((&call["repo"], &call["subfolder"]), (&json!(repo(&case)), &json!("")), "{call}");
    assert!(call["root"].as_str().is_some_and(|root| !root.is_empty()), "{call}");
    let words = recorded_words(&call);
    assert!(words[0].ends_with("tool.exe") && words[1..] == ["argv", "a"], "{call}");
    assert_eq!(call["outputs"], json!(["**/bin/**", "**/.godot/mono/temp/bin/**"]), "{call}");
    assert_eq!(call["log"], word(&case, "out.log"), "{call}");
    assert_eq!(
        (&call["timeout"], &call["stall"], &call["tail"], &call["noMarkers"]),
        (&json!(3600), &json!(120), &json!(20), &json!(false))
    );
}

#[test]
fn a_failing_client_status_is_the_gates_and_the_log_is_scanned() {
    let case = routed_case("routing-failing");
    let failing = shown(&stub(&case, &["err error CS1002: x", "status 1"]));
    let unscanned = shown(&stub(&case, &["out error CS1002: x", "status 0"]));
    let failed = finish(case.gate(&routed_words("failed.log", &failing, &[], &[r".\tool.exe", "out", "hi"])));
    let passed = finish(case.gate(&routed_words("passed.log", &unscanned, &["--no-markers"], &[r".\tool.exe", "out", "hi"])));

    assert_eq!((failed.code, passed.code), (Some(1), Some(0)), "{failed:?} {passed:?}");
    assert!(failed.stdout.contains(":error CS1002: x"), "the marker line is said: {failed:?}");
    assert!(failed.stderr.contains("gate: FAILED (status 1)"), "{failed:?}");
    assert!(passed.stdout.contains(" s on builder (wall clock"), "{passed:?}");
    let releases: Vec<Value> = events(&case).into_iter().filter(|event| event["event"] == "release").collect();
    assert_eq!((&releases[0]["verdict"], &releases[1]["verdict"]), (&json!("failed"), &json!("passed")));
}

#[test]
fn a_75_without_a_reason_is_the_commands_own_status() {
    let case = routed_case("routing-75");
    let steps = shown(&stub(&case, &["status 75"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "the command never ran here: {outcome:?}");
    assert_eq!(names(&events(&case)), ["queue", "take", "release"]);
}

#[test]
fn a_killed_client_run_is_a_kill_and_a_124_of_its_own_is_not() {
    let case = routed_case("routing-killed");
    let stalled = "err gate: STALLED: no output and no CPU for 120 s";
    let killed = shown(&stub(&case, &["killed", stalled, "status 124"]));
    let own = shown(&stub(&case, &[stalled, "status 124"]));
    let first = finish(case.gate(&routed_words("killed.log", &killed, &[], &[r".\tool.exe", "out", "hi"])));
    let second = finish(case.gate(&routed_words("own.log", &own, &[], &[r".\tool.exe", "out", "hi"])));

    assert_eq!((first.code, second.code), (Some(124), Some(124)), "{first:?} {second:?}");
    let releases: Vec<Value> = events(&case).into_iter().filter(|event| event["event"] == "release").collect();
    assert_eq!((&releases[0]["verdict"], &releases[1]["verdict"]), (&json!("stalled"), &json!("failed")));
}

#[test]
fn a_fallback_that_never_starts_the_command_keeps_the_attempts_lines_once() {
    let case = routed_case("routing-fallback-no-start");
    let steps = shown(&stub(&case, &["out attempt line", "refuse busy"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &["nonexistent-tool", "argv"])));

    assert_eq!(outcome.code, Some(127), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: could not start nonexistent-tool"), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_eq!(log.matches("attempt line").count(), 1, "the attempt's line stands once: {log}");
    assert_refused(&events(&case), "busy");
}

#[test]
fn a_kill_line_after_a_lone_cr_progress_line_still_reads_as_the_kill() {
    let case = routed_case("routing-kill-after-cr");
    let steps = shown(&stub(&case, &["killed", "err 50%\rgate: TIMED OUT after 5 s", "status 124"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "hi"])));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let released = release(&case);
    assert_eq!(released["verdict"], "timed-out", "{released:#?}");
}

#[test]
fn a_refused_run_falls_back_to_a_local_run_with_the_attempts_lines_in_the_log() {
    let case = routed_case("routing-refused");
    let go = case.folder().join("go");
    let steps = shown(&stub(&case, &["out attempt line", "refuse unreachable"]));
    let go_word = shown(&go);
    let running = case.start(&routed_words(
        "out.log",
        &steps,
        &[],
        &[r".\tool.exe", "out", "ran here", "wait", &go_word],
    ));
    let log = case.folder().join("out.log");
    wait_until("the local command writing its log", || {
        fs::read_to_string(&log).is_ok_and(|text| text.starts_with("ran here"))
    });

    assert!(!remote_claim(&case).exists(), "the remote claim is gone once the run fell back");
    assert!(case.root().join("slots").join("gate-slot-0.json").exists(), "the heavy claim is written");
    let_go(&go);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = "gate: remote builder unreachable; running here";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_in_order(
        &fs::read_to_string(&log).expect("the log was written"),
        &["ran here", "attempt line", line],
    );
    assert_refused(&events(&case), "unreachable");
}

#[test]
fn every_refusal_reason_names_itself_in_the_line_and_the_journal() {
    let case = routed_case("routing-reasons");
    let reasons = ["busy", "unreachable", "parity", "tools", "rebooted"];
    for reason in reasons {
        let steps = shown(&stub(&case, &[&format!("refuse {reason}")]));
        let log = format!("{reason}.log");
        let outcome = finish(case.gate(&routed_words(&log, &steps, &[], &[r".\tool.exe", "out", "hi"])));
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        assert!(
            outcome.stderr.contains(&format!("gate: remote builder {reason}; running here")),
            "{outcome:?}"
        );
    }
    let events = events(&case);
    for (index, reason) in reasons.iter().enumerate() {
        assert_refused(&events[index * 5..index * 5 + 5], reason);
    }
}

#[test]
fn no_build_is_dropped_for_the_box_and_kept_for_a_fallback_unless_the_box_ran_it() {
    let case = routed_case("routing-no-build");
    copy_tool(&case, "dotnet.exe");
    let (busy_call, tool_call) = (case.folder().join("busy.json"), case.folder().join("tool.json"));
    let busy = shown(&stub(&case, &[&format!("record {}", shown(&busy_call)), "refuse busy"]));
    let rebooted = shown(&stub(&case, &["refuse rebooted"]));
    let tool = shown(&stub(&case, &[&format!("record {}", shown(&tool_call)), "status 0"]));
    let dotnet = [r".\dotnet.exe", "argv", "build", "--no-build"];
    let before = finish(case.gate(&routed_words("busy.log", &busy, &[], &dotnet)));
    let after = finish(case.gate(&routed_words("rebooted.log", &rebooted, &[], &dotnet)));
    let other = finish(case.gate(&routed_words("tool.log", &tool, &[], &[r".\tool.exe", "argv", "--no-build"])));

    assert_eq!((before.code, after.code, other.code), (Some(0), Some(0), Some(0)));
    let call = recorded(&busy_call);
    assert!(!recorded_words(&call).contains(&"--no-build"), "{call}");
    assert_eq!(call["outputs"], json!([]), "{call}");
    assert_in_order(&before.stderr, &[DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE, BUSY_LINE]);
    assert!(stdout_lines(&before.stdout).contains(&"--no-build"), "{before:?}");
    let after_lines = stdout_lines(&after.stdout);
    assert!(after_lines.contains(&"build") && !after_lines.contains(&"--no-build"), "{after:?}");
    assert!(
        recorded_words(&recorded(&tool_call)).contains(&"--no-build"),
        "a non-dotnet command keeps it"
    );
}

#[test]
fn a_client_that_cannot_start_falls_back_with_reason_client() {
    let case = routed_case("routing-fail-start");
    let steps = shown(&stub(&case, &["fail-start no pwsh"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("gate: remote builder client could not start: no pwsh; running here"),
        "{outcome:?}"
    );
    assert_refused(&events(&case), "client");
}

#[test]
fn a_full_remote_pool_falls_back_with_reason_full() {
    let case = routed_case("routing-full");
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let waiter = case.start(&routed_words(
        "waiter.log",
        &steps,
        &["--test-remote-wait-ms", "1"],
        &[r".\tool.exe", "out", "ran here"],
    ));
    let pid = waiter.id();
    let outcome = waiter.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[REMOTE_WAITING, "gate: remote builder full; running here"]);
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "fallback", "take", "release"], "{own:#?}");
    assert!(own[..2].iter().all(has_remote_tag), "{own:#?}");
    assert_eq!(own[1]["reason"], "full");
    assert!(own[2..].iter().all(has_no_remote_fields), "{own:#?}");
    assert!(!call.exists(), "the waiter's client never ran");
    assert!(remote_claim(&case).exists(), "the holder's claim is intact");
    assert!(!has_ticket(&case, pid), "the waiter's ticket is gone");
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));
}

#[test]
fn a_waiter_takes_the_remote_slot_when_the_holder_releases() {
    let case = routed_case("routing-waiter");
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let steps = shown(&stub(&case, &["status 0"]));
    let seams = ["--test-remote-wait-ms", "3600000", "--test-heartbeat-ms", "50", "--test-poll-ms", "50"];
    let waiter = case.start(&routed_words("waiter.log", &steps, &seams, &[r".\tool.exe", "out", "hi"]));
    let pid = waiter.id();
    waiter.wait_for_stderr(REMOTE_WAITING);
    waiter.wait_for_stderr("gate: still waiting for a remote slot on builder (");
    let_go(&go);

    assert_eq!(holder.finish().code, Some(0));
    let outcome = waiter.finish();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "{own:#?}");
    assert!(own.iter().all(has_remote_tag), "{own:#?}");
    assert!(
        own[1]["slots"][0].as_str().is_some_and(|slot| slot.ends_with("remote-slot-0")),
        "{own:#?}"
    );
}

#[test]
fn remote_and_local_waiters_do_not_queue_behind_each_other() {
    let case = routed_case("routing-two-queues");
    write_counts(&case.root(), r#"{"v":1,"heavy":1}"#);
    let local_go = case.folder().join("local-go");
    let local_go_word = shown(&local_go);
    let local_holder = case.start(&heavy_words_with_limit("lh.log", &["--local"], &[r".\tool.exe", "wait", &local_go_word]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, local_holder.id()));
    let mut local_waiter = case.start(&heavy_words_with_limit("lw.log", &["--local"], &[r".\tool.exe", "out", "hi"]));
    local_waiter.wait_for_stderr("gate: waiting for a heavy slot");
    let free = shown(&stub(&case, &["status 0"]));
    let routed = finish(case.gate(&routed_words("routed.log", &free, &[], &[r".\tool.exe", "out", "hi"])));
    assert_eq!(
        routed.code,
        Some(0),
        "a local waiter ahead does not hold back a free remote slot: {routed:?}"
    );
    assert!(local_waiter.is_running(), "the local waiter still waits");
    let_go(&local_go);
    assert_eq!((local_holder.finish().code, local_waiter.finish().code), (Some(0), Some(0)));

    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let waits = ["--test-remote-wait-ms", "3600000", "--test-poll-ms", "50"];
    let mut remote_waiter = case.start(&routed_words("rw.log", &free, &waits, &[r".\tool.exe", "out", "hi"]));
    remote_waiter.wait_for_stderr(REMOTE_WAITING);
    let local = finish(case.gate(&heavy_words_with_limit("local.log", &["--local"], &[r".\tool.exe", "out", "hi"])));
    assert_eq!(
        local.code,
        Some(0),
        "a remote waiter ahead does not hold back a free heavy slot: {local:?}"
    );
    assert!(remote_waiter.is_running(), "the remote waiter still waits");
    let_go(&go);
    assert_eq!((holder.finish().code, remote_waiter.finish().code), (Some(0), Some(0)));
}

#[test]
fn a_ctrl_c_while_waiting_for_a_remote_slot_abandons_with_the_remote_fields() {
    let case = routed_case("routing-ctrl-c-waiting");
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let steps = shown(&stub(&case, &["status 0"]));
    let waiter = case.start(&routed_words(
        "waiter.log",
        &steps,
        &["--test-interrupt-queued"],
        &[r".\tool.exe", "out", "hi"],
    ));
    let pid = waiter.id();
    let outcome = waiter.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[REMOTE_WAITING, "gate: interrupted while waiting for a remote slot"]);
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "abandon"], "{own:#?}");
    assert!(own.iter().all(has_remote_tag), "{own:#?}");
    assert!(!has_ticket(&case, pid), "the waiter's ticket is gone");
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));
}

#[test]
fn a_ctrl_c_during_the_client_run_ends_the_gate_and_frees_the_remote_slot() {
    let case = routed_case("routing-ctrl-c-client");
    let never = word(&case, "never");
    let steps = shown(&stub(&case, &["interrupt", &format!("wait {never}")]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "hi"])));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
    assert!(!remote_claim(&case).exists(), "the remote claim is gone");
    let next = shown(&stub(&case, &["status 0"]));
    let after = finish(case.gate(&routed_words("next.log", &next, &[], &[r".\tool.exe", "out", "hi"])));
    assert_eq!(after.code, Some(0), "{after:?}");
    let take = &events_of_last(&case)[1];
    assert!(take["slots"][0].as_str().is_some_and(|slot| slot.ends_with("remote-slot-0")), "{take}");
}

#[test]
fn a_ctrl_c_during_the_client_call_wins_over_a_refusal() {
    let case = routed_case("routing-ctrl-c-refusal");
    let steps = shown(&stub(&case, &["interrupt", "refuse busy"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "the command never ran here: {outcome:?}");
    assert!(!outcome.stderr.contains(BUSY_LINE), "the refusal never fell back: {outcome:?}");
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
    assert!(!remote_claim(&case).exists(), "the remote claim is gone after the release");
}

#[test]
fn a_ctrl_c_in_the_local_wait_after_a_fallback_abandons_without_the_remote_fields() {
    let case = routed_case("routing-ctrl-c-fallback");
    write_counts(&case.root(), r#"{"v":1,"heavy":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let holder = case.start(&heavy_words_with_limit("holder.log", &["--local"], &[r".\tool.exe", "wait", &go_word]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));
    let steps = shown(&stub(&case, &["refuse busy"]));
    let gate = case.start(&routed_words(
        "out.log",
        &steps,
        &["--test-interrupt-queued"],
        &[r".\tool.exe", "out", "hi"],
    ));
    let pid = gate.id();
    let outcome = gate.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[BUSY_LINE, "gate: interrupted while waiting for a heavy slot"]);
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "fallback", "abandon"], "{own:#?}");
    assert!(own[..3].iter().all(has_remote_tag), "{own:#?}");
    assert!(has_no_remote_fields(&own[3]), "{own:#?}");
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));
}

/// The journal lines of the gate that wrote the journal's last line.
fn events_of_last(case: &Case) -> Vec<Value> {
    let events = events(case);
    let pid = events.last().expect("the journal has lines")["pid"].clone();
    events.into_iter().filter(|event| event["pid"] == pid).collect()
}

#[test]
fn a_word_that_is_not_valid_unicode_runs_here_saying_so() {
    let case = routed_case("routing-not-unicode");
    let mut command = case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "argv"]));
    command.arg(OsString::from_wide(&[u16::from(b'x'), 0xD800]));
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: not routed: a word of the command is not valid Unicode\n"),
        "{outcome:?}"
    );
    assert_local(&events(&case), None);
}

#[test]
fn a_git_that_does_not_answer_within_the_limit_runs_here_saying_so() {
    let case = routed_case("routing-git-late");
    let words = heavy_words("out.log", &["--test-git-limit-ms", "1"], &[r".\tool.exe", "out", "ran here"]);
    let outcome = finish(case.gate(&words));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: not routed: git did not answer within 1 ms\n"),
        "{outcome:?}"
    );
    assert!(outcome.stdout.contains("ran here"), "the command ran here: {outcome:?}");
    assert_local(&events(&case), None);
}
