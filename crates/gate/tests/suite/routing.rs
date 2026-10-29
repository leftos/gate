//! Routing: which heavy runs the gate sends to the builder, what it says and journals for each decision, the run on
//! the builder through the scripted remote client (`--test-remote-client`), the remote pool and its wait, and the
//! fallback here of a routed run that the pool, the builder or the client turned away.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gate_router::{DROPPED_NO_BUILD_LINE, DROPPED_UNSENT_LINE, NO_OUTPUTS_LINE, repo_name, work_folder_key};
use gate_state::{KernelObject, StateRoot, key_hash};
use gate_win32::NamedMutex;
use serde_json::{Value, json};

use crate::nesting::{LOOK_OFTEN, end_parent, first_event, marked, orphaned_as, spawned, test_marker};

use crate::remote_client::{Tuned, client_case, client_gate, client_gate_tuned, fallbacks};
use crate::support::{
    Case, DenyEntry, FAKE_TOOL, GATE, Go, Junction, LOCAL_REASON, Outcome, Running, UNTIL_KILLED, assert_in_order, beside, calls, copy_tool,
    critical_words, events, finish, git, git_init, heavy_words, log_texts, nothing_back, powershell_folder, record_path, release, run_words, shown,
    ssh_steps, start, wait_for_journal_line, wait_until, write_counts, write_remote_json,
};

pub(crate) const HOST: &str = "builder";
pub(crate) const BUSY_LINE: &str = "gate: remote builder busy; running here";
/// The line a routed run says as it falls back here from a full remote pool.
const FULL_LINE: &str = "gate: remote builder full; running here";
/// The first still-waiting line a local heavy wait says, which counts from the moment the wait counts from.
const STILL_WAITING_HEAVY: &str = "gate: still waiting for a heavy slot (";
const NOTHING_SENT_LINE: &str = "gate: not routed: sub holds nothing the snapshot sends";
const REMOTE_WAITING: &str = "gate: waiting for a remote slot on builder (1 busy)";
/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code.
pub(crate) const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

static NEXT_STUB: AtomicU64 = AtomicU64::new(0);

/// Writes `steps` as a step file of the scripted remote client in the case folder, one step a line; its path.
pub(crate) fn stub(case: &Case, steps: &[&str]) -> PathBuf {
    let path = case.folder().join(format!("stub-{}.txt", NEXT_STUB.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, steps.join("\n")).expect("the step file can be written");
    path
}

/// The words of a heavy run logging to `log` that calls the scripted client of the step file `stub`, `extra` before
/// `--` and `command` after it, with a routing git limit and drain no machine reaches.
pub(crate) fn routed_words<'a>(log: &'a str, stub: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut before = vec!["--test-remote-client", stub];
    before.extend(GIT_HANG_GUARDS);
    before.extend(extra);
    heavy_words(log, &before, command)
}

/// The routing git limit of an hour and the drain of the suite's 600 s hang guard: a starved gate whose git readers
/// reach their pipes' ends late still routes, so a case holds however long git takes on a loaded machine.
pub(crate) const GIT_HANG_GUARDS: [&str; 4] = ["--test-git-limit-ms", "3600000", "--test-git-drain-ms", "600000"];

/// The words of a heavy run of this suite, with [`GIT_HANG_GUARDS`] for the routing decision's git calls.
pub(crate) fn heavy_words_with_limit<'a>(log: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut before = GIT_HANG_GUARDS.to_vec();
    before.extend(extra);
    heavy_words(log, &before, command)
}

/// The path of a file `name` in the case folder, as a step's or a command's word.
fn word(case: &Case, name: &str) -> String {
    shown(&case.folder().join(name))
}

/// The path of a file `name` in the case folder, spelled with a `.` segment before its file name: a form of a path
/// outside another worktree that the rewrite judges outside and never reads as a program's name.
fn dotted_word(case: &Case, name: &str) -> String {
    format!(r"{}\.\{name}", shown(&case.folder()))
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
pub(crate) fn own(case: &Case, pid: u32) -> Vec<Value> {
    events(case).into_iter().filter(|event| event["pid"] == pid).collect()
}

pub(crate) fn has_remote_tag(event: &Value) -> bool {
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

pub(crate) fn remote_claim(case: &Case) -> PathBuf {
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
pub(crate) fn assert_refused(events: &[Value], reason: &str) {
    assert_eq!(names(events), ["queue", "take", "fallback", "take", "release"], "{events:#?}");
    assert!(events[..3].iter().all(has_remote_tag), "{events:#?}");
    assert_eq!(events[2]["reason"], reason, "{events:#?}");
    assert!(events[3..].iter().all(has_no_remote_fields), "{events:#?}");
}

/// A case folder made a checkout of its own, holding fake-tool as `tool.exe`, with a routing file that sends its
/// `*tool*`, `*dotnet.exe*` and `*dotnet.cmd*` commands to the builder.
pub(crate) fn routed_case(name: &str) -> Case {
    let case = Case::new(name);
    git_init(&case);
    copy_tool(&case, "tool.exe");
    let routing = json!({ "host": HOST, "repos": { repo(&case): ["*tool*", "*dotnet.exe*", "*dotnet.cmd*"] } });
    write_remote_json(&case, &routing.to_string());
    case
}

/// The repo name of the case folder's checkout.
pub(crate) fn repo(case: &Case) -> String {
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

pub(crate) fn names(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["event"].as_str().expect("a line names its event"))
        .collect()
}

pub(crate) fn has_no_remote_fields(event: &Value) -> bool {
    ["remote", "host", "key"].iter().all(|field| event.get(field).is_none())
}

/// Asserts one gate's journal lines of a routed run that fell back at once: `queue` and `fallback` (reason `client`)
/// with one remote tag, then a `take` and a `release` with none.
pub(crate) fn assert_fell_back(case: &Case, events: &[Value]) {
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

/// The suite's stand-in for ssh, which an ssh case copies as `ssh.exe` into a folder first on the gate's `PATH`, so the
/// gate finds it as ssh with no seam naming it.
const FAKE_SSH: &str = env!("CARGO_BIN_EXE_fake-ssh");
/// The line a routed run says when its builder's preflight ends with cmd's status for a program it cannot find.
const NO_BOX_LINE: &str = "gate: not routed: builder has no gate box that takes this run (preflight status 9009)";

/// A [`routed_case`] whose `.gitignore` leaves the gate's own files and an `ssh` folder out of the snapshot.
fn ssh_case(name: &str) -> Case {
    let case = routed_case(name);
    fs::write(case.folder().join(".gitignore"), "state/\nappdata/\n*.log\nssh/\n.tmp/\n").expect("the ignore file can be written");
    case
}

/// Copies fake-ssh as `ssh.exe` into `folder`, made when missing, beside the steps file `ssh.steps`, which answers the
/// first call with cmd's 9009; the steps file's path.
fn fake_ssh_in(folder: &Path) -> PathBuf {
    fs::create_dir_all(folder).expect("the stand-in's folder can be made");
    fs::copy(FAKE_SSH, folder.join("ssh.exe")).expect("fake-ssh can be copied as ssh.exe");
    let steps = folder.join("ssh.steps");
    fs::write(&steps, "exit 9009\n").expect("the steps file can be written");
    steps
}

/// A `PATH` of `folder`, then the suite's own.
fn path_with(folder: &Path) -> OsString {
    let path = std::env::var_os("PATH").expect("PATH is set");
    let folders = std::iter::once(folder.to_owned()).chain(std::env::split_paths(&path));
    std::env::join_paths(folders).expect("the folders join into a PATH")
}

/// The folder on the suite's `PATH` that holds git.exe.
fn git_folder() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set");
    std::env::split_paths(&path)
        .find(|folder| folder.join("git.exe").is_file())
        .expect("git.exe is on PATH; the routing cases need it")
}

/// The gate of a routed run of an [`ssh_case`] logging to `out.log`, with `path` as its `PATH` and the stand-in
/// reading `steps`.
fn ssh_gate(case: &Case, path: &OsStr, steps: &Path) -> Command {
    let mut gate = case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "ran here"]));
    gate.env("PATH", path).env("GATE_FAKE_SSH_STEPS", steps);
    gate
}

/// The verbs the stand-in reading `steps` was called with, in order; none before its first call.
fn ssh_calls(steps: &Path) -> Vec<String> {
    fs::read_to_string(beside(steps, ".log")).map_or_else(|_| Vec::new(), |text| text.lines().map(str::to_owned).collect())
}

/// Runs a routed run of an [`ssh_case`] named `name` through the stand-in ssh, its state root first holding `left`
/// (each a file name and its text), and checks it asked the builder's gate box through ssh and, on the stand-in's 9009,
/// ran here with reason `version`; what the gate said.
fn routed_through_ssh(name: &str, left: &[(&str, String)]) -> Outcome {
    let case = ssh_case(name);
    let outside = Case::new(&format!("{name}-stand-in"));
    let steps = fake_ssh_in(&outside.folder());
    let root = case.root();
    for (file, text) in left {
        fs::create_dir_all(&root).expect("the state root can be made");
        fs::write(root.join(file), text).expect("the left file can be written");
    }

    let outcome = finish(ssh_gate(&case, &path_with(&outside.folder()), &steps));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(NO_BOX_LINE), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder version; running here"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(ssh_calls(&steps), ["preflight"], "the builder's gate box was asked through ssh");
    assert_refused(&events(&case), "version");
    outcome
}

#[test]
fn a_routed_run_goes_through_ssh_to_the_builders_gate() {
    routed_through_ssh("routing-ssh", &[]);
}

#[test]
fn a_flag_left_in_the_state_root_changes_nothing() {
    let pid = process::id();
    let start = gate_win32::process_start_of(pid)
        .expect("this process's start can be read")
        .expect("this process runs")
        .as_millisecond();
    let marker = format!(r#"{{"v":1,"pid":{pid},"start":{start},"since":{start}}}"#);

    let outcome = routed_through_ssh(
        "routing-left-flag",
        &[("remote-cutover.flag", String::new()), ("remote-cutover.draining", marker)],
    );

    assert!(!outcome.stderr.contains("remote cutover"), "{outcome:?}");
}

#[test]
fn with_no_ssh_a_routed_run_falls_back_with_reason_client() {
    let case = ssh_case("routing-no-ssh");
    let git = git_folder();
    assert!(!git.join("ssh.exe").exists(), "git's folder holds no ssh.exe: {}", git.display());

    let outcome = finish(ssh_gate(&case, git.as_os_str(), &case.folder().join("no-ssh.steps")));

    let no_ssh = "gate: remote builder client could not start: cannot run ssh: ";
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(no_ssh), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_fell_back(&case, &events(&case));
    let log = log_texts(&case.folder().join("out.log"));
    // The attempt's line is a note said before the run here starts, so the log holds it, in time order, first.
    assert_in_order(&log, &[no_ssh, "ran here"]);
}

#[test]
fn an_ssh_inside_the_worktree_is_refused_with_reason_client() {
    let case = ssh_case("routing-ssh-inside");
    let folder = case.folder().join("ssh");
    let steps = fake_ssh_in(&folder);

    let outcome = finish(ssh_gate(&case, &path_with(&folder), &steps));

    let line = format!(
        "gate: remote builder client could not start: cannot run ssh: {} lies inside the worktree; running here",
        shown(&folder.join("ssh.exe"))
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(ssh_calls(&steps), Vec::<String>::new(), "the refused ssh was never run");
    assert_refused(&events(&case), "client");
}

#[test]
fn local_runs_here_saying_why_and_journals_not_routed() {
    let case = routed_case("routing-local");
    let local = ["--local", LOCAL_REASON[0], LOCAL_REASON[1]];
    let heavy = finish(case.gate(&heavy_words_with_limit("heavy.log", &local, &[r".\tool.exe", "out", "hi"])));
    let light = finish(case.gate(&run_words("light.log", &local, &[r".\tool.exe", "out", "hi"])));

    assert_eq!((heavy.code, light.code), (Some(0), Some(0)), "{heavy:?} {light:?}");
    assert!(heavy.stderr.contains("gate: not routed: --local\n"), "{heavy:?}");
    assert!(!light.stderr.contains("not routed"), "a light gate says nothing: {light:?}");
    let events = events(&case);
    assert_local(&events[..3], Some("--local"));
    assert_local(&events[3..], None);
    assert_eq!(events[0]["reason"], LOCAL_REASON[1], "{}", events[0]);
    assert_eq!(events[3]["reason"], LOCAL_REASON[1], "{}", events[3]);
    let log = log_texts(&case.folder().join("heavy.log"));
    assert!(log.contains("gate: not routed: --local"), "{log}");
}

/// A routed run that gave a reason writes it on its queue line though it was not kept here.
#[test]
fn a_queue_with_a_reason_but_no_not_routed_still_writes_it() {
    let case = routed_case("routing-reason-routed");
    let steps = shown(&stub(&case, &["status 0"]));

    let outcome = finish(case.gate(&routed_words(
        "out.log",
        &steps,
        &["--reason", "a reason given on a routed run"],
        &[r".\tool.exe", "out", "hi"],
    )));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let events = events(&case);
    assert_eq!(events[0]["event"], "queue", "{events:#?}");
    assert_eq!(events[0]["reason"], "a reason given on a routed run", "{}", events[0]);
    assert!(events[0].get("notRouted").is_none(), "{}", events[0]);
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
fn a_gate_inside_a_git_hook_routes_when_remote_json_consents_its_repo() {
    let case = routed_case("routing-hook-routed");
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["*tool*", "*dotnet.exe*", "*dotnet.cmd*"] },
        "hooks": [repo(&case)],
    });
    write_remote_json(&case, &routing.to_string());
    let steps = shown(&stub(&case, &["status 0"]));
    let mut command = case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"]));
    command.env("GIT_INDEX_FILE", "x");
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("not routed"), "the consented hook run routes: {outcome:?}");
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
}

/// A routed case named `name` whose `remote.json` consents its hooks, with a heavy pool of one slot and a `git.exe`
/// stand-in.
fn commit_hook_case(name: &str) -> Case {
    let case = routed_case(name);
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["*tool*"] },
        "hooks": [repo(&case)],
    });
    write_remote_json(&case, &routing.to_string());
    write_counts(&case.root(), r#"{"v":1,"heavy":1}"#);
    copy_tool(&case, "git.exe");
    case
}

/// A heavy commit run in `case`, holding the heavy pool's one slot, whose pre-commit hook starts a routed gate through
/// the scripted client of `steps`, logging to `inner.log`, which its slot covers: the commit's pid and the nested
/// gate's pid, once both passed.
fn run_heavy_commit_with_routed_hook(case: &Case, steps: &[&str]) -> (u32, u32) {
    let steps = shown(&stub(case, steps));
    let (pid_file, release) = (case.folder().join("inner.pid"), case.folder().join("outer.release"));
    for file in [&pid_file, &release] {
        let removed = fs::remove_file(file);
        assert!(
            removed.as_ref().is_ok() || removed.as_ref().is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "an earlier run's {} can be removed: {removed:?}",
            file.display()
        );
    }
    let (pid_text, release_text) = (shown(&pid_file), shown(&release));
    let mut command = vec![r".\git.exe", "commit", "spawn-pid", &pid_text, &release_text, GATE];
    command.extend(routed_words("inner.log", &steps, &[], &[r".\tool.exe", "out", "ran there"]));
    let mut outer = case.gate(&heavy_words("outer.log", &[], &command));
    // Inside a pre-commit hook, as git sets it for the hook and every gate the hook starts.
    outer.env("GIT_INDEX_FILE", "x");
    let outer = start(outer);
    let outer_pid = outer.id();

    let (inner, first) = first_event(case, &pid_file);
    assert_eq!(first["event"], "queue", "the nested gate queues for a remote slot: {first}");
    assert_eq!(inner.ended(), 0);
    let_go(&release);
    assert_eq!(outer.finish().code, Some(0));

    let outer_lines = own(case, outer_pid);
    assert_eq!(names(&outer_lines), ["queue", "take", "release"], "{outer_lines:#?}");
    assert!(
        outer_lines.iter().all(|line| line["commit"] == true && line["kind"] == "heavy"),
        "{outer_lines:#?}"
    );
    (outer_pid, inner.pid)
}

/// [`run_heavy_commit_with_routed_hook`] once in a case of its own named `name`: the case and both pids.
fn heavy_commit_with_routed_hook(name: &str, steps: &[&str]) -> (Case, u32, u32) {
    let case = commit_hook_case(name);
    let (outer_pid, inner_pid) = run_heavy_commit_with_routed_hook(&case, steps);
    (case, outer_pid, inner_pid)
}

#[test]
fn a_gate_in_a_commit_run_s_hooks_routes_and_takes_a_remote_slot_of_its_own() {
    let (case, outer_pid, inner_pid) = heavy_commit_with_routed_hook("routing-commit-hook", &["status 0"]);

    let inner_lines = own(&case, inner_pid);
    assert_eq!(names(&inner_lines), ["queue", "take", "release"], "{inner_lines:#?}");
    assert!(inner_lines.iter().all(has_remote_tag), "{inner_lines:#?}");
    assert_eq!(inner_lines[1]["parentPid"], Value::from(outer_pid), "{}", inner_lines[1]);
    let slot = inner_lines[1]["slots"][0].as_str();
    assert!(
        slot.is_some_and(|slot| slot.ends_with("remote-slot-0")),
        "a remote slot: {}",
        inner_lines[1]
    );
}

#[test]
fn a_gate_in_a_commit_run_s_hooks_that_ends_on_the_builder_files_its_release_in_the_durations_history() {
    let case = commit_hook_case("routing-commit-hook-durations");
    // A key says its expected time from its sixth run on: five runs on the builder file five releases.
    for _ in 0..5 {
        run_heavy_commit_with_routed_hook(&case, &["status 0"]);
    }
    let (_, inner_pid) = run_heavy_commit_with_routed_hook(&case, &["status 0"]);

    let release = own(&case, inner_pid).into_iter().find(|line| line["event"] == "release");
    assert!(release.is_some_and(|line| has_remote_tag(&line)), "the hook gate released on the builder");
    let log = log_texts(&case.folder().join("inner.log"));
    assert!(
        log.contains("gate: expected"),
        "the hook gate reads its history and says its expected time: {log}"
    );
}

#[test]
fn a_gate_in_a_commit_run_s_hooks_that_falls_back_runs_under_the_commit_s_slot_saying_so_once() {
    let (case, outer_pid, inner_pid) = heavy_commit_with_routed_hook("routing-commit-hook-fallback", &["refuse busy"]);

    let inner_lines = own(&case, inner_pid);
    // It queues and takes a remote slot beside the commit, then falls back under the commit's slot.
    assert_eq!(names(&inner_lines), ["queue", "take", "fallback", "covered"], "{inner_lines:#?}");
    assert_eq!(inner_lines[3]["parentPid"], Value::from(outer_pid), "{}", inner_lines[3]);
    let log = log_texts(&case.folder().join("inner.log"));
    let covered = format!("gate: running under the heavy slot of gate {outer_pid}");
    assert_eq!(log.lines().filter(|line| *line == covered).count(), 1, "one nesting line: {log}");
    assert!(log.contains("ran there"), "the command ran here: {log}");
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
    let log = log_texts(&case.folder().join("heavy.log"));
    assert!(log.contains(said[0]), "{log}");
    assert!(!light.stderr.contains("ignored"), "a light gate reads no routing file: {light:?}");
}

/// The line a matched command naming a path in the git worktree `tree`, another than the run's own, says: the tree
/// written with `\`, in double quotes when it holds a space or a character a shell reads apart.
fn cd_line(tree: &Path) -> String {
    let tree = shown(tree).replace('/', "\\");
    let quoted = tree.contains([' ', ';', '&', '\'', '(', ')', ',', '=', '$', '`']);
    let tree = if quoted { format!("\"{tree}\"") } else { tree };
    format!("gate: not routed: the command works in {tree}; run it with --cd {tree}")
}

/// The `notRouted` value of a matched command naming a path in another git worktree.
const ANOTHER_WORKTREE: &str = "another worktree";

#[test]
fn a_matched_command_holding_a_path_outside_the_worktree_runs_here_saying_so() {
    let case = routed_case("routing-outside");
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", FAKE_TOOL])));
    let share = finish(case.gate(&heavy_words_with_limit(
        "share.log",
        &[],
        &[r".\tool.exe", "out", r"\\host\share\x.csproj"],
    )));

    // The fake tool lies in this repo's build folder, whose repo the routing file does not list.
    assert_eq!((outcome.code, share.code), (Some(0), Some(0)), "{outcome:?} {share:?}");
    assert!(
        outcome.stderr.contains(&format!("gate: not routed: {FAKE_TOOL} is outside the worktree")),
        "only a command's first word is read as a program's path: {outcome:?}"
    );
    assert!(
        share.stderr.contains(r"gate: not routed: \\host\share\x.csproj is outside the worktree"),
        "a share path is looked up in no worktree: {share:?}"
    );
    let events = events(&case);
    assert_local(&events[..3], None);
    assert_local(&events[3..], None);
}

/// The folder beside the case's named the case's folder name and `suffix`, made empty.
fn beside_case(case: &Case, suffix: &str) -> PathBuf {
    let mut name = case.folder().file_name().expect("the case folder has a name").to_owned();
    name.push(suffix);
    let folder = case.folder().with_file_name(name);
    if folder.exists() {
        fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
    }
    fs::create_dir_all(&folder).expect("the folder beside the case can be made");
    folder
}

#[test]
fn a_command_in_another_worktree_is_told_to_cd_and_journals_why() {
    let case = routed_case("routing-other-worktree");
    let main = beside_case(&case, ".main");
    let git_folder = main.join(".git").join("worktrees").join("other");
    fs::create_dir_all(&git_folder).expect("the main checkout's git folder for the worktree can be made");
    fs::write(git_folder.join("commondir"), "../..\n").expect("the commondir file can be written");
    let other = beside_case(&case, ".other");
    let gitdir = shown(&git_folder).replace('\\', "/");
    fs::write(other.join(".git"), format!("gitdir: {gitdir}\n")).expect("a linked worktree's .git file can be written");
    let main_repo = repo_name(&main.file_name().expect("the main checkout has a name").to_string_lossy());
    let routing = json!({ "host": HOST, "repos": { repo(&case): ["*tool*"], main_repo: ["*tool*"] } });
    write_remote_json(&case, &routing.to_string());
    let path = shown(&other.join("src").join("x.csproj"));
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", &path])));

    let line = cd_line(&other);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(&format!("{line}\n")), "{outcome:?}");
    assert!(!outcome.stderr.contains("is outside the worktree"), "one line, not both: {outcome:?}");
    assert!(outcome.stdout.contains(&path), "the command ran here: {outcome:?}");
    assert_local(&events(&case), Some(ANOTHER_WORKTREE));
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.contains(&line), "{log}");
}

/// The line a routed run says for the caller's watched variables it does not send, as far as its first name.
const DROPPED_LINE: &str = "gate: a routed run does not send the caller's ";

#[test]
fn a_routed_run_says_which_watched_variables_it_drops() {
    let case = routed_case("routing-dropped-env");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let mut gate = case.gate(&routed_words(
        "out.log",
        &steps,
        &["--env", "rustflags=-D warnings"],
        &[FAKE_TOOL, "out", "ran"],
    ));
    gate.env("NuGetAudit", "false").env("RUSTFLAGS", "-D warnings");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let dropped: Vec<&str> = outcome.stderr.lines().filter(|line| line.starts_with(DROPPED_LINE)).collect();
    assert_eq!(dropped.len(), 1, "one line: {outcome:?}");
    assert!(dropped[0].ends_with("; --env NAME sends it"), "{dropped:?}");
    let names = &dropped[0][DROPPED_LINE.len()..];
    assert!(names.split([',', ';', ' ']).any(|name| name == "NuGetAudit"), "{dropped:?}");
    assert!(
        !names.to_ascii_uppercase().contains("RUSTFLAGS"),
        "a name --env sends is not dropped: {dropped:?}"
    );
    let name = fake_tool_name();
    let routed_line = format!("gate: routed {FAKE_TOOL} as {name}");
    assert_in_order(&outcome.stderr, &[routed_line.as_str(), DROPPED_LINE]);
    assert_eq!(
        recorded(&call)["env"],
        json!([["rustflags", "-D warnings"]]),
        "the client is given the pairs"
    );
}

/// The names of the one dropped-variable line of `stderr`.
fn dropped_names(stderr: &str) -> Vec<String> {
    let dropped: Vec<&str> = stderr.lines().filter(|line| line.starts_with(DROPPED_LINE)).collect();
    assert_eq!(dropped.len(), 1, "one line: {stderr}");
    let names = dropped[0][DROPPED_LINE.len()..]
        .strip_suffix("; --env NAME sends it")
        .expect("the line ends as it always has");
    names.split(", ").map(str::to_owned).collect()
}

/// Runs the routed fake tool in `case` with `ZZ_REPO_SEED` and `NuGetAudit` set, recording the client's call; the
/// outcome and whether the client ran.
fn run_with_repo_variable(case: &Case) -> (Outcome, bool) {
    let call = case.folder().join("call.json");
    let steps = shown(&stub(case, &[&format!("record {}", shown(&call)), "status 0"]));
    let mut gate = case.gate(&routed_words("out.log", &steps, &[], &[FAKE_TOOL, "out", "ran"]));
    gate.env("ZZ_REPO_SEED", "7").env("NuGetAudit", "false");
    (finish(gate), call.exists())
}

#[test]
fn a_routed_run_names_the_variables_its_repo_declares_after_the_route_lines() {
    let case = routed_case("routing-declared-env");
    fs::create_dir_all(case.folder().join(".gate")).expect("the .gate folder can be made");
    fs::write(case.folder().join(".gate").join("env.toml"), "prefixes = ['zz_repo_']\n").expect("the declaration can be written");

    let (outcome, called) = run_with_repo_variable(&case);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(called, "the run routed: {outcome:?}");
    let names = dropped_names(&outcome.stderr);
    assert!(
        names.iter().any(|name| name == "NuGetAudit") && names.iter().any(|name| name == "ZZ_REPO_SEED"),
        "{outcome:?}"
    );
    let routed_line = format!("gate: routed {FAKE_TOOL} as {}", fake_tool_name());
    assert_in_order(&outcome.stderr, &[routed_line.as_str(), DROPPED_LINE]);
}

#[test]
fn an_env_file_with_a_problem_says_it_and_routes_watching_the_build_variables_alone() {
    let case = routed_case("routing-declared-env-broken");
    fs::create_dir_all(case.folder().join(".gate")).expect("the .gate folder can be made");
    fs::write(case.folder().join(".gate").join("env.toml"), "prefixes = 'ZZ_REPO_'\n").expect("the declaration can be written");

    let (outcome, called) = run_with_repo_variable(&case);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(called, "a bad env file never keeps the run here: {outcome:?}");
    let line = "gate: .gate/env.toml: prefixes is not an array (TOML string); only the build variables are watched";
    assert!(outcome.stderr.contains(&format!("{line}\n")), "{outcome:?}");
    let names = dropped_names(&outcome.stderr);
    assert!(
        names.iter().any(|name| name == "NuGetAudit") && !names.iter().any(|name| name == "ZZ_REPO_SEED"),
        "{outcome:?}"
    );
    let routed_line = format!("gate: routed {FAKE_TOOL} as {}", fake_tool_name());
    assert_in_order(&outcome.stderr, &[routed_line.as_str(), line, DROPPED_LINE]);
}

#[test]
fn a_declared_prefix_never_names_a_variable_env_cannot_send() {
    let case = routed_case("routing-declared-env-refused");
    fs::create_dir_all(case.folder().join(".gate")).expect("the .gate folder can be made");
    fs::write(case.folder().join(".gate").join("env.toml"), "prefixes = ['GODOT_', 'GIT_CONFIG']\n").expect("the declaration can be written");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let mut gate = case.gate(&routed_words("out.log", &steps, &[], &[FAKE_TOOL, "out", "ran"]));
    gate.env("GODOT_PATH", r"C:\godot")
        .env("GODOT_X", "1")
        .env("GIT_CONFIG_KEY_7", "a.b")
        .env("GIT_CONFIG_PARAMETERS", "'a.b=c'");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(call.exists(), "the run routed: {outcome:?}");
    let names = dropped_names(&outcome.stderr);
    assert!(names.iter().any(|name| name == "GODOT_X"), "{outcome:?}");
    for refused in ["GODOT_PATH", "GIT_CONFIG_KEY_7", "GIT_CONFIG_PARAMETERS"] {
        assert!(!names.iter().any(|name| name == refused), "--env refuses {refused}: {outcome:?}");
    }
}

#[test]
fn an_env_file_in_the_declarations_folder_is_read_when_the_worktree_has_none() {
    let case = routed_case("routing-declared-env-folder");
    let folder = declarations_holding(&case, &[("env.toml", "prefixes = ['ZZ_REPO_SEED']\n")]);
    name_declarations(&case, &folder);

    let (outcome, called) = run_with_repo_variable(&case);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(called, "the run routed: {outcome:?}");
    let names = dropped_names(&outcome.stderr);
    assert!(
        names.iter().any(|name| name == "NuGetAudit") && names.iter().any(|name| name == "ZZ_REPO_SEED"),
        "{outcome:?}"
    );
    let log = log_texts(&case.folder().join("out.log"));
    let read = format!("gate: .gate/env.toml read from {}", folder.join("env.toml").display());
    assert!(log.lines().any(|line| line == read), "{read}: {log}");
}

#[test]
fn a_local_run_says_no_dropped_variables() {
    let case = routed_case("routing-local-dropped-env");
    let steps = shown(&stub(&case, &["status 0"]));
    let local = ["--local", LOCAL_REASON[0], LOCAL_REASON[1]];
    let mut gate = case.gate(&routed_words("out.log", &steps, &local, &[r".\tool.exe", "out", "ran"]));
    gate.env("NuGetAudit", "false");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran"), "the run ran here: {outcome:?}");
    assert!(!outcome.stderr.contains(DROPPED_LINE), "{outcome:?}");
}

#[test]
fn a_run_that_would_route_but_runs_here_says_no_dropped_variables() {
    let case = routed_case("routing-would-route-local");
    let mut gate = case.gate(&heavy_words_with_limit(
        "out.log",
        &["--env", "rustflags=-D warnings"],
        &[r".\tool.exe", "argv"],
    ));
    gate.arg(OsString::from_wide(&[u16::from(b'x'), 0xD800]));
    gate.env("NuGetAudit", "false");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: not routed: a word of the command is not valid Unicode\n"),
        "the decision would route: {outcome:?}"
    );
    assert!(!outcome.stderr.contains(DROPPED_LINE), "only a routed run says the line: {outcome:?}");
}

/// A forwarded `--env NAME` takes the path a written pair takes: the route's call carries it, and no line of the gate's
/// says its value.
#[test]
fn a_routed_run_sends_a_forwarded_env_pair() {
    let case = routed_case("routing-forwarded-env");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let mut gate = case.gate(&routed_words(
        "out.log",
        &steps,
        &["--env", "ZZ_GATE_FORWARDED"],
        &[FAKE_TOOL, "out", "ran"],
    ));
    gate.env("ZZ_GATE_FORWARDED", "from the caller's environment");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        recorded(&call)["env"],
        json!([["ZZ_GATE_FORWARDED", "from the caller's environment"]]),
        "the forwarded pair reaches the call's Env"
    );
    assert!(
        !outcome
            .stderr
            .lines()
            .any(|line| line.starts_with("gate: ") && line.contains("from the caller's environment")),
        "no line of the gate's says the value: {outcome:?}"
    );
}

/// A routed command that gets no `-p:SourceRevisionId` word still runs any build on the box at the worktree's HEAD.
#[test]
fn a_routed_command_without_the_stamp_word_carries_the_worktree_head_in_its_env() {
    let case = routed_case("routing-head-env");
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let head = git(&case, &["rev-parse", "HEAD"]);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));

    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let call = recorded(&call);
    assert_eq!(recorded_words(&call)[1..], ["out", "ran"], "the words get no stamp: {call}");
    assert_eq!(
        call["env"],
        json!([["SourceRevisionId", head]]),
        "the worktree's HEAD travels in the env: {call}"
    );
}

#[test]
fn a_caller_s_source_revision_pair_wins_over_the_worktree_head() {
    let case = routed_case("routing-head-env-caller");
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));

    let outcome = finish(case.gate(&routed_words(
        "out.log",
        &steps,
        &["--env", "sourcerevisionid=abc"],
        &[r".\tool.exe", "out", "ran"],
    )));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        recorded(&call)["env"],
        json!([["sourcerevisionid", "abc"]]),
        "the caller's pair is the only one sent"
    );
}

#[test]
fn a_command_naming_a_program_outside_the_worktree_routes_by_the_program_s_name() {
    let case = routed_case("routing-program-path");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[FAKE_TOOL, "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("not routed"), "the program's name matches *tool*: {outcome:?}");
    let name = fake_tool_name();
    let line = format!("gate: routed {FAKE_TOOL} as {name}");
    assert_eq!(
        outcome.stderr.matches(&line).count(),
        1,
        "the word as given and the name it was sent as, once: {outcome:?}"
    );
    assert_eq!(
        recorded_words(&recorded(&call)),
        [name.as_str(), "out", "ran here"],
        "the builder runs the name it finds on its own PATH"
    );
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
}

/// The name of the fake tool a builder looks up for it: its path's file name without its extension.
fn fake_tool_name() -> String {
    Path::new(FAKE_TOOL)
        .file_stem()
        .expect("the fake tool is a file")
        .to_string_lossy()
        .into_owned()
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

/// A routed case holding a checkout of its own, `other`, whose repo the routing file routes too, so a gate started in
/// `other` judges the case's own files outside its worktree.
fn case_with_another_checkout(name: &str) -> Case {
    let case = routed_case(name);
    git(&case, &["init", "--quiet", "other"]);
    let routing = json!({ "host": HOST, "repos": { repo(&case): ["*tool*"], repo_name("other"): ["*tool*"] } });
    write_remote_json(&case, &routing.to_string());
    case
}

/// The case's folder `name`, made, as a `--cd` value.
fn cd_folder(case: &Case, name: &str) -> PathBuf {
    let folder = case.folder().join(name);
    fs::create_dir_all(&folder).expect("the --cd folder can be made");
    folder
}

#[test]
fn a_gate_given_cd_routes_as_a_gate_started_in_that_folder() {
    let case = case_with_another_checkout("routing-cd");
    let tool = word(&case, "tool.exe");
    let dotted = dotted_word(&case, "tool.exe");
    let here_log = log_path(&case, "here.log");
    let here = finish(case.gate_in("other", &heavy_words_with_limit(&here_log, &[], &[&dotted, "out", "ran here"])));
    assert_eq!(here.code, Some(0), "{here:?}");
    assert!(
        here.stderr.contains(&cd_line(&case.folder())),
        "without --cd the case's files are in another worktree: {here:?}"
    );

    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let folder = shown(&case.folder());
    let routed_log = log_path(&case, "routed.log");
    let routed = finish(case.gate_in(
        "other",
        &routed_words(&routed_log, &steps, &["--cd", &folder], &[&tool, "out", "ran here"]),
    ));

    assert_eq!(routed.code, Some(0), "{routed:?}");
    assert!(!routed.stderr.contains("not routed"), "{routed:?}");
    let call = recorded(&call);
    assert_eq!((&call["repo"], &call["subfolder"]), (&json!(repo(&case)), &json!("")), "{call}");
    let events = events(&case);
    assert_eq!(names(&events[3..]), ["queue", "take", "release"], "{events:#?}");
    assert!(events[3..].iter().all(has_remote_tag), "{events:#?}");
}

#[test]
fn a_relative_log_lands_in_the_cd_folder() {
    let case = Case::new("cd-relative-log");
    let work = cd_folder(&case, "work");
    let outcome = finish(case.gate(&run_words("x.log", &["--cd", "work"], &[FAKE_TOOL, "out", "hi"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let log = log_texts(&work.join("x.log"));
    assert!(log.contains("hi"), "{log}");
    assert!(!case.folder().join("x.log").exists(), "no log under the caller's folder");
}

#[test]
fn a_local_run_given_cd_runs_its_command_in_that_folder() {
    let case = Case::new("cd-local-run");
    let there = cd_folder(&case, "there");
    copy_tool(&case, r"there\tool.exe");
    let outcome = finish(case.gate(&heavy_words_with_limit(
        "out.log",
        &["--local", LOCAL_REASON[0], LOCAL_REASON[1], "--cd", "there"],
        &[r".\tool.exe", "write", "here.txt", "ran"],
    )));

    assert_eq!(outcome.code, Some(0), "the command was found in the --cd folder: {outcome:?}");
    assert_eq!(
        fs::read_to_string(there.join("here.txt")).expect("the command wrote in the --cd folder"),
        "ran"
    );
    assert!(!case.folder().join("here.txt").exists(), "nothing written in the caller's folder");
}

/// The JSON file at `path`.
fn json_file(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("the file can be read")).expect("the file is JSON")
}

#[test]
fn a_cd_run_s_journal_and_claim_carry_the_cd_folder_as_cwd() {
    let case = Case::new("cd-cwd-records");
    let there = shown(&cd_folder(&case, "there"));
    let go = Go(case.folder().join("go"));
    let holder = case.start(&run_words("holder.log", &["--cd", "there"], &[FAKE_TOOL, "wait", &shown(go.path())]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));
    let claimed = json_file(&case.root().join("slots").join("gate-light-slot-0.json"));
    fs::write(go.path(), "").expect("the go file can be made");
    let holder = holder.finish();

    assert_eq!(holder.code, Some(0), "{holder:?}");
    assert_eq!(claimed["cwd"], there, "{claimed}");
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(|event| event["cwd"] == there), "{events:#?}");
}

#[test]
fn a_relative_state_dir_is_read_against_the_caller_s_folder_with_cd() {
    let case = Case::new("cd-relative-state-dir");
    let there = cd_folder(&case, "there");
    let words = run_words("out.log", &["--state-dir", "state", "--cd", "there"], &[FAKE_TOOL, "out", "hi"]);
    let outcome = finish(case.gate_without_state_dir(&words));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        names(&events(&case)),
        ["queue", "take", "release"],
        "the journal is in the caller's state root"
    );
    assert!(!there.join("state").exists(), "no state root under the --cd folder");
}

#[test]
fn a_routed_refusal_falls_back_in_the_cd_folder() {
    let case = case_with_another_checkout("routing-cd-fallback");
    let steps = shown(&stub(&case, &["refuse busy"]));
    let folder = shown(&case.folder());
    let log = log_path(&case, "fallback.log");
    let outcome = finish(case.gate_in(
        "other",
        &routed_words(&log, &steps, &["--cd", &folder], &[r".\tool.exe", "write", "here.txt", "ran"]),
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(case.folder().join("here.txt")).expect("the fallback ran in the --cd folder"),
        "ran"
    );
    assert!(
        !case.folder().join("other").join("here.txt").exists(),
        "nothing written where the gate started"
    );
}

#[test]
fn a_cd_into_a_folder_the_user_cannot_enter_names_the_os_error() {
    let case = Case::new("cd-cannot-enter");
    let there = cd_folder(&case, "there");
    let denied = DenyEntry::new(&there, "(X)");
    let outcome = finish(case.gate(&heavy_words_with_limit("refused.log", &["--cd", "there"], &[FAKE_TOOL, "out", "ran"])));
    drop(denied);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert!(outcome.stderr.starts_with("gate: cannot run in --cd there: "), "{outcome:?}");
    assert_eq!(outcome.stderr.lines().count(), 1, "the line alone: {outcome:?}");
    assert_eq!(case.root_entries(), Vec::<String>::new(), "no claim, ticket or journal line");
}

#[test]
fn a_cd_given_as_cd_names_the_flag_as_given() {
    let case = Case::new("cd-compat-spelling");
    let words = [
        "-Log",
        "out.log",
        "-TimeoutSeconds",
        "60",
        "-Slot",
        "light",
        "-Cd",
        "missing",
        "--",
        FAKE_TOOL,
        "out",
        "ran",
    ];
    let outcome = finish(case.gate(&words));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stderr, "gate: -Cd missing is not a folder\n");
}

#[test]
fn a_cd_to_a_unc_or_device_path_is_refused_before_any_check() {
    let case = Case::new("cd-not-on-a-drive");
    let verbatim = format!(r"\\?\{}", shown(&case.folder()));
    for value in [r"\\localhost\c$\x", verbatim.as_str(), r"\\.\C:\"] {
        let outcome = finish(case.gate(&heavy_words_with_limit("refused.log", &["--cd", value], &[FAKE_TOOL, "out", "ran"])));

        assert_eq!(outcome.code, Some(2), "{value}: {outcome:?}");
        assert_eq!(outcome.stderr, format!("gate: --cd {value} is not a folder on a drive\n"), "{value}");
    }
    assert_eq!(case.root_entries(), Vec::<String>::new(), "no claim, ticket or journal line");
}

#[test]
fn a_cd_that_is_not_a_folder_is_refused_before_any_slot() {
    let case = Case::new("cd-not-a-folder");
    fs::write(case.folder().join("a-file.txt"), "a\n").expect("a file can be written");
    for value in ["missing", "a-file.txt"] {
        let outcome = finish(case.gate(&heavy_words_with_limit("refused.log", &["--cd", value], &[FAKE_TOOL, "out", "ran"])));

        assert_eq!(outcome.code, Some(2), "{value}: {outcome:?}");
        assert_eq!(outcome.stderr, format!("gate: --cd {value} is not a folder\n"), "{value}");
        assert!(!outcome.stdout.contains("ran"), "{value}: {outcome:?}");
    }
    assert!(!case.folder().join("refused.log").exists(), "no log was opened");
    assert_eq!(case.root_entries(), Vec::<String>::new(), "no claim, ticket or journal line");
}

/// Declares in the case's `.gate/siblings.toml` one sibling, named for the case folder so no other case shares it, and
/// allows it in the routing file; its name and the folder it would be, beside the case folder.
fn declare_sibling(case: &Case) -> (String, PathBuf) {
    let folder = case.folder();
    let leaf = folder.file_name().expect("the case folder has a name").to_string_lossy();
    let name = format!("{leaf}-sibling");
    fs::create_dir_all(folder.join(".gate")).expect("the .gate folder can be made");
    fs::write(folder.join(".gate").join("siblings.toml"), format!("siblings = [\"{name}\"]\n")).expect("the declaration can be written");
    let path = case.local_app_data().join("gate").join("remote.json");
    let mut routing: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("the case has a routing file")).expect("the routing file is JSON");
    routing["siblings"] = json!({ repo(case): [&name] });
    write_remote_json(case, &routing.to_string());
    let beside = folder.parent().expect("the case folder has a parent").join(&name);
    if beside.exists() {
        fs::remove_dir_all(&beside).expect("a sibling left by an earlier run can be removed");
    }
    (name, beside)
}

#[test]
fn a_declared_sibling_absent_beside_the_worktree_routes_saying_so() {
    let case = routed_case("routing-sibling-missing");
    let (name, _) = declare_sibling(&case);
    let steps = shown(&stub(&case, &["refuse busy"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains(&format!("gate: routed without sibling {name}: not beside the worktree\n")),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("not routed"), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "the run was routed: {outcome:?}");
    assert_refused(&events(&case), "busy");
}

#[test]
fn a_declared_sibling_folder_with_no_git_keeps_the_run_local_saying_so() {
    let case = routed_case("routing-sibling-no-git");
    let (name, beside) = declare_sibling(&case);
    fs::create_dir_all(&beside).expect("the sibling folder can be made");
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "ran here"])));
    fs::remove_dir_all(&beside).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains(&format!("gate: not routed: sibling {name} has no .git\n")),
        "{outcome:?}"
    );
    assert_local(&events(&case), None);
}

#[test]
fn a_declared_sibling_with_a_git_beside_the_worktree_lets_the_run_route() {
    let case = routed_case("routing-sibling-present");
    let (_, beside) = declare_sibling(&case);
    fs::create_dir_all(beside.join(".git")).expect("the sibling's .git can be made");
    let steps = shown(&stub(&case, &["refuse busy"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));
    fs::remove_dir_all(&beside).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("not routed"), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "the run was routed: {outcome:?}");
    assert_refused(&events(&case), "busy");
}

#[test]
fn a_declared_sibling_the_routing_file_does_not_allow_runs_here_saying_so() {
    let case = routed_case("routing-sibling-not-allowed");
    let (name, beside) = declare_sibling(&case);
    fs::create_dir_all(beside.join(".git")).expect("the sibling's .git can be made");
    let routing = json!({ "host": HOST, "repos": { repo(&case): ["*tool*"] }, "siblings": { repo(&case): [name.to_uppercase()] } });
    write_remote_json(&case, &routing.to_string());
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "ran here"])));
    fs::remove_dir_all(&beside).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains(&format!("gate: not routed: sibling {name} is not allowed in remote.json\n")),
        "a name allowed in another letter case is not allowed: {outcome:?}"
    );
    assert_local(&events(&case), None);
}

/// Names `folder` as the case repo's declarations folder in the case's routing file, the rest of it kept.
fn name_declarations(case: &Case, folder: &Path) {
    let path = case.local_app_data().join("gate").join("remote.json");
    let mut routing: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("the case has a routing file")).expect("the routing file is JSON");
    routing["declarations"] = json!({ repo(case): folder.to_string_lossy() });
    write_remote_json(case, &routing.to_string());
}

/// A declarations folder under the case's LOCALAPPDATA holding each `(name, text)` of `files`; its path.
fn declarations_holding(case: &Case, files: &[(&str, &str)]) -> PathBuf {
    let folder = case.local_app_data().join("declarations");
    fs::create_dir_all(&folder).expect("the declarations folder can be made");
    for (name, text) in files {
        fs::write(folder.join(name), text).expect("a declaration file can be written");
    }
    folder
}

#[test]
fn a_siblings_file_in_the_declarations_folder_is_read_when_the_worktree_has_none() {
    let case = routed_case("routing-sibling-declared");
    let (name, _) = declare_sibling(&case);
    fs::remove_dir_all(case.folder().join(".gate")).expect("the worktree's .gate can be removed");
    let folder = declarations_holding(&case, &[("siblings.toml", &format!("siblings = [\"{name}\"]\n"))]);
    name_declarations(&case, &folder);
    let steps = shown(&stub(&case, &["refuse busy"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains(&format!("gate: routed without sibling {name}: not beside the worktree\n")),
        "the declared sibling was read: {outcome:?}"
    );
    assert!(!outcome.stderr.contains("read from"), "the line is the log's alone: {outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    let read = format!("gate: .gate/siblings.toml read from {}", folder.join("siblings.toml").display());
    assert!(log.lines().any(|line| line == read), "{read}: {log}");
    assert_refused(&events(&case), "busy");
}

#[test]
fn a_desktop_file_in_the_declarations_folder_needs_the_same_consent() {
    let case = routed_case("routing-desktop-declared");
    declare_desktop(&case, &["*TOOL* argv*"]);
    fs::remove_dir_all(case.folder().join(".gate")).expect("the worktree's .gate can be removed");
    let folder = declarations_holding(&case, &[("desktop.toml", "patterns = ['*tool* argv*']\n")]);
    name_declarations(&case, &folder);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let refused = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "argv", "a"])));

    assert_eq!(refused.code, Some(0), "{refused:?}");
    assert!(
        refused
            .stderr
            .contains("gate: not routed: desktop pattern '*tool* argv*' not allowed in remote.json\n"),
        "the declarations folder's pattern needs remote.json's consent: {refused:?}"
    );
    assert!(!call.exists(), "the client never ran");

    let path = case.local_app_data().join("gate").join("remote.json");
    let mut routing: Value = serde_json::from_str(&fs::read_to_string(&path).expect("the routing file is there")).expect("it is JSON");
    routing["desktop"] = json!({ repo(&case): ["*tool* argv*"] });
    write_remote_json(&case, &routing.to_string());
    let allowed = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "argv", "a"])));

    assert_eq!(allowed.code, Some(0), "{allowed:?}");
    assert_eq!(recorded(&call)["desktop"], json!(true), "the consented pattern asks for the desktop");
}

#[test]
fn a_broken_siblings_file_in_the_declarations_folder_names_its_path() {
    let case = routed_case("routing-sibling-declared-broken");
    let folder = declarations_holding(&case, &[("siblings.toml", "siblings = [\n")]);
    name_declarations(&case, &folder);
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let start = format!("gate: not routed: {}: ", folder.join("siblings.toml").display());
    assert!(outcome.stderr.lines().any(|line| line.starts_with(&start)), "{start}: {outcome:?}");
    assert!(!outcome.stderr.contains(".gate/siblings.toml:"), "{outcome:?}");
    assert_local(&events(&case), None);
}

#[test]
fn an_unusable_declarations_entry_reads_nothing_and_logs_why() {
    let case = routed_case("routing-declarations-unusable");
    let missing = case.local_app_data().join("no-such-folder");
    name_declarations(&case, &missing);
    let steps = shown(&stub(&case, &["refuse busy"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "the run still routed: {outcome:?}");
    assert!(!outcome.stderr.contains("declarations"), "the line is the log's alone: {outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    let why = format!("gate: declarations for {} not read: '{}' does not exist", repo(&case), missing.display());
    assert_eq!(log.lines().filter(|line| *line == why).count(), 1, "said once a run: {why}: {log}");
}

#[test]
fn a_broken_siblings_file_in_the_worktree_wins_and_notes_the_copy_it_shadows() {
    let case = routed_case("routing-sibling-broken-shadows");
    let own = case.folder().join(".gate").join("siblings.toml");
    if own.is_file() {
        fs::remove_file(&own).expect("the worktree's own file can be removed");
    }
    fs::create_dir_all(&own).expect("a folder named like the file can be made, so the worktree's file cannot be read");
    let folder = declarations_holding(&case, &[("siblings.toml", "siblings = []\n")]);
    name_declarations(&case, &folder);
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line.starts_with("gate: not routed: .gate/siblings.toml: ")),
        "the worktree's broken file wins: {outcome:?}"
    );
    assert!(!outcome.stderr.contains("shadows"), "the note is the log's alone: {outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    let note = format!("gate: .gate/siblings.toml shadows {}", folder.join("siblings.toml").display());
    assert!(log.lines().any(|line| line == note), "{note}: {log}");
    assert_local(&events(&case), None);
}

#[test]
fn the_routed_call_carries_the_leaf_and_the_allowed_siblings() {
    let case = routed_case("routing-sibling-call");
    let leaf = case
        .folder()
        .file_name()
        .expect("the case folder has a name")
        .to_string_lossy()
        .into_owned();
    let (alone_call, call) = (case.folder().join("alone.json"), case.folder().join("call.json"));
    let alone_steps = shown(&stub(
        &case,
        &[&format!("record {}", shown(&alone_call)), "err from the client", "status 0"],
    ));
    let alone = finish(case.gate(&routed_words("alone.log", &alone_steps, &[], &[r".\tool.exe", "out", "hi"])));
    let (name, beside) = declare_sibling(&case);
    fs::create_dir_all(beside.join(".git")).expect("the sibling's .git can be made");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "err from the client", "status 0"]));
    let carrying = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "hi"])));
    fs::remove_dir_all(&beside).expect("the sibling can be removed");

    assert_eq!((alone.code, carrying.code), (Some(0), Some(0)), "{alone:?} {carrying:?}");
    assert!(
        !alone.stderr.lines().any(|line| line.starts_with("gate: routed")),
        "a run carrying none says no routed line of any wording: {alone:?}"
    );
    let alone_call = recorded(&alone_call);
    assert_eq!((&alone_call["leaf"], &alone_call["siblings"]), (&json!(leaf), &json!([])), "{alone_call}");

    let alone_root_id = alone_call["rootId"].as_str().expect("the call carries the root's id");
    assert_eq!(alone_root_id.len(), 16 + 1 + 32, "the volume serial and the file id: {alone_root_id}");

    let call = recorded(&call);
    assert_eq!(call["leaf"], json!(leaf), "{call}");
    assert_eq!(call["rootId"].as_str(), Some(alone_root_id), "the root's id is read once a route: {call}");
    let root = call["root"].as_str().expect("the call has a root");
    let sibling_root = format!("{}/{name}", root.rsplit_once('/').expect("the root has a parent").0);
    let siblings = call["siblings"].as_array().expect("the call has siblings");
    assert_eq!(siblings.len(), 1, "{call}");
    assert_eq!(
        (&siblings[0]["name"], &siblings[0]["root"]),
        (&json!(name), &json!(sibling_root)),
        "{call}"
    );
    let id = siblings[0]["id"].as_str().expect("the sibling has an id");
    assert_eq!(id.len(), 16 + 1 + 32, "the volume serial and the file id: {id}");

    let line = format!("gate: routed with siblings: {name} ({sibling_root})");
    assert_eq!(carrying.stderr.lines().filter(|said| *said == line).count(), 1, "said once: {carrying:?}");
    assert_in_order(&carrying.stderr, &[&line, "from the client"]);
}

#[test]
fn a_folder_named_with_spaces_and_brackets_holding_a_file_routes() {
    let case = routed_case("routing-brackets");
    sub_folder(&case, "my dir [1]", true);
    let empty = case.folder().join("empty.gitconfig");
    fs::write(&empty, "").expect("an empty git config can be written");
    let log = log_path(&case, "out.log");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "refuse busy"]));
    let mut command = case.gate_in("my dir [1]", &routed_words(&log, &steps, &[], &[r".\tool.exe", "out", "ran here"]));
    command.env("GIT_CONFIG_GLOBAL", &empty).env("GIT_CONFIG_NOSYSTEM", "1");
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "{outcome:?}");
    let call = recorded(&call);
    assert_eq!(call["subfolder"], "my dir [1]", "the folder's name is a literal path: {call}");
    assert_refused(&events(&case), "busy");
}

#[test]
fn with_git_dir_and_work_tree_inherited_a_gate_in_a_subfolder_still_routes() {
    let case = routed_case("routing-git-dir");
    sub_folder(&case, "sub", true);
    let refuse = shown(&stub(&case, &["refuse busy"]));
    let at_root = finish(case.gate(&routed_words("root.log", &refuse, &[], &[r".\tool.exe", "out", "hi"])));
    let log = log_path(&case, "sub.log");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "refuse busy"]));
    let mut command = case.gate_in("sub", &routed_words(&log, &steps, &[], &[r".\tool.exe", "out", "hi"]));
    // As a hook run at the worktree's root exports them: relative, so they name the case's repo only from its root.
    command.env("GIT_DIR", ".git").env("GIT_WORK_TREE", ".");
    let in_sub = finish(command);

    assert_eq!((at_root.code, in_sub.code), (Some(0), Some(0)), "{at_root:?} {in_sub:?}");
    assert!(in_sub.stderr.contains(BUSY_LINE), "{in_sub:?}");
    assert_eq!(recorded(&call)["subfolder"], "sub", "the subfolder's gate is routed from its folder");
    let events = events(&case);
    assert_refused(&events[..5], "busy");
    assert_refused(&events[5..], "busy");
    assert_eq!(events[5]["key"], events[0]["key"], "the subfolder's gate is placed in the same worktree");
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
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "refuse busy"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "argv", word])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "{outcome:?}");
    assert_eq!(
        recorded_words(&recorded(&call))[1..],
        ["argv", word],
        "the builder is sent the word whole"
    );
    assert!(stdout_lines(&outcome.stdout).contains(&word), "{outcome:?}");
    assert_refused(&events(&case), "busy");
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
    dotnet_cmd(&case);
    let (other_call, dotnet_call) = (case.folder().join("other.json"), case.folder().join("dotnet.json"));
    let other_steps = shown(&stub(&case, &[&format!("record {}", shown(&other_call)), "refuse busy"]));
    let dotnet_steps = shown(&stub(&case, &[&format!("record {}", shown(&dotnet_call)), "refuse busy"]));
    let flags = ["--configuration=Release", "-p:Foo=1", "--no-build"];
    let mut tool = vec![r".\tool.exe", "argv"];
    tool.extend(flags);
    let other = finish(case.gate(&routed_words("tool.log", &other_steps, &[], &tool)));
    let dotnet_command = [r".\dotnet.cmd", "test", "--no-build"];
    let dotnet = finish(case.gate(&routed_words("dotnet.log", &dotnet_steps, &[], &dotnet_command)));

    assert_eq!((other.code, dotnet.code), (Some(0), Some(0)), "{other:?} {dotnet:?}");
    assert!(
        other.stderr.contains(BUSY_LINE) && !other.stderr.contains(DROPPED_NO_BUILD_LINE),
        "{other:?}"
    );
    assert_eq!(recorded_words(&recorded(&other_call))[2..], flags, "the box gets the flags as given");
    assert_eq!(
        recorded_words(&recorded(&dotnet_call))[1..],
        ["test", path_map(&case).as_str()],
        "the box's dotnet drops --no-build and, at an unborn HEAD, gets the path map alone"
    );
    assert!(stdout_lines(&other.stdout).windows(3).any(|words| words == flags), "{other:?}");
    assert_in_order(&dotnet.stderr, &[DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE, BUSY_LINE]);
    assert_eq!(
        command_lines(&dotnet.stdout),
        ["test --no-build"],
        "a fallback that finds no build here keeps --no-build: {dotnet:?}"
    );
    let events = events(&case);
    assert_refused(&events[..5], "busy");
    assert_refused(&events[5..], "busy");
}

#[test]
fn a_dotnet_no_build_command_on_a_full_pool_keeps_it_here() {
    let case = routed_case("routing-no-build-full");
    dotnet_cmd(&case);
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let call = case.folder().join("dotnet.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let waiter = case.start(&routed_words(
        "dotnet.log",
        &steps,
        &["--test-remote-wait-ms", "1"],
        &[r".\dotnet.cmd", "test", "--no-build"],
    ));
    let pid = waiter.id();
    let outcome = waiter.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[DROPPED_NO_BUILD_LINE, FULL_LINE]);
    assert_eq!(
        command_lines(&outcome.stdout),
        ["test --no-build"],
        "the fallback runs here with --no-build kept: {outcome:?}"
    );
    assert!(!call.exists(), "the waiter's client never ran");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "fallback", "take", "release"], "{own:#?}");
    assert!(own[..2].iter().all(has_remote_tag), "{own:#?}");
    assert_eq!(own[1]["reason"], "full");
    assert!(own[2..].iter().all(has_no_remote_fields), "{own:#?}");
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));
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
    let steps = shown(&stub(&case, &["out ran there", "adjusted 1.25", "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("gate: routed "),
        "no word of this command was sent as a program's name: {outcome:?}"
    );
    assert_in_order(
        &outcome.stdout,
        &[
            "ran there",
            "gate: passed in ",
            " s on builder (load-adjusted 1 s, ceiling 3600 s; command ",
            " s, push ",
            " s). Full output: ",
        ],
    );
    assert!(!outcome.stdout.contains("ran here"), "the command never ran here: {outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.contains("ran there") && !log.contains("ran here"), "{log}");
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
    let slot = events[1]["slots"][0].as_str().expect("the take names its slot");
    assert!(slot.ends_with("remote-slot-0"), "{slot}");
    assert_eq!((&events[2]["verdict"], &events[2]["adjustedSeconds"]), (&json!("passed"), &json!(1.25)));
    assert!(!remote_claim(&case).exists(), "the remote claim is gone after the release");
}

/// The claim file of slot 0 of the `builders` entry `mac-1`'s own remote pool, whose id is the first 8 hex digits of
/// the SHA-256 of `mac-1`.
const MAC_CLAIM: &str = "gate-remote-1f0efc62-slot-0.json";
/// The claim file of slot 0 of `host`'s remote pool.
const HOST_CLAIM: &str = "gate-remote-slot-0.json";

#[test]
fn a_run_to_a_builders_entry_takes_a_slot_of_that_builders_own_pool() {
    let case = routed_case("routing-own-pool");
    copy_tool(&case, "mac.exe");
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["*tool.exe*"] },
        "builders": [{ "host": "mac-1", "os": "macos", "root": "/m", "repos": { repo(&case): ["*mac.exe*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    let slots = case.root().join("slots");

    for (program, claim, other) in [(r".\mac.exe", MAC_CLAIM, HOST_CLAIM), (r".\tool.exe", HOST_CLAIM, MAC_CLAIM)] {
        let go = case.folder().join(format!("go-{claim}"));
        let steps = stub(&case, &[&format!("wait {}", shown(&go))]);
        let log = format!("{claim}.log");
        let running = case.start(&routed_words(&log, &shown(&steps), &[], &[program, "out", "hi"]));
        wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, running.id()));
        let held = (slots.join(claim).exists(), slots.join(other).exists());
        let_go(&go);
        let outcome = running.finish();

        assert_eq!(held, (true, false), "{program} holds a slot of its builder's pool alone: {outcome:?}");
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
    }
}

#[test]
fn a_heavy_gate_routed_from_under_a_light_parent_does_not_say_the_parent_does_not_cover_it() {
    let case = routed_case("routing-under-light");
    let steps = shown(&stub(&case, &["status 0"]));
    let words = routed_words("out.log", &steps, &[], &[r".\tool.exe", "exit", "0"]);
    let outcome = finish(marked(&case, &words, &test_marker(&case, "light")));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("does not cover"),
        "a routed gate takes no local slot: {outcome:?}"
    );
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
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
fn a_plain_run_s_call_asks_nothing_of_the_warm_up() {
    let case = routed_case("routing-call-warmth");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "argv", "a"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let call = recorded(&call);
    let recorded = call.as_object().expect("the record is an object");
    assert_eq!(
        recorded.get("warmth"),
        Some(&Value::Null),
        "the record names the call's warmth, none: {call}"
    );
}

#[test]
fn the_client_is_called_with_the_repo_s_inputs() {
    let case = routed_case("routing-call-inputs");
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["*tool*"] },
        "inputs": { repo(&case): [".tmp/helper/**", "keys.json"] },
    });
    write_remote_json(&case, &routing.to_string());
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "argv", "a"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let call = recorded(&call);
    assert_eq!(call["inputs"], json!([".tmp/helper/**", "keys.json"]), "{call}");
    assert_eq!(call["desktop"], json!(false), "a repo with no desktop file asks for no desktop: {call}");
}

/// Declares `*tool* argv*` in the case's `.gate/desktop.toml` and writes a routing file routing `*tool*` whose
/// `desktop` value allows the repo `allowed`.
fn declare_desktop(case: &Case, allowed: &[&str]) {
    let folder = case.folder();
    fs::create_dir_all(folder.join(".gate")).expect("the .gate folder can be made");
    fs::write(folder.join(".gate").join("desktop.toml"), "patterns = ['*tool* argv*']\n").expect("the declaration can be written");
    let routing = json!({ "host": HOST, "repos": { repo(case): ["*tool*"] }, "desktop": { repo(case): allowed } });
    write_remote_json(case, &routing.to_string());
}

/// Writes `text` as the case's `.gate/local.toml`.
fn declare_local(case: &Case, text: &str) {
    let folder = case.folder().join(".gate");
    fs::create_dir_all(&folder).expect("the .gate folder can be made");
    fs::write(folder.join("local.toml"), text).expect("the declaration can be written");
}

/// Runs `.\tool.exe out ran here` as a routed heavy gate of `case`, recording the client's call; the outcome and
/// whether the client ran.
fn run_routed_tool(case: &Case) -> (Outcome, bool) {
    let call = case.folder().join("call.json");
    let steps = shown(&stub(case, &[&format!("record {}", shown(&call)), "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));
    (outcome, call.exists())
}

#[test]
fn a_command_its_repo_declares_local_runs_here_saying_so_and_journals_declared_local() {
    let case = routed_case("routing-declared-local");
    declare_local(&case, "patterns = ['*TOOL* out *']\n");

    let (outcome, called) = run_routed_tool(&case);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!called, "the client never ran: {outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("gate: not routed: .gate/local.toml keeps it here (*TOOL* out *)\n"),
        "{outcome:?}"
    );
    assert!(outcome.stdout.contains("ran here"), "the command ran here: {outcome:?}");
    assert_local(&events(&case), Some("declared local"));
}

/// Writes `text` as the case's `.gitignore` and an empty file at the `/`-separated `path`, with the folders above it.
fn ignore_and_write(case: &Case, text: &str, path: &str) {
    let folder = case.folder();
    fs::write(folder.join(".gitignore"), text).expect("the ignore file can be written");
    let file = folder.join(path);
    fs::create_dir_all(file.parent().expect("the file has a folder")).expect("the file's folder can be made");
    fs::write(&file, "<Project />\n").expect("the file can be written");
}

/// Runs `.\tool.exe out <path>` as a routed heavy gate of `case`, recording the client's call; the outcome and whether
/// the client ran.
fn run_routed_naming(case: &Case, path: &str) -> (Outcome, bool) {
    let call = case.folder().join("call.json");
    let steps = shown(&stub(case, &[&format!("record {}", shown(&call)), "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", path])));
    (outcome, call.exists())
}

#[test]
fn a_command_naming_a_gitignored_project_routes_and_the_client_is_called_with_it() {
    let case = routed_case("routing-gitignored-project");
    ignore_and_write(&case, ".tmp/\n", ".tmp/probe/Probe.csproj");
    fs::write(case.folder().join(".tmp").join("probe").join("Program.cs"), "class P {}\n").expect("the file can be written");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));

    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", ".tmp/probe/Probe.csproj"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("gitignored"), "{outcome:?}");
    let call = recorded(&call);
    assert_eq!(
        call["inputs"],
        json!(["[carried].tmp/probe/Probe.csproj", "[carried].tmp/probe/Program.cs"]),
        "{call}"
    );
}

#[test]
fn a_command_naming_a_gitignored_project_an_inputs_glob_covers_still_routes() {
    let case = routed_case("routing-gitignored-project-input");
    let routing = json!({ "host": HOST, "repos": { repo(&case): ["*tool*"] }, "inputs": { repo(&case): [".tmp/**"] } });
    write_remote_json(&case, &routing.to_string());
    ignore_and_write(&case, ".tmp/\n", ".tmp/probe/Probe.csproj");

    let (outcome, called) = run_routed_naming(&case, ".tmp/probe/Probe.csproj");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(called, "the run routed: {outcome:?}");
    assert!(!outcome.stderr.contains("gitignored"), "{outcome:?}");
}

#[test]
fn a_command_naming_a_tracked_project_still_routes() {
    let case = routed_case("routing-tracked-project");
    ignore_and_write(&case, "*.csproj\n", "src/A.csproj");
    git(&case, &["add", "-f", "src/A.csproj"]);

    let (outcome, called) = run_routed_naming(&case, "src/A.csproj");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(called, "a tracked file is in the snapshot, ignored or not: {outcome:?}");
    assert!(!outcome.stderr.contains("gitignored"), "{outcome:?}");
}

#[test]
fn a_dotnet_command_naming_a_gitignored_project_folder_routes_carrying_its_files() {
    let case = routed_case("routing-gitignored-folder");
    copy_tool(&case, "dotnet.exe");
    ignore_and_write(&case, ".tmp/\n", ".tmp/probe/Probe.csproj");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));

    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\dotnet.exe", "test", r".tmp\probe"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("gitignored"), "{outcome:?}");
    let call = recorded(&call);
    assert_eq!(call["inputs"], json!(["[carried].tmp/probe/Probe.csproj"]), "{call}");
}

#[test]
fn a_dotnet_command_naming_a_folder_with_a_tracked_project_still_routes() {
    let case = routed_case("routing-gitignored-folder-mixed");
    copy_tool(&case, "dotnet.exe");
    ignore_and_write(&case, "*.Ignored.csproj\n", "proj/A.csproj");
    git(&case, &["add", "proj/A.csproj"]);
    fs::write(case.folder().join("proj").join("B.Ignored.csproj"), "<Project />\n").expect("the file can be written");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));

    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\dotnet.exe", "test", "proj"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(call.exists(), "the builder has the tracked project, so the run routed: {outcome:?}");
    assert!(!outcome.stderr.contains("gitignored"), "{outcome:?}");
}

/// Makes the case's folder `sub` a repository of its own holding a committed `A.csproj`, added to the case's index as
/// a submodule's entry, which `git check-ignore` refuses to judge a path in.
fn with_submodule(case: &Case) {
    let sub = case.folder().join("sub");
    git(case, &["init", "--quiet", "sub"]);
    fs::write(sub.join("A.csproj"), "<Project />\n").expect("the file can be written");
    commit_all(case, &sub, "sub");
    git(case, &["add", "sub"]);
}

#[test]
fn a_command_whose_gitignored_check_fails_routes_and_says_why_in_the_log() {
    let case = routed_case("routing-gitignored-check-fails");
    with_submodule(&case);

    let (outcome, called) = run_routed_naming(&case, "sub/A.csproj");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(called, "a check that cannot be told routes: {outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert!(
        log.contains("gate: cannot tell which words are gitignored: git check-ignore failed: status "),
        "{log}"
    );
}

#[test]
fn a_path_git_refuses_hides_no_gitignored_project_beside_it() {
    let case = routed_case("routing-gitignored-beside-submodule");
    with_submodule(&case);
    ignore_and_write(&case, ".tmp/\n", ".tmp/probe/Probe.csproj");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let command = [r".\tool.exe", "out", "sub/A.csproj", "out", ".tmp/probe/Probe.csproj"];

    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &command)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let recorded = recorded(&call);
    assert_eq!(recorded["inputs"], json!(["[carried].tmp/probe/Probe.csproj"]), "{recorded}");
    let log = log_texts(&case.folder().join("out.log"));
    assert_eq!(
        log.matches("gate: cannot tell which words are gitignored: ").count(),
        1,
        "one note, for sub/A.csproj alone: {log}"
    );
}

#[test]
fn a_command_its_repo_does_not_declare_local_routes() {
    let case = routed_case("routing-declared-local-other");
    declare_local(&case, "patterns = ['*tool* argv*']\n");

    let (outcome, called) = run_routed_tool(&case);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(called, "the run routed: {outcome:?}");
    assert!(!outcome.stderr.contains("local.toml"), "{outcome:?}");
}

#[test]
fn a_local_file_with_a_problem_keeps_the_command_here() {
    let case = routed_case("routing-declared-local-broken");
    declare_local(&case, "patterns = '*tool*'\n");

    let (outcome, called) = run_routed_tool(&case);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!called, "a file the gate cannot use routes nothing: {outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("gate: not routed: .gate/local.toml: patterns is not an array (TOML string)\n"),
        "{outcome:?}"
    );
    assert!(outcome.stdout.contains("ran here"), "the command ran here: {outcome:?}");
    assert_local(&events(&case), Some("declared local"));
}

#[test]
fn a_command_matching_a_consented_desktop_pattern_asks_the_client_for_the_desktop() {
    let case = routed_case("routing-call-desktop");
    declare_desktop(&case, &["*tool* argv*"]);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "argv", "a"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let call = recorded(&call);
    assert_eq!(call["desktop"], json!(true), "{call}");
}

#[test]
fn a_declared_desktop_pattern_the_routing_file_does_not_allow_runs_here_saying_so() {
    let case = routed_case("routing-desktop-not-allowed");
    declare_desktop(&case, &["*TOOL* argv*"]);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "argv", "a"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("gate: not routed: desktop pattern '*tool* argv*' not allowed in remote.json\n"),
        "a pattern allowed in another letter case is not allowed: {outcome:?}"
    );
    assert!(!call.exists(), "the client never ran");
    assert_local(&events(&case), None);
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
    assert!(
        passed
            .stdout
            .contains(" s on builder (load-adjusted unknown: the sampler failed, ceiling 3600 s; command "),
        "a client that says no load-adjusted time: {passed:?}"
    );
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

/// The box's kill tag names the kill: the verdict is its kind, whatever kill line the log holds last, or none at all, as
/// when the log passed its cap before the box's kill line or a nested gate's kill line came after it.
#[test]
fn a_box_kill_s_verdict_is_its_tag_s_kind_not_the_log_s_last_kill_line() {
    let case = routed_case("routing-box-killed");
    let silent = shown(&stub(&case, &["box-killed timed-out", "status 124"]));
    let misleading = shown(&stub(&case, &["box-killed backstop", "err gate: TIMED OUT after 5 s", "status 124"]));
    let first = finish(case.gate(&routed_words("silent.log", &silent, &[], &[r".\tool.exe", "out", "hi"])));
    let second = finish(case.gate(&routed_words("misleading.log", &misleading, &[], &[r".\tool.exe", "out", "hi"])));

    assert_eq!((first.code, second.code), (Some(124), Some(124)), "{first:?} {second:?}");
    let releases: Vec<Value> = events(&case).into_iter().filter(|event| event["event"] == "release").collect();
    assert_eq!(
        (&releases[0]["verdict"], &releases[1]["verdict"]),
        (&json!("timed-out"), &json!("backstop"))
    );
}

#[test]
fn a_fallback_that_never_starts_the_command_keeps_the_attempts_lines_once() {
    let case = routed_case("routing-fallback-no-start");
    let steps = shown(&stub(&case, &["out attempt line", "refuse busy"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &["nonexistent-tool", "argv"])));

    assert_eq!(outcome.code, Some(127), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: could not start nonexistent-tool"), "{outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert_eq!(log.matches("attempt line").count(), 1, "the attempt's line stands once: {log}");
    assert_refused(&events(&case), "busy");
}

#[test]
fn a_kill_line_after_a_lone_cr_progress_line_still_reads_as_the_kill() {
    let case = routed_case("routing-kill-after-cr");
    let steps = shown(&stub(
        &case,
        &[
            "killed",
            "err 50%\rgate: TIMED OUT on builder after 5 s of load-adjusted time (wall 9 s, machine free 50% on average); killed \
             tool and its children",
            "status 124",
        ],
    ));
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
        log_texts(&log).lines().any(|text| text == "ran here")
    });

    assert!(!remote_claim(&case).exists(), "the remote claim is gone once the run fell back");
    assert!(case.root().join("slots").join("gate-slot-0.json").exists(), "the heavy claim is written");
    let_go(&go);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = "gate: remote builder unreachable; running here";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_in_order(
        &log_texts(&log),
        // The attempt's lines are notes said before the run here starts, so the log holds them, in time order, first.
        &["attempt line", line, "ran here"],
    );
    assert_refused(&events(&case), "unreachable");
}

#[test]
fn every_refusal_reason_names_itself_in_the_line_and_the_journal() {
    let case = routed_case("routing-reasons");
    let reasons = [
        "busy",
        "unreachable",
        "parity",
        "tools",
        "rebooted",
        "layout",
        "inputs",
        "cold",
        "desktop",
    ];
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
fn a_routed_dotnet_build_carries_the_worktree_head_as_its_source_revision() {
    let case = routed_case("routing-dotnet-build");
    copy_tool(&case, "dotnet.exe");
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let head = git(&case, &["rev-parse", "HEAD"]);
    let call = case.folder().join("dotnet.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let outcome = finish(case.gate(&routed_words("dotnet.log", &steps, &[], &[r".\dotnet.exe", "build"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let stamp = format!("-p:SourceRevisionId={head}");
    let call = recorded(&call);
    assert_eq!(
        recorded_words(&call)[1..],
        ["build", stamp.as_str(), path_map(&case).as_str()],
        "a routed dotnet build is pinned to the worktree HEAD and maps its work folder to the worktree root: {call}"
    );
    assert_eq!(names(&events(&case)), ["queue", "take", "release"]);
}

#[test]
fn no_build_is_dropped_for_the_box_and_kept_by_the_fallback() {
    let case = routed_case("routing-no-build");
    dotnet_cmd(&case);
    let (busy_call, tool_call) = (case.folder().join("busy.json"), case.folder().join("tool.json"));
    let busy = shown(&stub(&case, &[&format!("record {}", shown(&busy_call)), "refuse busy"]));
    let rebooted = shown(&stub(&case, &["refuse rebooted"]));
    let tool = shown(&stub(&case, &[&format!("record {}", shown(&tool_call)), "status 0"]));
    let pack_call = case.folder().join("pack.json");
    let pack_steps = shown(&stub(&case, &[&format!("record {}", shown(&pack_call)), "refuse busy"]));
    let dotnet = [r".\dotnet.cmd", "test", "--no-build"];
    let before = finish(case.gate(&routed_words("busy.log", &busy, &[], &dotnet)));
    let after = finish(case.gate(&routed_words("rebooted.log", &rebooted, &[], &dotnet)));
    let other = finish(case.gate(&routed_words("tool.log", &tool, &[], &[r".\tool.exe", "argv", "--no-build"])));
    let pack = finish(case.gate(&routed_words("pack.log", &pack_steps, &[], &[r".\dotnet.cmd", "pack", "--no-build"])));

    assert_eq!((before.code, after.code, other.code, pack.code), (Some(0), Some(0), Some(0), Some(0)));
    let call = recorded(&busy_call);
    assert!(!recorded_words(&call).contains(&"--no-build"), "{call}");
    assert_eq!(call["outputs"], json!([]), "{call}");
    assert_in_order(&before.stderr, &[DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE, BUSY_LINE]);
    assert_eq!(
        command_lines(&before.stdout),
        ["test --no-build"],
        "a fallback that finds no build here keeps --no-build: {before:?}"
    );
    assert_eq!(command_lines(&after.stdout), ["test --no-build"], "{after:?}");
    assert!(
        recorded_words(&recorded(&tool_call)).contains(&"--no-build"),
        "a non-dotnet command keeps it"
    );
    assert_eq!(
        recorded_words(&recorded(&pack_call))[1..],
        ["pack", "--no-build"],
        "pack keeps it on the box, unstamped, as a command that does not build"
    );
    assert!(!pack.stderr.contains(DROPPED_NO_BUILD_LINE), "{pack:?}");
    assert_eq!(command_lines(&pack.stdout), ["pack --no-build"], "and here: {pack:?}");
}

/// A fallback of a routed `--no-build` command whose worktree has no nothing-back record runs here with the word
/// kept, whatever refused the routed run, and says no drop of its own.
#[test]
fn a_fallback_with_no_record_keeps_no_build() {
    let case = routed_case("routing-fallback-kept");
    dotnet_cmd(&case);
    let busy = shown(&stub(&case, &["refuse busy"]));
    let rebooted = shown(&stub(&case, &["refuse rebooted"]));
    let busy_run = finish(case.gate(&routed_words("busy.log", &busy, &[], &NO_BUILD_TEST)));
    let rebooted_run = finish(case.gate(&routed_words("rebooted.log", &rebooted, &[], &NO_BUILD_TEST)));
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let full = shown(&stub(&case, &["status 0"]));
    let full_run = finish(case.gate(&routed_words("full.log", &full, &["--test-remote-wait-ms", "1"], &NO_BUILD_TEST)));
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));

    for (reason, outcome) in [("busy", &busy_run), ("rebooted", &rebooted_run), ("full", &full_run)] {
        assert_eq!(outcome.code, Some(0), "{reason}: {outcome:?}");
        assert_eq!(command_lines(&outcome.stdout), ["test --no-build"], "{reason}: {outcome:?}");
        assert!(
            !outcome.stderr.contains("building on a heavy slot"),
            "{reason}: the fallback drops nothing and says no drop of its own: {outcome:?}"
        );
    }
}

/// A fallback whose worktree has a nothing-back record drops `--no-build`, says why as a run here does, and leaves
/// the record standing.
#[test]
fn a_fallback_with_a_record_drops_no_build_and_says_the_local_line() {
    let case = routed_case("routing-fallback-record");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");
    let busy = shown(&stub(&case, &["refuse busy"]));

    let outcome = finish(case.gate(&routed_words("busy.log", &busy, &[], &[r".\dotnet.cmd", "test", "bad", "--no-build"])));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains(NOTHING_BACK_LINE), "{outcome:?}");
    assert_eq!(command_lines(&outcome.stdout), ["test bad"], "{outcome:?}");
    assert_eq!(
        nothing_back(&case).as_deref(),
        Some(NOTHING_BACK_RECORD),
        "the run here failed, so the record that dropped the word stands: {outcome:?}"
    );
    assert!(
        took_heavy(&case, r".\dotnet.cmd test bad --no-build"),
        "a fallback journals the command as given, on a heavy slot: {:#?}",
        events(&case)
    );
}

/// A fallback whose worktree has a tracked project with no assets file drops `--no-build`, naming the project; one
/// whose project is restored keeps the word.
#[test]
fn a_fallback_whose_project_has_no_assets_file_drops_no_build() {
    let case = routed_case("routing-fallback-assets");
    dotnet_cmd(&case);
    tracked_project(&case, "app/App.csproj", false);

    let busy = shown(&stub(&case, &["refuse busy"]));
    let missing = finish(case.gate(&routed_words("missing.log", &busy, &[], &NO_BUILD_TEST)));
    fs::write(case.folder().join(r"app\obj\project.assets.json"), "{}\n").expect("the assets file can be written");
    let busy = shown(&stub(&case, &["refuse busy"]));
    let present = finish(case.gate(&routed_words("present.log", &busy, &[], &NO_BUILD_TEST)));

    let line = r"gate: dropped --no-build: app/App.csproj has no obj\project.assets.json here; building on a heavy slot";
    assert!(missing.stderr.contains(line), "{missing:?}");
    assert_eq!(command_lines(&missing.stdout), ["test"], "{missing:?}");
    assert!(!present.stderr.contains("building on a heavy slot"), "{present:?}");
    assert_eq!(command_lines(&present.stdout), ["test --no-build"], "{present:?}");
}

/// A fallback that drops `--no-build` and passes rebuilds the whole worktree here, so it clears the record that made
/// it drop, as a passing run here of the dropped words does (`Run::cleared_record`).
#[test]
fn a_passing_fallback_that_drops_no_build_clears_the_record() {
    let case = routed_case("routing-fallback-clears");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");
    let busy = shown(&stub(&case, &["refuse busy"]));

    let outcome = finish(case.gate(&routed_words("busy.log", &busy, &[], &NO_BUILD_TEST)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(NOTHING_BACK_LINE), "{outcome:?}");
    assert_eq!(command_lines(&outcome.stdout), ["test"], "{outcome:?}");
    assert_eq!(
        nothing_back(&case),
        None,
        "a passing fallback builds the whole worktree here, which clears the record: {outcome:?}"
    );
}

/// A fallback that keeps `--no-build`, its worktree holding no nothing-back record and its restored project's assets
/// file, runs the words as given and leaves no record: a passing run here clears a record (`Run::cleared_record`) but
/// writes none, which only a routed run does (`Run::leave_nothing_back`).
#[test]
fn a_passing_fallback_that_keeps_no_build_leaves_the_record_alone() {
    let case = routed_case("routing-fallback-keeps-record");
    dotnet_cmd(&case);
    tracked_project(&case, "app/App.csproj", true);
    let busy = shown(&stub(&case, &["refuse busy"]));

    let outcome = finish(case.gate(&routed_words("busy.log", &busy, &[], &NO_BUILD_TEST)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(command_lines(&outcome.stdout), ["test --no-build"], "{outcome:?}");
    assert_eq!(
        nothing_back(&case),
        None,
        "a kept --no-build writes no record and clears none: {outcome:?}"
    );
}

/// The line a run here that takes a slot of its own says as it drops `--no-build` after a routed build on [`HOST`]
/// brought nothing back.
const NOTHING_BACK_LINE: &str = "gate: dropped --no-build: the last build ran on builder and brought nothing back; building on a heavy slot";
/// The line a run here says as it drops `--no-build` for a record that names no usable builder.
const UNKNOWN_BUILDER_LINE: &str = "gate: dropped --no-build: the last build ran on the builder and brought nothing back; building on a heavy slot";
/// The record a routed build on [`HOST`] that brought nothing back leaves.
const NOTHING_BACK_RECORD: &str = r#"{"v":1,"host":"builder"}"#;
/// The start of every line a run here says as it drops `--no-build`.
const DROPPED: &str = "gate: dropped --no-build";
/// A dotnet test with `--no-build`, run by the stand-in dotnet ([`dotnet_cmd`]).
const NO_BUILD_TEST: [&str; 3] = [r".\dotnet.cmd", "test", "--no-build"];

/// Writes `text` over the case's nothing-back record.
fn rewrite_record(case: &Case, text: &str) {
    let path = record_path(case).expect("the case has a record");
    fs::write(path, text).expect("the record can be written");
}

/// The lines the command printed: the stdout lines after the notes said before the run and before the next of the
/// gate's own, which the tail repeats.
fn command_lines(stdout: &str) -> Vec<&str> {
    let note = |line: &&str| line.starts_with("gate: ");
    stdout.lines().skip_while(note).take_while(|line| !note(line)).collect()
}

/// Whether the last slot the case's gates took is a heavy one (`...-slot-<n>`), taken by a gate of the heavy kind
/// running `command`.
fn took_heavy(case: &Case, command: &str) -> bool {
    events(case).iter().rev().find(|event| event["event"] == "take").is_some_and(|take| {
        let slot = take["slots"][0].as_str().unwrap_or_default();
        let heavy_slot = slot.contains("-slot-") && !slot.contains("light-slot") && !slot.contains("remote-slot");
        heavy_slot && take["kind"] == "heavy" && take["command"] == command
    })
}

/// Writes `dotnet.cmd` in the case folder: a dotnet that says its words on one line and exits 1 when its second word
/// is `bad`, else 0, so a run here can be a dotnet command of any verb that passes or fails.
fn dotnet_cmd(case: &Case) {
    let script = "@echo off\r\necho %*\r\nif \"%2\"==\"bad\" exit /b 1\r\nexit /b 0\r\n";
    fs::write(case.folder().join("dotnet.cmd"), script).expect("the stand-in dotnet can be written");
}

/// Writes the project file `project` in the case folder and tracks it, with an `obj\project.assets.json` beside it
/// when `restored`.
fn tracked_project(case: &Case, project: &str, restored: bool) {
    let path = case.folder().join(project);
    let folder = path.parent().expect("a project has a folder");
    fs::create_dir_all(folder.join("obj")).expect("the project's folders can be made");
    fs::write(&path, "<Project />\n").expect("the project can be written");
    if restored {
        fs::write(folder.join("obj").join("project.assets.json"), "{}\n").expect("the assets file can be written");
    }
    git(case, &["add", project]);
}

/// Runs a routed `dotnet build` the scripted client passes, logging to `log`, and asserts the record it leaves.
fn routed_build(case: &Case, log: &str) {
    let steps = shown(&stub(case, &["status 0"]));
    let routed = finish(case.gate(&routed_words(log, &steps, &[], &[r".\dotnet.exe", "build"])));
    assert_eq!(routed.code, Some(0), "{routed:?}");
    assert_eq!(nothing_back(case).as_deref(), Some(NOTHING_BACK_RECORD), "{routed:?}");
}

#[test]
fn a_routed_build_leaves_a_record_a_later_light_no_build_drops_the_word_on_a_heavy_slot() {
    let case = routed_case("routing-nothing-back");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");

    let here = finish(case.gate(&run_words("here.log", &[], &NO_BUILD_TEST)));

    assert_eq!(here.code, Some(0), "{here:?}");
    assert!(here.stderr.contains(NOTHING_BACK_LINE), "{here:?}");
    assert_eq!(command_lines(&here.stdout), ["test"], "the words that ran lack --no-build: {here:?}");
    assert!(
        took_heavy(&case, r".\dotnet.cmd test"),
        "a dropped light run takes a heavy slot: {:#?}",
        events(&case)
    );
}

#[test]
fn a_build_here_clears_the_record_only_when_it_passes() {
    let case = routed_case("routing-nothing-back-cleared");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");

    let failed = finish(case.gate(&run_words("failed.log", &[], &[r".\dotnet.cmd", "build", "bad"])));
    let dropped = finish(case.gate(&run_words("dropped.log", &[], &NO_BUILD_TEST)));
    let passed = finish(case.gate(&run_words("passed.log", &[], &[r".\dotnet.cmd", "build"])));
    let cleared = nothing_back(&case);
    let kept = finish(case.gate(&run_words("kept.log", &[], &NO_BUILD_TEST)));

    assert_eq!(failed.code, Some(1), "{failed:?}");
    assert!(
        dropped.stderr.contains(NOTHING_BACK_LINE),
        "a failed build here leaves the record: {dropped:?}"
    );
    assert_eq!(command_lines(&dropped.stdout), ["test"], "{dropped:?}");
    assert_eq!(passed.code, Some(0), "{passed:?}");
    assert_eq!(cleared, None, "a passing build here clears the record: {passed:?}");
    assert!(!kept.stderr.contains(DROPPED), "{kept:?}");
    assert_eq!(command_lines(&kept.stdout), ["test --no-build"], "{kept:?}");
}

#[test]
fn only_a_passing_build_of_the_whole_worktree_clears_the_record() {
    let case = routed_case("routing-nothing-back-whole");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");

    let one = finish(case.gate(&run_words("one.log", &[], &[r".\dotnet.cmd", "build", "One.csproj"])));
    let after_one = nothing_back(&case);
    let project = finish(case.gate(&run_words("project.log", &[], &[r".\dotnet.cmd", "run", "--project", "One"])));
    let after_project = nothing_back(&case);
    let whole = finish(case.gate(&run_words("whole.log", &[], &[r".\dotnet.cmd", "build"])));
    let after_whole = nothing_back(&case);
    routed_build(&case, "routed-again.log");
    let solution = finish(case.gate(&run_words("solution.log", &[], &[r".\dotnet.cmd", "build", "All.sln"])));

    assert_eq!((one.code, project.code, whole.code, solution.code), (Some(0), Some(0), Some(0), Some(0)));
    assert_eq!(
        after_one.as_deref(),
        Some(r#"{"v":1,"host":"builder","built":["."]}"#),
        "a build of one project leaves it, listing the project built: {one:?}"
    );
    assert_eq!(
        after_project.as_deref(),
        Some(r#"{"v":1,"host":"builder","built":[".","one"]}"#),
        "--project leaves it, listing the project built: {project:?}"
    );
    assert_eq!(after_whole, None, "a build naming no project clears it: {whole:?}");
    assert_eq!(nothing_back(&case), None, "a build of a solution clears it: {solution:?}");
}

/// The record a routed `dotnet build -c Release` on [`HOST`] that brought nothing back leaves.
const RELEASE_RECORD: &str = r#"{"v":1,"host":"builder","configuration":"Release"}"#;

/// Runs a routed `dotnet build -c Release` the scripted client passes, and asserts the record it leaves.
fn routed_release_build(case: &Case) {
    let steps = shown(&stub(case, &["status 0"]));
    let routed = finish(case.gate(&routed_words("routed.log", &steps, &[], &[r".\dotnet.exe", "build", "-c", "Release"])));
    assert_eq!(routed.code, Some(0), "{routed:?}");
    assert_eq!(nothing_back(case).as_deref(), Some(RELEASE_RECORD), "{routed:?}");
}

/// A case with the restored projects `One` and `Two` tracked, the stand-in dotnet, and the record a routed
/// `dotnet build -c Release` left.
fn project_case(name: &str) -> Case {
    let case = routed_case(name);
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    tracked_project(&case, "One/One.csproj", true);
    tracked_project(&case, "Two/Two.csproj", true);
    routed_release_build(&case);
    case
}

#[test]
fn a_passing_local_build_of_a_project_lets_its_no_build_run_keep_the_word() {
    let case = project_case("routing-nothing-back-project");

    let build = finish(case.gate(&run_words(
        "build.log",
        &[],
        &[r".\dotnet.cmd", "build", r"One\One.csproj", "-c", "Release"],
    )));
    let after_build = nothing_back(&case);
    let kept = finish(case.gate(&run_words(
        "kept.log",
        &[],
        &[r".\dotnet.cmd", "run", "--project", "One", "-c", "Release", "--no-build"],
    )));

    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(
        after_build.as_deref(),
        Some(r#"{"v":1,"host":"builder","configuration":"Release","built":["one"]}"#),
        "{build:?}"
    );
    assert_eq!(kept.code, Some(0), "{kept:?}");
    assert!(!kept.stderr.contains(DROPPED), "{kept:?}");
    assert_eq!(command_lines(&kept.stdout), ["run --project One -c Release --no-build"], "{kept:?}");
}

#[test]
fn a_passing_local_build_of_a_project_folder_lets_its_no_build_run_keep_the_word() {
    let case = project_case("routing-nothing-back-project-folder");

    let build = finish(case.gate(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", "One", "-c", "Release"])));
    let after_build = nothing_back(&case);
    let kept = finish(case.gate(&run_words(
        "kept.log",
        &[],
        &[r".\dotnet.cmd", "run", "--project", "One", "-c", "Release", "--no-build"],
    )));
    let missing = finish(case.gate(&run_words("missing.log", &[], &[r".\dotnet.cmd", "build", "Gone", "-c", "Release"])));
    let after_missing = nothing_back(&case);

    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(
        after_build.as_deref(),
        Some(r#"{"v":1,"host":"builder","configuration":"Release","built":["one"]}"#),
        "{build:?}"
    );
    assert_eq!(kept.code, Some(0), "{kept:?}");
    assert!(!kept.stderr.contains(DROPPED), "{kept:?}");
    assert_eq!(command_lines(&kept.stdout), ["run --project One -c Release --no-build"], "{kept:?}");
    assert_eq!(missing.code, Some(0), "{missing:?}");
    assert_eq!(after_missing, after_build, "a word naming no folder adds nothing: {missing:?}");
}

#[test]
fn a_no_build_run_under_other_settings_keeps_its_word_under_another_configuration() {
    let case = project_case("routing-nothing-back-listed-other-settings");

    let build = finish(case.gate(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", "One", "-c", "Release"])));
    let after_build = nothing_back(&case);
    let plain = finish(case.gate(&run_words("plain.log", &[], &[r".\dotnet.cmd", "run", "--project", "One", "--no-build"])));
    let debug = finish(case.gate(&run_words(
        "debug.log",
        &[],
        &[r".\dotnet.cmd", "run", "--project", "One", "-c", "Debug", "--no-build"],
    )));

    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(
        after_build.as_deref(),
        Some(r#"{"v":1,"host":"builder","configuration":"Release","built":["one"]}"#),
        "{build:?}"
    );
    assert!(plain.stderr.contains(NOTHING_BACK_LINE), "no configuration: {plain:?}");
    assert_eq!(command_lines(&plain.stdout), ["run --project One"], "{plain:?}");
    assert_eq!(debug.code, Some(0), "{debug:?}");
    assert!(!debug.stderr.contains(DROPPED), "a Release record says nothing of bin\\Debug: {debug:?}");
    assert_eq!(command_lines(&debug.stdout), ["run --project One -c Debug --no-build"], "{debug:?}");
}

/// A record a routed `-c Debug` build left says nothing of the PC's `bin\Release`: a `-c Release` `--no-build` run here
/// of a restored project keeps its word, as a routed run's send does for a record of another configuration, while a run
/// that sets no configuration still drops it. A record set aside is no record, so the obj check still decides.
#[test]
fn a_record_of_another_configuration_lets_a_no_build_run_here_keep_the_word() {
    let case = project_case("routing-nothing-back-other-configuration");
    tracked_project(&case, "Three/Three.csproj", false);
    rewrite_record(&case, r#"{"v":1,"host":"builder","configuration":"Debug"}"#);

    let release = finish(case.gate(&run_words(
        "release.log",
        &[],
        &[r".\dotnet.cmd", "test", "--project", "One", "-c", "Release", "--no-build"],
    )));
    let plain = finish(case.gate(&run_words("plain.log", &[], &[r".\dotnet.cmd", "test", "--project", "One", "--no-build"])));
    let unrestored = finish(case.gate(&run_words(
        "unrestored.log",
        &[],
        &[r".\dotnet.cmd", "test", "--project", "Three", "-c", "Release", "--no-build"],
    )));

    assert_eq!(release.code, Some(0), "{release:?}");
    assert!(
        !release.stderr.contains(DROPPED),
        "a Debug record says nothing of bin\\Release: {release:?}"
    );
    assert_eq!(
        command_lines(&release.stdout),
        ["test --project One -c Release --no-build"],
        "{release:?}"
    );
    assert!(
        plain.stderr.contains(NOTHING_BACK_LINE),
        "a run naming no configuration may build Debug: {plain:?}"
    );
    assert_eq!(command_lines(&plain.stdout), ["test --project One"], "{plain:?}");
    assert!(
        !unrestored.stderr.contains(NOTHING_BACK_LINE),
        "a record set aside is no reason to drop: {unrestored:?}"
    );
    assert!(
        unrestored.stderr.contains(r"has no obj\project.assets.json here"),
        "the obj check decides a run of another project: {unrestored:?}"
    );
    assert_eq!(command_lines(&unrestored.stdout), ["test --project Three -c Release"], "{unrestored:?}");
}

#[test]
fn a_record_of_another_letter_case_names_the_same_configuration() {
    let case = project_case("routing-nothing-back-letter-case");
    rewrite_record(&case, r#"{"v":1,"host":"builder","configuration":"release"}"#);

    let release = finish(case.gate(&run_words(
        "release.log",
        &[],
        &[r".\dotnet.cmd", "test", "--project", "One", "-c", "Release", "--no-build"],
    )));

    assert_eq!(release.code, Some(0), "{release:?}");
    assert!(
        release.stderr.contains(NOTHING_BACK_LINE),
        "another letter case is the same configuration: {release:?}"
    );
    assert_eq!(command_lines(&release.stdout), ["test --project One -c Release"], "{release:?}");
}

#[test]
fn a_routed_build_of_another_configuration_leaves_a_record_that_drops_every_run() {
    let case = routed_case("routing-nothing-back-two-configurations");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);

    let debug_steps = shown(&stub(&case, &["status 0"]));
    let debug = finish(case.gate(&routed_words("debug.log", &debug_steps, &[], &[r".\dotnet.exe", "build", "-c", "Debug"])));
    let first = nothing_back(&case);
    let release_steps = shown(&stub(&case, &["status 0"]));
    let release = finish(case.gate(&routed_words(
        "release.log",
        &release_steps,
        &[],
        &[r".\dotnet.exe", "build", "-c", "Release"],
    )));
    let second = nothing_back(&case);
    let dropped = finish(case.gate(&run_words("dropped.log", &[], &[r".\dotnet.cmd", "test", "-c", "Debug", "--no-build"])));

    assert_eq!(debug.code, Some(0), "{debug:?}");
    assert_eq!(first.as_deref(), Some(r#"{"v":1,"host":"builder","configuration":"Debug"}"#), "{debug:?}");
    assert_eq!(release.code, Some(0), "{release:?}");
    assert_eq!(
        second.as_deref(),
        Some(NOTHING_BACK_RECORD),
        "another configuration names none: {release:?}"
    );
    assert!(dropped.stderr.contains(NOTHING_BACK_LINE), "{dropped:?}");
    assert_eq!(command_lines(&dropped.stdout), ["test -c Debug"], "{dropped:?}");
}

#[test]
fn a_project_build_that_moves_its_output_adds_nothing() {
    let case = project_case("routing-nothing-back-moved-output");
    let rows: [&[&str]; 8] = [
        &[r".\dotnet.cmd", "build", "One", "-c", "Release", "-o", "out"],
        &[r".\dotnet.cmd", "build", "One", "-c", "Release", "--artifacts-path:art"],
        &[r".\dotnet.cmd", "build", "One", "-c", "Release", "-p:outputpath=x"],
        &[r".\dotnet.cmd", "build", "One", "-c", "Release", "-p:Platform=x64"],
        &[r".\dotnet.cmd", "build", "One", "-c", "Release", r"-p:OutDir=..\out\"],
        &[r".\dotnet.cmd", "build", "One", "-c", "Release", "-p:UseArtifactsOutput=true"],
        &[r".\dotnet.cmd", "build", "One", "-c", "Release", "-a", "x64"],
        &[r".\dotnet.cmd", "build", "One", "-c", "Release", "-t:Clean"],
    ];

    for (index, command) in rows.into_iter().enumerate() {
        let build = finish(case.gate(&run_words(&format!("build-{index}.log"), &[], command)));
        assert_eq!(build.code, Some(0), "{build:?}");
        assert_eq!(
            nothing_back(&case).as_deref(),
            Some(RELEASE_RECORD),
            "{command:?} adds nothing: {build:?}"
        );
    }
}

#[test]
fn a_project_build_adds_nothing_while_another_run_holds_the_work_folder_key() {
    let case = project_case("routing-nothing-back-key-held");
    let path = record_path(&case).expect("the case has a record");
    let key = path.file_stem().expect("the record has a name").to_string_lossy().into_owned();
    let name = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::Routed { hash: key_hash(&key) });
    let mutex = NamedMutex::open(&name).expect("the key's mutex opens");
    let held = mutex.acquire(Duration::from_secs(5)).expect("the key's mutex is free");

    let build = finish(case.gate(&run_words(
        "build.log",
        &["--test-key-wait-ms", "200"],
        &[r".\dotnet.cmd", "build", "One", "-c", "Release"],
    )));
    drop(held);

    assert_eq!(build.code, Some(0), "{build:?}");
    assert!(build.stderr.contains(&format!("gate: cannot add to {}: ", path.display())), "{build:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{build:?}");
}

#[test]
fn a_no_build_run_of_another_project_still_drops_its_word() {
    let case = project_case("routing-nothing-back-other-project");

    let build = finish(case.gate(&run_words(
        "build.log",
        &[],
        &[r".\dotnet.cmd", "build", r"One\One.csproj", "-c", "Release"],
    )));
    let dropped = finish(case.gate(&run_words(
        "dropped.log",
        &[],
        &[r".\dotnet.cmd", "run", "--project", "Two", "-c", "Release", "--no-build"],
    )));

    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(dropped.code, Some(0), "{dropped:?}");
    assert!(dropped.stderr.contains(NOTHING_BACK_LINE), "{dropped:?}");
    assert_eq!(command_lines(&dropped.stdout), ["run --project Two -c Release"], "{dropped:?}");
}

#[test]
fn a_project_build_with_other_settings_adds_nothing() {
    let case = project_case("routing-nothing-back-project-debug");

    let build = finish(case.gate(&run_words(
        "build.log",
        &[],
        &[r".\dotnet.cmd", "build", r"One\One.csproj", "-c", "Debug"],
    )));
    let after_build = nothing_back(&case);
    let dropped = finish(case.gate(&run_words(
        "dropped.log",
        &[],
        &[r".\dotnet.cmd", "run", "--project", "One", "-c", "Release", "--no-build"],
    )));

    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(
        after_build.as_deref(),
        Some(RELEASE_RECORD),
        "another configuration adds nothing: {build:?}"
    );
    assert!(dropped.stderr.contains(NOTHING_BACK_LINE), "{dropped:?}");
    assert_eq!(command_lines(&dropped.stdout), ["run --project One -c Release"], "{dropped:?}");
}

#[test]
fn a_routed_build_that_leaves_nothing_back_starts_built_empty() {
    let case = routed_case("routing-nothing-back-built-empty");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    tracked_project(&case, "One/One.csproj", true);
    routed_build(&case, "routed.log");

    let build = finish(case.gate(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", r"One\One.csproj"])));
    let after_build = nothing_back(&case);
    routed_build(&case, "routed-again.log");
    let dropped = finish(case.gate(&run_words(
        "dropped.log",
        &[],
        &[r".\dotnet.cmd", "run", "--project", "One", "--no-build"],
    )));

    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(after_build.as_deref(), Some(r#"{"v":1,"host":"builder","built":["one"]}"#), "{build:?}");
    assert!(
        dropped.stderr.contains(NOTHING_BACK_LINE),
        "the fresh record lists no project: {dropped:?}"
    );
    assert_eq!(command_lines(&dropped.stdout), ["run --project One"], "{dropped:?}");
}

#[test]
fn a_routed_build_that_falls_back_here_writes_no_record_and_its_passing_build_clears_one() {
    let case = routed_case("routing-nothing-back-fallback");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    let steps = shown(&stub(&case, &["refuse busy"]));
    // The fallback runs fake-tool as `dotnet.exe build`, which fails on the word it does not take: no build clears.
    let failed = finish(case.gate(&routed_words("failed.log", &steps, &[], &[r".\dotnet.exe", "build"])));
    let wrote = nothing_back(&case);
    routed_build(&case, "routed.log");
    let steps = shown(&stub(&case, &["refuse busy"]));
    let passed = finish(case.gate(&routed_words("passed.log", &steps, &[], &[r".\dotnet.cmd", "build"])));

    assert!(failed.stderr.contains(BUSY_LINE), "{failed:?}");
    assert_eq!(wrote, None, "{failed:?}");
    assert!(passed.stderr.contains(BUSY_LINE), "{passed:?}");
    assert_eq!(passed.code, Some(0), "{passed:?}");
    assert_eq!(nothing_back(&case), None, "a passing fallback build clears the record: {passed:?}");
}

/// The path of the case worktree's built-here record, under the key of its root as git gives it.
fn built_here_path(case: &Case) -> PathBuf {
    let key = work_folder_key(&git(case, &["rev-parse", "--show-toplevel"]));
    case.root().join("remote").join(format!("{key}.built-here"))
}

/// The text of the case worktree's built-here record; `None` when there is none.
fn built_here(case: &Case) -> Option<String> {
    let path = built_here_path(case);
    path.exists().then(|| fs::read_to_string(&path).expect("the record can be read"))
}

/// Writes `text` as the case worktree's built-here record.
fn seed_built_here(case: &Case, text: &str) {
    let path = built_here_path(case);
    fs::create_dir_all(path.parent().expect("the record has a folder")).expect("the record's folder can be made");
    fs::write(path, text).expect("the record can be written");
}

/// A case with the stand-in dotnet and the restored projects `One` and `Two` tracked, and no record of any kind.
fn built_here_case(name: &str) -> Case {
    let case = routed_case(name);
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    tracked_project(&case, "One/One.csproj", true);
    tracked_project(&case, "Two/Two.csproj", true);
    case
}

#[test]
fn a_passing_local_build_records_its_project_as_built_here() {
    let case = built_here_case("routing-built-here-recorded");
    let outputs_from = built_here_path(&case).with_extension("outputs-from");
    fs::create_dir_all(outputs_from.parent().expect("the record has a folder")).expect("the record's folder can be made");
    fs::write(&outputs_from, "builder").expect("the outputs-from record can be written");

    let project = finish(case.gate(&run_words(
        "project.log",
        &[],
        &[r".\dotnet.cmd", "build", r"One\One.csproj", "-c", "Release"],
    )));
    let after_project = built_here(&case);
    let outputs_from_left = outputs_from.exists();
    let unnamed = finish(case.gate(&run_words("unnamed.log", &[], &[r".\dotnet.cmd", "build"])));
    let after_unnamed = built_here(&case);
    fs::write(case.folder().join("App.sln"), APP_SLN).expect("the solution can be written");
    let solution = finish(case.gate(&run_words("solution.log", &[], &[r".\dotnet.cmd", "build", "App.sln", "-c", "Debug"])));
    let after_solution = built_here(&case);
    let whole = finish(case.gate(&run_words("whole.log", &[], &[r".\dotnet.cmd", "build"])));

    let codes = [project.code, unnamed.code, solution.code, whole.code];
    assert_eq!(codes, [Some(0); 4], "{project:?} {unnamed:?} {solution:?} {whole:?}");
    assert_eq!(
        after_project.as_deref(),
        Some(r#"{"v":1,"projects":{"one":{"configuration":"Release","seq":1}}}"#),
        "{project:?}"
    );
    assert!(!outputs_from_left, "a passing build here removes the outputs-from record: {project:?}");
    assert_eq!(
        after_unnamed, after_project,
        "a folder with no project or solution file records nothing: {unnamed:?}"
    );
    assert_eq!(
        after_solution.as_deref(),
        Some(r#"{"v":1,"projects":{"one":{"configuration":"Debug","seq":2},"two":{"configuration":"Debug","seq":2}}}"#),
        "a solution build records each project it lists: {solution:?}"
    );
    assert_eq!(
        built_here(&case).as_deref(),
        Some(r#"{"v":1,"projects":{"one":{"seq":3},"two":{"seq":3}}}"#),
        "a build naming nothing in the folder of one solution records its projects: {whole:?}"
    );
}

/// A solution at the case root listing the projects `One` and `Two`.
const APP_SLN: &str = "Microsoft Visual Studio Solution File, Format Version 12.00\r\n\
                       Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"One\", \"One\\One.csproj\", \
                       \"{11111111-1111-1111-1111-111111111111}\"\r\n\
                       EndProject\r\n\
                       Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Two\", \"Two\\Two.csproj\", \
                       \"{22222222-2222-2222-2222-222222222222}\"\r\n\
                       EndProject\r\n";

/// The case worktree's work-folder key and the name of its key lock's mutex.
fn key_lock(case: &Case) -> (String, String) {
    let key = work_folder_key(&git(case, &["rev-parse", "--show-toplevel"]));
    let name = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::Routed { hash: key_hash(&key) });
    (key, name)
}

/// The key stays held past the build's brief retry, so the build records nothing; it then waits for the key to remove
/// the outputs-from record, which is still there until the case lets the key go.
#[test]
fn a_local_build_while_the_work_folder_key_is_held_records_nothing_and_removes_outputs_from_once_the_key_is_let_go() {
    let case = built_here_case("routing-built-here-key-held");
    fs::write(case.folder().join("App.sln"), APP_SLN).expect("the solution can be written");
    let outputs_from = built_here_path(&case).with_extension("outputs-from");
    fs::create_dir_all(outputs_from.parent().expect("the record has a folder")).expect("the record's folder can be made");
    fs::write(&outputs_from, "builder").expect("the outputs-from record can be written");
    let (key, name) = key_lock(&case);
    let mutex = NamedMutex::open(&name).expect("the key's mutex opens");
    let held = mutex.acquire(Duration::from_secs(5)).expect("the key's mutex is free");
    let line = format!("gate: built-here record not written: work folder {key} is held");

    let build = case.start(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", "App.sln"]));
    build.wait_until("the skipped write's line", || {
        fs::read_to_string(case.folder().join("build.log")).is_ok_and(|text| text.contains(&line))
    });
    let left_while_held = outputs_from.exists();
    drop(held);
    let build = build.finish();

    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(built_here(&case), None, "{build:?}");
    let logged = log_text(&case, "build.log");
    assert!(logged.lines().any(|said| said.ends_with(&line)), "{logged}");
    assert!(!build.stderr.contains(&line), "said in the log only: {build:?}");
    assert!(left_while_held, "removed only under the key: {build:?}");
    assert!(!outputs_from.exists(), "removed once the key was let go: {build:?}");
}

/// A holder that lets the key go while the build retries its take, as a local write does after milliseconds: the
/// build's entry is written. The case lets the key go once the take has timed out a first slice and is retrying, which
/// the reached seam says, and the key-wait seam stretches the retry past any delay of the case's.
#[test]
fn a_local_build_records_its_project_once_a_brief_holder_lets_the_work_folder_key_go() {
    let case = built_here_case("routing-built-here-key-brief");
    let (_, name) = key_lock(&case);
    let mutex = NamedMutex::open(&name).expect("the key's mutex opens");
    let held = mutex.acquire(Duration::from_secs(5)).expect("the key's mutex is free");
    let reached = case.folder().join("key-reached");
    let reached_arg = shown(&reached);

    let build = case.start(&run_words(
        "build.log",
        &["--test-key-wait-ms", "600000", "--test-built-here-key-reached", &reached_arg],
        &[r".\dotnet.cmd", "build", r"One\One.csproj"],
    ));
    // The skipped write's line ends the wait too, so a take that gave up fails the case at once.
    build.wait_until("the take's first timed-out slice", || {
        reached.exists() || fs::read_to_string(case.folder().join("build.log")).is_ok_and(|text| text.contains("built-here record not written"))
    });
    drop(held);
    let build = build.finish();

    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(built_here(&case).as_deref(), Some(r#"{"v":1,"projects":{"one":{"seq":1}}}"#), "{build:?}");
    assert!(!log_text(&case, "build.log").contains("built-here record not written"), "{build:?}");
}

/// The start of the line a passing build here says before it waits for the work folder's key to remove the
/// outputs-from record.
const KEY_WAIT_TO_REMOVE: &str = "gate: waiting for the work folder's key to remove ";

/// Plants the case worktree's outputs-from record; its path.
fn planted_outputs_from(case: &Case) -> PathBuf {
    let outputs_from = built_here_path(case).with_extension("outputs-from");
    fs::create_dir_all(outputs_from.parent().expect("the record has a folder")).expect("the record's folder can be made");
    fs::write(&outputs_from, "builder").expect("the outputs-from record can be written");
    outputs_from
}

/// The key stays held past both bounds, which the seam makes short, while a drain marker stands: the wait for the key
/// still ends at its bound, and the outputs-from record is removed without the key.
#[test]
fn a_local_build_s_wait_for_a_held_key_ends_at_its_bound_with_a_drain_marker_standing_and_removes_outputs_from_without_it() {
    use std::os::windows::fs::OpenOptionsExt;

    let case = built_here_case("routing-built-here-key-bound");
    let outputs_from = planted_outputs_from(&case);
    let marker = marker_folder(&case, None).join("1-0");
    fs::create_dir_all(marker.parent().expect("the marker has a folder")).expect("the marker folder can be made");
    let standing = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(0)
        .open(&marker)
        .expect("the marker can be held");
    let (_, name) = key_lock(&case);
    let mutex = NamedMutex::open(&name).expect("the key's mutex opens");
    let held = mutex.acquire(Duration::from_secs(5)).expect("the key's mutex is free");

    let build = case.start(&run_words(
        "build.log",
        &["--test-key-wait-ms", "300"],
        &[r".\dotnet.cmd", "build", r"One\One.csproj"],
    ));
    // A wait the marker holds off says so and goes on waiting: either line ends the case's wait.
    build.wait_until("the wait for the key ending or being held off", || {
        fs::read_to_string(case.folder().join("build.log"))
            .is_ok_and(|text| text.contains("without the work folder's key") || text.contains("behind a builder drain"))
    });
    let held_off = fs::read_to_string(case.folder().join("build.log")).is_ok_and(|text| text.contains("behind a builder drain"));
    drop(held);
    drop(standing);
    let build = build.finish();

    assert!(!held_off, "the drain marker held the bound off: {build:?}");
    let logged = log_text(&case, "build.log");
    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(built_here(&case), None, "{build:?}");
    assert_eq!(logged.lines().filter(|line| line.contains(KEY_WAIT_TO_REMOVE)).count(), 1, "{logged}");
    assert!(
        logged
            .lines()
            .any(|line| line.contains("gate: removed ") && line.ends_with("outputs-from without the work folder's key: held for 300 ms")),
        "{logged}"
    );
    assert!(!logged.contains("behind a builder drain"), "{logged}");
    assert!(!build.stderr.contains(KEY_WAIT_TO_REMOVE), "said in the log only: {build:?}");
    assert!(!outputs_from.exists(), "{build:?}");
}

/// One passing build of a project that removes the outputs-from record and adds the project to a standing nothing-back
/// record waits for the held key once, for both.
#[test]
fn a_local_build_that_removes_outputs_from_and_adds_to_nothing_back_waits_for_the_held_key_once() {
    let case = built_here_case("routing-built-here-key-once");
    let outputs_from = planted_outputs_from(&case);
    let record = built_here_path(&case).with_extension("nothing-back");
    fs::write(&record, NOTHING_BACK_RECORD).expect("the nothing-back record can be written");
    let (_, name) = key_lock(&case);
    let mutex = NamedMutex::open(&name).expect("the key's mutex opens");
    let held = mutex.acquire(Duration::from_secs(5)).expect("the key's mutex is free");
    let waits = case.folder().join("key-waits");

    let build = case.start(&run_words(
        "build.log",
        &["--test-key-waits", &shown(&waits)],
        &[r".\dotnet.cmd", "build", r"One\One.csproj"],
    ));
    build.wait_until("the wait for the key", || {
        fs::read_to_string(case.folder().join("build.log")).is_ok_and(|text| text.contains(KEY_WAIT_TO_REMOVE))
    });
    drop(held);
    let build = build.finish();

    let logged = log_text(&case, "build.log");
    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(logged.lines().filter(|line| line.contains(KEY_WAIT_TO_REMOVE)).count(), 1, "{logged}");
    assert_eq!(
        fs::read_to_string(&waits).ok().as_deref(),
        Some("wait\n"),
        "the key is waited for once: {logged}"
    );
    assert_eq!(
        fs::read_to_string(&record).ok().as_deref(),
        Some(r#"{"v":1,"host":"builder","built":["one"]}"#),
        "{build:?}"
    );
    assert!(!outputs_from.exists(), "{build:?}");
    assert!(!logged.contains("without the work folder's key"), "{logged}");
}

/// A routed `--no-build` run whose word the route drops builds on the builder, so it removes the record: the next
/// routed `--no-build` run of a project built here earlier plans no send.
#[test]
fn a_routed_no_build_run_whose_word_is_dropped_removes_the_built_here_record() {
    let case = built_here_case("routing-built-here-dropped-clears");

    let build = finish(case.gate(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", "One"])));
    let recorded_here = built_here(&case);
    let steps = shown(&stub(&case, &["status 0"]));
    let other = finish(case.gate(&routed_words("other.log", &steps, &[], &[r".\dotnet.cmd", "test", "Two", "--no-build"])));
    let after_other = built_here(&case);
    let steps = shown(&stub(&case, &["status 0"]));
    let again = finish(case.gate(&routed_words("again.log", &steps, &[], &[r".\dotnet.cmd", "test", "One", "--no-build"])));

    assert_eq!([build.code, other.code, again.code], [Some(0); 3], "{build:?} {other:?} {again:?}");
    assert!(recorded_here.is_some(), "{build:?}");
    assert!(other.stderr.contains(DROPPED_NO_BUILD_LINE), "{other:?}");
    assert_eq!(after_other, None, "the builder built: {other:?}");
    assert!(again.stderr.contains(DROPPED_NO_BUILD_LINE), "no send planned: {again:?}");
    assert!(!again.stderr.contains(DROPPED_UNSENT_LINE), "{again:?}");
}

/// A routed run that would build on the builder while its built-here record cannot be removed, held open without delete
/// sharing, is not sent: it falls back here `inputs`, saying why first, and the builder is never called.
#[test]
fn a_routed_build_whose_built_here_record_cannot_be_removed_falls_back_here() {
    use std::os::windows::fs::OpenOptionsExt;

    let case = built_here_case("routing-built-here-clear-fails");
    seed_built_here(&case, r#"{"v":1,"projects":{"two":{}}}"#);
    let open = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_WRITE)
        .open(built_here_path(&case))
        .expect("the record can be held");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let routed = case.start(&routed_words("routed.log", &steps, &[], &[r".\dotnet.cmd", "test", "One", "--no-build"]));
    let pid = routed.id();
    let routed = routed.finish();
    drop(open);

    let not_sent = "gate: remote build not sent: cannot remove the built-here record: ";
    assert_eq!(routed.code, Some(0), "{routed:?}");
    let said: Vec<&str> = routed.stderr.lines().collect();
    let first = said.iter().position(|line| line.starts_with(not_sent));
    let then = said
        .iter()
        .position(|line| line.starts_with("gate: remote ") && line.ends_with(" inputs; running here"));
    assert!(matches!((first, then), (Some(first), Some(then)) if first < then), "{routed:?}");
    assert!(!call.exists(), "the builder is never called: {routed:?}");
    assert!(built_here(&case).is_some(), "{routed:?}");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "fallback", "take", "release"], "{own:#?}");
    assert_eq!(own[1]["reason"], "inputs", "{own:#?}");
}

/// The sharing of a handle that lets others read and write a file but not delete it.
const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;

/// A `--no-build` test of a solution keeps its word, here or routed, only when each project the solution lists has an
/// entry; with one of them missing it drops the word.
#[test]
fn a_no_build_test_of_a_solution_keeps_the_word_only_when_every_project_it_lists_was_built_here() {
    let case = built_here_case("routing-built-here-solution");
    fs::write(case.folder().join("App.sln"), APP_SLN).expect("the solution can be written");
    let record = built_here_path(&case).with_extension("nothing-back");
    fs::create_dir_all(record.parent().expect("the record has a folder")).expect("the record's folder can be made");
    fs::write(&record, NOTHING_BACK_RECORD).expect("the nothing-back record can be written");
    let command = [r".\dotnet.cmd", "test", "App.sln", "--no-build"];
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let some = r#"{"v":1,"projects":{"one":{}}}"#;

    seed_built_here(&case, some);
    let some_here = finish(case.gate(&run_words("some-here.log", &[], &command)));
    seed_built_here(&case, some);
    let some_routed = finish(case.gate(&routed_words("some-routed.log", &steps, &[], &command)));
    let some_routed_call = recorded(&call);
    fs::remove_file(&call).expect("the recorded call can be removed");
    seed_built_here(&case, r#"{"v":1,"projects":{"one":{},"two":{}}}"#);
    let all_here = finish(case.gate(&run_words("all-here.log", &[], &command)));
    let all_routed = finish(case.gate(&routed_words("all-routed.log", &steps, &[], &command)));

    let codes = [some_here.code, some_routed.code, all_here.code, all_routed.code];
    assert_eq!(codes, [Some(0); 4], "{some_here:?} {some_routed:?} {all_here:?} {all_routed:?}");
    assert!(some_here.stderr.contains(DROPPED), "{some_here:?}");
    assert_eq!(command_lines(&some_here.stdout), ["test App.sln"], "{some_here:?}");
    assert!(some_routed.stderr.contains(DROPPED_NO_BUILD_LINE), "{some_routed:?}");
    assert!(!recorded_words(&some_routed_call).contains(&"--no-build"), "{some_routed_call}");
    assert!(!all_here.stderr.contains(DROPPED), "{all_here:?}");
    assert_eq!(command_lines(&all_here.stdout), ["test App.sln --no-build"], "{all_here:?}");
    assert!(
        all_routed
            .stderr
            .contains("gate: not routed: the last build of App.sln ran here; --no-build kept"),
        "{all_routed:?}"
    );
    assert!(!call.exists(), "the builder is never called: {all_routed:?}");
    assert_eq!(command_lines(&all_routed.stdout), ["test App.sln --no-build"], "{all_routed:?}");
}

/// A local build naming nothing in a folder holding one project records that project, so a routed `dotnet run
/// --no-build` there plans the send of the PC's build, which the scripted client never makes.
#[test]
fn a_routed_no_build_run_in_a_one_project_folder_built_here_plans_the_send() {
    let case = built_here_case("routing-built-here-folder");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));

    let build = finish(case.gate_in("One", &run_words("build.log", &[], &[r"..\dotnet.cmd", "build"])));
    let recorded_here = built_here(&case);
    let sent = finish(case.gate_in("One", &routed_words("sent.log", &steps, &[], &[r"..\dotnet.cmd", "run", "--no-build"])));

    assert_eq!((build.code, sent.code), (Some(0), Some(0)), "{build:?} {sent:?}");
    assert_eq!(recorded_here.as_deref(), Some(r#"{"v":1,"projects":{"one":{"seq":1}}}"#), "{build:?}");
    assert!(!sent.stderr.contains("gate: not routed"), "{sent:?}");
    assert!(sent.stderr.contains(DROPPED_UNSENT_LINE), "a send was planned: {sent:?}");
    assert!(!sent.stderr.contains(DROPPED_NO_BUILD_LINE), "{sent:?}");
}

/// A routed `dotnet test --no-build` naming no project, in a folder holding one project built here, plans the send of
/// that project, as a `dotnet run` there does, and does not run here.
#[test]
fn a_routed_no_build_test_in_a_one_project_folder_built_here_plans_the_send() {
    let case = built_here_case("routing-built-here-folder-test");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));

    let build = finish(case.gate_in("One", &run_words("build.log", &[], &[r"..\dotnet.cmd", "build"])));
    let sent = finish(case.gate_in("One", &routed_words("sent.log", &steps, &[], &[r"..\dotnet.cmd", "test", "--no-build"])));

    assert_eq!((build.code, sent.code), (Some(0), Some(0)), "{build:?} {sent:?}");
    assert!(!sent.stderr.contains("gate: not routed"), "{sent:?}");
    assert!(sent.stderr.contains(DROPPED_UNSENT_LINE), "a send was planned: {sent:?}");
    assert!(call.exists(), "the builder runs it: {sent:?}");
}

/// A repo the routing file's `sendBuild` lists reads the built-here record too: its `dotnet run --no-build`, which the
/// listed rule sends nothing of, plans the send of the PC's build after a build of its project here.
#[test]
fn a_listed_repo_s_no_build_run_built_here_plans_the_send() {
    let case = built_here_case("routing-built-here-listed");
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["*tool*", "*dotnet.exe*", "*dotnet.cmd*"] },
        "sendBuild": [repo(&case)],
    });
    write_remote_json(&case, &routing.to_string());
    let steps = shown(&stub(&case, &["status 0"]));

    let build = finish(case.gate(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", "One"])));
    let sent = finish(case.gate(&routed_words(
        "sent.log",
        &steps,
        &[],
        &[r".\dotnet.cmd", "run", "--project", "One", "--no-build"],
    )));

    assert_eq!((build.code, sent.code), (Some(0), Some(0)), "{build:?} {sent:?}");
    assert!(sent.stderr.contains(DROPPED_UNSENT_LINE), "a send was planned: {sent:?}");
    assert!(!sent.stderr.contains(DROPPED_NO_BUILD_LINE), "{sent:?}");
}

/// A `dotnet run --no-build` in a folder holding only a solution, whose projects were built here, gets no built-here
/// line: `dotnet run` takes no solution, so the record says nothing of it and the word is dropped.
#[test]
fn a_routed_no_build_run_in_a_folder_of_one_solution_reads_no_built_here_entry() {
    let case = built_here_case("routing-built-here-run-solution");
    fs::write(case.folder().join("App.sln"), APP_SLN).expect("the solution can be written");
    seed_built_here(&case, r#"{"v":1,"projects":{"one":{},"two":{}}}"#);
    let steps = shown(&stub(&case, &["status 0"]));

    let routed = finish(case.gate(&routed_words("routed.log", &steps, &[], &[r".\dotnet.cmd", "run", "--no-build"])));

    assert_eq!(routed.code, Some(0), "{routed:?}");
    assert!(!routed.stderr.contains("the last build of App.sln"), "{routed:?}");
    assert!(routed.stderr.contains(DROPPED_NO_BUILD_LINE), "{routed:?}");
}

#[test]
fn a_failing_local_build_records_nothing() {
    let case = built_here_case("routing-built-here-failed");
    // The stand-in dotnet fails a command whose second word is `bad`, here the folder of a project.
    tracked_project(&case, "bad/Bad.csproj", true);

    let failed = finish(case.gate(&run_words("failed.log", &[], &[r".\dotnet.cmd", "build", "bad"])));

    assert_eq!(failed.code, Some(1), "{failed:?}");
    assert_eq!(built_here(&case), None, "{failed:?}");
}

#[test]
fn an_out_of_place_local_build_records_nothing() {
    let case = built_here_case("routing-built-here-out-of-place");
    seed_built_here(&case, r#"{"v":1,"projects":{"two":{}}}"#);

    let build = finish(case.gate(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", "One", "-o", "out"])));

    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(
        built_here(&case).as_deref(),
        Some(r#"{"v":1,"projects":{"two":{}}}"#),
        "an out-of-place build leaves the entries as they are: {build:?}"
    );
}

#[test]
fn a_routed_build_of_one_project_removes_the_whole_built_here_record() {
    let case = built_here_case("routing-built-here-cleared");
    seed_built_here(&case, r#"{"v":1,"projects":{"one":{},"two":{"configuration":"Release"}}}"#);

    let steps = shown(&stub(&case, &["status 0"]));
    let project = finish(case.gate(&routed_words("project.log", &steps, &[], &[r".\dotnet.exe", "build", r"One\One.csproj"])));

    assert_eq!(project.code, Some(0), "{project:?}");
    assert_eq!(
        built_here(&case),
        None,
        "the builder may build and bring back the projects One references: {project:?}"
    );
}

/// The line a routed `--no-build` run of the project `One` says as it runs here after a build of it here.
const BUILT_HERE_ONE_LINE: &str = "gate: not routed: the last build of one ran here; --no-build kept";

/// After a local build of `One`, a routed `--no-build` run of it plans the send of the PC's build, which the scripted
/// client never makes, so it takes the drop saying the build could not be sent; one whose words the send cannot take
/// runs here and keeps the word.
#[test]
fn a_routed_no_build_run_after_a_local_build_plans_the_send_or_runs_here_when_it_cannot() {
    let case = built_here_case("routing-built-here-kept");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));

    let build = finish(case.gate(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", "One"])));
    let sent = finish(case.gate(&routed_words(
        "sent.log",
        &steps,
        &[],
        &[r".\dotnet.cmd", "run", "--project", "One", "--no-build"],
    )));

    assert_eq!((build.code, sent.code), (Some(0), Some(0)), "{build:?} {sent:?}");
    assert!(!sent.stderr.contains("gate: not routed"), "{sent:?}");
    assert!(sent.stderr.contains(DROPPED_UNSENT_LINE), "{sent:?}");
    assert!(!sent.stderr.contains(DROPPED_NO_BUILD_LINE), "{sent:?}");
    let recorded_call = recorded(&call);
    assert!(!recorded_words(&recorded_call).contains(&"--no-build"), "{recorded_call}");

    fs::remove_file(&call).expect("the recorded call can be removed");
    // The drop removed One's entry, as the builder built: a build here records it again.
    let rebuilt = finish(case.gate(&run_words("rebuilt.log", &[], &[r".\dotnet.cmd", "build", "One"])));
    assert_eq!(rebuilt.code, Some(0), "{rebuilt:?}");
    let kept = finish(case.gate(&routed_words(
        "kept.log",
        &steps,
        &[],
        &[r".\dotnet.cmd", "run", "--project", "One", "--no-build", "-o", "out"],
    )));

    assert_eq!(kept.code, Some(0), "{kept:?}");
    assert!(kept.stderr.contains(BUILT_HERE_ONE_LINE), "{kept:?}");
    assert!(!kept.stderr.contains(DROPPED), "{kept:?}");
    assert!(!call.exists(), "the builder is never called: {kept:?}");
    assert_eq!(command_lines(&kept.stdout), ["run --project One --no-build -o out"], "{kept:?}");
}

#[test]
fn a_no_build_run_whose_settings_differ_from_the_local_build_routes() {
    let case = built_here_case("routing-built-here-other-settings");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));

    let build = finish(case.gate(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", "One", "-c", "Debug"])));
    let routed = finish(case.gate(&routed_words(
        "routed.log",
        &steps,
        &[],
        &[r".\dotnet.cmd", "run", "--project", "One", "-c", "Release", "--no-build"],
    )));

    assert_eq!((build.code, routed.code), (Some(0), Some(0)), "{build:?} {routed:?}");
    assert!(!routed.stderr.contains("gate: not routed"), "{routed:?}");
    assert!(routed.stderr.contains(DROPPED_NO_BUILD_LINE), "{routed:?}");
    let call = recorded(&call);
    assert!(!recorded_words(&call).contains(&"--no-build"), "{call}");
}

#[test]
fn a_local_flag_run_after_a_local_build_keeps_the_word_despite_a_nothing_back_record() {
    let case = built_here_case("routing-built-here-local-flag");
    let nothing_back_path = built_here_path(&case).with_extension("nothing-back");
    fs::create_dir_all(nothing_back_path.parent().expect("the record has a folder")).expect("the record's folder can be made");
    fs::write(&nothing_back_path, NOTHING_BACK_RECORD).expect("the record can be written");
    seed_built_here(&case, r#"{"v":1,"projects":{"one":{}}}"#);

    let local = ["--local", LOCAL_REASON[0], LOCAL_REASON[1]];
    let kept = finish(case.gate(&heavy_words_with_limit(
        "local.log",
        &local,
        &[r".\dotnet.cmd", "run", "--project", "One", "--no-build"],
    )));
    let dropped = finish(case.gate(&heavy_words_with_limit(
        "dropped.log",
        &local,
        &[r".\dotnet.cmd", "run", "--project", "Two", "--no-build"],
    )));

    assert_eq!((kept.code, dropped.code), (Some(0), Some(0)), "{kept:?} {dropped:?}");
    assert!(!kept.stderr.contains(DROPPED), "{kept:?}");
    assert_eq!(command_lines(&kept.stdout), ["run --project One --no-build"], "{kept:?}");
    assert!(
        dropped.stderr.contains(NOTHING_BACK_LINE),
        "a project with no entry drops as today: {dropped:?}"
    );
}

#[test]
fn a_failed_or_killed_routed_build_leaves_a_record() {
    let case = routed_case("routing-nothing-back-failed");
    copy_tool(&case, "dotnet.exe");
    for (log, step) in [("failed.log", "status 1"), ("killed.log", "killed")] {
        let steps = shown(&stub(&case, &[step]));
        let routed = finish(case.gate(&routed_words(log, &steps, &[], &[r".\dotnet.exe", "build"])));
        assert_eq!(nothing_back(&case).as_deref(), Some(NOTHING_BACK_RECORD), "{step}: {routed:?}");
        fs::remove_file(record_path(&case).expect("the record is there")).expect("the record can be removed");
    }
}

/// The builder builds for a routed `--no-build` command, but a record already stands, so it is left as it is.
#[test]
fn a_routed_no_build_is_not_dropped_here_and_leaves_the_record_alone() {
    let case = routed_case("routing-nothing-back-routed-no-build");
    copy_tool(&case, "dotnet.exe");
    routed_build(&case, "routed.log");
    let earlier = r#"{"v":1,"host":"earlier"}"#;
    rewrite_record(&case, earlier);
    let steps = shown(&stub(&case, &["status 0"]));

    let outcome = finish(case.gate(&routed_words("no-build.log", &steps, &[], &[r".\dotnet.exe", "test", "--no-build"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(DROPPED_NO_BUILD_LINE), "{outcome:?}");
    assert!(!outcome.stderr.contains("the last build ran on"), "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(earlier), "{outcome:?}");
}

#[test]
fn a_heavy_local_no_build_drops_the_word() {
    let case = routed_case("routing-nothing-back-local");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");

    let local = ["--local", LOCAL_REASON[0], LOCAL_REASON[1]];
    let outcome = finish(case.gate(&heavy_words_with_limit("local.log", &local, &NO_BUILD_TEST)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(NOTHING_BACK_LINE), "{outcome:?}");
    assert_eq!(command_lines(&outcome.stdout), ["test"], "{outcome:?}");
}

#[test]
fn a_critical_local_no_build_drops_the_word_and_keeps_its_critical_slot() {
    let case = routed_case("routing-nothing-back-critical");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");

    let outcome = finish(case.gate(&critical_words("critical.log", &[], &NO_BUILD_TEST)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("gate: dropped --no-build: the last build ran on builder and brought nothing back; building on its critical slot"),
        "{outcome:?}"
    );
    assert_eq!(command_lines(&outcome.stdout), ["test"], "{outcome:?}");
    let take = events(&case)
        .into_iter()
        .rev()
        .find(|event| event["event"] == "take")
        .expect("the critical run took its slots");
    assert_eq!(
        (&take["kind"], &take["command"]),
        (&Value::from("critical"), &Value::from(r".\dotnet.cmd test")),
        "a dropped critical run holds a critical slot: {take}"
    );
}

#[test]
fn an_alone_no_build_that_would_drop_its_word_is_refused() {
    let case = routed_case("routing-nothing-back-alone");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");

    let earlier = [("alone.log", "the earlier log\n"), ("alone.log.err", "the earlier err\n")];
    for (name, text) in earlier {
        fs::write(case.folder().join(name), text).expect("an earlier log can be planted");
    }

    let outcome = finish(case.gate(&critical_words("alone.log", &["--alone"], &NO_BUILD_TEST)));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    for (name, text) in earlier {
        assert_eq!(
            fs::read_to_string(case.folder().join(name)).ok().as_deref(),
            Some(text),
            "the refusal leaves the earlier {name} in place: {outcome:?}"
        );
    }
    assert!(
        outcome.stderr.contains(
            "gate: refused: --no-build cannot stand: the last build ran on builder and brought nothing back; an --alone run measures \
             the command as given, so build first (a passing local build of the project clears the record) and run it again"
        ),
        "{outcome:?}"
    );
    assert!(command_lines(&outcome.stdout).is_empty(), "the command never ran: {outcome:?}");
    assert!(
        !events(&case).iter().any(|event| event["kind"] == "critical"),
        "nothing is journaled as a run: {:#?}",
        events(&case)
    );
    assert_eq!(
        nothing_back(&case).as_deref(),
        Some(NOTHING_BACK_RECORD),
        "the record stands: {outcome:?}"
    );
}

#[test]
fn a_light_no_build_under_a_light_parent_takes_a_heavy_slot_of_its_own() {
    let case = routed_case("routing-nothing-back-under-light");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");

    let outcome = finish(marked(&case, &run_words("nested.log", &[], &NO_BUILD_TEST), &test_marker(&case, "light")));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[NOTHING_BACK_LINE, "does not cover a heavy gate; taking a heavy slot"]);
    assert!(
        !outcome.stderr.contains("gate: running under the light slot"),
        "a gate the drop uncovered never says it runs under the parent's slot: {outcome:?}"
    );
    assert_eq!(command_lines(&outcome.stdout), ["test"], "{outcome:?}");
    assert!(took_heavy(&case, r".\dotnet.cmd test"), "{:#?}", events(&case));
}

#[test]
fn a_light_no_build_under_a_light_stand_in_runs_under_the_heavy_slot_its_own_gate_took() {
    let case = routed_case("routing-nothing-back-stand-in");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");
    // The record is left; with no routing file, no gate of the chain below is routed.
    fs::remove_file(powershell_folder(&case).join("remote.json")).expect("the routing file can be removed");
    write_counts(&case.root(), r#"{"v":1,"heavy":1}"#);
    let paths = ["b.pid", "b.release", "b-go", "g.pid", "g.release", "outer.release"].map(|name| case.folder().join(name));
    let [b_pid, b_release, b_go, g_pid, g_release, release] = &paths;
    let texts = paths.each_ref().map(|path| shown(path));
    let mut b_command = vec![FAKE_TOOL, "wait", &texts[2], "spawn-pid", &texts[3], &texts[4], GATE];
    b_command.extend(run_words("g.log", &[], &NO_BUILD_TEST));
    let mut a_command = vec![FAKE_TOOL, "spawn-pid", &texts[0], &texts[1], GATE];
    a_command.extend(heavy_words("b.log", &LOOK_OFTEN, &b_command));
    let a_words = run_words("a.log", &LOOK_OFTEN, &a_command);

    // P (heavy) > A (light) > B (heavy). P ends: A takes a light slot of its own, and B, which A's light slot does
    // not cover, takes the one heavy slot.
    let (outer, a) = orphaned_as(&case, "heavy", &a_words, release);
    let b = spawned(&case, b_pid);
    end_parent(outer, release);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, a.pid));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, b.pid));

    // B's command starts G, a light dotnet test under P's marker whose --no-build is dropped, so it runs as a heavy gate:
    // under B's heavy slot, not queued behind it as a gate A's light slot does not cover.
    let_go(b_go);
    let (g, first) = first_event(&case, g_pid);
    assert_eq!(
        (&first["event"], &first["parentPid"]),
        (&Value::from("covered"), &Value::from(b.pid)),
        "G runs under B's heavy slot: {first}"
    );
    assert_eq!(g.ended(), 0);
    let_go(g_release);
    assert_eq!(b.ended(), 0);
    let_go(b_release);
    assert_eq!(a.ended(), 0);
}

#[test]
fn an_ignored_marker_line_names_the_heavy_slot_a_dropped_light_run_takes() {
    let case = routed_case("routing-nothing-back-ignored-marker");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");

    let outcome = finish(marked(&case, &run_words("nested.log", &[], &NO_BUILD_TEST), "x:y"));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[
            NOTHING_BACK_LINE,
            "gate: ignored GATE_SLOT_HELD='x:y', which is not <kind>:<pid>:<start>; taking a heavy slot",
        ],
    );
    assert!(!outcome.stderr.contains("taking a light slot"), "{outcome:?}");
    assert!(took_heavy(&case, r".\dotnet.cmd test"), "{:#?}", events(&case));
}

#[test]
fn a_light_no_build_under_a_critical_parent_builds_on_the_parent_slot() {
    let case = routed_case("routing-nothing-back-under-critical");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");

    let outcome = finish(marked(
        &case,
        &run_words("nested.log", &[], &NO_BUILD_TEST),
        &test_marker(&case, "critical"),
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains(
            "gate: dropped --no-build: the last build ran on builder and brought nothing back; building on the parent gate's critical slot"
        ),
        "{outcome:?}"
    );
    assert_eq!(command_lines(&outcome.stdout), ["test"], "{outcome:?}");
}

#[test]
fn a_command_whose_verb_takes_no_no_build_keeps_it_while_a_record_stands() {
    let case = routed_case("routing-nothing-back-kept");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");
    let rows: [&[&str]; 5] = [
        &[r".\dotnet.cmd", "app.dll", "--no-build"],
        &[r".\dotnet.cmd", "exec", "x.dll", "--no-build"],
        &[r".\dotnet.cmd", "tool", "run", "t", "--no-build"],
        &[r".\dotnet.cmd", "pack", "--no-build"],
        &[r".\dotnet.cmd", "publish", "--no-build"],
    ];

    for (index, command) in rows.into_iter().enumerate() {
        let outcome = finish(case.gate(&run_words(&format!("kept-{index}.log"), &[], command)));
        assert!(!outcome.stderr.contains(DROPPED), "{outcome:?}");
        assert_eq!(command_lines(&outcome.stdout), [command[1..].join(" ")], "{outcome:?}");
        assert_eq!(
            nothing_back(&case).as_deref(),
            Some(NOTHING_BACK_RECORD),
            "the record stands: {outcome:?}"
        );
    }
    let other = finish(case.gate(&run_words("tool.log", &[], &[r".\tool.exe", "argv", "--no-build"])));
    assert!(!other.stderr.contains(DROPPED), "{other:?}");
    assert_eq!(
        command_lines(&other.stdout),
        ["--no-build"],
        "a command that is no dotnet command: {other:?}"
    );
    assert_eq!(nothing_back(&case).as_deref(), Some(NOTHING_BACK_RECORD), "the record stands: {other:?}");
}

#[test]
fn a_build_that_may_not_have_rebuilt_what_the_record_stands_for_leaves_it() {
    let case = routed_case("routing-nothing-back-settings");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    fs::create_dir_all(case.folder().join("sub")).expect("the subfolder can be made");
    let steps = shown(&stub(&case, &["status 0"]));
    let routed = finish(case.gate(&routed_words("routed.log", &steps, &[], &[r".\dotnet.exe", "build", "-c", "Release"])));
    let written = nothing_back(&case);
    let run = |log: &str, command: &[&str]| {
        let outcome = finish(case.gate(&run_words(log, &[], command)));
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        (outcome, nothing_back(&case))
    };

    let (restore, after_restore) = run("restore.log", &[r".\dotnet.cmd", "msbuild", "-t:Restore"]);
    let sub = finish(case.gate_in("sub", &run_words("sub.log", &[], &[r"..\dotnet.cmd", "build", "-c", "Release"])));
    let after_sub = nothing_back(&case);
    let (debug, after_debug) = run("debug.log", &[r".\dotnet.cmd", "build"]);
    let (release, after_release) = run("release.log", &[r".\dotnet.cmd", "build", "--configuration:release"]);

    let record = r#"{"v":1,"host":"builder","configuration":"Release"}"#;
    assert_eq!(written.as_deref(), Some(record), "{routed:?}");
    assert_eq!(after_restore.as_deref(), Some(record), "msbuild may build nothing: {restore:?}");
    assert_eq!(sub.code, Some(0), "{sub:?}");
    assert_eq!(after_sub.as_deref(), Some(record), "a build below the root: {sub:?}");
    assert_eq!(after_debug.as_deref(), Some(record), "another configuration: {debug:?}");
    assert_eq!(after_release, None, "the same configuration, its letter case aside: {release:?}");
}

#[test]
fn a_dotnet_command_naming_a_response_file_is_opaque() {
    let case = routed_case("routing-nothing-back-response-file");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    let steps = shown(&stub(&case, &["status 0"]));

    let routed = finish(case.gate(&routed_words("routed.log", &steps, &[], &[r".\dotnet.exe", "build", "@x.rsp"])));
    let unwritten = nothing_back(&case);
    routed_build(&case, "routed-plain.log");
    let test = finish(case.gate(&run_words("test.log", &[], &[r".\dotnet.cmd", "test", "@x.rsp", "--no-build"])));
    let build = finish(case.gate(&run_words("build.log", &[], &[r".\dotnet.cmd", "build", "@x.rsp"])));

    assert_eq!(routed.code, Some(0), "{routed:?}");
    assert_eq!(unwritten, None, "no record: {routed:?}");
    assert!(!test.stderr.contains(DROPPED), "no drop: {test:?}");
    assert_eq!(command_lines(&test.stdout), ["test @x.rsp --no-build"], "{test:?}");
    assert_eq!(build.code, Some(0), "{build:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(NOTHING_BACK_RECORD), "no clear: {build:?}");
}

#[test]
fn a_record_naming_no_usable_builder_or_that_cannot_be_read_names_the_builder() {
    let case = routed_case("routing-nothing-back-unknown");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    routed_build(&case, "routed.log");
    rewrite_record(&case, r#"{"v":1,"host":"bad host!"}"#);

    let path = record_path(&case).expect("the case has a record");
    let unusable = finish(case.gate(&run_words("unusable.log", &[], &NO_BUILD_TEST)));
    assert_eq!(nothing_back(&case), None, "the passing dropped test cleared the record: {unusable:?}");
    // A built-here entry the passing test wrote would keep the next word; at a root with no project it writes none.
    if let Err(error) = fs::remove_file(built_here_path(&case)) {
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "the built-here record cannot be removed: {error}");
    }
    fs::create_dir(&path).expect("a folder stands where the record would");
    let unreadable = finish(case.gate(&run_words("unreadable.log", &[], &NO_BUILD_TEST)));

    assert!(unusable.stderr.contains(UNKNOWN_BUILDER_LINE), "{unusable:?}");
    assert_eq!(command_lines(&unusable.stdout), ["test"], "{unusable:?}");
    assert_in_order(
        &unreadable.stderr,
        &[&format!("gate: cannot read {}: ", path.display()), UNKNOWN_BUILDER_LINE],
    );
    assert_eq!(command_lines(&unreadable.stdout), ["test"], "{unreadable:?}");
}

#[test]
fn a_wrapped_dotnet_no_build_after_a_routed_build_drops_only_the_word_after_the_separator() {
    let case = routed_case("routing-nothing-back-wrapped");
    copy_tool(&case, "dotnet.exe");
    routed_build(&case, "routed.log");
    let script = case.folder().join("words.ps1");
    fs::write(&script, "[Environment]::GetCommandLineArgs()\n").expect("the script can be written");
    let script = shown(&script);
    let wrapped = [
        "pwsh",
        "-NoProfile",
        "-NonInteractive",
        "-File",
        &script,
        "--no-build",
        "--",
        "dotnet",
        "test",
        "--no-build",
    ];

    let outcome = finish(case.gate(&run_words("wrapped.log", &[], &wrapped)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(NOTHING_BACK_LINE), "{outcome:?}");
    let lines = command_lines(&outcome.stdout);
    let separator = lines.iter().position(|line| *line == "--").expect("pwsh was handed the --");
    assert_eq!(lines[separator - 1], "--no-build", "the script's own word stays: {outcome:?}");
    assert_eq!(lines[separator + 1..], ["dotnet", "test"], "{outcome:?}");
}

#[test]
fn a_wrapped_routed_dotnet_command_is_stamped_before_its_own_separator() {
    let case = routed_case("routing-wrapped-stamp");
    fs::write(case.folder().join("a.txt"), "a\n").expect("a file can be written");
    git(&case, &["add", "a.txt"]);
    git(&case, &["commit", "--quiet", "-m", "first"]);
    let sha = git(&case, &["rev-parse", "HEAD"]);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let wrapped = ["pwsh", "-File", "x.ps1", "--", r".\dotnet.exe", "test", "--", "--x"];

    let outcome = finish(case.gate(&routed_words("wrapped.log", &steps, &[], &wrapped)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let recorded = recorded(&call);
    let words = recorded_words(&recorded);
    let stamp = format!("-p:SourceRevisionId={sha}");
    assert_eq!(words[5..], ["test", stamp.as_str(), path_map(&case).as_str(), "--", "--x"], "{recorded}");
}

/// The path-map word a routed dotnet build of `case`'s worktree gets from a client no transport seam names: its work
/// folder on the ssh transport's builder root mapped to the worktree root as this PC spells it.
pub(super) fn path_map(case: &Case) -> String {
    let root = git(case, &["rev-parse", "--show-toplevel"]);
    let spelled = std::path::absolute(&root).expect("git names the worktree by a full path");
    format!(r"-p:PathMap=D:\gate-remote\work\{}\={}", work_folder_key(&root), spelled.display())
}

#[test]
fn a_no_build_here_is_dropped_while_a_tracked_project_has_no_assets_file() {
    let case = Case::new("routing-no-build-assets");
    git_init(&case);
    dotnet_cmd(&case);
    tracked_project(&case, "app/App.csproj", false);

    let missing = finish(case.gate(&run_words("missing.log", &[], &NO_BUILD_TEST)));
    fs::write(case.folder().join(r"app\obj\project.assets.json"), "{}\n").expect("the assets file can be written");
    let present = finish(case.gate(&run_words("present.log", &[], &NO_BUILD_TEST)));

    assert!(
        missing
            .stderr
            .contains(r"gate: dropped --no-build: app/App.csproj has no obj\project.assets.json here; building on a heavy slot"),
        "{missing:?}"
    );
    assert_eq!(command_lines(&missing.stdout), ["test"], "{missing:?}");
    assert!(!present.stderr.contains(DROPPED), "{present:?}");
    assert_eq!(command_lines(&present.stdout), ["test --no-build"], "{present:?}");
}

#[test]
fn a_named_project_or_solution_alone_is_checked_for_its_assets_file() {
    let case = Case::new("routing-no-build-named");
    git_init(&case);
    dotnet_cmd(&case);
    tracked_project(&case, "app/App.csproj", false);
    tracked_project(&case, "lib/Lib.csproj", true);
    let sln = "Project(\"{FAE04EC0}\") = \"Lib\", \"lib\\Lib.csproj\", \"{1}\"\r\nEndProject\r\n";
    fs::write(case.folder().join("Lib.sln"), sln).expect("the solution can be written");
    let app_sln = "Project(\"{FAE04EC0}\") = \"App\", \"app\\App.csproj\", \"{2}\"\r\nEndProject\r\n";
    fs::write(case.folder().join("App.sln"), app_sln).expect("the solution can be written");
    let line = r"gate: dropped --no-build: app/App.csproj has no obj\project.assets.json here; building on a heavy slot";
    let run = |log: &str, command: &[&str]| finish(case.gate(&run_words(log, &[], command)));

    let project = run("project.log", &[r".\dotnet.cmd", "test", "lib/Lib.csproj", "--no-build"]);
    let solution = run("solution.log", &[r".\dotnet.cmd", "test", "Lib.sln", "--no-build"]);
    // A passing dropped test builds in place and writes a built-here entry, which would keep the next run's word: each
    // run sees no built-here record.
    let forget_built_here = || {
        if let Err(error) = fs::remove_file(built_here_path(&case)) {
            assert_eq!(error.kind(), io::ErrorKind::NotFound, "the built-here record cannot be removed: {error}");
        }
    };
    forget_built_here();
    let unrestored = run("unrestored.log", &[r".\dotnet.cmd", "test", "App.sln", "--no-build"]);
    assert!(
        unrestored.stderr.contains(line),
        "a solution listing an unrestored project drops it: {unrestored:?}"
    );
    assert_eq!(command_lines(&unrestored.stdout), ["test App.sln"], "{unrestored:?}");
    forget_built_here();
    let named = run("named.log", &[r".\dotnet.cmd", "test", "app/App.csproj", "--no-build"]);
    forget_built_here();
    let whole = run("whole.log", &NO_BUILD_TEST);

    assert!(!project.stderr.contains(DROPPED), "only the named project is checked: {project:?}");
    assert_eq!(command_lines(&project.stdout), ["test lib/Lib.csproj --no-build"], "{project:?}");
    assert!(
        !solution.stderr.contains(DROPPED),
        "only the solution's projects are checked: {solution:?}"
    );
    assert!(named.stderr.contains(line), "{named:?}");
    assert_eq!(command_lines(&named.stdout), ["test app/App.csproj"], "{named:?}");
    assert!(whole.stderr.contains(line), "{whole:?}");
}

/// A gate whose folder is spelled through a junction to the worktree, as it is through a `subst` drive, while git names
/// the worktree by its real path, still counts a named solution's projects inside the worktree.
#[test]
fn a_solution_named_from_another_spelling_of_the_worktree_is_checked_for_its_assets_file() {
    let case = Case::new("routing-no-build-spelled");
    git_init(&case);
    dotnet_cmd(&case);
    tracked_project(&case, "app/App.csproj", false);
    let app_sln = "Project(\"{FAE04EC0}\") = \"App\", \"app\\App.csproj\", \"{2}\"\r\nEndProject\r\n";
    fs::write(case.folder().join("App.sln"), app_sln).expect("the solution can be written");
    let link = Junction::to(&case, &case.folder());
    let mut gate = case.gate(&run_words("spelled.log", &[], &[r".\dotnet.cmd", "test", "App.sln", "--no-build"]));
    gate.current_dir(&link.0);

    let spelled = finish(gate);

    let line = r"gate: dropped --no-build: app/App.csproj has no obj\project.assets.json here; building on a heavy slot";
    assert!(spelled.stderr.contains(line), "{spelled:?}");
    assert_eq!(command_lines(&spelled.stdout), ["test App.sln"], "{spelled:?}");
}

#[test]
fn a_repo_whose_build_files_move_the_assets_skips_the_check_but_not_the_record() {
    let case = routed_case("routing-no-build-artifacts");
    copy_tool(&case, "dotnet.exe");
    dotnet_cmd(&case);
    tracked_project(&case, "app/App.csproj", false);
    let props = "<Project><PropertyGroup><UseArtifactsOutput>true</UseArtifactsOutput></PropertyGroup></Project>\n";
    fs::write(case.folder().join("Directory.Build.props"), props).expect("the props file can be written");

    let unchecked = finish(case.gate(&run_words("unchecked.log", &[], &NO_BUILD_TEST)));
    routed_build(&case, "routed.log");
    let recorded = finish(case.gate(&run_words("recorded.log", &[], &NO_BUILD_TEST)));

    assert!(!unchecked.stderr.contains(DROPPED), "{unchecked:?}");
    assert_eq!(command_lines(&unchecked.stdout), ["test --no-build"], "{unchecked:?}");
    assert!(recorded.stderr.contains(NOTHING_BACK_LINE), "{recorded:?}");
    assert_eq!(command_lines(&recorded.stdout), ["test"], "{recorded:?}");
}

#[test]
fn a_project_listing_git_cannot_give_keeps_the_word() {
    let case = Case::new("routing-no-build-listing");
    git_init(&case);
    dotnet_cmd(&case);
    fs::write(case.folder().join(".git").join("index"), "not an index").expect("the index can be spoiled");

    let outcome = finish(case.gate(&run_words("listing.log", &[], &NO_BUILD_TEST)));

    assert!(outcome.stderr.contains("gate: cannot list the worktree's projects: "), "{outcome:?}");
    assert!(!outcome.stderr.contains(DROPPED), "{outcome:?}");
    assert_eq!(command_lines(&outcome.stdout), ["test --no-build"], "{outcome:?}");
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
    assert_in_order(&outcome.stderr, &[REMOTE_WAITING, FULL_LINE]);
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

/// A run the full pool sent back counts its local take's wait from its own queue line: the wait was the remote pool's,
/// and the local take follows the fallback at once.
#[test]
fn a_full_fallback_counts_the_local_takes_wait_from_the_queue_line() {
    let case = routed_case("routing-full-wait");
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let steps = shown(&stub(&case, &["status 0"]));
    let waiter = case.start(&routed_words(
        "waiter.log",
        &steps,
        &["--test-remote-wait-ms", "400"],
        &[r".\tool.exe", "out", "hi"],
    ));
    let pid = waiter.id();
    let outcome = waiter.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[REMOTE_WAITING, FULL_LINE]);
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "fallback", "take", "release"], "{own:#?}");
    assert!(own[..2].iter().all(has_remote_tag), "{own:#?}");
    assert_eq!(own[1]["reason"], "full");
    assert!(own[2..].iter().all(has_no_remote_fields), "{own:#?}");
    let waited = own[2]["waitedSeconds"].as_f64().expect("the local take says how long the run waited");
    assert!(
        waited >= 0.35,
        "the local take counts the wait from the run's queue line, so the remote wait is in it: {waited} s"
    );
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));
}

/// A run that waited out the remote pool's limit and fell back to a held local heavy pool says its first still-waiting
/// line with the whole wait in it, never with the seconds since its local take began.
#[test]
fn a_full_fallback_says_the_whole_wait_in_its_still_waiting_lines() {
    let case = routed_case("routing-full-still-waiting");
    write_counts(&case.root(), r#"{"v":1,"heavy":1}"#);
    let local_go = case.folder().join("local-go");
    let local_go_word = shown(&local_go);
    let local_holder = case.start(&heavy_words_with_limit(
        "local.log",
        &["--local", LOCAL_REASON[0], LOCAL_REASON[1]],
        &[r".\tool.exe", "wait", &local_go_word],
    ));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, local_holder.id()));
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let steps = shown(&stub(&case, &["status 0"]));
    let seams = ["--test-remote-wait-ms", "2000", "--test-heartbeat-ms", "1000", "--test-poll-ms", "100"];
    let waiter = case.start(&routed_words("waiter.log", &steps, &seams, &[r".\tool.exe", "out", "hi"]));
    let stderr = waiter.wait_for_stderr_where("the local wait's first still-waiting line", |text| text.contains(STILL_WAITING_HEAVY));

    let line = stderr
        .lines()
        .find(|line| line.starts_with(STILL_WAITING_HEAVY))
        .expect("the local wait says it still waits");
    // The seconds come first, before the busy count of a pool with no slot free: `(3 s)` or `(3 s; 1 busy)`.
    let seconds: u64 = line
        .trim_start_matches(STILL_WAITING_HEAVY)
        .split_once(" s")
        .map(|(seconds, _)| seconds)
        .expect("the line names the whole seconds waited")
        .parse()
        .expect("the seconds are a whole number");
    assert!(
        seconds >= 2,
        "the local wait counts from the run's queue line, so the 2 s remote wait is in it: {line}"
    );

    let_go(&local_go);
    let_go(&go);
    let outcome = waiter.finish();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[REMOTE_WAITING, FULL_LINE, STILL_WAITING_HEAVY]);
    assert_eq!((local_holder.finish().code, holder.finish().code), (Some(0), Some(0)));
}

/// A Ctrl+C during the local wait that follows a full fallback journals its abandon with the waited time counted from
/// the run's own queue line, so the remote wait before the fallback is in it.
#[test]
fn a_ctrl_c_in_the_local_wait_after_a_full_fallback_journals_the_abandon_from_the_queue_line() {
    let case = routed_case("routing-ctrl-c-full-fallback");
    write_counts(&case.root(), r#"{"v":1,"heavy":1}"#);
    let local_go = case.folder().join("local-go");
    let local_go_word = shown(&local_go);
    let local_holder = case.start(&heavy_words_with_limit(
        "local.log",
        &["--local", LOCAL_REASON[0], LOCAL_REASON[1]],
        &[r".\tool.exe", "wait", &local_go_word],
    ));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, local_holder.id()));
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let steps = shown(&stub(&case, &["status 0"]));
    let waiter = case.start(&routed_words(
        "waiter.log",
        &steps,
        &["--test-remote-wait-ms", "400", "--test-interrupt-queued-after-fallback"],
        &[r".\tool.exe", "out", "hi"],
    ));
    let pid = waiter.id();
    let outcome = waiter.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[REMOTE_WAITING, FULL_LINE, "gate: interrupted while waiting for a heavy slot"],
    );
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "fallback", "abandon"], "{own:#?}");
    assert!(own[..2].iter().all(has_remote_tag), "{own:#?}");
    assert_eq!(own[1]["reason"], "full");
    assert!(has_no_remote_fields(&own[2]), "{own:#?}");
    assert!(!has_ticket(&case, pid), "the waiter's ticket is gone");
    let waited = own[2]["waitedSeconds"].as_f64().expect("the abandon says how long the run waited");
    assert!(
        waited >= 0.35,
        "the abandon counts the wait from the run's queue line, so the remote wait is in it: {waited} s"
    );
    let_go(&local_go);
    let_go(&go);
    assert_eq!((local_holder.finish().code, holder.finish().code), (Some(0), Some(0)));
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
    let local_holder = case.start(&heavy_words_with_limit(
        "lh.log",
        &["--local", LOCAL_REASON[0], LOCAL_REASON[1]],
        &[r".\tool.exe", "wait", &local_go_word],
    ));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, local_holder.id()));
    let mut local_waiter = case.start(&heavy_words_with_limit(
        "lw.log",
        &["--local", LOCAL_REASON[0], LOCAL_REASON[1]],
        &[r".\tool.exe", "out", "hi"],
    ));
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
    let local = finish(case.gate(&heavy_words_with_limit(
        "local.log",
        &["--local", LOCAL_REASON[0], LOCAL_REASON[1]],
        &[r".\tool.exe", "out", "hi"],
    )));
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
    let holder = case.start(&heavy_words_with_limit(
        "holder.log",
        &["--local", LOCAL_REASON[0], LOCAL_REASON[1]],
        &[r".\tool.exe", "wait", &go_word],
    ));
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

/// The `builders` entry a two-builder case tries first.
const MAC: &str = "mac-1";

/// A [`routed_case`] whose routing file sends its `*tool*` commands to the `builders` entry `mac-1`, a macOS builder,
/// then to `host`, which alone runs its `*host.exe*` commands; the case holds fake-tool as `host.exe` too.
fn two_builder_case(name: &str) -> Case {
    let case = routed_case(name);
    copy_tool(&case, "host.exe");
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["*tool*", "*host.exe*"] },
        "builders": [{ "host": MAC, "os": "macos", "root": "/m", "repos": { repo(&case): ["*tool*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    case
}

/// The `host` of each journal line, `None` for one without.
fn hosts(events: &[Value]) -> Vec<Option<&str>> {
    events.iter().map(|event| event["host"].as_str()).collect()
}

/// Whether `text` holds `line` as one of its lines.
fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|said| said == line)
}

/// Whether `text` holds `line` as exactly one of its lines.
fn has_line_once(text: &str, line: &str) -> bool {
    text.lines().filter(|said| *said == line).count() == 1
}

#[test]
fn a_dotnet_test_of_a_windows_only_project_goes_past_the_mac_to_host() {
    let case = routed_case("routing-windows-only-test");
    let project = case.folder().join("tests").join("Win");
    fs::create_dir_all(&project).expect("the project folder can be made");
    let text = "<Project><PropertyGroup><TargetFramework>net10.0-windows</TargetFramework></PropertyGroup></Project>\n";
    fs::write(project.join("Win.csproj"), text).expect("the project can be written");
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["dotnet test *"] },
        "builders": [{ "host": MAC, "os": "macos", "root": "/m", "repos": { repo(&case): ["dotnet test *"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let steps = shown(&stub(&case, &[&record, "status 0"]));

    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &["dotnet", "test", "tests/Win"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        recorded(&call)["host"],
        HOST,
        "the Mac cannot run a Windows-only test project: {outcome:?}"
    );
    assert!(events(&case).iter().all(has_remote_tag), "every journal line names host");
}

/// The note a routed run's log holds when the change touches `path`, one of `.gate/windows.toml`'s paths.
fn touched_note(path: &str) -> String {
    format!("gate: kept off the macOS builder: the change touches {path} (.gate/windows.toml)")
}

/// A [`two_builder_case`] whose `.gate/windows.toml` declares `paths`, its checkout holding one commit of every file
/// but the logs and step files, which `.gitignore` leaves out; that commit is `refs/remotes/origin/main` when `origin`.
/// The tools stay in it: the Mac takes `.\tool.exe` only when the snapshot carries it.
fn windows_paths_case(name: &str, paths: &str, origin: bool) -> Case {
    let case = two_builder_case(name);
    let folder = case.folder();
    fs::write(folder.join(".gitignore"), "*.log\nstub-*.txt\ncall.json\n").expect("the ignore file can be written");
    fs::create_dir_all(folder.join(".gate")).expect("the .gate folder can be made");
    fs::write(folder.join(".gate").join("windows.toml"), format!("paths = {paths}\n")).expect("the declaration can be written");
    commit_all(&case, &folder, "base");
    if origin {
        git(&case, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    }
    case
}

/// Commits every file of the repo at `folder` not ignored, with the message `message`.
fn commit_all(case: &Case, folder: &Path, message: &str) {
    let at = folder.to_str().expect("the folder is Unicode");
    git(case, &["-C", at, "add", "--all"]);
    git(case, &["-C", at, "commit", "--quiet", "--message", message]);
}

/// Writes the file `path`, `/`-separated below the repo at `folder`, and the folders above it.
fn write_below(folder: &Path, path: &str) {
    let file = folder.join(path);
    fs::create_dir_all(file.parent().expect("the file has a folder")).expect("the file's folder can be made");
    fs::write(&file, "a\n").expect("the file can be written");
}

/// Runs the case's `.\tool.exe` routed through a scripted client that records its call; the host the call went to and
/// the run's log.
fn windows_paths_run(case: &Case) -> (Value, String) {
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let steps = shown(&stub(case, &[&record, "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran"])));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    (recorded(&call)["host"].clone(), log_texts(&case.folder().join("out.log")))
}

#[test]
fn a_committed_change_under_a_windows_path_goes_past_the_mac_to_host_saying_why() {
    let case = windows_paths_case("routing-windows-paths-committed", "['tools/Hook/*']", true);
    write_below(&case.folder(), "tools/Hook/a.cs");
    commit_all(&case, &case.folder(), "hook");

    let (host, log) = windows_paths_run(&case);

    assert_eq!(host, HOST, "{log}");
    assert!(has_line(&log, &touched_note("tools/Hook/a.cs")), "{log}");
}

#[test]
fn an_untracked_file_under_a_windows_path_goes_past_the_mac_to_host_saying_why() {
    let case = windows_paths_case("routing-windows-paths-untracked", "['tools/Hook/*']", true);
    write_below(&case.folder(), "tools/Hook/b.cs");

    let (host, log) = windows_paths_run(&case);

    assert_eq!(host, HOST, "{log}");
    assert!(has_line(&log, &touched_note("tools/Hook/b.cs")), "{log}");
}

#[test]
fn a_change_outside_the_windows_paths_stays_on_the_mac() {
    let case = windows_paths_case("routing-windows-paths-outside", "['tools/Hook/*']", true);
    write_below(&case.folder(), "src/a.cs");
    commit_all(&case, &case.folder(), "outside");
    write_below(&case.folder(), "docs/b.md");

    let (host, log) = windows_paths_run(&case);

    assert_eq!(host, MAC, "{log}");
    assert!(!log.contains("kept off the macOS builder"), "{log}");
}

#[test]
fn a_checkout_with_no_origin_ref_goes_past_the_mac_to_host_saying_it_cannot_tell() {
    let case = windows_paths_case("routing-windows-paths-no-origin", "['tools/Hook/*']", false);

    let (host, log) = windows_paths_run(&case);

    assert_eq!(host, HOST, "{log}");
    let note = "gate: kept off the macOS builder: cannot tell whether the change touches .gate/windows.toml's paths: ";
    assert!(
        log.lines()
            .any(|line| line.starts_with(note) && line.ends_with("has no refs/remotes/origin/HEAD or refs/remotes/origin/main")),
        "{log}"
    );
}

#[test]
fn a_change_under_a_siblings_windows_path_goes_past_the_mac_to_host_saying_why() {
    let case = two_builder_case("routing-windows-paths-sibling");
    let (name, beside) = declare_sibling(&case);
    fs::create_dir_all(&beside).expect("the sibling can be made");
    let sibling = beside.to_str().expect("the sibling's folder is Unicode");
    git(&case, &["-C", sibling, "init", "--quiet"]);
    write_below(&beside, "tools/X/base.cs");
    commit_all(&case, &beside, "base");
    git(&case, &["-C", sibling, "update-ref", "refs/remotes/origin/main", "HEAD"]);
    write_below(&beside, "tools/X/a.cs");
    commit_all(&case, &beside, "hook");
    let folder = case.folder();
    fs::write(folder.join(".gitignore"), "*.log\nstub-*.txt\ncall.json\n").expect("the ignore file can be written");
    let paths = format!("paths = ['tools/Hook/*', '../{name}/tools/X/*']\n");
    fs::write(folder.join(".gate").join("windows.toml"), paths).expect("the declaration can be written");
    commit_all(&case, &folder, "base");
    git(&case, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

    let (host, log) = windows_paths_run(&case);
    fs::remove_dir_all(&beside).expect("the sibling can be removed");

    assert_eq!(host, HOST, "{log}");
    assert!(has_line(&log, &touched_note(&format!("../{name}/tools/X/a.cs"))), "{log}");
}

#[test]
fn a_windows_file_with_a_problem_goes_past_the_mac_to_host_naming_the_problem() {
    let case = windows_paths_case("routing-windows-paths-problem", "['']", true);

    let (host, log) = windows_paths_run(&case);

    assert_eq!(host, HOST, "{log}");
    let note = "gate: kept off the macOS builder: cannot tell whether the change touches .gate/windows.toml's paths: paths[0] is empty";
    assert!(has_line(&log, note), "{log}");
}

/// The checkout has no origin ref, so a read of its change could not tell it and would keep the run off the Mac.
#[test]
fn a_windows_file_declaring_no_paths_asks_no_git_and_leaves_the_mac() {
    let case = windows_paths_case("routing-windows-paths-none", "[]", false);

    let (host, log) = windows_paths_run(&case);

    assert_eq!(host, MAC, "{log}");
    assert!(!log.contains("kept off the macOS builder"), "{log}");
}

/// The checkout has no commit and no origin ref, so a read of its change could not tell it and would keep the run off
/// the Mac.
#[test]
fn a_repo_without_a_windows_file_asks_no_git_for_it_and_leaves_the_mac() {
    let case = two_builder_case("routing-windows-paths-no-file");

    let (host, log) = windows_paths_run(&case);

    assert_eq!(host, MAC, "{log}");
    assert!(!log.contains("kept off the macOS builder"), "{log}");
}

#[test]
fn a_mac_refusing_version_sends_the_run_on_to_host_which_runs_it() {
    let case = two_builder_case("routing-next-builder");
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let steps = shown(&stub(&case, &["on mac-1", "refuse version", "on builder", &record, "status 0"]));
    let running = case.start(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, "gate: remote mac-1 version; trying builder"), "{outcome:?}");
    assert!(!outcome.stderr.contains("running here"), "{outcome:?}");
    assert_eq!(recorded(&call)["host"], HOST, "host ran the command");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "fallback", "take", "release"], "{own:#?}");
    assert_eq!(hosts(&own), [Some(MAC), Some(MAC), Some(MAC), Some(HOST), Some(HOST)], "{own:#?}");
    assert_eq!(own[2]["reason"], "version", "{own:#?}");
    assert!(own.iter().all(|event| event["remote"] == true), "{own:#?}");
}

/// A run pinned with `--builder` whose command the repo does not route is a usage error said before anything runs: it
/// exits 2 with the pin's line, runs nothing here or on a builder, and writes no journal line.
#[test]
fn a_pin_the_router_refuses_exits_2_and_journals_nothing() {
    let case = two_builder_case("routing-pin-refused");
    let steps = shown(&stub(&case, &["on mac-1", "status 0"]));
    let running = case.start(&routed_words("out.log", &steps, &["--builder", "MAC-1"], &["cmd", "/c", "echo ran here"]));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert!(
        has_line_once(
            &outcome.stderr,
            "gate: --builder MAC-1: this command is not one the repo routes (none of its patterns matches the command)"
        ),
        "{outcome:?}"
    );
    assert!(!outcome.stdout.contains("ran here"), "nothing ran here: {outcome:?}");
    let journal = fs::read_to_string(case.root().join("events.jsonl")).unwrap_or_default();
    let own: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a journal line is JSON"))
        .filter(|event| event["pid"] == pid)
        .collect();
    assert!(own.is_empty(), "no journal line of the refused run: {own:#?}");
}

/// The `builders` entry marked not routed that a pinned case names.
const TRIAL: &str = "win-9";

/// A [`routed_case`] whose `host` runs its `*tool*` commands, with the Windows `builders` entry `win-9` marked
/// `"routed": false`, which lists no repo.
fn not_routed_case(name: &str) -> Case {
    let case = routed_case(name);
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["*tool*"] },
        "builders": [{ "host": TRIAL, "os": "windows", "routed": false }],
    });
    write_remote_json(&case, &routing.to_string());
    case
}

#[test]
fn a_pinned_run_goes_to_a_not_routed_builder_and_runs_there() {
    let case = not_routed_case("routing-pin-trial");
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let steps = shown(&stub(&case, &["on win-9", &record, "status 0"]));
    let running = case.start(&routed_words(
        "out.log",
        &steps,
        &["--builder", TRIAL],
        &[r".\tool.exe", "out", "ran here"],
    ));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(recorded(&call)["host"], TRIAL, "the trial builder ran the command");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "{own:#?}");
    assert_eq!(hosts(&own), [Some(TRIAL), Some(TRIAL), Some(TRIAL)], "{own:#?}");
    assert_eq!(own[0]["pinned"], true, "{own:#?}");
}

#[test]
fn an_unpinned_run_never_picks_a_not_routed_builder() {
    let case = not_routed_case("routing-not-routed-skipped");
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let steps = shown(&stub(&case, &["on win-9", "refuse version", "on builder", &record, "status 0"]));
    let running = case.start(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(recorded(&call)["host"], HOST, "host ran the command");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "{own:#?}");
    assert_eq!(hosts(&own), [Some(HOST), Some(HOST), Some(HOST)], "{own:#?}");
    assert!(own[0].get("pinned").is_none(), "an unpinned queue line has no pinned field: {own:#?}");
}

/// Runs the two-builder case `name` pinned to `mac-1`, which refuses with `reason` while host would run it: the run
/// ends failed with the pin's line, host is never called, nothing runs here, and no fallback is journaled.
fn assert_pinned_refusal_ends_the_run(name: &str, reason: &str) {
    let case = two_builder_case(name);
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let refuse = format!("refuse {reason}");
    let steps = shown(&stub(&case, &["on mac-1", &refuse, "on builder", &record, "status 0"]));
    let running = case.start(&routed_words("out.log", &steps, &["--builder", MAC], &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let line = format!("gate: remote mac-1 {reason}; --builder given, so not running here");
    assert!(has_line(&outcome.stderr, &line), "{outcome:?}");
    assert!(!outcome.stderr.contains("trying builder"), "no other builder is tried: {outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "nothing ran here: {outcome:?}");
    assert!(!call.exists(), "host was never called");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "no fallback line: {own:#?}");
    assert_eq!(hosts(&own), [Some(MAC), Some(MAC), Some(MAC)], "{own:#?}");
    assert_eq!(own[0]["pinned"], true, "{own:#?}");
}

#[test]
fn a_pinned_run_the_builder_refuses_exits_1_and_runs_nothing_here() {
    assert_pinned_refusal_ends_the_run("routing-pin-version", "version");
}

#[test]
fn a_pinned_run_to_an_unreachable_builder_exits_1_without_trying_another() {
    assert_pinned_refusal_ends_the_run("routing-pin-unreachable", "unreachable");
}

/// What a run pinned with `--builder` says after its builder's fallback head when the builder did not run it.
const PIN_NOT_HERE: &str = "; --builder given, so not running here";

/// Asserts the gate `pid` of `case` ended its pinned run, its `outcome`, before the builder ran it: exit 1, a line
/// starting with `head` and ending with [`PIN_NOT_HERE`], nothing run here, no other builder tried, and its journal's
/// queue line pinned with no `fallback` line after it.
fn assert_pin_ended(case: &Case, pid: u32, outcome: &Outcome, head: &str) {
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome.stderr.lines().any(|line| line.starts_with(head) && line.ends_with(PIN_NOT_HERE)),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("; running here"), "{outcome:?}");
    assert!(!outcome.stderr.contains("; trying "), "no other builder is tried: {outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "nothing ran here: {outcome:?}");
    let own = own(case, pid);
    assert_eq!(names(&own).first(), Some(&"queue"), "{own:#?}");
    assert_eq!(own[0]["pinned"], true, "{own:#?}");
    assert!(!names(&own).contains(&"fallback"), "no fallback line: {own:#?}");
}

#[test]
fn a_pinned_run_whose_client_cannot_start_exits_1_and_runs_nothing_here() {
    let case = ssh_case("routing-pin-no-ssh");
    let git = git_folder();
    let mut gate = case.gate(&heavy_words_with_limit(
        "out.log",
        &["--builder", HOST],
        &[r".\tool.exe", "out", "ran here"],
    ));
    gate.env("PATH", &git).env("GATE_FAKE_SSH_STEPS", case.folder().join("no-ssh.steps"));
    let running = start(gate);
    let pid = running.id();
    let outcome = running.finish();

    assert_pin_ended(&case, pid, &outcome, "gate: remote builder client could not start: cannot run ssh: ");
}

#[test]
fn a_pinned_run_whose_work_folder_is_busy_exits_1_and_runs_nothing_here() {
    let case = client_case("routing-pin-key-busy");
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "100", "--builder", HOST],
        ..Tuned::default()
    };
    let second = start(client_gate_tuned(
        &case,
        "second.log",
        &second_steps,
        &tuned,
        &[r".\tool.exe", "out", "ran here"],
    ));
    let pid = second.id();
    let outcome = second.finish();

    assert_pin_ended(&case, pid, &outcome, "gate: remote builder busy");
    assert_eq!(calls(&second_steps), Vec::<String>::new(), "the pinned run made no ssh call");
    drop(go);
    assert_eq!(first.finish().code, Some(0));
}

/// The pinned `mac-1`'s pool is full while host's is free, so a hand-off to host would be seen. The run waited for the
/// pool but never held a slot, so its release counts no time held.
#[test]
fn a_pinned_run_whose_builder_pool_is_full_exits_1_and_runs_nothing_here() {
    let case = two_builder_case("routing-pin-full");
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let waiter = case.start(&routed_words(
        "waiter.log",
        &steps,
        &["--test-remote-wait-ms", "1500", "--builder", MAC],
        &[r".\tool.exe", "out", "ran here"],
    ));
    let pid = waiter.id();
    let outcome = waiter.finish();

    assert_pin_ended(&case, pid, &outcome, "gate: remote mac-1 full");
    assert!(!call.exists(), "neither builder's client ran the pinned run");
    let own = own(&case, pid);
    let release = own
        .iter()
        .find(|event| event["event"] == "release")
        .expect("the run journals its release");
    let held = release["heldSeconds"].as_f64().expect("the release says how long it held");
    assert!(held < 1.0, "the run held no slot through its wait: {release}");
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));
}

/// Every pin refusal is said before the gate readies its log: one the router gives leaves an earlier log as it was.
#[test]
fn a_pin_to_an_unlisted_host_leaves_an_earlier_log_untouched() {
    let case = routed_case("routing-pin-unlisted-log");
    let log = case.folder().join("out.log");
    fs::write(&log, "an earlier run's log\n").expect("the earlier log can be written");
    let steps = shown(&stub(&case, &["status 0"]));
    let outcome = finish(case.gate(&routed_words(
        "out.log",
        &steps,
        &["--builder", "nobody"],
        &[r".\tool.exe", "out", "ran here"],
    )));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert!(
        has_line_once(&outcome.stderr, "gate: --builder nobody is not a builder remote.json lists"),
        "{outcome:?}"
    );
    assert!(!outcome.stdout.contains("ran here"), "nothing ran here: {outcome:?}");
    assert_eq!(
        fs::read_to_string(&log).expect("the earlier log is still there"),
        "an earlier run's log\n",
        "the refusal leaves the earlier log as it was"
    );
}

/// A pin under a covering parent's slot is a usage error said before the gate readies its log: an earlier run's log
/// at `--log` is left as it was, as every usage refusal leaves it.
#[test]
fn a_pin_under_a_covering_parent_is_refused_leaving_an_earlier_log_untouched() {
    let case = routed_case("routing-pin-covered");
    let log = case.folder().join("out.log");
    fs::write(&log, "an earlier run's log\n").expect("the earlier log can be written");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let words = routed_words("out.log", &steps, &["--builder", HOST], &[r".\tool.exe", "out", "ran here"]);
    let running = start(marked(&case, &words, &test_marker(&case, "heavy")));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert!(
        has_line_once(
            &outcome.stderr,
            "gate: --builder builder cannot route this run: a parent gate's slot covers this gate"
        ),
        "{outcome:?}"
    );
    assert!(!outcome.stdout.contains("ran here"), "nothing ran here: {outcome:?}");
    assert!(!call.exists(), "no builder was tried");
    assert_eq!(
        fs::read_to_string(&log).expect("the earlier log is still there"),
        "an earlier run's log\n",
        "the refusal leaves the earlier log as it was"
    );
    let journal = fs::read_to_string(case.root().join("events.jsonl")).unwrap_or_default();
    assert!(
        !journal.contains(&format!(r#""pid":{pid},"#)),
        "the refused gate journals nothing: {journal}"
    );
}

#[test]
fn a_run_both_builders_refuse_runs_here_after_trying_each_once() {
    let case = two_builder_case("routing-both-refuse");
    let steps = shown(&stub(&case, &["on mac-1", "refuse version", "on builder", "refuse unreachable"]));
    let running = case.start(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "the command ran here: {outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[
            "gate: remote mac-1 version; trying builder",
            "gate: remote builder unreachable; running here",
        ],
    );
    assert_eq!(outcome.stderr.matches("running here").count(), 1, "{outcome:?}");
    let own = own(&case, pid);
    assert_eq!(
        names(&own),
        ["queue", "take", "fallback", "take", "fallback", "take", "release"],
        "{own:#?}"
    );
    assert_eq!(
        hosts(&own),
        [Some(MAC), Some(MAC), Some(MAC), Some(HOST), Some(HOST), None, None],
        "{own:#?}"
    );
    assert_eq!((&own[2]["reason"], &own[4]["reason"]), (&json!("version"), &json!("unreachable")));
    assert!(own[5..].iter().all(has_no_remote_fields), "{own:#?}");
}

#[test]
fn every_retried_reason_from_the_first_builder_tries_the_next() {
    let case = two_builder_case("routing-retried-reasons");
    for reason in ["full", "busy", "unreachable", "version", "parity", "install", "tools"] {
        let refuse = format!("refuse {reason}");
        let steps = shown(&stub(&case, &["on mac-1", &refuse, "on builder", "status 0"]));
        let log = format!("{reason}.log");
        let running = case.start(&routed_words(&log, &steps, &[], &[r".\tool.exe", "out", "ran here"]));
        let pid = running.id();
        let outcome = running.finish();

        assert_eq!(outcome.code, Some(0), "{reason}: {outcome:?}");
        let line = format!("gate: remote mac-1 {reason}; trying builder");
        assert!(has_line(&outcome.stderr, &line), "{reason}: {outcome:?}");
        assert!(!outcome.stderr.contains("running here"), "{reason}: {outcome:?}");
        let own = own(&case, pid);
        assert_eq!(names(&own), ["queue", "take", "fallback", "take", "release"], "{reason}: {own:#?}");
        assert_eq!(own[3]["host"], HOST, "{reason}: {own:#?}");
    }
}

#[test]
fn every_other_reason_from_the_first_builder_runs_here_without_trying_host() {
    let case = two_builder_case("routing-not-retried");
    for reason in ["client", "inputs", "lfs", "layout", "desktop", "rebooted", "root"] {
        let call = case.folder().join(format!("{reason}.json"));
        let refuse = format!("refuse {reason}");
        let record = format!("record {}", shown(&call));
        let steps = shown(&stub(&case, &["on mac-1", &refuse, "on builder", &record, "status 0"]));
        let log = format!("{reason}.log");
        let running = case.start(&routed_words(&log, &steps, &[], &[r".\tool.exe", "out", "ran here"]));
        let pid = running.id();
        let outcome = running.finish();

        assert_eq!(outcome.code, Some(0), "{reason}: {outcome:?}");
        assert!(outcome.stdout.contains("ran here"), "{reason}: {outcome:?}");
        let fell = outcome
            .stderr
            .lines()
            .find(|line| line.starts_with("gate: remote mac-1 "))
            .expect("the fallback line names the Mac");
        assert!(fell.ends_with("; running here"), "{reason}: {fell}");
        assert!(!outcome.stderr.contains("trying"), "{reason}: {outcome:?}");
        assert!(!call.exists(), "{reason}: host was never called");
        let own = own(&case, pid);
        assert_eq!(names(&own), ["queue", "take", "fallback", "take", "release"], "{reason}: {own:#?}");
        assert_eq!(hosts(&own), [Some(MAC), Some(MAC), Some(MAC), None, None], "{reason}: {own:#?}");
        assert_eq!(own[2]["reason"], reason, "{own:#?}");
    }
}

#[test]
fn a_held_work_folder_key_runs_here_without_trying_the_next_builder() {
    // The key is the worktree's on every builder: a key another run holds is busy on host too.
    let case = client_case("routing-key-busy");
    copy_tool(&case, "win.exe");
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["*tool*", "*win.exe*"] },
        "outputs": { repo(&case): [] },
        "builders": [{ "host": "win-2", "os": "windows", "repos": { repo(&case): ["*win.exe*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "100"],
        ..Tuned::default()
    };
    let second = start(client_gate_tuned(
        &case,
        "second.log",
        &second_steps,
        &tuned,
        &[r".\win.exe", "out", "ran here"],
    ));
    let pid = second.id();
    let outcome = second.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, "gate: remote win-2 busy; running here"), "{outcome:?}");
    assert!(!outcome.stderr.contains("trying"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&second_steps), Vec::<String>::new(), "the second run made no ssh call");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "fallback", "take", "release"], "{own:#?}");
    assert_eq!(own[1]["host"], "win-2", "{own:#?}");
    fs::write(go.path(), "").expect("the go file can be made");
    assert_eq!(first.finish().code, Some(0));
}

#[test]
fn full_pools_on_both_builders_share_one_remote_wait() {
    let case = two_builder_case("routing-shared-budget");
    let go = case.folder().join("go");
    let wait_go = format!("wait {}", shown(&go));
    let mut holders = Vec::new();
    for (log, program) in [("mac.log", r".\tool.exe"), ("host.log", r".\host.exe")] {
        let steps = shown(&stub(&case, &[&wait_go]));
        let holder = case.start(&routed_words(log, &steps, &[], &[program, "out", "hi"]));
        wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));
        holders.push(holder);
    }
    let steps = shown(&stub(&case, &["status 0"]));
    let waiter = case.start(&routed_words(
        "waiter.log",
        &steps,
        &["--test-remote-wait-ms", "400"],
        &[r".\tool.exe", "out", "ran here"],
    ));
    let pid = waiter.id();
    let outcome = waiter.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[
            "gate: waiting for a remote slot on mac-1",
            "gate: remote mac-1 full; trying builder",
            "gate: remote builder full; running here",
        ],
    );
    assert_eq!(
        outcome.stderr.matches("gate: waiting for a remote slot").count(),
        1,
        "host's pool, with nothing left of the budget, is not waited for: {outcome:?}"
    );
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "fallback", "fallback", "take", "release"], "{own:#?}");
    assert_eq!(hosts(&own), [Some(MAC), Some(MAC), Some(HOST), None, None], "{own:#?}");
    let waited = own[3]["waitedSeconds"].as_f64().expect("the local take says how long the run waited");
    assert!(
        waited >= 0.35,
        "the last fallback is full, so the local take counts from the queue line: {waited} s"
    );
    let_go(&go);
    for holder in holders {
        assert_eq!(holder.finish().code, Some(0));
    }
}

#[test]
fn a_skipped_candidate_is_noted_in_the_log_after_the_refusing_attempt_s_lines() {
    let case = two_builder_case("routing-skipped-noted");
    // The Mac's attempt writes over the routing file, so host is no longer one the run can be built for.
    let routing = case.local_app_data().join("gate").join("remote.json");
    let spoil = format!("record {}", shown(&routing));
    let steps = shown(&stub(&case, &["on mac-1", "out attempt line", &spoil, "refuse version"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert!(has_line(&outcome.stderr, "gate: remote mac-1 version; running here"), "{outcome:?}");
    assert!(!outcome.stderr.contains("not trying"), "the skip is for the log alone: {outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert_in_order(
        &log,
        &["attempt line", "gate: not trying builder: ", "gate: remote mac-1 version; running here"],
    );
    let note = format!("gate: not trying builder: ignored {}: repos is missing", routing.display());
    assert!(has_line(&log, &note), "the reason alone, after the builder: {log}");
}

#[test]
fn a_ctrl_c_as_the_next_builder_is_built_ends_the_run_without_saying_where_it_goes() {
    let case = two_builder_case("routing-ctrl-c-walk");
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let steps = shown(&stub(&case, &["on mac-1", "refuse version", "on builder", &record, "status 0"]));
    let running = case.start(&routed_words(
        "out.log",
        &steps,
        &["--test-interrupt-before-later"],
        &[r".\tool.exe", "out", "ran here"],
    ));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(!outcome.stderr.contains("trying"), "{outcome:?}");
    assert!(!outcome.stderr.contains("running here"), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &["gate: remote mac-1 version\n", "gate: interrupted before the command started"],
    );
    assert!(!outcome.stdout.contains("ran here"), "{outcome:?}");
    assert!(!call.exists(), "host was never called");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "fallback", "release"], "{own:#?}");
    assert_eq!(hosts(&own), [Some(MAC), Some(MAC), Some(MAC), None], "{own:#?}");
    assert_eq!(
        own[3]["heldSeconds"].as_f64(),
        Some(0.0),
        "the release holds no slot, the Mac's ended with its fallback: {own:#?}"
    );
}

#[test]
fn a_run_going_on_to_the_next_builder_says_the_refusing_attempt_s_lines_first() {
    let case = two_builder_case("routing-next-order");
    let folder = declarations_holding(&case, &[]);
    name_declarations(&case, &folder);
    let go = case.folder().join("go");
    let wait_go = format!("wait {}", shown(&go));
    let steps = shown(&stub(
        &case,
        &[
            "on mac-1",
            "err attempt line",
            &wait_go,
            "refuse version",
            "on builder",
            "err host line",
            "status 0",
        ],
    ));
    let running = case.start(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "hi"]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, running.id()));
    // The route has read the declarations by the Mac's take, so host's build is the first read to find this file.
    let local = folder.join("local.toml");
    fs::write(&local, "patterns = ['*matches nothing*']\n").expect("the local file can be written");
    let_go(&go);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let trying = "gate: remote mac-1 version; trying builder";
    assert_in_order(&outcome.stderr, &["attempt line", trying, "host line"]);
    let log = log_texts(&case.folder().join("out.log"));
    let read = format!("gate: .gate/local.toml read from {}", local.display());
    assert_in_order(&log, &["attempt line", &read, trying]);
}

#[test]
fn the_refusing_builder_s_slot_is_let_go_before_the_next_builder_takes_its_own() {
    let case = two_builder_case("routing-next-slot");
    let (host_go, mac_go) = (case.folder().join("host-go"), case.folder().join("mac-go"));
    let host_steps = shown(&stub(&case, &[&format!("wait {}", shown(&host_go))]));
    let host_holder = case.start(&routed_words("holder.log", &host_steps, &[], &[r".\host.exe", "out", "hi"]));
    let holder_pid = host_holder.id();
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{holder_pid},"#));
    let wait_mac = format!("wait {}", shown(&mac_go));
    let steps = shown(&stub(&case, &["on mac-1", &wait_mac, "refuse version", "on builder", "status 0"]));
    // Both remote waits last as long as the hang guard, so only the awaited events end them, however loaded the PC.
    let budget = ["--test-remote-wait-ms", "600000"];
    let running = case.start(&routed_words("out.log", &steps, &budget, &[r".\tool.exe", "out", "hi"]));
    let pid = running.id();
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{pid},"#));
    // A run waiting on the Mac's pool takes its slot once the refusing run lets it go.
    let waiter_steps = shown(&stub(&case, &["status 0"]));
    let waiter = case.start(&routed_words("waiter.log", &waiter_steps, &budget, &[r".\tool.exe", "out", "hi"]));
    let waiter_pid = waiter.id();
    wait_for_journal_line(&case.root(), &format!(r#""event":"queue","pid":{waiter_pid},"#));
    let_go(&mac_go);
    // Host's pool stays full until the waiter holds the Mac's slot: the refusing run cannot take its own before.
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{waiter_pid},"#));
    let_go(&host_go);
    let outcome = running.finish();
    let waited = waiter.finish();

    assert_eq!((outcome.code, waited.code), (Some(0), Some(0)), "{outcome:?} {waited:?}");
    assert_eq!(host_holder.finish().code, Some(0));
    let journal = events(&case);
    let at = |pid: u32, event: &str, host: &str| {
        journal
            .iter()
            .position(|line| line["pid"] == pid && line["event"] == event && line["host"] == host)
    };
    let found = (at(pid, "fallback", MAC), at(waiter_pid, "take", MAC), at(pid, "take", HOST));
    assert!(
        matches!(found, (Some(fallback), Some(waiter_take), Some(host_take)) if fallback < waiter_take && waiter_take < host_take),
        "the Mac's slot is let go, and taken by the waiter, before host's take: {found:?} {journal:#?}"
    );
}

#[test]
fn a_ctrl_c_on_the_first_builder_tries_no_other() {
    let case = two_builder_case("routing-ctrl-c-next");
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let steps = shown(&stub(
        &case,
        &["on mac-1", "interrupt", "refuse version", "on builder", &record, "status 0"],
    ));
    let running = case.start(&routed_words("out.log", &steps, &[], &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(!outcome.stderr.contains("trying"), "{outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "{outcome:?}");
    assert!(!call.exists(), "host was never called");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "{own:#?}");
    assert_eq!(hosts(&own), [Some(MAC); 3], "{own:#?}");
}

/// The drain poll of the drain cases, short so a wait of a few polls ends soon.
const DRAIN_POLL: [&str; 2] = ["--test-drain-poll-ms", "20"];
/// The line a routed run says once the builder admits it under a barrier that still stood.
const DRAIN_LIFTED: &str = "gate: the builder drain lifted; starting the routed run";

/// The Unix second now, from the clock the gate reads.
fn unix_now() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is past 1970");
    i64::try_from(since.as_secs()).expect("the seconds fit")
}

/// The Unix second `seconds` as the drain's lines say a time: `hh:mm`, in UTC.
fn utc_clock(seconds: i64) -> String {
    let day = seconds.rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3_600, day % 3_600 / 60)
}

/// The scripted client's step answering its first `calls` calls as the drain's refusal at `place`, under a barrier
/// raised at `raised` that lapses at `lapses`.
fn draining(calls: u32, place: &str, raised: i64, lapses: i64) -> String {
    format!("draining {calls} {place} {raised} {lapses}")
}

/// The line a routed run says once as it starts to wait behind the drain of a barrier raised at `raised` that lapses
/// at `lapses`, at `place` when it has one.
fn drain_waiting(raised: i64, lapses: i64, place: Option<u32>) -> String {
    let head = format!(
        "gate: waiting behind the builder drain (raised {} UTC, lapses {} UTC)",
        utc_clock(raised),
        utc_clock(lapses)
    );
    place.map_or_else(|| head.clone(), |place| format!("{head}; place {place} in line"))
}

#[test]
fn a_routed_run_behind_the_builder_drain_waits_then_runs_there_and_never_falls_back() {
    let case = routed_case("routing-drain-wait");
    let now = unix_now();
    let (raised, lapses) = (now - 60, now + 1800);
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let steps = shown(&stub(&case, &[&draining(3, "2", raised, lapses), &record, "out ran there", "status 0"]));
    let running = case.start(&routed_words("out.log", &steps, &DRAIN_POLL, &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let waiting = drain_waiting(raised, lapses, Some(2));
    assert!(has_line_once(&outcome.stderr, &waiting), "the waiting line is said once: {outcome:?}");
    assert_in_order(&outcome.stderr, &[&waiting, DRAIN_LIFTED]);
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "nothing ran here: {outcome:?}");
    let session = recorded(&call)["session"].as_str().map(str::to_owned).unwrap_or_default();
    assert!(
        session.len() == 32 && session.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
        "the run names its session: {session}"
    );
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "no fallback: {own:#?}");
    assert!(own.iter().all(has_remote_tag), "{own:#?}");
}

#[test]
fn a_drain_wait_says_still_waiting_every_heartbeat() {
    let case = routed_case("routing-drain-heartbeat");
    let now = unix_now();
    let (raised, lapses) = (now - 60, now + 1800);
    let steps = shown(&stub(&case, &[&draining(20, "none", raised, lapses), "status 0"]));
    let mut extra = DRAIN_POLL.to_vec();
    extra.extend(["--test-heartbeat-ms", "100"]);
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &extra, &[r".\tool.exe", "out", "hi"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    // Twenty polls a drain poll apart wait past the heartbeat at least once; a place of none drops the place.
    assert!(has_line_once(&outcome.stderr, &drain_waiting(raised, lapses, None)), "{outcome:?}");
    let still: Vec<&str> = outcome
        .stderr
        .lines()
        .filter(|line| line.starts_with("gate: still waiting behind the builder drain ("))
        .collect();
    assert!(!still.is_empty(), "{outcome:?}");
    for line in &still {
        let seconds = line
            .strip_prefix("gate: still waiting behind the builder drain (")
            .and_then(|rest| rest.strip_suffix(" s)"));
        assert!(seconds.is_some_and(|seconds| seconds.parse::<u64>().is_ok()), "{line}");
    }
    assert_in_order(&outcome.stderr, &[still[0], DRAIN_LIFTED]);
}

#[test]
fn a_run_admitted_after_a_lapse_says_the_lapse_line() {
    let case = routed_case("routing-drain-lapsed");
    let now = unix_now();
    let (raised, lapses) = (now - 1860, now - 60);
    let steps = shown(&stub(&case, &[&draining(2, "1", raised, lapses), "status 0"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &DRAIN_POLL, &[r".\tool.exe", "out", "hi"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lapsed = format!(
        "gate: the builder drain lapsed at {} UTC without a lift; starting the routed run",
        utc_clock(lapses)
    );
    assert_in_order(&outcome.stderr, &[&drain_waiting(raised, lapses, Some(1)), &lapsed]);
    assert!(!outcome.stderr.contains(DRAIN_LIFTED), "{outcome:?}");
}

#[test]
fn a_ctrl_c_in_the_drain_wait_frees_the_remote_slot_and_the_key() {
    let case = routed_case("routing-drain-ctrl-c");
    let now = unix_now();
    // The poll after the refused call raises the interrupt, which the wait reads before it starts the run.
    let steps = shown(&stub(
        &case,
        &[&draining(1, "1", now - 60, now + 1800), "interrupt", "out ran there", "status 0"],
    ));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &DRAIN_POLL, &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(
        !outcome.stdout.contains("ran here") && !outcome.stdout.contains("ran there"),
        "the command ran nowhere: {outcome:?}"
    );
    assert!(!outcome.stderr.contains(DRAIN_LIFTED), "{outcome:?}");
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
    assert!(!remote_claim(&case).exists(), "the remote claim is gone");
    let next = shown(&stub(&case, &["status 0"]));
    let after = finish(case.gate(&routed_words("next.log", &next, &[], &[r".\tool.exe", "out", "hi"])));
    assert_eq!(after.code, Some(0), "the next run takes the key and the slot: {after:?}");
    assert!(!after.stderr.contains("still waiting for this worktree's work folder"), "{after:?}");
}

#[test]
fn a_pinned_run_waits_for_the_drain_instead_of_ending_not_here() {
    let case = two_builder_case("routing-drain-pinned");
    let now = unix_now();
    let (raised, lapses) = (now - 60, now + 1800);
    let call = case.folder().join("call.json");
    let record = format!("record {}", shown(&call));
    let drained = draining(2, "1", raised, lapses);
    let steps = shown(&stub(&case, &["on mac-1", &drained, &record, "status 0", "on builder", "refuse busy"]));
    let mut extra = vec!["--builder", MAC];
    extra.extend(DRAIN_POLL);
    let running = case.start(&routed_words("out.log", &steps, &extra, &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[&drain_waiting(raised, lapses, Some(1)), DRAIN_LIFTED]);
    assert!(!outcome.stderr.contains("--builder given"), "the pin never ends the run: {outcome:?}");
    assert_eq!(recorded(&call)["host"], MAC, "the pinned builder ran it");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "{own:#?}");
    assert_eq!(hosts(&own), [Some(MAC); 3], "{own:#?}");
}

#[test]
fn an_unreachable_builder_while_a_barrier_stands_counts_as_waiting_and_after_its_lapse_falls_back() {
    let case = routed_case("routing-drain-unreachable");
    let now = unix_now();
    // Under a standing barrier the unreachable poll waits on, noted in the log alone; the interrupt the same poll
    // raised ends the wait.
    let standing = now + 3600;
    let steps = shown(&stub(&case, &[&draining(1, "1", now - 60, standing), "refuse unreachable", "interrupt"]));
    let waited = finish(case.gate(&routed_words("waited.log", &steps, &DRAIN_POLL, &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(waited.code, Some(CONTROL_C_EXIT), "{waited:?}");
    let note = format!(
        "gate: the builder is unreachable while its drain stands (it lapses at {} UTC); still waiting",
        utc_clock(standing)
    );
    assert!(!waited.stderr.contains(&note), "the note is the log's alone: {waited:?}");
    assert!(log_text(&case, "waited.log").contains(&note), "the log notes the wait");
    assert!(!waited.stderr.contains("unreachable; running here"), "{waited:?}");

    // Past the barrier's lapse the same poll falls back.
    let lapsed = now - 60;
    let steps = shown(&stub(&case, &[&draining(1, "1", now - 1860, lapsed), "refuse unreachable"]));
    let running = case.start(&routed_words("fell.log", &steps, &DRAIN_POLL, &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let fell = running.finish();

    assert_eq!(fell.code, Some(0), "{fell:?}");
    assert!(fell.stdout.contains("ran here"), "{fell:?}");
    assert!(fell.stderr.contains("gate: remote builder unreachable; running here"), "{fell:?}");
    assert_refused(&own(&case, pid), "unreachable");
}

/// The text of the case's log `name`.
fn log_text(case: &Case, name: &str) -> String {
    fs::read_to_string(case.folder().join(name)).expect("the log can be read")
}

/// The case's drain marker folder of the builder whose id is `id`, `host` for none.
fn marker_folder(case: &Case, id: Option<&str>) -> PathBuf {
    case.root().join("remote").join("drain").join(id.unwrap_or("host"))
}

/// Holds a drain marker in the case's marker folder of the builder `id`, as a run waiting behind that builder's drain
/// holds one: a file open sharing nothing, held until it is dropped.
fn hold_marker(case: &Case, id: Option<&str>) -> fs::File {
    use std::os::windows::fs::OpenOptionsExt;

    let folder = marker_folder(case, id);
    fs::create_dir_all(&folder).expect("the marker folder can be made");
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(folder.join("1-0123456789abcdef0123456789abcdef"))
        .expect("the marker can be held")
}

/// The names in the marker folder `folder`, sorted, with whether another open finds each held; none when it is missing.
fn markers(folder: &Path) -> Vec<(String, bool)> {
    use std::os::windows::fs::OpenOptionsExt;

    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found: Vec<(String, bool)> = entries
        .map(|entry| {
            let path = entry.expect("a marker entry can be read").path();
            // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE: only a holder sharing nothing refuses it.
            let held = fs::OpenOptions::new().read(true).share_mode(7).open(&path).is_err();
            (path.file_name().expect("a marker has a name").to_string_lossy().into_owned(), held)
        })
        .collect();
    found.sort();
    found
}

#[test]
fn a_drain_wait_keeps_a_live_marker_and_removes_it_when_the_wait_ends() {
    let case = routed_case("routing-drain-marker");
    let now = unix_now();
    let go = case.folder().join("go");
    // The poll after the refused call waits on the go file, so the run stays behind the drain until the case lets it go.
    let wait_go = format!("wait {}", shown(&go));
    let steps = shown(&stub(&case, &[&draining(1, "1", now - 60, now + 1800), &wait_go, "status 0"]));
    let running = case.start(&routed_words("out.log", &steps, &DRAIN_POLL, &[r".\tool.exe", "out", "hi"]));
    let pid = running.id();
    let folder = marker_folder(&case, None);

    running.wait_until("the run's drain marker", || markers(&folder).iter().any(|(_, held)| *held));
    let held = markers(&folder);
    let_go(&go);
    let outcome = running.finish();

    assert_eq!(held.len(), 1, "{held:?}");
    let (name, _) = &held[0];
    let session = name.strip_prefix(&format!("{pid}-")).expect("the marker is named by the run's pid");
    assert_eq!(session.len(), 32, "and its session: {name}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(markers(&folder), Vec::new(), "the wait's end removed its marker");
}

#[test]
fn a_second_run_waiting_for_the_remote_slot_does_not_fall_back_full_while_the_marker_stands() {
    let case = routed_case("routing-drain-marker-full");
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let marker = hold_marker(&case, None);
    let steps = shown(&stub(&case, &["out ran there", "status 0"]));
    let seams = ["--test-remote-wait-ms", "1", "--test-heartbeat-ms", "50", "--test-poll-ms", "20"];
    let waiter = case.start(&routed_words("waiter.log", &steps, &seams, &[r".\tool.exe", "out", "ran here"]));
    let pid = waiter.id();

    // The still-waiting line comes past the wait's 1 ms limit: the marker held the limit off.
    waiter.wait_for_stderr("gate: still waiting for a remote slot on builder (");
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));
    let outcome = waiter.finish();
    drop(marker);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains(FULL_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "no fallback: {own:#?}");
}

#[test]
fn a_marker_for_one_builder_does_not_hold_back_a_run_for_another() {
    let case = routed_case("routing-drain-marker-other");
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    // Another builder's marker: `host`'s pool keeps its limit.
    let marker = hold_marker(&case, Some("1f0efc62"));
    let steps = shown(&stub(&case, &["status 0"]));
    let waiter = case.start(&routed_words(
        "waiter.log",
        &steps,
        &["--test-remote-wait-ms", "1"],
        &[r".\tool.exe", "out", "ran here"],
    ));
    let pid = waiter.id();
    let outcome = waiter.finish();
    drop(marker);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[REMOTE_WAITING, FULL_LINE]);
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "fallback", "take", "release"], "{own:#?}");
    assert_eq!(own[1]["reason"], "full");
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));
}

#[test]
fn a_second_run_of_the_worktree_waiting_for_the_key_does_not_fall_back_busy_while_the_marker_stands() {
    let case = client_case("routing-drain-marker-key");
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let marker = hold_marker(&case, None);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    // The heartbeat comes after the key wait's bound.
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "100", "--test-heartbeat-ms", "300"],
        ..Tuned::default()
    };
    let second = start(client_gate_tuned(
        &case,
        "second.log",
        &second_steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there"],
    ));

    second.wait_for_stderr("gate: still waiting for this worktree's work folder on builder (");
    fs::write(go.path(), "").expect("the go file can be made");
    assert_eq!(first.finish().code, Some(0));
    let outcome = second.finish();
    drop(marker);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!has_line(&outcome.stderr, BUSY_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert_eq!(
        calls(&second_steps),
        ["preflight", "run"],
        "the second run ran there once the key was free"
    );
}

/// The seconds the passed line of `outcome` says, on either stream.
fn passed_seconds(outcome: &Outcome) -> u32 {
    let line = outcome
        .stdout
        .lines()
        .chain(outcome.stderr.lines())
        .find_map(|line| line.strip_prefix("gate: passed in "))
        .expect("the run says its passed line");
    line.split_once(" s")
        .and_then(|(seconds, _)| seconds.parse().ok())
        .expect("the passed line says whole seconds")
}

#[test]
fn a_drain_wait_is_left_out_of_the_release_s_held_and_wall_seconds_and_the_durations_store() {
    let case = routed_case("routing-drain-durations");
    let now = unix_now();
    // Ten refused calls, each followed by a sleep of a 200 ms drain poll: the wait alone lasts 2 s at least.
    let wait_least = 2.0;
    let steps = shown(&stub(&case, &[&draining(10, "1", now - 60, now + 1800), "status 0"]));
    let begun = std::time::Instant::now();
    let running = case.start(&routed_words(
        "out.log",
        &steps,
        &["--test-drain-poll-ms", "200"],
        &[r".\tool.exe", "out", "hi"],
    ));
    let pid = running.id();
    let outcome = running.finish();
    // Whatever the load, the run's own time fits in what the whole run took past its wait.
    let left = begun.elapsed().as_secs_f64() - wait_least;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let release = own(&case, pid)
        .into_iter()
        .find(|event| event["event"] == "release")
        .expect("the run journals its release");
    let held = release["heldSeconds"].as_f64().expect("the release says how long it held");
    assert!(held <= left, "the slot's hold counts from the admission: {held} s of {left} s: {release}");
    let passed = passed_seconds(&outcome);
    assert!(
        f64::from(passed) <= left.ceil(),
        "the passed line counts from the admission: {passed} s of {left} s: {outcome:?}"
    );
    let store: Value =
        serde_json::from_str(&fs::read_to_string(case.root().join("durations.json")).expect("the store was written")).expect("the store is JSON");
    let walls: Vec<f64> = store["keys"]
        .as_object()
        .expect("the store holds its keys")
        .values()
        .flat_map(|history| history["samples"].as_array().expect("a history holds its samples").clone())
        .map(|sample| sample["wall"].as_f64().expect("a sample has its wall"))
        .collect();
    assert_eq!(walls.len(), 1, "{store:#}");
    assert!(walls[0] <= left, "the stored wall counts from the admission: {} s of {left} s", walls[0]);
}

/// What a run whose key wait passed its bound says once a drain marker holds that bound off.
const KEY_HELD_OFF: &str = "gate: still waiting for the worktree's key past its bound: a routed run waits behind a builder drain";

/// The Mac's pool is full past the run's whole remote budget, so `host`'s attempt has none left: a live marker of
/// `host` does not hold that wait off, and the run falls back full at once instead of waiting on `host`'s pool.
#[test]
fn a_wait_with_no_remote_budget_is_not_held_off_by_a_drain_marker() {
    let case = two_builder_case("routing-drain-no-budget");
    let (host_go, mac_go) = (case.folder().join("host-go"), case.folder().join("mac-go"));
    let mac_holder = remote_holder(&case, "mac.log", &mac_go);
    let host_steps = shown(&stub(&case, &[&format!("wait {}", shown(&host_go))]));
    let host_holder = case.start(&routed_words("host.log", &host_steps, &[], &[r".\host.exe", "out", "hi"]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, host_holder.id()));
    let marker = hold_marker(&case, None);
    let steps = shown(&stub(&case, &["status 0"]));
    let seams = ["--test-remote-wait-ms", "1", "--test-heartbeat-ms", "50", "--test-poll-ms", "20"];
    let waiter = case.start(&routed_words("waiter.log", &steps, &seams, &[r".\tool.exe", "out", "ran here"]));
    let pid = waiter.id();

    let said = waiter.wait_for_stderr_where("the fallback or a still-waiting line", |said| {
        said.contains(FULL_LINE) || said.contains("gate: still waiting for a remote slot on builder (")
    });
    // Let go before the run's end, so a run held off ends too and fails on what it said.
    drop(marker);
    let_go(&mac_go);
    let_go(&host_go);
    let outcome = waiter.finish();
    assert_eq!((mac_holder.finish().code, host_holder.finish().code), (Some(0), Some(0)));

    assert!(said.contains(FULL_LINE), "the run left the remote queue at once: {said}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    let own = own(&case, pid);
    assert_eq!(hosts(&own).last(), Some(&None), "the run ran here: {own:#?}");
    assert!(
        own.iter()
            .any(|event| event["event"] == "fallback" && event["host"] == HOST && event["reason"] == "full"),
        "host's pool fell back full: {own:#?}"
    );
}

#[test]
fn a_held_off_remote_wait_takes_its_limit_again_once_the_marker_is_gone() {
    let case = routed_case("routing-drain-marker-gone");
    let go = case.folder().join("go");
    let holder = remote_holder(&case, "holder.log", &go);
    let marker = hold_marker(&case, None);
    let steps = shown(&stub(&case, &["out ran there", "status 0"]));
    let seams = ["--test-remote-wait-ms", "1", "--test-heartbeat-ms", "50", "--test-poll-ms", "20"];
    let waiter = case.start(&routed_words("waiter.log", &steps, &seams, &[r".\tool.exe", "out", "ran here"]));
    let pid = waiter.id();

    // Past the 1 ms limit the marker holds the wait off; once it is gone, the next pass ends the wait full.
    waiter.wait_for_stderr("gate: still waiting for a remote slot on builder (");
    drop(marker);
    let outcome = waiter.finish();
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(FULL_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "fallback", "take", "release"], "{own:#?}");
    assert_eq!(own[1]["reason"], "full", "{own:#?}");
}

#[test]
fn a_key_wait_is_held_off_by_a_marker_of_another_builder() {
    let case = client_case("routing-drain-marker-key-other");
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    // A run waiting behind another builder's drain: the key is the worktree's on every builder.
    let marker = hold_marker(&case, Some("1f0efc62"));
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "100", "--test-heartbeat-ms", "300"],
        ..Tuned::default()
    };
    let mut second = start(client_gate_tuned(
        &case,
        "second.log",
        &second_steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there"],
    ));

    second.wait_for_stderr_while_running("the key wait past its bound", |said| said.contains(KEY_HELD_OFF));
    fs::write(go.path(), "").expect("the go file can be made");
    assert_eq!(first.finish().code, Some(0));
    let outcome = second.finish();
    drop(marker);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!has_line(&outcome.stderr, BUSY_LINE), "{outcome:?}");
    assert!(has_line_once(&outcome.stderr, KEY_HELD_OFF), "said once: {outcome:?}");
    assert_eq!(
        calls(&second_steps),
        ["preflight", "run"],
        "the second run ran there once the key was free"
    );
}

/// A finished build's wait for the held key to add to the nothing-back record ends at its bound although a drain
/// marker stands: the add is missed, said, and the record stands as it was, the conservative direction.
#[test]
fn a_finished_run_s_wait_to_add_to_nothing_back_ends_at_its_bound_with_a_drain_marker_standing() {
    let case = project_case("routing-nothing-back-key-marker");
    let path = record_path(&case).expect("the case has a record");
    let key = path.file_stem().expect("the record has a name").to_string_lossy().into_owned();
    let name = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::Routed { hash: key_hash(&key) });
    let mutex = NamedMutex::open(&name).expect("the key's mutex opens");
    let held = mutex.acquire(Duration::from_secs(5)).expect("the key's mutex is free");
    let marker = hold_marker(&case, None);

    let build = case.start(&run_words(
        "build.log",
        &["--test-key-wait-ms", "200"],
        &[r".\dotnet.cmd", "build", "One", "-c", "Release"],
    ));
    let build = build.finish();
    drop(held);
    drop(marker);

    assert_eq!(build.code, Some(0), "{build:?}");
    assert!(
        build
            .stderr
            .lines()
            .any(|line| line.starts_with("gate: cannot add to ") && line.ends_with(".nothing-back: held for 200 ms")),
        "{build:?}"
    );
    assert!(!build.stderr.contains(KEY_HELD_OFF), "{build:?}");
    assert!(!log_text(&case, "build.log").contains("behind a builder drain"), "{build:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "the add was missed: {build:?}");
}

#[test]
fn a_run_drained_again_after_its_admission_waits_again_and_holds_its_marker() {
    let case = routed_case("routing-drain-again");
    let now = unix_now();
    let (raised, lapses) = (now - 60, now + 1800);
    let go = case.folder().join("go");
    // The run's call is refused, the poll after it admits the run, whose call is refused again; the poll after that,
    // the fourth call, waits on the go file, so the run stays in its second wait until the case lets it go.
    let steps = shown(&stub(
        &case,
        &[
            &draining(1, "1", raised, lapses),
            &format!("draining-at 3 2 {raised} {lapses}"),
            &format!("wait-at 4 {}", shown(&go)),
            "out ran there",
            "status 0",
        ],
    ));
    let running = case.start(&routed_words("out.log", &steps, &DRAIN_POLL, &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let folder = marker_folder(&case, None);

    // The first wait's marker is let go before the lifted line: a marker held after it is the second wait's.
    running.wait_for_stderr(DRAIN_LIFTED);
    running.wait_until("the second wait's drain marker", || markers(&folder).iter().any(|(_, held)| *held));
    let_go(&go);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let (first, again) = (drain_waiting(raised, lapses, Some(1)), drain_waiting(raised, lapses, Some(2)));
    assert_in_order(&outcome.stderr, &[&first, DRAIN_LIFTED, &again, DRAIN_LIFTED]);
    assert!(has_line_once(&outcome.stderr, &again), "the waiting line again, once: {outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert_eq!(markers(&folder), Vec::new(), "the second wait's end removed its marker");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "no fallback: {own:#?}");
}

#[test]
fn each_routed_run_has_its_own_session() {
    let case = routed_case("routing-drain-sessions");
    let sessions: Vec<String> = ["one", "two"]
        .into_iter()
        .map(|name| {
            let call = case.folder().join(format!("{name}.json"));
            let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
            let outcome = finish(case.gate(&routed_words(&format!("{name}.log"), &steps, &[], &[r".\tool.exe", "out", "hi"])));
            assert_eq!(outcome.code, Some(0), "{outcome:?}");
            recorded(&call)["session"].as_str().map(str::to_owned).unwrap_or_default()
        })
        .collect();

    for session in &sessions {
        assert!(
            session.len() == 32 && session.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "{session}"
        );
    }
    assert_ne!(sessions[0], sessions[1], "each run makes its own session");
}

/// The line a routed run says once as it starts to wait for a slot on the full builder [`HOST`].
const BUILDER_SLOT_WAITING: &str = "gate: waiting for a slot on the builder builder (its slots are all held)";
/// The start of the line a routed run says every heartbeat of its wait for a slot on [`HOST`].
const BUILDER_SLOT_STILL_WAITING: &str = "gate: still waiting for a slot on the builder builder (";
/// The `builders` entry a two-builder slot case lists before [`HOST`].
const SLOT_FIRST: &str = "builder-a";

/// Gives the builder whose root is `root` a slot count of 1 and holds its one slot with a `gate box hold` the case
/// `case` starts, returned once the hold's claim is there; the hold's command sleeps until the case ends it.
fn held_builder_slot(case: &Case, root: &Path) -> Running {
    fs::create_dir_all(root).expect("the builder root can be made");
    fs::write(root.join("slots.json"), r#"{"v":1,"slots":1}"#).expect("the count file can be written");
    let (stop, held_root) = (shown(&case.folder()), shown(root));
    let hold = start(case.gate_without_state_dir(&[
        "box",
        "--test-box-root-walk-stop",
        &stop,
        "--test-box-drain-readable-only",
        "true",
        "hold",
        "--root",
        &held_root,
        "--holder",
        "a routing case",
        "--ceiling",
        "600",
        "--",
        FAKE_TOOL,
        "sleep",
        UNTIL_KILLED,
    ]));
    let claim = root.join("slots").join("0.json");
    hold.wait_until("the hold claims the builder's one slot", || claim.exists());
    hold
}

/// A routed run of the [`client_case`] `case` through the stand-in of the steps file `steps`, polling the full builder
/// every 20 ms with a heartbeat of 50 ms, its remote budget `budget_ms`, `extra` given before the command.
fn builder_slot_run(case: &Case, steps: &Path, budget_ms: &str, extra: &[&str], command: &[&str]) -> Command {
    let mut words = vec![
        "--test-drain-poll-ms",
        "20",
        "--test-heartbeat-ms",
        "50",
        "--test-remote-wait-ms",
        budget_ms,
    ];
    words.extend_from_slice(extra);
    let tuned = Tuned {
        extra: &words,
        ..Tuned::default()
    };
    client_gate_tuned(case, "out.log", steps, &tuned, command)
}

/// The stand-in's steps for a run the builder refuses `full` and then polls: the refused call and three polls answer at
/// once, so the wait has slept at least four 20 ms polls, past its 50 ms heartbeat, and said the still-waiting line
/// whatever the load; the next poll waits until `go` exists; then enough answers for the run's own calls.
fn polled_until(go: &Path) -> Vec<String> {
    let mut steps = vec!["ok".to_owned(); 4];
    steps.push(format!("hold {}", shown(go)));
    steps.extend(vec!["ok".to_owned(); 10]);
    steps
}

/// Starts a routed run of `command` that its builder, its one slot held by `hold`, refuses `full`; waits until it says
/// the still-waiting line, then ends the hold and lets the stand-in answer the poll after. The run, which then runs on
/// the builder.
fn admitted_after_a_slot_wait(case: &Case, hold: Running, command: &[&str]) -> Running {
    let go = case.folder().join("go");
    let steps: Vec<String> = polled_until(&go);
    let steps = ssh_steps(case, "ssh", &steps.iter().map(String::as_str).collect::<Vec<_>>());
    // A budget no busy machine spends, so the run waits until the case lets the slot go.
    let running = start(builder_slot_run(case, &steps, "3600000", &[], command));
    running.wait_for_stderr(BUILDER_SLOT_STILL_WAITING);
    hold.kill();
    fs::write(&go, "").expect("the go file can be made");
    running
}

#[test]
fn a_routed_run_refused_full_by_its_builder_waits_for_a_builder_slot_then_runs_there() {
    let case = client_case("routing-builder-slot-wait");
    let hold = held_builder_slot(&case, &case.folder().join("builder"));
    let running = admitted_after_a_slot_wait(&case, hold, &[r".\tool.exe", "out", "ran there"]);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(has_line_once(&outcome.stderr, BUILDER_SLOT_WAITING), "{outcome:?}");
    for line in outcome.stderr.lines().filter(|line| line.starts_with(BUILDER_SLOT_STILL_WAITING)) {
        let seconds = line.strip_prefix(BUILDER_SLOT_STILL_WAITING).and_then(|rest| rest.strip_suffix(" s)"));
        assert!(seconds.is_some_and(|seconds| seconds.parse::<u64>().is_ok()), "{line}");
    }
    assert!(!outcome.stderr.contains(FULL_LINE), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "it ran on the builder");
}

/// The builder slot's held time counts from the builder's admission, so it covers the command's whole run there.
#[test]
fn a_run_admitted_after_a_builder_slot_wait_holds_its_slot_for_the_whole_command() {
    let case = client_case("routing-builder-slot-held");
    let hold = held_builder_slot(&case, &case.folder().join("builder"));
    let running = admitted_after_a_slot_wait(&case, hold, &[r".\tool.exe", "sleep", "1500"]);
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let own = own(&case, pid);
    let release = own.iter().find(|event| event["event"] == "release").expect("the run released its slot");
    let held = release["heldSeconds"].as_f64().expect("the release has its held seconds");
    assert!(held >= 1.5, "the hold covers the command's 1.5 s sleep: {release}");
}

#[test]
fn a_builder_slot_wait_that_spends_the_remote_budget_falls_back_full_and_runs_here() {
    let case = client_case("routing-builder-slot-spent");
    let hold = held_builder_slot(&case, &case.folder().join("builder"));
    // Polls 20 ms apart within a 200 ms budget make at most eleven calls besides the refused one.
    let steps = ssh_steps(&case, "ssh", &["ok"; 20]);

    let outcome = finish(builder_slot_run(&case, &steps, "200", &[], &[r".\tool.exe", "out", "ran"]));
    hold.kill();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran"), "{outcome:?}");
    assert!(outcome.stderr.contains(FULL_LINE), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), ["full"], "it fell back full and ran here");
}

#[test]
fn a_pinned_run_whose_builder_slot_wait_spends_the_budget_ends_as_a_pinned_full_refusal() {
    let case = client_case("routing-builder-slot-pinned");
    let hold = held_builder_slot(&case, &case.folder().join("builder"));
    let steps = ssh_steps(&case, "ssh", &["ok"; 20]);

    let pin = ["--builder", HOST];
    let outcome = finish(builder_slot_run(&case, &steps, "200", &pin, &[r".\tool.exe", "out", "pinned"]));
    hold.kill();

    assert_ne!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stdout.contains("pinned"), "nothing ran here: {outcome:?}");
    assert!(
        outcome.stderr.contains("gate: remote builder full; --builder given, so not running here"),
        "{outcome:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "a pinned refusal journals no fallback");
}

/// A run whose first builder stays full spends the remote budget waiting there, and the next builder, given only what
/// is left of it, still takes and runs it.
#[test]
fn a_builder_slot_wait_that_spends_the_budget_hands_the_run_to_the_next_builder() {
    let case = client_case("routing-builder-slot-next");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": HOST,
        "repos": { &repo: ["*tool*"] },
        "outputs": { &repo: [] },
        "builders": [{ "host": SLOT_FIRST, "os": "windows", "repos": { &repo: ["*tool*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    let (first, next) = (case.folder().join("builder").join("a"), case.folder().join("builder").join("b"));
    fs::create_dir_all(&next).expect("the next builder's folder can be made");
    let hold = held_builder_slot(&case, &first);
    let mut answers = vec!["on builder-a"];
    answers.extend(["ok"; 20]);
    answers.extend(["on builder", "ok", "ok", "ok", "ok"]);
    let steps = ssh_steps(&case, "ssh", &answers);
    let folders = format!("{SLOT_FIRST}={}|{HOST}={}", shown(&first), shown(&next));
    let words = ["--test-drain-poll-ms", "20", "--test-remote-wait-ms", "200"];
    let tuned = Tuned {
        extra: &words,
        folders: Some(&folders),
        ..Tuned::default()
    };
    let running = start(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));
    let pid = running.id();
    let outcome = running.finish();
    hold.kill();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line == format!("gate: remote {SLOT_FIRST} full; trying {HOST}")),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("running here"), "nothing ran here: {outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    let own = own(&case, pid);
    let took = |host: &str| own.iter().filter(|event| event["event"] == "take" && event["host"] == host).count();
    assert_eq!((took(SLOT_FIRST), took(HOST)), (1, 1), "{own:#?}");
}

/// A [`routed_case`] whose routing file sends its `*tool*` commands to the `builders` entry [`SLOT_FIRST`], a Windows
/// builder listed first, and to [`HOST`], each with one remote slot on this PC.
pub(crate) fn balance_case(name: &str) -> Case {
    let case = routed_case(name);
    let routing = json!({
        "host": HOST,
        "repos": { repo(&case): ["*tool*"] },
        "builders": [{ "host": SLOT_FIRST, "os": "windows", "repos": { repo(&case): ["*tool*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    case
}

/// The builder each `take` line of the gate `pid` names, in order.
fn took_on(case: &Case, pid: u32) -> Vec<String> {
    own(case, pid)
        .iter()
        .filter(|event| event["event"] == "take")
        .map(|event| event["host"].as_str().unwrap_or("here").to_owned())
        .collect()
}

/// A routed run of a [`balance_case`] whose scripted client records its call at `call.json` and passes; its pid and
/// outcome, and the call it recorded.
fn balanced_run(case: &Case, log: &str) -> (u32, Outcome, Value) {
    let call = case.folder().join(format!("{log}.call.json"));
    let steps = shown(&stub(case, &[&format!("record {}", shown(&call)), "status 0"]));
    let running = case.start(&routed_words(log, &steps, &[], &[r".\tool.exe", "out", "ran there"]));
    let pid = running.id();
    let outcome = running.finish();
    assert!(call.exists(), "the builder was called: {outcome:?}");
    let recorded = fs::read_to_string(&call).expect("the call can be read");
    (pid, outcome, serde_json::from_str(&recorded).expect("the call is JSON"))
}

/// Writes the worktree record of the case's worktree, listing `builders` as holding its work folder.
fn plant_worktree_record(case: &Case, builders: &[&str]) {
    let root = git(case, &["rev-parse", "--show-toplevel"]);
    let common = git(case, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let path = case.root().join("remote").join(format!("{}.worktree", work_folder_key(&root)));
    fs::create_dir_all(path.parent().expect("the record has a folder")).expect("the remote folder can be made");
    let record = json!({ "v": 1, "root": root, "commonDir": common, "repo": repo(case), "lastRouted": "2026-10-21T12:00:00Z", "builders": builders });
    fs::write(&path, record.to_string()).expect("the record can be written");
}

/// Files five passing runs of the [`balanced_run`] command on [`SLOT_FIRST`], pinned there, then rewrites the case's
/// store so the command's runs took `first` wall seconds on [`SLOT_FIRST`] and `next` on [`HOST`].
fn seed_both(case: &Case, first: f64, next: f64) {
    let steps = shown(&stub(case, &["status 0"]));
    for index in 0..5 {
        let log = format!("seed-{index}.log");
        let outcome = finish(case.gate(&routed_words(
            &log,
            &steps,
            &["--builder", SLOT_FIRST],
            &[r".\tool.exe", "out", "ran there"],
        )));
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
    }
    let path = case.root().join("durations.json");
    let mut store: Value = serde_json::from_str(&fs::read_to_string(&path).expect("the store was written")).expect("the store is JSON");
    let keys = store["keys"].as_object_mut().expect("the store holds its keys");
    let prefix = format!("remote:{SLOT_FIRST}");
    let (key, history) = keys
        .iter()
        .find(|(key, _)| key.starts_with(&prefix))
        .map(|(key, history)| (key.clone(), history.clone()))
        .expect("the runs are filed on the first builder");
    let rest = key.strip_prefix(&prefix).expect("the key starts with its place").to_owned();
    for (place, wall) in [(SLOT_FIRST, first), (HOST, next)] {
        let mut seeded = history.clone();
        for sample in seeded["samples"].as_array_mut().expect("a history holds its samples") {
            sample["wall"] = Value::from(wall);
        }
        keys.insert(format!("remote:{place}{rest}"), seeded);
    }
    fs::write(&path, store.to_string()).expect("the store can be seeded");
}

#[test]
fn a_builder_with_a_free_slot_wins_over_a_full_builder_listed_first() {
    let case = balance_case("routing-balance-free-slot");
    let go = case.folder().join("go");
    let holder_steps = shown(&stub(&case, &[&format!("wait {}", shown(&go))]));
    let holder = case.start(&routed_words(
        "holder.log",
        &holder_steps,
        &["--builder", SLOT_FIRST],
        &[r".\tool.exe", "out", "hi"],
    ));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));

    let (pid, outcome, call) = balanced_run(&case, "out.log");
    let_go(&go);
    assert_eq!(holder.finish().code, Some(0));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "the full builder is never tried: {own:#?}");
    assert_eq!(took_on(&case, pid), [HOST], "{outcome:?}");
    assert_eq!(call["host"], HOST, "{call}");
}

#[test]
fn a_builder_holding_the_worktree_s_folder_wins_when_durations_tie() {
    let case = balance_case("routing-balance-folder");
    plant_worktree_record(&case, &[HOST]);

    let (pid, outcome, call) = balanced_run(&case, "out.log");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(took_on(&case, pid), [HOST], "{outcome:?}");
    assert_eq!(call["host"], HOST, "{call}");
}

#[test]
fn a_faster_builder_wins_over_a_warm_one_more_than_twice_as_slow() {
    let case = balance_case("routing-balance-faster");
    seed_both(&case, 100.0, 40.0);
    plant_worktree_record(&case, &[SLOT_FIRST, HOST]);

    let (pid, outcome, call) = balanced_run(&case, "out.log");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(took_on(&case, pid), [HOST], "{outcome:?}");
    assert_eq!(call["host"], HOST, "{call}");
}

#[test]
fn with_nothing_known_the_first_builder_in_file_order_takes_the_run() {
    let case = balance_case("routing-balance-nothing-known");

    let (pid, outcome, call) = balanced_run(&case, "out.log");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(took_on(&case, pid), [SLOT_FIRST], "{outcome:?}");
    assert_eq!(call["host"], SLOT_FIRST, "{call}");
}

/// The lines of `outcome`'s stderr that say a balanced pick.
fn balanced_lines(outcome: &Outcome) -> Vec<&str> {
    outcome.stderr.lines().filter(|line| line.starts_with("gate: balanced to ")).collect()
}

/// The `release` line of the gate `pid`.
fn release_of(case: &Case, pid: u32) -> Value {
    own(case, pid)
        .into_iter()
        .find(|event| event["event"] == "release")
        .expect("the run journaled its release")
}

#[test]
fn a_balanced_run_says_the_pick_and_journals_every_candidate() {
    let case = balance_case("routing-balance-line");
    seed_both(&case, 100.0, 40.0);
    plant_worktree_record(&case, &[SLOT_FIRST, HOST]);

    let (pid, outcome, call) = balanced_run(&case, "out.log");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(call["host"], HOST, "{call}");
    assert_eq!(
        balanced_lines(&outcome),
        [format!("gate: balanced to {HOST}: about 40 s, 1 free ({SLOT_FIRST} about 2 min, 1 free)")],
        "{outcome:?}"
    );
    assert_eq!(
        release_of(&case, pid)["balanced"],
        json!([
            { "builder": HOST, "expectedSeconds": 40.0, "free": 1, "cold": false },
            { "builder": SLOT_FIRST, "expectedSeconds": 100.0, "free": 1, "cold": false },
        ])
    );
}

#[test]
fn a_pinned_two_builder_run_says_no_balanced_line() {
    let case = balance_case("routing-balance-pinned");
    let steps = shown(&stub(&case, &["status 0"]));
    let running = case.start(&routed_words(
        "out.log",
        &steps,
        &["--builder", SLOT_FIRST],
        &[r".\tool.exe", "out", "ran there"],
    ));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(took_on(&case, pid), [SLOT_FIRST], "{outcome:?}");
    assert!(balanced_lines(&outcome).is_empty(), "{outcome:?}");
    let release = release_of(&case, pid);
    assert!(release.get("balanced").is_none(), "{release}");
}

/// A [`client_case`] whose routing file lists [`SLOT_FIRST`] before [`HOST`], both taking its `*tool*` commands, each
/// builder laying its work folder out under a folder of its own; with the folders part of the run's transport.
fn two_folder_case(name: &str) -> (Case, String) {
    let case = client_case(name);
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": HOST,
        "repos": { &repo: ["*tool*"] },
        "outputs": { &repo: [] },
        "builders": [{ "host": SLOT_FIRST, "os": "windows", "repos": { &repo: ["*tool*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    let (first, next) = (case.folder().join("builder").join("a"), case.folder().join("builder").join("b"));
    for folder in [&first, &next] {
        fs::create_dir_all(folder).expect("a builder's folder can be made");
    }
    let folders = format!("{SLOT_FIRST}={}|{HOST}={}", shown(&first), shown(&next));
    (case, folders)
}

/// A routed run of `case` through the stand-in of the steps file `steps`, each builder under its folder of `folders`.
fn two_folder_run(case: &Case, log: &str, steps: &Path, folders: &str) -> Outcome {
    let tuned = Tuned {
        folders: Some(folders),
        ..Tuned::default()
    };
    finish(client_gate_tuned(case, log, steps, &tuned, &[r".\tool.exe", "out", "ran there"]))
}

/// The worktree record of the case's one work-folder key, `<root>\remote\<key>.worktree`.
fn worktree_record_of(case: &Case) -> PathBuf {
    let remote = case.root().join("remote");
    let records: Vec<PathBuf> = fs::read_dir(&remote)
        .expect("the remote folder can be listed")
        .map(|entry| entry.expect("an entry can be read").path())
        .filter(|path| path.extension() == Some(OsStr::new("worktree")))
        .collect();
    assert_eq!(records.len(), 1, "one worktree record: {records:?}");
    records.into_iter().next().expect("one record")
}

/// The `builders` of the case's worktree record, in their order; none when the record has no such key.
fn worktree_builders(case: &Case) -> Vec<String> {
    let text = fs::read_to_string(worktree_record_of(case)).expect("the worktree record can be read");
    let record: Value = serde_json::from_str(&text).expect("the worktree record is JSON");
    record
        .get("builders")
        .map(|builders| serde_json::from_value(builders.clone()).expect("the builders are a list of aliases"))
        .unwrap_or_default()
}

#[test]
fn a_routed_run_s_run_call_adds_its_builder_to_the_worktree_record() {
    let (case, folders) = two_folder_case("routing-record-adds");
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let outcome = two_folder_run(&case, "first.log", &first, &folders);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(worktree_builders(&case), [SLOT_FIRST], "the first builder ran it");

    // The first builder turns the second run away at its preflight; host's run call adds host after it, once.
    let second = ssh_steps(&case, "second", &["on builder-a", "version 0.0.0-other", "on builder", "ok", "ok"]);
    let outcome = two_folder_run(&case, "second.log", &second, &folders);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&second), ["preflight", "preflight", "run"], "{outcome:?}");
    assert_eq!(worktree_builders(&case), [SLOT_FIRST, HOST], "in the order each was first added");

    let third = ssh_steps(&case, "third", &["ok", "ok"]);
    let outcome = two_folder_run(&case, "third.log", &third, &folders);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(worktree_builders(&case), [SLOT_FIRST, HOST], "a builder already listed is listed once");
}

#[test]
fn a_run_refused_at_preflight_adds_no_builder_to_the_worktree_record() {
    let (case, folders) = two_folder_case("routing-record-preflight");
    let steps = ssh_steps(&case, "ssh", &["on builder-a", "version 0.0.0-other", "on builder", "ok", "ok"]);

    let outcome = two_folder_run(&case, "out.log", &steps, &folders);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        has_line(&outcome.stderr, &format!("gate: remote {SLOT_FIRST} version; trying {HOST}")),
        "{outcome:?}"
    );
    assert_eq!(worktree_builders(&case), [HOST], "only the builder whose run call began");
}

/// The box refuses the run `tools` at its run call, after the preflight passed: the run call began, so the builder is
/// listed though the command ran here.
#[test]
fn a_refusal_at_the_run_call_still_adds_its_builder_to_the_worktree_record() {
    let case = client_case("routing-record-run-call-refused");
    fs::create_dir_all(case.folder().join(".config")).expect("the tools folder can be made");
    fs::write(case.folder().join(".config").join("dotnet-tools.json"), "{\n  \"version\": 1\n}\n").expect("the manifest can be written");
    let stub = case.folder().join("ssh").join("stub");
    fs::create_dir_all(&stub).expect("the stub's folder can be made");
    fs::write(stub.join("dotnet.cmd"), "@exit /b 1\r\n").expect("the stub dotnet can be written");
    let own_path = std::env::var_os("PATH").expect("PATH is set");
    let path = std::env::join_paths(std::iter::once(stub).chain(std::env::split_paths(&own_path))).expect("the folders join into a PATH");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran"]);
    gate.env("PATH", path);

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), ["tools"], "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "{outcome:?}");
    assert_eq!(worktree_builders(&case), [HOST]);
}

/// The box cannot clear the work folder, whose layout record says another layout, for a file held open in it, so it
/// refuses the run `layout` at its run call: the refusing builder leaves the record, and another builder stays.
#[test]
fn a_layout_refusal_takes_the_refusing_builder_off_the_worktree_record() {
    use std::os::windows::fs::OpenOptionsExt;

    let case = client_case("routing-record-layout");
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let outcome = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(worktree_builders(&case), [HOST]);
    let record = worktree_record_of(&case);
    let mut written: Value = serde_json::from_str(&fs::read_to_string(&record).expect("the record can be read")).expect("JSON");
    written["builders"] = json!(["other", HOST]);
    fs::write(&record, written.to_string()).expect("the record can be rewritten");
    let key = record.file_stem().expect("the record has a name").to_string_lossy().into_owned();
    let work = case.folder().join("builder").join("work").join(&key);
    fs::write(format!("{}.layout", shown(&work)), "pair app lib").expect("the layout record can be written");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(work.join("held.txt"))
        .expect("a file in the work folder can be held");
    let second = ssh_steps(&case, "second", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran"]));
    drop(held);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder layout; running here"), "{outcome:?}");
    assert_eq!(worktree_builders(&case), ["other"]);
}

/// A Ctrl+C during the wait for a builder slot ends the run before its command started, its remote slot released.
#[test]
fn a_ctrl_c_in_the_builder_slot_wait_ends_the_run_and_frees_the_remote_slot() {
    let case = routed_case("routing-builder-slot-ctrl-c");
    // The builder refuses the run's call full; the wait's first poll, the client's second call, raises the interrupt.
    let steps = shown(&stub(&case, &["refuse full", "interrupt-at 2"]));
    let outcome = finish(case.gate(&routed_words("out.log", &steps, &DRAIN_POLL, &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "the command ran nowhere: {outcome:?}");
    assert_in_order(&outcome.stderr, &[BUILDER_SLOT_WAITING, "gate: interrupted before the command started"]);
    assert!(!outcome.stderr.contains(FULL_LINE), "{outcome:?}");
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
    assert!(!remote_claim(&case).exists(), "the remote claim is gone");
}

/// A run the builder admits after a wait behind its drain, whose admitted call the builder then refuses `full`, waits
/// for a slot there like any other, and runs there once a poll passes.
#[test]
fn a_run_admitted_after_a_drain_wait_and_refused_full_waits_for_a_builder_slot() {
    let case = routed_case("routing-drain-then-full");
    let now = unix_now();
    let (raised, lapses) = (now - 60, now + 1800);
    // Call 1 is the drain's refusal, call 2 the drain poll that passes, call 3 the admitted run the builder refuses
    // full, call 4 the slot poll that passes, and call 5 the run.
    let drained = draining(1, "1", raised, lapses);
    let steps = shown(&stub(&case, &[&drained, "refuse-at 3 full", "out ran there", "status 0"]));
    let running = case.start(&routed_words("out.log", &steps, &DRAIN_POLL, &[r".\tool.exe", "out", "ran here"]));
    let pid = running.id();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[&drain_waiting(raised, lapses, Some(1)), DRAIN_LIFTED, BUILDER_SLOT_WAITING],
    );
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "nothing ran here: {outcome:?}");
    assert!(!outcome.stderr.contains(FULL_LINE), "{outcome:?}");
    let own = own(&case, pid);
    assert_eq!(names(&own), ["queue", "take", "release"], "no fallback: {own:#?}");
}

#[test]
fn a_heavy_run_logging_into_another_worktree_of_its_repo_sends_no_snapshot_and_says_to_cd() {
    let case = routed_case("routing-log-other-worktree");
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let other = beside_case(&case, ".other");
    git(&case, &["worktree", "add", "--quiet", "--detach", &shown(&other)]);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let log = shown(&other.join("out.log"));

    // Started in the case's checkout, its log in the linked worktree: the run the caller meant is the other tree's.
    let outcome = finish(case.gate(&routed_words(&log, &steps, &[], &[FAKE_TOOL, "out", "ran"])));

    assert!(
        !call.exists(),
        "no snapshot of the checkout the gate started in goes to a builder: {outcome:?}"
    );
    let tree = shown(&other).replace('/', "\\");
    assert!(outcome.stderr.contains(&format!("run it with --cd {tree}")), "{outcome:?}");
    assert_eq!(outcome.code, Some(2), "refused as a usage error, nothing run: {outcome:?}");
    let log_text = log_texts(&other.join("out.log"));
    assert!(
        log_text.contains(&format!("run it with --cd {tree}")),
        "the log the run readied first holds its line: {log_text}"
    );
}

#[test]
fn a_run_logging_into_a_worktree_of_another_repository_routes_as_before() {
    let case = routed_case("routing-log-other-repository");
    let main = beside_case(&case, ".main");
    let git_folder = main.join(".git").join("worktrees").join("other");
    fs::create_dir_all(&git_folder).expect("the other repository's git folder can be made");
    fs::write(git_folder.join("commondir"), "../..\n").expect("the commondir file can be written");
    let other = beside_case(&case, ".other");
    let gitdir = shown(&git_folder).replace('\\', "/");
    fs::write(other.join(".git"), format!("gitdir: {gitdir}\n")).expect("a linked worktree's .git file can be written");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let log = shown(&other.join("out.log"));

    let outcome = finish(case.gate(&routed_words(&log, &steps, &[], &[FAKE_TOOL, "out", "ran"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(call.exists(), "a worktree of another repository is no reason to refuse: {outcome:?}");
    assert!(!outcome.stderr.contains("run it with --cd"), "no refusal: {outcome:?}");
}

#[test]
fn a_run_logging_into_a_carried_sibling_s_worktree_routes_as_before() {
    let case = routed_case("routing-log-sibling-worktree");
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let (_, beside) = declare_sibling(&case);
    git(&case, &["worktree", "add", "--quiet", "--detach", &shown(&beside)]);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let log = shown(&beside.join("out.log"));

    let outcome = finish(case.gate(&routed_words(&log, &steps, &[], &[FAKE_TOOL, "out", "ran"])));
    fs::remove_dir_all(&beside).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(call.exists(), "a carried sibling's tree is the run's own: {outcome:?}");
    assert!(!outcome.stderr.contains("run it with --cd"), "no refusal: {outcome:?}");
}

#[test]
fn a_pinned_run_logging_into_another_worktree_of_its_repo_is_refused() {
    let case = routed_case("routing-log-other-worktree-pinned");
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let other = beside_case(&case, ".other");
    git(&case, &["worktree", "add", "--quiet", "--detach", &shown(&other)]);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let log = shown(&other.join("out.log"));
    // A pinned run's log is readied only once routing accepted the pin, so an earlier file stays as it was.
    fs::write(other.join("out.log"), "earlier\n").expect("an earlier log can be written");

    let outcome = finish(case.gate(&routed_words(&log, &steps, &["--builder", HOST], &[FAKE_TOOL, "out", "ran"])));

    assert_eq!(outcome.code, Some(2), "refused as a usage error, nothing run: {outcome:?}");
    assert!(!call.exists(), "nothing goes to the pinned builder: {outcome:?}");
    let tree = shown(&other).replace('/', "\\");
    assert!(outcome.stderr.contains(&format!("run it with --cd {tree}")), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(other.join("out.log")).expect("the earlier log is still there"),
        "earlier\n",
        "an earlier log of a pinned run is left as it was"
    );
}

#[test]
fn a_routed_run_logging_into_its_own_worktree_routes_as_before() {
    let case = routed_case("routing-log-own-worktree");
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let log = shown(&case.folder().join("out.log"));

    let outcome = finish(case.gate(&routed_words(&log, &steps, &[], &[FAKE_TOOL, "out", "ran"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(call.exists(), "a log in the run's own worktree is no reason to refuse: {outcome:?}");
    assert!(!outcome.stderr.contains("run it with --cd"), "no refusal: {outcome:?}");
}

#[test]
fn a_local_run_logging_into_another_worktree_of_its_repo_runs_here_as_before() {
    let case = routed_case("routing-log-other-worktree-local");
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let other = beside_case(&case, ".other");
    git(&case, &["worktree", "add", "--quiet", "--detach", &shown(&other)]);
    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let log = shown(&other.join("out.log"));

    let outcome = finish(case.gate(&routed_words(
        &log,
        &steps,
        &["--local", LOCAL_REASON[0], LOCAL_REASON[1]],
        &[FAKE_TOOL, "out", "ran"],
    )));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!call.exists(), "a run that stays local sends nothing: {outcome:?}");
    assert!(outcome.stdout.contains("ran"), "the command ran here: {outcome:?}");
    assert!(
        !outcome.stderr.contains("run it with --cd"),
        "a run that stays local is not checked: {outcome:?}"
    );
}
