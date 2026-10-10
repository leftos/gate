//! The remote client: its snapshot, through `--test-remote-snapshot`, a routed run that stops once it has made the
//! snapshot and bridge commits of its worktree, a real git repository in the case folder, and says them; and the whole
//! client, through `--test-remote-transport local:<folder>`, whose builder is `gate box` run here on a builder root in
//! the case folder, each ssh call made through the `fake-ssh` stand-in (`--test-remote-ssh`) that a steps file drives.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use gate_router::repo_name;
use serde_json::{Value, json};

use crate::support::{
    Case, GATE, GIT_REDIRECTS, Outcome, allow_breakaway, copy_tool, events, finish, git, git_init, heavy_words, shown, start, wait_for_journal_line,
    wait_until, write_remote_json,
};

const HOST: &str = "builder";
/// How a run of the seam says its commits: `gate: remote snapshot <commit> bridge <commit or none>`.
const SAID: &str = "gate: remote snapshot ";
/// What no snapshot sends: the gate's state root, LOCALAPPDATA and logs, the tool, the holder's step and go files,
/// and the case's own ignored file.
const IGNORED: &str = "state/\nappdata/\n*.log\n*.exe\n*.steps\n/go\nignored.txt\n";
/// The folders at the top of the case that git or the gate itself writes, which a snapshot is not asked to keep.
const NOT_THE_WORKTREE: [&str; 3] = [".git", "state", "appdata"];
const REMOTE_WAITING: &str = "gate: waiting for a remote slot on builder (1 busy)";

/// A file's length, a hash of its bytes, and its modified time.
type Stamp = (u64, u64, SystemTime);

/// What a snapshot must leave as it was: `.git/index`, every file of the worktree, ignored ones too, and git's status.
#[derive(Debug, PartialEq)]
struct Fingerprint {
    index: Option<Stamp>,
    files: BTreeMap<PathBuf, Stamp>,
    status: String,
}

/// A case folder made a checkout of its own, holding fake-tool as `tool.exe` and a `.gitignore` that leaves out the
/// gate's own files, with a routing file that sends its `*tool*` commands to the builder.
fn snapshot_case(name: &str) -> Case {
    let case = Case::new(name);
    git_init(&case);
    copy_tool(&case, "tool.exe");
    write(&case, ".gitignore", IGNORED);
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    write_remote_json(&case, &json!({ "host": HOST, "repos": { repo: ["*tool*"] } }).to_string());
    case
}

/// The words of a routed run logging to `log` that stops after its snapshot, `extra` before `--`.
fn snapshot_words<'a>(log: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut before = vec!["--test-remote-snapshot", "--test-git-limit-ms", "3600000"];
    before.extend(extra);
    heavy_words(log, &before, &[r".\tool.exe", "out", "hi"])
}

/// Writes `text` to the case folder's file `name`, making its folder.
fn write(case: &Case, name: &str, text: &str) {
    let path = case.folder().join(name);
    fs::create_dir_all(path.parent().expect("a worktree file has a folder")).expect("the file's folder can be made");
    fs::write(path, text).expect("a worktree file can be written");
}

/// What git with `args` printed on stdout in the case folder, its trailing line end cut, with `env` set; fails the
/// case when git fails.
fn git_out(case: &Case, env: &[(&str, &Path)], args: &[&str]) -> String {
    let mut command = Command::new("git");
    for name in GIT_REDIRECTS {
        command.env_remove(name);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command
        .args(args)
        .current_dir(case.folder())
        .stdin(Stdio::null())
        .output()
        .expect("git starts");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("git's output is UTF-8").trim_end().to_owned()
}

fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The snapshot and bridge commits a run that ended with status 0 said, once on stderr and once in its log `log`; the
/// bridge `None` when it said none.
fn said(case: &Case, outcome: &Outcome, log: &str) -> (String, Option<String>) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines: Vec<&str> = outcome.stderr.lines().filter(|line| line.starts_with(SAID)).collect();
    assert_eq!(lines.len(), 1, "one snapshot line: {outcome:?}");
    let logged = fs::read_to_string(case.folder().join(log)).expect("the log was written");
    assert_eq!(logged.lines().filter(|line| *line == lines[0]).count(), 1, "{logged}");
    let (commit, bridge) = lines[0][SAID.len()..].split_once(" bridge ").expect("the line names the bridge");
    assert!(is_sha(commit), "a full commit: {commit}");
    let bridge = (bridge != "none").then(|| bridge.to_owned());
    assert!(bridge.as_deref().is_none_or(is_sha), "a full bridge commit: {bridge:?}");
    (commit.to_owned(), bridge)
}

fn stamp(path: &Path) -> Option<Stamp> {
    if !path.try_exists().expect("a path can be looked at") {
        return None;
    }
    let bytes = fs::read(path).expect("a file can be read");
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .expect("a file's modified time can be read");
    let mut hasher = DefaultHasher::new();
    hasher.write(&bytes);
    Some((bytes.len() as u64, hasher.finish(), modified))
}

fn worktree_files(folder: &Path, top: bool, files: &mut BTreeMap<PathBuf, Stamp>) {
    for entry in fs::read_dir(folder).expect("a worktree folder can be listed") {
        let path = entry.expect("a worktree entry can be read").path();
        let name = path.file_name().expect("an entry has a name").to_string_lossy().into_owned();
        let the_gates = path.extension().is_some_and(|extension| extension == "log");
        if the_gates || (top && NOT_THE_WORKTREE.contains(&name.as_str())) {
            continue;
        }
        if path.is_dir() {
            worktree_files(&path, false, files);
        } else {
            files.insert(path.clone(), stamp(&path).expect("a listed file is there"));
        }
    }
}

/// The case's fingerprint, git's status taken last and without the optional locks, so it refreshes no index.
fn fingerprint(case: &Case) -> Fingerprint {
    let index = stamp(&case.folder().join(".git").join("index"));
    let mut files = BTreeMap::new();
    worktree_files(&case.folder(), true, &mut files);
    let status = git_out(case, &[], &["--no-optional-locks", "status", "--porcelain=v2", "--untracked-files=all"]);
    Fingerprint { index, files, status }
}

/// The tree an independent `git add -A` and `git write-tree` make of the worktree on a scratch copy of the index, or on
/// an empty index when there is none.
fn independent_tree(case: &Case) -> String {
    let git_folder = case.folder().join(".git");
    let scratch = git_folder.join("scratch.index");
    let index = git_folder.join("index");
    if index.try_exists().expect("the index can be looked at") {
        fs::copy(&index, &scratch).expect("the index can be copied");
    }
    git_out(case, &[("GIT_INDEX_FILE", &scratch)], &["add", "-A"]);
    git_out(case, &[("GIT_INDEX_FILE", &scratch)], &["write-tree"])
}

fn tree_of(case: &Case, commit: &str) -> String {
    git_out(case, &[], &["rev-parse", &format!("{commit}^{{tree}}")])
}

/// The `gate-remote-*.index` files in the case's git folder.
fn index_copies(case: &Case) -> Vec<String> {
    fs::read_dir(case.folder().join(".git"))
        .expect("the git folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("gate-remote-"))
        .collect()
}

#[test]
fn a_snapshot_leaves_the_index_and_worktree_byte_identical() {
    let case = snapshot_case("snapshot-identical");
    git(&case, &["config", "core.autocrlf", "true"]);
    for (name, text) in [
        ("kept.txt", "kept\n"),
        ("staged.txt", "before\n"),
        ("unstaged.txt", "before\n"),
        ("deleted.txt", "gone soon\n"),
        ("lf.txt", "one\ntwo\n"),
        ("crlf.txt", "one\r\ntwo\r\n"),
        (r"sub\nested.txt", "deep\n"),
    ] {
        write(&case, name, text);
    }
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    write(&case, "staged.txt", "after\n");
    git(&case, &["add", "staged.txt"]);
    write(&case, "unstaged.txt", "after\n");
    fs::remove_file(case.folder().join("deleted.txt")).expect("a tracked file can be deleted");
    write(&case, "untracked.txt", "new\n");
    write(&case, "untracked-crlf.txt", "a\r\nb\r\n");
    write(&case, "ignored.txt", "left out\n");
    let before = fingerprint(&case);
    for name in ["staged.txt", "unstaged.txt", "deleted.txt", "untracked.txt"] {
        assert!(before.status.contains(name), "the status shows {name}: {}", before.status);
    }

    let outcome = finish(case.gate(&snapshot_words("out.log", &[])));

    said(&case, &outcome, "out.log");
    assert_eq!(fingerprint(&case), before, "{outcome:?}");
    assert_eq!(index_copies(&case), Vec::<String>::new(), "the index copy is removed");
}

#[test]
fn a_snapshot_is_the_tree_git_would_write_for_the_worktree() {
    let case = snapshot_case("snapshot-tree");
    write(&case, "a.txt", "a\n");
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    write(&case, "a.txt", "changed\n");
    write(&case, "b.txt", "untracked\n");
    write(&case, r"sub\c.txt", "deep\n");
    write(&case, "ignored.txt", "left out\n");

    let outcome = finish(case.gate(&snapshot_words("out.log", &[])));

    let (commit, bridge) = said(&case, &outcome, "out.log");
    assert_eq!(bridge, None, "no snapshot before it");
    assert_eq!(tree_of(&case, &commit), independent_tree(&case));
    let listed = git_out(&case, &[], &["ls-tree", "-r", "--name-only", &commit]);
    assert_eq!(listed.lines().collect::<Vec<_>>(), [".gitignore", "a.txt", "b.txt", "sub/c.txt"]);
    let object = git_out(&case, &[], &["cat-file", "commit", &commit]);
    let lines: Vec<&str> = object.lines().collect();
    assert!(!lines.iter().any(|line| line.starts_with("parent ")), "no parent: {object}");
    assert!(lines[1].starts_with("author gate-remote <gate-remote@localhost> "), "{object}");
    assert!(lines[2].starts_with("committer gate-remote <gate-remote@localhost> "), "{object}");
    assert_eq!(lines.last(), Some(&"gate-remote snapshot"), "{object}");
}

#[test]
fn a_snapshot_of_a_repo_with_no_index_and_no_commit() {
    let case = snapshot_case("snapshot-unborn");
    write(&case, "a.txt", "a\n");
    let index = case.folder().join(".git").join("index");
    assert!(!index.exists(), "a new repository has no index");

    let outcome = finish(case.gate(&snapshot_words("out.log", &[])));

    let (commit, bridge) = said(&case, &outcome, "out.log");
    assert_eq!(bridge, None);
    let listed = git_out(&case, &[], &["ls-tree", "-r", "--name-only", &commit]);
    assert_eq!(listed.lines().collect::<Vec<_>>(), [".gitignore", "a.txt"]);
    assert!(!index.exists(), "no index file appears");
    assert_eq!(index_copies(&case), Vec::<String>::new(), "the index copy is removed");
}

/// The gate's index copy is named for its own process, so a copy and the lock beside it, left by a dead process of the
/// same id, are planted for the waiting gate once it says it waits for the remote slot a holder keeps. With no real
/// index nothing is copied over the copy, so only its deletion keeps git from reading it, and the lock would make the
/// `git add` that writes the copy refuse to run at all.
#[test]
fn a_left_over_index_copy_is_replaced() {
    let case = snapshot_case("snapshot-left-over");
    write(&case, "a.txt", "a\n");
    let go = case.folder().join("go");
    let steps = case.folder().join("holder.steps");
    fs::write(&steps, format!("wait {}", shown(&go))).expect("the holder's steps can be written");
    let steps = shown(&steps);
    let holder_seams = ["--test-remote-client", &steps, "--test-git-limit-ms", "3600000"];
    let holder = case.start(&heavy_words("holder.log", &holder_seams, &[r".\tool.exe", "out", "hi"]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));
    let waiter = case.start(&snapshot_words("waiter.log", &["--test-remote-wait-ms", "3600000"]));
    waiter.wait_for_stderr(REMOTE_WAITING);
    let left = case.folder().join(".git").join(format!("gate-remote-{}.index", waiter.id()));
    let lock = case.folder().join(".git").join(format!("gate-remote-{}.index.lock", waiter.id()));
    fs::write(&left, "not an index").expect("the left-over copy can be planted");
    fs::write(&lock, "not a lock").expect("the left-over lock can be planted");
    fs::write(&go, "").expect("the go file can be made");

    let outcome = waiter.finish();
    let held = holder.finish();

    assert_eq!(held.code, Some(0), "{held:?}");
    let (commit, _) = said(&case, &outcome, "waiter.log");
    assert!(!left.exists(), "the left-over copy is gone");
    assert!(!lock.exists(), "the lock beside it is gone");
    assert_eq!(tree_of(&case, &commit), independent_tree(&case));
}

#[test]
fn a_second_snapshot_bridges_from_the_first() {
    let case = snapshot_case("snapshot-bridge");
    write(&case, "a.txt", "a\n");
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    let first = finish(case.gate(&snapshot_words("first.log", &[])));
    let (snapshot, bridge) = said(&case, &first, "first.log");
    assert_eq!(bridge, None, "no snapshot before the first");
    let key = events(&case)[0]["key"].as_str().expect("the queue line has a key").to_owned();
    git(&case, &["update-ref", &format!("refs/gate-remote/{key}"), &snapshot]);
    write(&case, "a.txt", "b\n");

    let second = finish(case.gate(&snapshot_words("second.log", &[])));

    let (next, bridge) = said(&case, &second, "second.log");
    let bridge = bridge.expect("a bridge commit from the first snapshot");
    assert_eq!(
        git_out(&case, &[], &["rev-list", "--parents", "-n", "1", &bridge]),
        format!("{bridge} {snapshot}"),
        "the bridge's one parent is the first snapshot"
    );
    assert_eq!(tree_of(&case, &bridge), tree_of(&case, &next), "the bridge carries the snapshot's tree");
    assert_ne!(tree_of(&case, &next), tree_of(&case, &snapshot), "the worktree changed between them");
}

/// A machine whose git is asked to sign every commit and whose signing program is not there: the snapshot makes its
/// commits all the same, and signs none of them.
#[test]
fn a_snapshot_ignores_commit_signing() {
    let case = snapshot_case("snapshot-signing");
    write(&case, "a.txt", "a\n");
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    git(&case, &["config", "commit.gpgsign", "true"]);
    git(&case, &["config", "gpg.program", r"\no-such-gpg-program"]);

    let outcome = finish(case.gate(&snapshot_words("out.log", &[])));

    let (commit, _) = said(&case, &outcome, "out.log");
    let object = git_out(&case, &[], &["cat-file", "commit", &commit]);
    assert!(!object.contains("gpgsig"), "the commit carries no signature: {object}");
}

/// The suite's stand-in for ssh, which `--test-remote-ssh` names.
const FAKE_SSH: &str = env!("CARGO_BIN_EXE_fake-ssh");
/// The variable naming the stand-in's steps file; the stand-in logs each call's verb to `<steps>.log`.
const STEPS_VARIABLE: &str = "GATE_FAKE_SSH_STEPS";
/// The variable naming the gate the stand-in runs `box <verb> <payload>` with.
const GATE_VARIABLE: &str = "GATE_FAKE_SSH_GATE";
/// What a client case's snapshot leaves out: the gate's state root, LOCALAPPDATA and logs, the builder root and the
/// stand-in's files. `tool.exe` is sent, so the builder runs it.
const CLIENT_IGNORED: &str = "state/\nappdata/\n*.log\nbuilder/\nssh/\n";
const RETRY_LINE: &str = "gate: builder did not answer the ssh banner; trying once more";
const BUSY_LINE: &str = "gate: remote builder busy; running here";
const UNREACHABLE_LINE: &str = "gate: remote builder unreachable; running here";

/// A case folder made a checkout of its own holding fake-tool as `tool.exe`, which its snapshot sends, with a routing
/// file that sends its `*tool*` commands to the builder and gives the remote pool two slots. The suite enters a job
/// that allows breakaway first, as the box's command needs.
fn client_case(name: &str) -> Case {
    client_case_of(name, 2)
}

/// [`client_case`] with a remote pool of `slots` slots.
fn client_case_of(name: &str, slots: u32) -> Case {
    allow_breakaway();
    let case = Case::new(name);
    git_init(&case);
    copy_tool(&case, "tool.exe");
    write(&case, ".gitignore", CLIENT_IGNORED);
    fs::create_dir_all(case.folder().join("ssh")).expect("the stand-in's folder can be made");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    write_remote_json(&case, &json!({ "host": HOST, "slots": slots, "repos": { repo: ["*tool*"] } }).to_string());
    case
}

/// Writes `steps`, one a line, as the stand-in's steps file `name` in the case's `ssh` folder; its path.
fn ssh_steps(case: &Case, name: &str, steps: &[&str]) -> PathBuf {
    let path = case.folder().join("ssh").join(format!("{name}.steps"));
    fs::write(&path, steps.join("\n")).expect("the steps file can be written");
    path
}

/// The verbs the stand-in of the steps file `steps` was called with, in order; none before its first call.
fn calls(steps: &Path) -> Vec<String> {
    let mut log = steps.as_os_str().to_owned();
    log.push(".log");
    fs::read_to_string(PathBuf::from(log)).map_or_else(|_| Vec::new(), |text| text.lines().map(str::to_owned).collect())
}

/// The gate of a routed run logging to `log` through the Rust client: the builder root `builder` in the case folder,
/// each ssh call made through the stand-in of the steps file `steps`, the retry pause 1 ms.
fn client_gate(case: &Case, log: &str, steps: &Path, command: &[&str]) -> Command {
    let transport = format!("local:{}", shown(&case.folder().join("builder")));
    let seams = [
        "--test-remote-transport",
        transport.as_str(),
        "--test-remote-ssh",
        FAKE_SSH,
        "--test-remote-retry-pause-ms",
        "1",
        "--test-git-limit-ms",
        "3600000",
    ];
    let mut gate = case.gate(&heavy_words(log, &seams, command));
    gate.env(STEPS_VARIABLE, steps).env(GATE_VARIABLE, GATE);
    gate
}

/// The journal lines of the gate `pid`.
fn own(case: &Case, pid: u32) -> Vec<Value> {
    events(case).into_iter().filter(|event| event["pid"] == pid).collect()
}

/// The reasons of the `fallback` lines among `events`.
fn fallbacks(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter(|event| event["event"] == "fallback")
        .map(|event| event["reason"].as_str().expect("a fallback names its reason"))
        .collect()
}

/// The work-folder key the run's queue line names.
fn key_of(events: &[Value]) -> String {
    events[0]["key"].as_str().expect("the queue line has a key").to_owned()
}

/// The commit `name` names in the case's builder's bare repository, empty when it names none.
fn builder_ref(case: &Case, name: &str) -> String {
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let bare = shown(&case.folder().join("builder").join(format!("{repo}.git")));
    git_out(case, &[], &["--git-dir", &bare, "for-each-ref", "--format=%(objectname)", name])
}

/// Runs `gate` to its end, with its pid.
fn finish_with_pid(gate: Command) -> (u32, Outcome) {
    let running = start(gate);
    let pid = running.id();
    (pid, running.finish())
}

#[test]
fn a_routed_run_runs_on_the_box_and_passes_its_output_and_status() {
    let case = client_case("client-through");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there", "exit", "3"]));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: pushed snapshot "), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.contains("ran there") && log.contains("gate: pushed snapshot "), "{log}");
    let events = events(&case);
    assert_eq!(fallbacks(&events), Vec::<&str>::new(), "{events:#?}");
    assert!(events.iter().all(|event| event["remote"] == true), "a remote run: {events:#?}");
    let key = key_of(&events);
    let snapshot = builder_ref(&case, &format!("refs/gate/{key}"));
    assert!(is_sha(&snapshot), "the builder holds the snapshot: {snapshot:?}");
    assert_eq!(git_out(&case, &[], &["rev-parse", &format!("refs/gate-remote/{key}")]), snapshot);
    assert!(log.contains(&format!("gate: pushed snapshot {} in ", &snapshot[..8])), "{log}");
}

#[test]
fn a_second_routed_run_of_one_worktree_falls_back_busy_at_once() {
    // One slot, which the first run holds: the second finds the key's lock taken before it would wait for the slot.
    let case = client_case_of("client-busy", 1);
    let go = case.folder().join("ssh").join("go");
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(&go))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);

    let (pid, second) = finish_with_pid(client_gate(&case, "second.log", &second_steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(second.stderr.contains(BUSY_LINE), "{second:?}");
    assert!(second.stdout.contains("ran here"), "it ran here: {second:?}");
    assert_eq!(calls(&second_steps), Vec::<String>::new(), "the second run made no ssh call");
    assert_eq!(fallbacks(&own(&case, pid)), ["busy"]);
    fs::write(&go, "").expect("the go file can be made");
    let first = first.finish();
    assert_eq!(first.code, Some(0), "{first:?}");
    assert!(first.stdout.contains("ran there"), "{first:?}");
    assert_eq!(calls(&first_steps), ["preflight", "run"]);
}

#[test]
fn a_connection_dropped_before_the_banner_is_retried_once() {
    let case = client_case("client-retry");
    let steps = ssh_steps(&case, "ssh", &["fail-banner", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 1, "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "preflight", "run"]);
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn two_banner_drops_of_the_preflight_fall_back_unreachable() {
    let case = client_case("client-preflight-drops");
    let steps = ssh_steps(&case, "ssh", &["fail-banner", "fail-banner"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 1, "{outcome:?}");
    assert!(outcome.stderr.contains(UNREACHABLE_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "preflight"]);
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

#[test]
fn two_banner_drops_of_the_run_call_fall_back_unreachable() {
    let case = client_case("client-run-drops");
    let steps = ssh_steps(&case, "ssh", &["ok", "fail-banner", "fail-banner"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 1, "{outcome:?}");
    assert!(outcome.stderr.contains(UNREACHABLE_LINE), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.contains("ran here"), "it ran here: {log}");
    assert_eq!(calls(&steps), ["preflight", "run", "run"]);
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

#[test]
fn a_drop_after_output_is_not_retried() {
    let case = client_case("client-drop-after-output");
    let steps = ssh_steps(&case, "ssh", &["ok", "out-then-drop"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(255), "{outcome:?}");
    assert!(!outcome.stderr.contains(RETRY_LINE), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_drop_with_another_first_line_is_not_retried() {
    let case = client_case("client-drop-other");
    let steps = ssh_steps(&case, "ssh", &["fail-other"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains(RETRY_LINE), "{outcome:?}");
    assert!(outcome.stderr.contains(UNREACHABLE_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

#[test]
fn a_run_call_longer_than_the_cap_fails_before_anything_is_sent() {
    let case = client_case("client-cap");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let long = "x".repeat(6200);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", &long]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: the remote run failed: the run call is "), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains(" characters, longer than the 8127 cmd takes (8191 less 64 for sshd's own cmd.exe /c); shorten the command"),
        "{outcome:?}"
    );
    assert!(!outcome.stdout.contains(&long), "the command ran nowhere: {outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "nothing was sent");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_builder_without_a_usable_gate_box_is_not_routed() {
    let case = client_case("client-no-box");
    let steps = ssh_steps(&case, "ssh", &["exit 9009"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = "gate: not routed: builder has no gate box that takes this run (preflight status 9009)";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder version; running here"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    assert_eq!(fallbacks(&events(&case)), ["version"]);
    assert!(!case.folder().join("builder").exists(), "nothing reached the builder");
}

#[test]
fn a_box_of_another_version_is_not_routed() {
    let case = client_case("client-version");
    let steps = ssh_steps(&case, "ssh", &["version 0.0.0-other"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = format!("gate: not routed: builder runs gate 0.0.0-other, this PC {}", env!("CARGO_PKG_VERSION"));
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder version; running here"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    let events = events(&case);
    assert_eq!(fallbacks(&events), ["version"]);
    assert_eq!(builder_ref(&case, &format!("refs/gate/{}", key_of(&events))), "", "nothing was pushed");
}
