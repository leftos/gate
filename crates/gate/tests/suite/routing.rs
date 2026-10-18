//! Routing: which heavy runs the gate sends to the builder, what it says and journals for each decision, the run on
//! the builder through the scripted remote client (`--test-remote-client`), the remote pool and its wait, and the
//! fallback here of a routed run that the pool, the builder or the client turned away.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicU64, Ordering};

use gate_router::{DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE, repo_name};
use serde_json::{Value, json};

use crate::nesting::{LOOK_OFTEN, end_parent, first_event, marked, orphaned_as, spawned, test_marker};

use crate::support::{
    Case, DenyEntry, FAKE_TOOL, GATE, Go, Junction, Outcome, Running, assert_in_order, beside, copy_tool, events, finish, git, git_init, heavy_words,
    nothing_back, powershell_folder, record_path, release, run_words, shown, wait_for_journal_line, wait_until, write_counts, write_remote_json,
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
const GIT_HANG_GUARDS: [&str; 4] = ["--test-git-limit-ms", "3600000", "--test-git-drain-ms", "600000"];

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

fn has_no_remote_fields(event: &Value) -> bool {
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
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_in_order(&log, &["ran here", no_ssh]);
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
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
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

#[test]
fn a_local_run_says_no_dropped_variables() {
    let case = routed_case("routing-local-dropped-env");
    let steps = shown(&stub(&case, &["status 0"]));
    let mut gate = case.gate(&routed_words("out.log", &steps, &["--local"], &[r".\tool.exe", "out", "ran"]));
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
fn a_relative_log_is_read_against_the_caller_s_folder_not_the_cd_folder() {
    let case = Case::new("cd-relative-log");
    let there = cd_folder(&case, "there");
    for folder in [case.folder(), there.clone()] {
        fs::create_dir_all(folder.join(".tmp")).expect("a .tmp folder can be made");
    }
    let outcome = finish(case.gate(&run_words(".tmp/x.log", &["--cd", "there"], &[FAKE_TOOL, "out", "logged"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join(".tmp").join("x.log")).expect("the log is under the caller's folder");
    assert!(log.contains("logged"), "{log}");
    assert!(!there.join(".tmp").join("x.log").exists(), "no log under the --cd folder");
}

#[test]
fn a_local_run_given_cd_runs_its_command_in_that_folder() {
    let case = Case::new("cd-local-run");
    let there = cd_folder(&case, "there");
    copy_tool(&case, r"there\tool.exe");
    let outcome = finish(case.gate(&heavy_words_with_limit(
        "out.log",
        &["--local", "--cd", "there"],
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
fn a_declared_sibling_missing_beside_the_worktree_runs_a_matched_heavy_gate_here_saying_so() {
    let case = routed_case("routing-sibling-missing");
    let (name, _) = declare_sibling(&case);
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains(&format!("gate: not routed: sibling {name} not found beside the worktree\n")),
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
    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "ran here"])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains(&format!("gate: not routed: sibling {name} not found beside the worktree\n")),
        "the declared sibling was read: {outcome:?}"
    );
    assert!(!outcome.stderr.contains("read from"), "the line is the log's alone: {outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    let read = format!("gate: .gate/siblings.toml read from {}", folder.join("siblings.toml").display());
    assert!(log.lines().any(|line| line == read), "{read}: {log}");
    assert_local(&events(&case), None);
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
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
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
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
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
        ["test"],
        "the box's dotnet drops --no-build"
    );
    assert!(stdout_lines(&other.stdout).windows(3).any(|words| words == flags), "{other:?}");
    assert_in_order(&dotnet.stderr, &[DROPPED_NO_BUILD_LINE, NO_OUTPUTS_LINE, BUSY_LINE]);
    assert_eq!(
        command_lines(&dotnet.stdout),
        ["test"],
        "a fallback after any refusal drops --no-build: {dotnet:?}"
    );
    let events = events(&case);
    assert_refused(&events[..5], "busy");
    assert_refused(&events[5..], "busy");
}

#[test]
fn a_dotnet_no_build_command_on_a_full_pool_runs_here_without_it() {
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
        ["test"],
        "the fallback runs here as `test`, without --no-build: {outcome:?}"
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
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.contains("ran there") && !log.contains("ran here"), "{log}");
    let events = events(&case);
    assert_eq!(names(&events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_remote_tag), "{events:#?}");
    let slot = events[1]["slots"][0].as_str().expect("the take names its slot");
    assert!(slot.ends_with("remote-slot-0"), "{slot}");
    assert_eq!((&events[2]["verdict"], &events[2]["adjustedSeconds"]), (&json!("passed"), &json!(1.25)));
    assert!(!remote_claim(&case).exists(), "the remote claim is gone after the release");
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
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
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
        ["build", stamp.as_str()],
        "a routed dotnet build is pinned to the worktree HEAD: {call}"
    );
    assert_eq!(names(&events(&case)), ["queue", "take", "release"]);
}

#[test]
fn no_build_is_dropped_for_the_box_and_the_fallback() {
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
        ["test"],
        "a fallback after any refusal drops --no-build: {before:?}"
    );
    assert_eq!(command_lines(&after.stdout), ["test"], "{after:?}");
    assert!(
        recorded_words(&recorded(&tool_call)).contains(&"--no-build"),
        "a non-dotnet command keeps it"
    );
    assert_eq!(
        recorded_words(&recorded(&pack_call))[1..],
        ["pack", "--no-build"],
        "pack keeps it on the box"
    );
    assert!(!pack.stderr.contains(DROPPED_NO_BUILD_LINE), "{pack:?}");
    assert_eq!(command_lines(&pack.stdout), ["pack --no-build"], "and here: {pack:?}");
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

/// The lines the command printed: the stdout lines before the first of the gate's own, which the tail repeats.
fn command_lines(stdout: &str) -> Vec<&str> {
    stdout.lines().take_while(|line| !line.starts_with("gate: ")).collect()
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
        Some(NOTHING_BACK_RECORD),
        "a build of one project leaves it: {one:?}"
    );
    assert_eq!(after_project.as_deref(), Some(NOTHING_BACK_RECORD), "--project leaves it: {project:?}");
    assert_eq!(after_whole, None, "a build naming no project clears it: {whole:?}");
    assert_eq!(nothing_back(&case), None, "a build of a solution clears it: {solution:?}");
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

    let outcome = finish(case.gate(&heavy_words_with_limit("local.log", &["--local"], &NO_BUILD_TEST)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(NOTHING_BACK_LINE), "{outcome:?}");
    assert_eq!(command_lines(&outcome.stdout), ["test"], "{outcome:?}");
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
    assert_eq!(words[5..], ["test", stamp.as_str(), "--", "--x"], "{recorded}");
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
    let unrestored = run("unrestored.log", &[r".\dotnet.cmd", "test", "App.sln", "--no-build"]);
    assert!(
        unrestored.stderr.contains(line),
        "a solution listing an unrestored project drops it: {unrestored:?}"
    );
    assert_eq!(command_lines(&unrestored.stdout), ["test App.sln"], "{unrestored:?}");
    let named = run("named.log", &[r".\dotnet.cmd", "test", "app/App.csproj", "--no-build"]);
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
        &["--local"],
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
        &["--local"],
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
