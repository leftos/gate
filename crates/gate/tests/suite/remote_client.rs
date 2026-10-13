//! The remote client: its snapshot, through `--test-remote-snapshot`, a routed run that stops once it has made the
//! snapshot and bridge commits of its worktree, a real git repository in the case folder, and says them; and the whole
//! client, through `--test-remote-transport local:<folder>`, whose builder is `gate box` run here on a builder root in
//! the case folder, each ssh call made through the `fake-ssh` stand-in (`--test-remote-ssh`) that a steps file drives.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::io::{self, ErrorKind, Write as _};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use gate_router::{repo_name, work_folder_key};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::support::{
    Case, FAKE_TOOL, GATE, GIT_REDIRECTS, Go, Outcome, allow_breakaway, build_version, copy_tool, events, finish, git, git_init, heavy_words,
    nothing_back, release, shown, start, wait_for_journal_line, wait_until, wait_until_closed, write_remote_json,
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
/// stand-in's files, and `.tmp/`, which the client writes in. `tool.exe` is sent, so the builder runs it.
const CLIENT_IGNORED: &str = "state/\nappdata/\n*.log\nbuilder/\nssh/\n.tmp/\n";
const RETRY_LINE: &str = "gate: builder did not answer the ssh banner; trying once more";
const BUSY_LINE: &str = "gate: remote builder busy; running here";
const UNREACHABLE_LINE: &str = "gate: remote builder unreachable; running here";
/// The line of a run call whose connection was lost on a builder that did not restart.
const LOST_LINE: &str = "gate: lost the connection to builder";
/// The boot time a case's preflight says the builder booted at.
const BOOTED: &str = "2026-10-10T08:00:00.0000000Z";

/// A case folder made a checkout of its own holding fake-tool as `tool.exe`, which its snapshot sends, with a routing
/// file that sends its `*tool*` commands to the builder with no output globs and gives the remote pool two slots. The
/// suite enters a job that allows breakaway first, as the box's command needs.
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
    let routing = json!({ "host": HOST, "slots": slots, "repos": { &repo: ["*tool*"] }, "outputs": { &repo: [] } });
    write_remote_json(&case, &routing.to_string());
    case
}

/// The output globs of an outputs case: every file under a `bin` folder.
const BIN: [&str; 1] = ["**/bin/**"];

/// A [`client_case`] whose routing file gives its repo the output globs `globs` and sends its `*dotnet*` commands to
/// the builder too, and whose `.gitignore` leaves `bin/` out, as a build's does: what a command builds there comes back
/// as outputs, not as synced-back changes, and stays in the builder's work folder from one run to the next.
fn outputs_case(name: &str, globs: &[&str]) -> Case {
    let case = client_case(name);
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\n"));
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: ["*tool*", "*dotnet*"] }, "outputs": { &repo: globs } });
    write_remote_json(&case, &routing.to_string());
    case
}

/// The outputs pending marker of the case's work-folder key, which its first run's queue line names, in its state root.
fn pending_marker(case: &Case) -> PathBuf {
    case.root().join("remote").join(format!("{}.outputs-pending", key_of(&events(case))))
}

/// The hash cache of the case's work-folder key, which its first run's queue line names, in its state root.
fn hashes_file(case: &Case) -> PathBuf {
    case.root().join("remote").join(format!("{}.outputs-hashes", key_of(&events(case))))
}

/// The case's hash cache, read as JSON.
fn hashes(case: &Case) -> Value {
    let text = fs::read_to_string(hashes_file(case)).expect("the hash cache was saved");
    serde_json::from_str(&text).expect("the hash cache is JSON")
}

/// Rewrites the case's hash cache so it holds `sha256` for the file `path`, its stamp left as saved.
fn plant(case: &Case, path: &str, sha256: &str) {
    let mut cache = hashes(case);
    assert!(cache["files"].get(path).is_some(), "the cache holds {path}: {cache}");
    cache["files"][path]["sha256"] = Value::from(sha256);
    fs::write(hashes_file(case), format!("{cache}\n")).expect("the hash cache can be rewritten");
}

/// An hour, the age a file is given so a run takes it as settled.
const HOUR: Duration = Duration::from_hours(1);

/// Sets the last write of the case folder's file `name` to `at`.
fn written_at(case: &Case, name: &str, at: SystemTime) {
    fs::File::options()
        .write(true)
        .open(case.folder().join(name))
        .and_then(|file| file.set_modified(at))
        .expect("the file's last write can be set");
}

/// Sets the last write of the case folder's file `name` to `age` before now: a freshly written file is not settled,
/// and a run keeps no hash of it.
fn backdate(case: &Case, name: &str, age: Duration) {
    written_at(case, name, SystemTime::now() - age);
}

/// The SHA-256 of `text`, in lower-case hex.
fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().fold(String::new(), |mut said, byte| {
        let _ = write!(said, "{byte:02x}");
        said
    })
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
    client_gate_through(case, "local", log, steps, command)
}

/// [`client_gate`] through the local transport of the form `kind`: `local`, or `local-ssh`, which pushes and fetches
/// through the stand-in as git's own ssh, so each of git's calls to the builder is a call of the stand-in too.
fn client_gate_through(case: &Case, kind: &str, log: &str, steps: &Path, command: &[&str]) -> Command {
    let tuned = Tuned { kind, ..Tuned::default() };
    client_gate_tuned(case, log, steps, &tuned, command)
}

/// What a case changes in the words of a client run: the transport's form, the ceiling, and more options before the
/// command.
struct Tuned<'a> {
    kind: &'a str,
    timeout: &'a str,
    extra: &'a [&'a str],
    /// The stand-in `--test-remote-ssh` names, [`FAKE_SSH`] when none.
    ssh: Option<&'a str>,
}

impl Default for Tuned<'_> {
    fn default() -> Self {
        Tuned {
            kind: "local",
            timeout: "3600",
            extra: &[],
            ssh: None,
        }
    }
}

/// [`client_gate`] with the words `tuned` changes.
fn client_gate_tuned(case: &Case, log: &str, steps: &Path, tuned: &Tuned<'_>, command: &[&str]) -> Command {
    let transport = format!("{}:{}", tuned.kind, shown(&case.folder().join("builder")));
    let ssh = tuned.ssh.unwrap_or(FAKE_SSH);
    let mut seams = vec![
        "--test-remote-transport",
        transport.as_str(),
        "--test-remote-ssh",
        ssh,
        "--test-remote-retry-pause-ms",
        "1",
        "--test-git-limit-ms",
        "3600000",
    ];
    seams.extend(tuned.extra);
    let mut words = heavy_words(log, &seams, command);
    assert_eq!(words[3], "--timeout", "the ceiling's value follows it");
    words[4] = tuned.timeout;
    let mut gate = case.gate(&words);
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

/// The builder's bare repository of the case's repo.
fn bare_of(case: &Case) -> String {
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    shown(&case.folder().join("builder").join(format!("{repo}.git")))
}

/// The commit `name` names in the case's builder's bare repository, empty when it names none.
fn builder_ref(case: &Case, name: &str) -> String {
    git_out(case, &[], &["--git-dir", &bare_of(case), "for-each-ref", "--format=%(objectname)", name])
}

/// The 8 hex digits of the snapshot the run logging to `log` pushed, from its `gate: pushed snapshot <8> in …` line.
fn pushed_snapshot(case: &Case, log: &str) -> String {
    let text = fs::read_to_string(case.folder().join(log)).expect("the log was written");
    let line = text
        .lines()
        .find(|line| line.starts_with("gate: pushed snapshot "))
        .expect("the run pushed a snapshot");
    line["gate: pushed snapshot ".len()..].chars().take(8).collect()
}

/// The `gate-remote-*.patch` files the client left in the case's git folder.
fn patch_files(case: &Case) -> Vec<String> {
    fs::read_dir(case.folder().join(".git"))
        .expect("the git folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("gate-remote-") && Path::new(name).extension().is_some_and(|kind| kind.to_str() == Some("patch")))
        .collect()
}

/// The case worktree's git status, without the optional locks, so reading it refreshes no index.
fn porcelain(case: &Case) -> String {
    git_out(case, &[], &["--no-optional-locks", "status", "--porcelain=v2", "--untracked-files=all"])
}

/// Pushes an empty commit of the case's own, whose parent is the case's commit before it, into the builder's bare
/// repository at `refs/gate-back/<key>`: the stale back commit a client must refuse to apply. Its sha.
fn plant_back(case: &Case, key: &str) -> String {
    git(case, &["commit", "--quiet", "--allow-empty", "-m", "stale"]);
    let stale = git_out(case, &[], &["rev-parse", "HEAD"]);
    git_out(case, &[], &["push", "--quiet", &bare_of(case), &format!("{stale}:refs/gate-back/{key}")]);
    stale
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
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);

    let (pid, second) = finish_with_pid(client_gate(&case, "second.log", &second_steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(second.stderr.contains(BUSY_LINE), "{second:?}");
    assert!(second.stdout.contains("ran here"), "it ran here: {second:?}");
    assert_eq!(calls(&second_steps), Vec::<String>::new(), "the second run made no ssh call");
    assert_eq!(fallbacks(&own(&case, pid)), ["busy"]);
    fs::write(go.path(), "").expect("the go file can be made");
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
    let booted = format!("boot {BOOTED}");
    let steps = ssh_steps(&case, "ssh", &[&booted, "out-then-drop", "ok", &booted]);

    let outcome = finish(lost_gate(&case, "local", &steps, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(255), "{outcome:?}");
    assert!(!outcome.stderr.contains(RETRY_LINE), "{outcome:?}");
    assert!(outcome.stderr.contains(LOST_LINE), "a lost connection: {outcome:?}");
    assert!(!outcome.stderr.contains("gate: synced back"), "nothing is synced back: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop", "boot"], "the stop, then the boot query");
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

/// The file a case's command marks each time it runs, relative to the folder it runs in: the case folder here, the work
/// folder on the builder. A path the command names inside the worktree is sent relative, and one outside it keeps the
/// run here, so each place keeps its own count; a `*.log`, so no snapshot sends it and no sync-back brings it.
const RAN: &str = "ran.log";

/// How many times the case's command marked [`RAN`] in `folder`: none when it never ran there.
fn runs_in(folder: &Path) -> usize {
    match fs::read_to_string(folder.join(RAN)) {
        Err(error) if error.kind() == ErrorKind::NotFound => 0,
        read => read.expect("the ran file can be read").lines().count(),
    }
}

/// How many times the case's command ran here, and how many on the builder in the work folder of the case's first run.
fn runs(case: &Case) -> (usize, usize) {
    let work = case.folder().join("builder").join("work").join(key_of(&events(case)));
    (runs_in(&case.folder()), runs_in(&work))
}

/// Holds the builder's lock of the work folder `key`, as another run on the builder holds it, until it is dropped.
fn hold_box_lock(case: &Case, key: &str) -> fs::File {
    let lock = case.folder().join("builder").join("work").join(format!("{key}.lock"));
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&lock)
        .expect("the builder's work folder lock can be held")
}

#[test]
fn a_command_that_prints_the_banner_words_and_exits_255_runs_once() {
    let case = client_case("client-own-banner-words");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "ok"]);
    let command = [
        r".\tool.exe",
        "mark",
        RAN,
        "err",
        "Connection timed out during banner exchange",
        "exit",
        "255",
    ];

    let outcome = finish(lost_gate(&case, "local", &steps, &command));

    assert_eq!(outcome.code, Some(255), "a lost connection's status: {outcome:?}");
    assert!(!outcome.stderr.contains(RETRY_LINE), "no retry: {outcome:?}");
    assert!(outcome.stderr.contains(LOST_LINE), "a lost connection: {outcome:?}");
    assert!(!outcome.stderr.contains(UNREACHABLE_LINE), "{outcome:?}");
    assert_eq!(runs(&case), (0, 1), "the command ran once, on the builder: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop", "boot"], "the stop, then the boot query");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// A run whose command says the box's human refusal line for its work folder, `line` of the key, and exits 75, after a
/// first run that names the key: the 75 is the command's own, with no fallback and no run here.
fn a_command_s_own_refusal_line_is_no_refusal(name: &str, line: fn(&str) -> String) {
    let case = client_case(name);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let named = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(named.code, Some(0), "{named:?}");
    let said = line(&key_of(&events(&case)));
    let second = ssh_steps(&case, "second", &["ok", "ok"]);

    let command = [r".\tool.exe", "mark", RAN, "err", &said, "exit", "75"];
    let (pid, outcome) = finish_with_pid(client_gate(&case, "second.log", &second, &command));

    assert_eq!(outcome.code, Some(75), "the command's own status: {outcome:?}");
    assert!(outcome.stderr.contains(&said), "the command's line is passed on: {outcome:?}");
    assert_eq!(fallbacks(&own(&case, pid)), Vec::<&str>::new(), "no fallback: {outcome:?}");
    assert_eq!(runs(&case), (0, 1), "the command ran once, on the builder: {outcome:?}");
    assert_eq!(calls(&second), ["preflight", "run"]);
}

#[test]
fn a_command_that_prints_the_busy_line_and_exits_75_is_not_a_refusal() {
    a_command_s_own_refusal_line_is_no_refusal("client-own-busy-line", |key| format!("gate: remote work folder {key} is busy"));
}

#[test]
fn a_command_that_prints_the_tools_line_and_exits_75_is_not_a_refusal() {
    a_command_s_own_refusal_line_is_no_refusal("client-own-tools-line", |key| {
        format!("gate: remote work folder {key} could not restore its dotnet tools")
    });
}

/// The builder's `dotnet` fails the restore of the snapshot's tool manifest, so the box refuses the run `tools`.
#[test]
fn a_tools_refusal_of_the_box_falls_back_and_runs_the_command_here_once() {
    let case = client_case("client-tools-refused");
    write(&case, ".config/dotnet-tools.json", "{\n  \"version\": 1\n}\n");
    let stub = case.folder().join("ssh").join("stub");
    fs::create_dir_all(&stub).expect("the stub's folder can be made");
    fs::write(stub.join("dotnet.cmd"), "@exit /b 1\r\n").expect("the stub dotnet can be written");
    let own_path = std::env::var_os("PATH").expect("PATH is set");
    let path = std::env::join_paths(std::iter::once(stub).chain(std::env::split_paths(&own_path))).expect("the folders join into a PATH");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", RAN, "out", "ran"]);
    gate.env("PATH", path);

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    assert_eq!(fallbacks(&own(&case, pid)), ["tools"], "{outcome:?}");
    assert_eq!(runs(&case), (1, 0), "the command ran once, here: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
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
    let line = format!("gate: not routed: builder runs gate 0.0.0-other, this PC {}", build_version(&case));
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder version; running here"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    let events = events(&case);
    assert_eq!(fallbacks(&events), ["version"]);
    assert_eq!(builder_ref(&case, &format!("refs/gate/{}", key_of(&events))), "", "nothing was pushed");
}

#[test]
fn a_builder_gate_of_the_same_version_without_this_build_falls_back_version() {
    let case = client_case("client-version-no-build");
    let step = format!("version {}", env!("CARGO_PKG_VERSION"));
    let steps = ssh_steps(&case, "ssh", &[step.as_str()]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    // The same package version, another build: the whole text is compared, so this is not the builder's own gate.
    let line = format!(
        "gate: not routed: builder runs gate {}, this PC {}",
        env!("CARGO_PKG_VERSION"),
        build_version(&case)
    );
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder version; running here"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    assert_eq!(fallbacks(&events(&case)), ["version"]);
}

/// The line a run the builder lacks the toolchain for falls back under.
const PARITY_LINE: &str = "gate: remote builder parity; running here";
/// This PC's SDK list a parity case gives the client: a major no machine has.
const LOCAL_SDKS: [&str; 2] = ["--test-local-sdks", "999.0.100"];

/// A folder holding a `dotnet.cmd` that lists `versions` for `--list-sdks`, one to a line as dotnet does, and this
/// PC's own `PATH` with it first and every folder holding a real `dotnet` dropped, so the builder's `dotnet` is this
/// stub and no other: what a case that fixes the SDKs the builder serves passes to its run. The folder is inside the
/// worktree, where this PC's gate never looks for its own dotnet, so it serves a case whose tree has a pin.
fn box_path_with_dotnet(case: &Case, versions: &[&str]) -> OsString {
    path_with_stub(&case.folder().join("ssh").join("stub-dotnet"), &listing(versions))
}

/// A stub dotnet beside the case folder, outside its worktree, so this PC's gate finds it as the builder's does, and
/// the `PATH` that names it; the folder is removed when it drops.
struct PcDotnet {
    path: OsString,
    folder: PathBuf,
}

impl Drop for PcDotnet {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.folder) {
            // A folder left under the target directory harms no later case, and a panic in drop would abort the run.
            let _ = writeln!(io::stderr(), "could not remove the stub folder {}: {error}", self.folder.display());
        }
    }
}

/// A [`PcDotnet`] whose `dotnet.cmd` runs `body` after `@echo off`.
fn pc_dotnet(case: &Case, body: &str) -> PcDotnet {
    let mut folder = case.folder().into_os_string();
    folder.push("-dotnet");
    let folder = PathBuf::from(folder);
    let path = path_with_stub(&folder, body);
    PcDotnet { path, folder }
}

/// A [`PcDotnet`] that runs `pc`, lines of a batch file, for this PC's gate, behind a `dotnet.cmd` that lists
/// `versions` for the builder's `gate box`: that one is in a folder inside the worktree that `PATH` names first, which
/// this PC's gate skips and the builder's, which skips no folder, takes. Both read their list in the system folder, so
/// the folder each stub sits in is what lets this PC's SDK list and the builder's differ.
fn box_path_with_pc_dotnet(case: &Case, versions: &[&str], pc: &str) -> PcDotnet {
    let mut dotnet = pc_dotnet(case, pc);
    let builder = case.folder().join("ssh").join("builder-dotnet");
    fs::create_dir_all(&builder).expect("the builder's stub folder can be made");
    fs::write(builder.join("dotnet.cmd"), format!("@echo off\r\n{}", listing(versions))).expect("the builder's stub dotnet can be written");
    let folders = std::iter::once(builder).chain(std::env::split_paths(&dotnet.path));
    dotnet.path = std::env::join_paths(folders).expect("the folders join into a PATH");
    dotnet
}

/// The system folder as `%CD%` says it, with no trailing backslash: where this PC's gate runs its dotnet.
fn system_folder() -> String {
    let folder = gate_win32::system_directory().expect("the system folder can be read");
    folder.to_string_lossy().trim_end_matches('\\').to_owned()
}

/// The batch lines that list `versions` as `dotnet --list-sdks` does, one to a line.
fn listing(versions: &[&str]) -> String {
    let mut listed = String::new();
    for version in versions {
        write!(listed, "echo {version} [C:\\stub]\r\n").expect("a String writes");
    }
    listed
}

/// The folder `stub`, made to hold a `dotnet.cmd` that runs `body` after `@echo off`, and this PC's own `PATH` with it
/// first and every folder holding a real `dotnet` dropped, so every `dotnet` the run finds is this stub.
fn path_with_stub(stub: &Path, body: &str) -> OsString {
    fs::create_dir_all(stub).expect("the stub's folder can be made");
    fs::write(stub.join("dotnet.cmd"), format!("@echo off\r\n{body}")).expect("the stub dotnet can be written");
    let own = std::env::var_os("PATH").expect("PATH is set");
    let without_dotnet = std::env::split_paths(&own).filter(|folder| !holds_dotnet(folder));
    std::env::join_paths(std::iter::once(stub.to_path_buf()).chain(without_dotnet)).expect("the folders join into a PATH")
}

/// Whether `folder` holds a `dotnet` under any of the extensions a `PATHEXT` search tries, so [`box_path_with_dotnet`]
/// drops it.
fn holds_dotnet(folder: &Path) -> bool {
    ["exe", "cmd", "bat", "com"]
        .iter()
        .any(|extension| folder.join(format!("dotnet.{extension}")).is_file())
}

/// The one SDK the builder of every parity refusal lists, which its mismatch line ends in.
const BUILDER_SDK: &str = "8.0.404";

/// A routed run of a case whose tree the builder lacks the toolchain for, the client given `extra` and the `PATH`
/// `box_path`, else one whose builder's dotnet lists [`BUILDER_SDK`] alone: it falls back `parity` after the preflight
/// alone, so nothing is pushed, says the whole mismatch line, `need` and then [`BUILDER_SDK`], on the console and in the
/// log, so a builder that found no dotnet cannot pass for one that did, and runs the command here once.
fn a_parity_refusal(case: &Case, extra: &[&str], need: &str, box_path: Option<&OsString>) -> Outcome {
    let steps = ssh_steps(case, "ssh", &["ok", "ok"]);
    let tuned = Tuned { extra, ..Tuned::default() };
    let command = [r".\tool.exe", "mark", RAN, "out", "ran"];

    let mut gate = client_gate_tuned(case, "out.log", &steps, &tuned, &command);
    gate.env("PATH", box_path.cloned().unwrap_or_else(|| box_path_with_dotnet(case, &[BUILDER_SDK])));
    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    let events = own(case, pid);
    assert_eq!(fallbacks(&events), ["parity"], "{outcome:?}");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let mismatch = format!("gate: remote toolchain mismatch: {repo} needs {need}; builder has {BUILDER_SDK}");
    assert!(outcome.stderr.lines().any(|said| said == mismatch), "{mismatch}: {outcome:?}");
    assert!(outcome.stderr.contains(PARITY_LINE), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.lines().any(|said| said == mismatch), "{mismatch}: {log}");
    assert_eq!(calls(&steps), ["preflight"], "the preflight alone reached the builder");
    assert_eq!(builder_ref(case, &format!("refs/gate/{}", key_of(&events))), "", "nothing was pushed");
    assert_eq!(runs(case), (1, 0), "the command ran once, here: {outcome:?}");
    outcome
}

/// A root `global.json` named in another letter case is the file dotnet reads, as Windows opens it: its pin is read,
/// so the mismatch line names the pin and not the major.
#[test]
fn a_global_json_named_in_another_case_is_read() {
    let case = client_case("client-parity-global-case");
    write(&case, "Global.json", r#"{"sdk": {"version": "999.0.0"}}"#);
    write(&case, "src/App/App.csproj", "<Project />\n");

    let outcome = a_parity_refusal(&case, &[], ".NET SDK 999.0.0 (rollForward patch)", None);

    assert!(!outcome.stderr.contains("gate: global.json has no readable sdk pin"), "{outcome:?}");
}

#[test]
fn a_pin_no_builder_sdk_serves_falls_back_parity_before_the_push() {
    let case = client_case("client-parity-pin");
    write(&case, "global.json", r#"{"sdk": {"version": "999.0.0"}}"#);
    let box_path = box_path_with_dotnet(&case, &["8.0.404"]);

    let outcome = a_parity_refusal(&case, &[], ".NET SDK 999.0.0 (rollForward patch)", Some(&box_path));

    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let line = format!("gate: remote toolchain mismatch: {repo} needs .NET SDK 999.0.0 (rollForward patch); builder has 8.0.404");
    assert!(outcome.stderr.lines().any(|said| said == line), "{line}: {outcome:?}");
    assert!(!outcome.stderr.contains("gate: global.json has no readable sdk pin"), "{outcome:?}");
}

/// The builder serves the pin the worktree names: its own stub `dotnet` lists it, so the parity check passes and the
/// run goes remote. A second run of the worktree whose builder lists another SDK is refused, which only a check that
/// read the builder's SDK line shows.
#[test]
fn a_pin_the_builder_serves_goes_remote() {
    let case = client_case("client-parity-served");
    write(&case, "global.json", r#"{"sdk": {"version": "999.0.100"}}"#);
    write(&case, "src/App/App.csproj", "<Project />\n");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", RAN, "out", "ran there"]);
    gate.env("PATH", box_path_with_dotnet(&case, &["999.0.100"]));

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: remote toolchain mismatch: "), "{outcome:?}");
    assert!(!outcome.stderr.contains(PARITY_LINE), "{outcome:?}");
    let events = own(&case, pid);
    assert_eq!(fallbacks(&events), Vec::<&str>::new(), "{events:#?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert!(
        is_sha(&builder_ref(&case, &format!("refs/gate/{}", key_of(&events)))),
        "the snapshot was pushed"
    );
    assert_eq!(runs(&case), (0, 1), "the command ran once, on the builder: {outcome:?}");

    let other = ssh_steps(&case, "other", &["ok", "ok"]);
    let mut other_gate = client_gate(&case, "other.log", &other, &[r".\tool.exe", "out", "ran here"]);
    other_gate.env("PATH", box_path_with_dotnet(&case, &["8.0.404"]));
    let (other_pid, refused) = finish_with_pid(other_gate);

    assert_eq!(refused.code, Some(0), "{refused:?}");
    assert_eq!(fallbacks(&own(&case, other_pid)), ["parity"], "{refused:?}");
    assert!(refused.stderr.contains(PARITY_LINE), "{refused:?}");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let line = format!("gate: remote toolchain mismatch: {repo} needs .NET SDK 999.0.100 (rollForward patch); builder has 8.0.404");
    assert!(refused.stderr.lines().any(|said| said == line), "{line}: {refused:?}");
    assert_eq!(calls(&other), ["preflight"], "the preflight alone reached the builder");
}

#[test]
fn a_dotnet_tree_without_a_pin_needs_the_pcs_newest_major() {
    let case = client_case("client-parity-major");
    write(&case, "src/App/App.csproj", "<Project />\n");

    let outcome = a_parity_refusal(&case, &LOCAL_SDKS, "a .NET 999 SDK", None);

    assert!(
        !outcome.stderr.contains("gate: global.json has no readable sdk pin"),
        "no global.json: {outcome:?}"
    );
}

/// A `dotnet.cmd` in the repo root, the gate's working folder, is never run for this PC's SDK list: dotnet is found
/// along `PATH` alone, so the mismatch names the major the `PATH` stub lists for this PC, and the decoy leaves no mark.
#[test]
fn a_dotnet_in_the_repo_root_is_not_run_for_the_sdk_list() {
    let case = client_case("client-parity-root-dotnet");
    write(&case, "src/App/App.csproj", "<Project />\n");
    let marker = case.folder().join("ssh").join("decoy-ran");
    write(
        &case,
        "dotnet.cmd",
        &format!("@echo off\r\n>\"{}\" echo ran\r\necho 997.0.100 [C:\\decoy]\r\n", marker.display()),
    );
    let dotnet = box_path_with_pc_dotnet(&case, &["8.0.404"], "echo 999.0.100 [C:\\stub]\r\n");

    a_parity_refusal(&case, &[], "a .NET 999 SDK", Some(&dotnet.path));

    assert!(!marker.exists(), "the repo root's dotnet.cmd ran");
}

/// This PC's `dotnet --list-sdks` that never answers is ended with its job at the bound `--test-dotnet-limit-ms` sets:
/// the list counts as unreadable, the run checks no SDK major and goes remote, and nothing the stub started is left
/// running. Only the bound ends the call, so no load changes what the run says.
#[test]
fn a_dotnet_that_does_not_answer_counts_as_an_unreadable_sdk_list() {
    let case = client_case("client-parity-dotnet-hangs");
    write(&case, "src/App/App.csproj", "<Project />\n");
    let go = Go(case.folder().join("ssh").join("dotnet-go"));
    let held = case.folder().join("ssh").join("dotnet-held");
    let hang = format!("\"{FAKE_TOOL}\" wait \"{}\" >\"{}\"\r\n", go.path().display(), held.display());
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-dotnet-limit-ms", "2000"],
        ..Tuned::default()
    };
    let mut gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "mark", RAN, "out", "ran there"]);
    let dotnet = box_path_with_pc_dotnet(&case, &["8.0.404"], &hang);
    gate.env("PATH", &dotnet.path);

    let (pid, outcome) = finish_with_pid(gate);

    let unreadable = "gate: cannot read this PC's SDK list: dotnet --list-sdks did not answer in 2 s; checking no SDK major";
    assert!(outcome.stderr.lines().any(|said| said == unreadable), "{unreadable}: {outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: remote toolchain mismatch: "), "{outcome:?}");
    assert_eq!(fallbacks(&own(&case, pid)), Vec::<&str>::new(), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(runs(&case), (0, 1), "the command ran once, on the builder: {outcome:?}");
    // A stub that started holds its output file open until its job's kill ends it; one the kill came first never made it.
    if held.exists() {
        wait_until_closed(&held);
    }
}

/// A `dotnet.cmd` in a folder inside the worktree that `PATH` names first, as an `npm run` or an activated venv puts
/// one, is skipped for this PC's SDK list: the stub later on `PATH` is read, so this PC lists 999. The builder's
/// `gate box`, which skips no folder, takes the one inside, which lists `8.0.404` for it and notes each run: it ran
/// once, for the builder alone.
#[test]
fn a_dotnet_on_path_inside_the_worktree_is_not_run_for_the_sdk_list() {
    let case = client_case("client-parity-path-in-worktree");
    write(&case, "src/App/App.csproj", "<Project />\n");
    let inside = case.folder().join("ssh").join("repo-bin");
    fs::create_dir_all(&inside).expect("the folder inside the worktree can be made");
    let marker = case.folder().join("ssh").join("inside-ran");
    let decoy = format!("@echo off\r\n>>\"{}\" echo ran\r\n{}", marker.display(), listing(&["8.0.404"]));
    fs::write(inside.join("dotnet.cmd"), decoy).expect("the dotnet inside the worktree can be written");
    let later = pc_dotnet(&case, "echo 999.0.100 [C:\\stub]\r\n");
    let path = std::env::join_paths(std::iter::once(inside).chain(std::env::split_paths(&later.path))).expect("the folders join into a PATH");

    a_parity_refusal(&case, &[], "a .NET 999 SDK", Some(&path));

    let ran = fs::read_to_string(&marker).expect("the builder ran the dotnet.cmd inside the worktree");
    assert_eq!(
        ran.lines().count(),
        1,
        "the dotnet.cmd inside the worktree ran for the builder alone: {ran:?}"
    );
}

/// A junction outside the worktree that leads into a folder inside it, first on `PATH`, is skipped for this PC's SDK
/// list as that folder is, each `PATH` folder compared by its canonical path: the stub later on `PATH` is read, so this
/// PC lists 999. The builder's `gate box`, which skips no folder, takes the `dotnet.cmd` behind the junction, which
/// notes each run: it ran once, for the builder alone.
#[test]
fn a_junction_on_path_into_the_worktree_is_not_run_for_the_sdk_list() {
    let case = client_case("client-parity-path-junction");
    write(&case, "src/App/App.csproj", "<Project />\n");
    let inside = case.folder().join("ssh").join("repo-bin");
    fs::create_dir_all(&inside).expect("the folder inside the worktree can be made");
    let marker = case.folder().join("ssh").join("inside-ran");
    let decoy = format!("@echo off\r\n>>\"{}\" echo ran\r\n{}", marker.display(), listing(&[BUILDER_SDK]));
    fs::write(inside.join("dotnet.cmd"), decoy).expect("the dotnet inside the worktree can be written");
    let junction = Junction::to(&case, &inside);
    let later = pc_dotnet(&case, "echo 999.0.100 [C:\\stub]\r\n");
    let folders = std::iter::once(junction.0.clone()).chain(std::env::split_paths(&later.path));
    let path = std::env::join_paths(folders).expect("the folders join into a PATH");

    a_parity_refusal(&case, &[], "a .NET 999 SDK", Some(&path));

    let ran = fs::read_to_string(&marker).expect("the builder ran the dotnet.cmd behind the junction");
    assert_eq!(
        ran.lines().count(),
        1,
        "the dotnet.cmd behind the junction ran for the builder alone: {ran:?}"
    );
}

/// A junction beside the case folder, outside its worktree, made with `mklink /J`; removed, and not what it leads to,
/// when it drops.
struct Junction(PathBuf);

impl Junction {
    /// A junction that leads to `target`.
    fn to(case: &Case, target: &Path) -> Junction {
        let mut link = case.folder().into_os_string();
        link.push("-junction");
        let link = PathBuf::from(link);
        // A junction an earlier run of the case left would make `mklink` refuse.
        drop(fs::remove_dir(&link));
        let made = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .expect("cmd starts");
        assert!(made.success(), "the junction is made");
        Junction(link)
    }
}

impl Drop for Junction {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir(&self.0) {
            // A junction left beside the case harms no later case, which removes it first, and a panic in drop aborts.
            let _ = writeln!(io::stderr(), "could not remove the junction {}: {error}", self.0.display());
        }
    }
}

/// A `PATH` with an unmatched quote reads every folder after it as one, so the `dotnet` there would not be found and
/// this PC's SDK list would read as empty: the list counts as unreadable instead, the run says so and checks no SDK
/// major.
#[test]
fn a_path_with_an_unmatched_quote_is_an_unreadable_sdk_list() {
    let case = client_case("client-parity-path-quote");
    write(&case, "src/App/App.csproj", "<Project />\n");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", RAN, "out", "ran there"]);
    let dotnet = pc_dotnet(&case, "echo 999.0.100 [C:\\stub]\r\n");
    let mut folders = std::env::split_paths(&dotnet.path);
    let stub = folders.next().expect("the stub's folder is first");
    let mut path = std::env::join_paths(folders).expect("the folders join into a PATH");
    path.push(format!(";\"C:\\gate-no-such-folder;{}", stub.display()));
    gate.env("PATH", &path);

    let outcome = finish(gate);

    let unreadable = "gate: cannot read this PC's SDK list: PATH holds an unmatched quote, so the folders after it read as one; \
                      checking no SDK major";
    assert!(outcome.stderr.lines().any(|said| said == unreadable), "{unreadable}: {outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: remote toolchain mismatch: "), "{outcome:?}");
}

/// The log cap a log cap case lowers the client's to, in bytes, and the line its log then gets, once a call's output
/// passes it.
const LOG_CAP: &str = "65536";
const LOG_CUT: &str = "gate: the builder's output passed 64 KiB; the rest is not logged";
/// The output of the command a log cap case runs on the builder, after the flood the stand-in writes.
const RAN_THERE: &str = "ran there";

/// A routed run of a [`client_case`] whose ssh calls take `steps` and whose log cap is [`LOG_CAP`]: what it said, and its
/// log. The run passes, and its command ran once, on the builder.
fn log_capped(name: &str, steps: &[&str]) -> (Outcome, String) {
    log_capped_at(name, steps, LOG_CAP)
}

/// [`log_capped`] with the log cap `cap`, in bytes.
fn log_capped_at(name: &str, steps: &[&str], cap: &str) -> (Outcome, String) {
    let case = client_case(name);
    let steps = ssh_steps(&case, "ssh", steps);
    let tuned = Tuned {
        extra: &["--test-remote-log-cap-bytes", cap],
        ..Tuned::default()
    };
    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "mark", RAN, "out", RAN_THERE],
    ));
    assert_eq!(outcome.code, Some(0), "the run keeps its status: {outcome:?}");
    assert_eq!(runs(&case), (0, 1), "the command ran once, on the builder: {outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    (outcome, log)
}

/// How many times `log` says a call's output passed the cap.
fn cuts(log: &str) -> usize {
    log.lines().filter(|line| *line == LOG_CUT).count()
}

/// The bytes of the stand-in's `lines` flood `log` holds, line ends counted.
fn flood_logged(log: &str) -> usize {
    log.lines().filter(|line| line.starts_with("line ")).map(|line| line.len() + 1).sum()
}

/// The index of the line `want` in `log`, when it holds it.
fn line_at(log: &str, want: &str) -> Option<usize> {
    log.lines().position(|line| line == want)
}

/// A preflight that floods the log is logged up to the cap and then says so once, and none of it reaches the console:
/// the run goes on, and the run call, counted afresh, is logged whole after it.
#[test]
fn a_flooded_preflight_writes_at_most_the_cap_to_the_log() {
    let (outcome, log) = log_capped("client-log-cap-preflight", &["lines 200 1000", "ok"]);

    assert_eq!(cuts(&log), 1, "{log}");
    assert!(flood_logged(&log) <= 65_536, "{} bytes of the flood were logged", flood_logged(&log));
    assert!(!log.contains("line 199 "), "the flood's end is not logged");
    assert!(
        !outcome.stdout.contains("line 0 ") && !outcome.stderr.contains("line 1 "),
        "the preflight is not echoed: {outcome:?}"
    );
    let (cut, ran) = (line_at(&log, LOG_CUT), line_at(&log, RAN_THERE));
    assert!(cut.is_some() && cut < ran, "the run call is logged after the preflight's cut: {log}");
}

/// A run call that floods the log is logged up to the cap and then says so once, while the console keeps echoing it to
/// its end, so the end of the run is still seen live; nothing past the cap reaches the log.
#[test]
fn a_flooded_run_call_is_echoed_whole_and_logged_to_the_cap() {
    let (outcome, log) = log_capped("client-log-cap-run", &["ok", "lines 200 1000"]);

    let said = format!("{}{}", outcome.stdout, outcome.stderr);
    assert!(
        said.contains("line 198 ") && said.contains("line 199 "),
        "the console shows the whole flood"
    );
    assert!(
        outcome.stdout.lines().any(|line| line == RAN_THERE),
        "the end of the run is seen live: {outcome:?}"
    );
    assert_eq!(cuts(&log), 1, "{log}");
    assert!(flood_logged(&log) <= 65_536, "{} bytes of the flood were logged", flood_logged(&log));
    assert!(!log.contains("line 199 "), "the flood's end is not logged");
    assert_eq!(line_at(&log, RAN_THERE), None, "the command's output past the cap is not logged");
}

/// Each stream of the run call carries 50 of the flood's lines, about 50 KB, under the 64 KiB cap; together they pass
/// it, so the cap counts a call's two streams together.
#[test]
fn the_log_cap_counts_a_calls_two_streams_together() {
    let (_, log) = log_capped("client-log-cap-streams", &["ok", "lines 100 1000"]);

    assert_eq!(cuts(&log), 1, "{log}");
    assert!(flood_logged(&log) <= 65_536, "{} bytes of the flood were logged", flood_logged(&log));
    assert_eq!(line_at(&log, RAN_THERE), None, "the command's output past the cap is not logged");
}

/// A run call that floods one line with no line end, 4 MiB passed on in pieces past the 64 KiB line cap, is logged in
/// pieces up to its 1 MiB cap, which a cut line of its own then says once, while the console shows the whole flood.
#[test]
fn a_run_call_flooding_one_endless_line_is_logged_to_the_cap() {
    const FLOOD: usize = 4 << 20;
    let (outcome, log) = log_capped_at("client-log-cap-one-line", &["ok", &format!("flood-ok {FLOOD}")], "1048576");

    let cut = "gate: the builder's output passed 1 MiB; the rest is not logged";
    assert_eq!(log.lines().filter(|line| *line == cut).count(), 1, "one cut line of its own");
    let flood: usize = log.lines().filter(|line| line.starts_with("xxxx")).map(str::len).sum();
    assert!(flood > 0 && flood <= 1 << 20, "{flood} bytes of the flood were logged");
    assert!(outcome.stdout.matches('x').count() >= FLOOD, "the console shows the whole flood");
    assert!(outcome.stdout.contains(RAN_THERE), "the end of the run is seen live");
}

/// A run call whose output stays under the cap is logged whole, with no cut line.
#[test]
fn a_call_under_the_log_cap_is_logged_whole() {
    let (_, log) = log_capped("client-log-cap-under", &["ok", "lines 20 1000"]);

    assert_eq!(cuts(&log), 0, "{log}");
    for number in 0..20 {
        let start = format!("line {number} ");
        assert!(log.lines().any(|line| line.starts_with(&start)), "{start}is logged");
    }
    assert!(line_at(&log, RAN_THERE).is_some(), "the command's output is logged: {log}");
}

/// A Ctrl+C raised just before this PC's SDK list is read ends the run as an interrupt: no `cannot read` line, nothing
/// pushed, and no call after the preflight reaches the builder.
#[test]
fn a_ctrl_c_before_the_sdk_list_read_ends_the_run_as_an_interrupt() {
    let case = client_case("client-parity-dotnet-ctrl-c");
    write(&case, "src/App/App.csproj", "<Project />\n");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "sdks"],
        ..Tuned::default()
    };
    let mut gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "mark", RAN, "out", "ran"]);
    let dotnet = box_path_with_pc_dotnet(&case, &["8.0.404"], "echo 999.0.100 [C:\\stub]\r\n");
    gate.env("PATH", &dotnet.path);

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: cannot read this PC's SDK list"), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: remote toolchain mismatch: "), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "nothing after the preflight reached the builder");
    let events = own(&case, pid);
    assert_eq!(builder_ref(&case, &format!("refs/gate/{}", key_of(&events))), "", "nothing was pushed");
    assert_eq!(runs(&case), (0, 0), "the command ran nowhere: {outcome:?}");
}

/// `dotnet --list-sdks` runs in the system folder, not the repo root nor the folder the builder's `gate box` starts in:
/// the `PATH` stub writes the folder it runs in, first for the builder's call, made by the preflight, then for this PC's.
#[test]
fn the_pcs_dotnet_runs_in_the_system_folder() {
    let case = client_case("client-parity-dotnet-folder");
    write(&case, "src/App/App.csproj", "<Project />\n");
    let folders = case.folder().join("ssh").join("dotnet-folders");
    let body = format!(">>\"{}\" echo %CD%\r\n{}", folders.display(), listing(&["999.0.100"]));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", RAN, "out", "ran there"]);
    let dotnet = pc_dotnet(&case, &body);
    gate.env("PATH", &dotnet.path);

    let (_, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let ran_in = fs::read_to_string(&folders).expect("the stub wrote the folders it ran in");
    let ran_in: Vec<&str> = ran_in.lines().map(str::trim_end).collect();
    let want = [system_folder(), system_folder()];
    assert!(
        ran_in.len() == want.len() && ran_in.iter().zip(&want).all(|(ran, want)| ran.eq_ignore_ascii_case(want)),
        "{want:?}, the builder's then this PC's: {ran_in:?}"
    );
}

#[test]
fn a_global_json_without_a_readable_pin_checks_the_major_only() {
    let case = client_case("client-parity-no-pin");
    write(&case, "global.json", r#"{"test": {"runner": "Microsoft.Testing.Platform"}}"#);

    let outcome = a_parity_refusal(&case, &LOCAL_SDKS, "a .NET 999 SDK", None);

    let no_pin = "gate: global.json has no readable sdk pin; checking the SDK major only";
    let (said, mismatch) = (outcome.stderr.find(no_pin), outcome.stderr.find("gate: remote toolchain mismatch: "));
    assert!(said.is_some() && said < mismatch, "the no-pin line, then the mismatch: {outcome:?}");
}

/// A `global.json` that is not UTF-8 is read lossily, and the byte that could not be read is no readable pin: the
/// no-pin line, then the major rule.
#[test]
fn a_global_json_that_is_not_utf8_checks_the_major_only() {
    let case = client_case("client-parity-not-utf8");
    write(&case, "src/App/App.csproj", "<Project />\n");
    let mut global_json = b"{\"sdk\":{\"version\":\"9".to_vec();
    global_json.push(0xff);
    global_json.extend_from_slice(b"\"}}");
    fs::write(case.folder().join("global.json"), global_json).expect("the global.json can be written");

    let outcome = a_parity_refusal(&case, &LOCAL_SDKS, "a .NET 999 SDK", None);

    let no_pin = "gate: global.json has no readable sdk pin; checking the SDK major only";
    let (said, mismatch) = (outcome.stderr.find(no_pin), outcome.stderr.find("gate: remote toolchain mismatch: "));
    assert!(said.is_some() && said < mismatch, "the no-pin line, then the mismatch: {outcome:?}");
}

#[test]
fn a_tree_that_is_not_dotnet_skips_the_parity_check() {
    let case = client_case("client-parity-not-dotnet");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &LOCAL_SDKS,
        ..Tuned::default()
    };

    let (pid, outcome) = finish_with_pid(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "mark", RAN, "out", "ran"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: remote toolchain mismatch: "), "{outcome:?}");
    assert_eq!(fallbacks(&own(&case, pid)), Vec::<&str>::new(), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(runs(&case), (0, 1), "the command ran once, on the builder: {outcome:?}");
}

#[test]
fn a_builder_version_holding_escapes_is_refused_in_an_escaped_line() {
    let case = client_case("client-version-escaped");
    let steps = ssh_steps(&case, "ssh", &["version 9.9.9\u{1b}[2J\u{202e}"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = format!(
        r"gate: not routed: builder runs gate 9.9.9\u{{1b}}[2J\u{{202e}}, this PC {}",
        build_version(&case)
    );
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(!outcome.stderr.contains(['\u{1b}', '\u{202e}']), "no raw escape: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), ["version"]);
}

/// A preflight that fails as ssh does, with status 255 and a line that is no failure before the banner, so it is not
/// made again; the line holds ESC, BEL and a carriage return.
#[test]
fn a_preflight_s_failure_line_is_shown_escaped_as_its_last_update() {
    let case = client_case("client-preflight-escaped");
    let steps = ssh_steps(
        &case,
        "ssh",
        &["say-exit 255 \u{1b}[31mearlier-update\rlater \u{1b}]8;;http://x\u{7}link"],
    );

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains(r"gate: remote builder unreachable: later \u{1b}]8;;http://x\u{7}link"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains('\u{1b}'), "no raw ESC: {outcome:?}");
    assert!(
        !outcome.stderr.contains("earlier-update"),
        "only the line's last update is shown: {outcome:?}"
    );
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

#[test]
fn a_file_the_command_changes_on_the_box_is_synced_back() {
    let case = client_case("client-synced-back");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", "made-on-box.txt"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(case.folder().join("made-on-box.txt")).expect("the box's file is here"),
        "run\n",
        "the box's file, with the box's content: {outcome:?}"
    );
    assert!(outcome.stderr.contains("gate: synced back 1 file(s): made-on-box.txt"), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.contains("gate: synced back 1 file(s): made-on-box.txt"), "{log}");
    assert!(!log.contains("refs/gate-back/"), "the sync-back's git output stays out of the log: {log}");
    assert_eq!(patch_files(&case), Vec::<String>::new(), "the patch is removed");
    let key = key_of(&events(&case));
    assert!(
        is_sha(&builder_ref(&case, &format!("refs/gate-back/{key}"))),
        "the box wrote the back commit"
    );
    assert_eq!(calls(&steps), ["preflight", "run"]);
}

/// The bytes of the file a fetch-cap case's command writes on the box, which no compression shrinks: 1 MiB.
const NOISE_BYTES: &str = "1048576";

/// The `incoming-gate-*` quarantine folders a sync-back left in the case's own `objects` folder.
fn quarantines(case: &Case) -> Vec<String> {
    fs::read_dir(case.folder().join(".git").join("objects"))
        .expect("the objects folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("incoming-gate-"))
        .collect()
}

/// The box's command writes 1 MiB that no compression shrinks, and the fetch's job may write 256 KiB: the fetch is
/// stopped, nothing is fetched into the worktree's refs or applied, and the passing run fails with the line saying the
/// changes stay on the builder.
#[test]
fn a_back_commit_over_the_fetch_cap_is_not_applied_and_fails_a_passing_run() {
    let case = client_case("client-fetch-over-cap");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let before = porcelain(&case);
    let tuned = Tuned {
        extra: &["--test-remote-fetch-cap-bytes", "262144"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "noise", "noise.bin", NOISE_BYTES],
    ));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let key = key_of(&events(&case));
    let line =
        format!("gate: remote sync-back stopped: the fetch wrote more than 256.0 KiB; the run's changes stay on the builder (refs/gate-back/{key})");
    assert!(outcome.stderr.lines().any(|said| said == line), "{outcome:?}");
    assert!(!case.folder().join("noise.bin").exists(), "the box's file is not here: {outcome:?}");
    assert_eq!(porcelain(&case), before, "the worktree has no new file: {outcome:?}");
    let kept = git_out(&case, &[], &["for-each-ref", "--format=%(refname)", "refs/gate-remote/"]);
    assert!(
        !kept.lines().any(|name| name.ends_with("-back")),
        "nothing was kept in the worktree: {kept}"
    );
    assert_eq!(
        quarantines(&case),
        Vec::<String>::new(),
        "no quarantine, and so no part of the pack, is left"
    );
    assert!(
        is_sha(&builder_ref(&case, &format!("refs/gate-back/{key}"))),
        "the changes stay on the builder"
    );
    assert_eq!(calls(&steps), ["preflight", "run"]);
}

/// The same 1 MiB under a 16 MiB cap syncs back: what a fetch writes besides the pack does not trip the cap.
#[test]
fn a_back_commit_under_the_fetch_cap_syncs_back() {
    let case = client_case("client-fetch-under-cap");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-fetch-cap-bytes", "16777216"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "noise", "noise.bin", NOISE_BYTES],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: synced back 1 file(s): noise.bin"), "{outcome:?}");
    let synced = fs::metadata(case.folder().join("noise.bin")).expect("the box's file is here").len();
    assert_eq!(synced.to_string(), NOISE_BYTES, "the whole file came back");
    assert_eq!(calls(&steps), ["preflight", "run"]);
}

/// The bytes of the file an apply-cap case's command writes on the box, all one byte, so its pack is a few KiB: 8 MiB.
const FILL_BYTES: &str = "8388608";

/// The box's command writes 8 MiB that pack to a few KiB, well under the fetch cap, and the sync-back may write 1 MiB
/// into the worktree: the back commit is kept at the key's -back ref, no patch is made and nothing is written, and the
/// passing run fails with the line saying how much the changes would write.
#[test]
fn a_back_commit_that_would_write_more_than_the_apply_cap_is_kept_and_fails_a_passing_run() {
    let case = client_case("client-apply-over-cap");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let before = porcelain(&case);
    let tuned = Tuned {
        extra: &["--test-remote-apply-cap-bytes", "1048576"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "fill", "big.bin", FILL_BYTES],
    ));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let events = events(&case);
    let key = key_of(&events);
    let back = builder_ref(&case, &format!("refs/gate-back/{key}"));
    assert!(is_sha(&back), "the box wrote the back commit: {back:?}");
    let line = format!(
        "gate: remote changes not applied: they would write 8.0 MiB into the worktree, more than 1.0 MiB; see git diff {} {}",
        pushed_snapshot(&case, "out.log"),
        &back[..8]
    );
    assert!(outcome.stderr.lines().any(|said| said == line), "{outcome:?}");
    assert_eq!(
        git_out(&case, &[], &["rev-parse", &format!("refs/gate-remote/{key}-back")]),
        back,
        "the changes are kept at the key's -back ref"
    );
    assert!(!case.folder().join("big.bin").exists(), "the box's file is not here: {outcome:?}");
    assert_eq!(porcelain(&case), before, "the worktree has no new file: {outcome:?}");
    assert_eq!(patch_files(&case), Vec::<String>::new(), "no patch was made");
    assert_eq!(fallbacks(&events), Vec::<&str>::new(), "the refusal is no fallback");
    assert_eq!(calls(&steps), ["preflight", "run"]);
}

/// 1 KiB of the same byte under the same 1 MiB cap syncs back.
#[test]
fn a_back_commit_under_the_apply_cap_syncs_back() {
    let case = client_case("client-apply-under-cap");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-apply-cap-bytes", "1048576"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "fill", "small.bin", "1024"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: synced back 1 file(s): small.bin"), "{outcome:?}");
    let synced = fs::metadata(case.folder().join("small.bin")).expect("the box's file is here").len();
    assert_eq!(synced, 1024, "the whole file came back");
    assert_eq!(calls(&steps), ["preflight", "run"]);
}

#[test]
fn a_command_that_changes_nothing_syncs_nothing_back() {
    let case = client_case("client-nothing-back");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: synced back "), "{outcome:?}");
    let key = key_of(&events(&case));
    assert_eq!(builder_ref(&case, &format!("refs/gate-back/{key}")), "", "the box wrote no back commit");
    assert_eq!(calls(&steps), ["preflight", "run"]);
}

/// The box clears `refs/gate-back/<key>` as it checks the snapshot out, before the command runs, so a ref planted while
/// the run call parks on a `hold` step is gone before the client looks for one. The stale ref is planted between two
/// runs instead, and the second run's call runs nothing (`exit 0`), so nothing clears it before the client reads it.
#[test]
fn a_back_commit_not_made_from_this_snapshot_is_ignored() {
    let case = client_case("client-stale-back");
    write(&case, "a.txt", "a\n");
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let pushed = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(pushed.code, Some(0), "{pushed:?}");
    let key = key_of(&events(&case));
    let stale = plant_back(&case, &key);
    let before = porcelain(&case);

    let second = ssh_steps(&case, "second", &["ok", "exit 0"]);
    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "the run's status stands: {outcome:?}");
    let line = format!(
        "gate: ignored stale remote changes {}: not made from snapshot {}",
        &stale[..8],
        pushed_snapshot(&case, "second.log")
    );
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert_eq!(porcelain(&case), before, "the worktree is untouched: {outcome:?}");
    let kept = git_out(
        &case,
        &[],
        &["for-each-ref", "--format=%(objectname)", &format!("refs/gate-remote/{key}-back")],
    );
    assert_eq!(kept, "", "nothing was kept in the worktree");
    assert_eq!(calls(&second), ["preflight", "run"]);
}

#[test]
fn a_worktree_changed_during_the_run_keeps_the_changes_and_fails_a_passing_run() {
    let case = client_case("client-worktree-changed");
    write(&case, "a.txt", "before\n");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", "made-on-box.txt"]));
    wait_until("the run call", || calls(&steps).len() == 2);
    write(&case, "a.txt", "after\n");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let key = key_of(&events(&case));
    let back = builder_ref(&case, &format!("refs/gate-back/{key}"));
    assert!(is_sha(&back), "the box wrote the back commit: {back:?}");
    let line = format!(
        "gate: remote changes not applied: the worktree changed during the run; see git diff {} {}",
        pushed_snapshot(&case, "out.log"),
        &back[..8]
    );
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert_eq!(
        git_out(&case, &[], &["rev-parse", &format!("refs/gate-remote/{key}-back")]),
        back,
        "the changes are kept at the key's -back ref"
    );
    assert!(!case.folder().join("made-on-box.txt").exists(), "the box's change is not here");
    assert_eq!(
        fs::read_to_string(case.folder().join("a.txt")).expect("the file is there"),
        "after\n",
        "the worktree's own change stands"
    );
}

#[test]
fn a_failing_run_keeps_its_status_when_the_worktree_changed_during_it() {
    let case = client_case("client-worktree-changed-failing");
    write(&case, "a.txt", "before\n");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(
        &case,
        "out.log",
        &steps,
        &[r".\tool.exe", "mark", "made-on-box.txt", "exit", "3"],
    ));
    wait_until("the run call", || calls(&steps).len() == 2);
    write(&case, "a.txt", "after\n");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(3), "a failing run keeps its own status: {outcome:?}");
    let key = key_of(&events(&case));
    let back = builder_ref(&case, &format!("refs/gate-back/{key}"));
    assert!(is_sha(&back), "the box wrote the back commit: {back:?}");
    let line = format!(
        "gate: remote changes not applied: the worktree changed during the run; see git diff {} {}",
        pushed_snapshot(&case, "out.log"),
        &back[..8]
    );
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(!case.folder().join("made-on-box.txt").exists(), "the box's change is not here");
}

#[test]
fn a_back_commit_that_touches_a_git_file_is_kept_and_fails_a_passing_run() {
    let case = client_case("client-back-git-file");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", ".gitignore"]));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let key = key_of(&events(&case));
    let back = builder_ref(&case, &format!("refs/gate-back/{key}"));
    assert!(is_sha(&back), "the box wrote the back commit: {back:?}");
    let line = format!(
        "gate: remote changes not applied: they touch .gitignore, which a sync-back never writes; see git diff {} {}",
        pushed_snapshot(&case, "out.log"),
        &back[..8]
    );
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert_eq!(
        git_out(&case, &[], &["rev-parse", &format!("refs/gate-remote/{key}-back")]),
        back,
        "the changes are kept at the key's -back ref"
    );
    assert_eq!(
        fs::read_to_string(case.folder().join(".gitignore")).expect("the .gitignore is there"),
        CLIENT_IGNORED,
        "the worktree's .gitignore is untouched"
    );
    assert!(!outcome.stderr.contains("gate: synced back "), "{outcome:?}");
}

/// The box makes `made-on-box.txt`, which this worktree holds already but excludes in its own `.git/info/exclude`: the
/// snapshot leaves it out, so the worktree still matches it, and `git apply` refuses to make a file that is there.
#[test]
fn a_back_commit_that_cannot_be_applied_is_kept_and_fails_the_run() {
    let case = client_case("client-back-apply-fails");
    fs::write(case.folder().join(".git").join("info").join("exclude"), "made-on-box.txt\n").expect("the exclude file can be written");
    write(&case, "made-on-box.txt", "here\n");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", "made-on-box.txt"]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let failed = outcome
        .stderr
        .lines()
        .find(|line| line.starts_with("gate: the remote run failed: git -c core.protectNTFS=true apply "))
        .unwrap_or_default();
    let key = key_of(&events(&case));
    let back = builder_ref(&case, &format!("refs/gate-back/{key}"));
    assert!(is_sha(&back), "the box wrote the back commit: {back:?}");
    let ending = format!(
        "made-on-box.txt: already exists in working directory; the changes are kept: see git diff {} {}",
        pushed_snapshot(&case, "out.log"),
        &back[..8]
    );
    assert!(
        failed.ends_with(&ending),
        "the run failed in git apply, its line ending with apply's reason and where the changes are: {outcome:?}"
    );
    assert_eq!(
        git_out(&case, &[], &["rev-parse", &format!("refs/gate-remote/{key}-back")]),
        back,
        "the changes are kept at the key's -back ref"
    );
    assert_eq!(
        fs::read_to_string(case.folder().join("made-on-box.txt")).expect("the worktree's file is there"),
        "here\n",
        "the worktree's own file stands"
    );
    assert_eq!(patch_files(&case), Vec::<String>::new(), "the patch is removed");
}

/// The worktree's `held.slow` runs through a required clean filter that passes while the case's `broken` file is
/// absent and fails once it is there; the case writes it while the run call parks, so the snapshot's `git add -A`
/// passes and the sync-back's check of the worktree, before any apply, fails.
#[test]
fn a_back_commit_whose_worktree_check_fails_is_kept_and_fails_the_run() {
    let case = client_case("client-back-check-fails");
    let broken = case.folder().join("ssh").join("broken");
    git(&case, &["config", "filter.slow.clean", &format!("test ! -e '{}' && cat", shown(&broken))]);
    git(&case, &["config", "filter.slow.required", "true"]);
    write(&case, ".gitattributes", "*.slow filter=slow\n");
    write(&case, "held.slow", "held\n");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", "made-on-box.txt"]));
    wait_until("the run call", || calls(&steps).len() == 2);
    fs::write(&broken, "").expect("the broken file can be made");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let key = key_of(&events(&case));
    let back = builder_ref(&case, &format!("refs/gate-back/{key}"));
    assert!(is_sha(&back), "the box wrote the back commit: {back:?}");
    let failed = outcome
        .stderr
        .lines()
        .find(|line| line.starts_with("gate: the remote run failed: git ") && line.contains(" add -A "))
        .unwrap_or_default();
    let ending = format!(
        "; the changes are kept: see git diff {} {}",
        pushed_snapshot(&case, "out.log"),
        &back[..8]
    );
    assert!(
        failed.contains("held.slow: clean filter 'slow' failed") && failed.ends_with(&ending),
        "the worktree check failed, its line ending with where the changes are: {outcome:?}"
    );
    assert_eq!(
        git_out(&case, &[], &["rev-parse", &format!("refs/gate-remote/{key}-back")]),
        back,
        "the changes are kept at the key's -back ref"
    );
    assert!(!case.folder().join("made-on-box.txt").exists(), "nothing was applied");
}

/// The case plants a back commit between two runs, as the stale-back case does; the second run's call is the box's own
/// busy refusal, so a sync-back after it would find the planted commit and say it ignored it.
#[test]
fn a_refused_run_call_syncs_nothing_back() {
    let case = client_case("client-refused-back");
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let pushed = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(pushed.code, Some(0), "{pushed:?}");
    let key = key_of(&events(&case));
    plant_back(&case, &key);
    let held = hold_box_lock(&case, &key);
    let second = ssh_steps(&case, "second", &["ok", "ok"]);

    let command = [r".\tool.exe", "mark", RAN, "out", "ran here"];
    let (pid, outcome) = finish_with_pid(client_gate(&case, "second.log", &second, &command));

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "it ran here: {outcome:?}");
    assert_eq!(runs(&case), (1, 0), "the command ran once, here: {outcome:?}");
    assert!(
        !outcome.stderr.contains("gate: ignored stale remote changes"),
        "no sync-back: {outcome:?}"
    );
    assert_eq!(fallbacks(&own(&case, pid)), ["busy"]);
    assert_eq!(calls(&second), ["preflight", "run"]);
}

/// Through `local-ssh`, the box's run succeeds, then the sync-back's `ls-remote` is refused by ssh: the run fails
/// with no fallback, and nothing after that call runs.
#[test]
fn a_git_failure_of_the_sync_back_fails_the_run_with_no_fallback() {
    let case = client_case("client-back-git-failure");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "fail-other"]);

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "mark", "made-on-box.txt"],
    ));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: the remote run failed: git ls-remote"), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
    assert_eq!(calls(&steps), ["preflight", "git-receive-pack", "run", "git-upload-pack"]);
}

/// The case's own exclude file leaves `made-on-box.txt` out of the snapshot, so the box's command makes it anew while
/// the worktree holds a file of that name: the worktree still matches the snapshot, and the patch cannot apply.
#[test]
fn a_patch_that_does_not_apply_fails_the_run_and_is_removed() {
    let case = client_case("client-apply-fails");
    let exclude = case.folder().join(".git").join("info").join("exclude");
    fs::create_dir_all(exclude.parent().expect("the exclude file has a folder")).expect("the info folder can be made");
    let excluded = if exclude.exists() {
        fs::read_to_string(&exclude).expect("the exclude file can be read")
    } else {
        String::new()
    };
    fs::write(&exclude, format!("{excluded}\nmade-on-box.txt\n")).expect("the exclude file can be written");
    write(&case, "made-on-box.txt", "here\n");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", "made-on-box.txt"]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("gate: the remote run failed: git -c core.protectNTFS=true apply "),
        "{outcome:?}"
    );
    assert_eq!(patch_files(&case), Vec::<String>::new(), "the patch is removed");
    assert_eq!(
        fs::read_to_string(case.folder().join("made-on-box.txt")).expect("the worktree's file is there"),
        "here\n",
        "the worktree's own file stands"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
}

/// Through `local-ssh`, git's own ssh is the stand-in, so its `ls-remote` can be dropped before the banner: the
/// preflight, the push's receive-pack and the run call, then the sync-back's ls-remote twice and its fetch.
#[test]
fn a_banner_drop_of_the_sync_back_s_ls_remote_is_retried_once() {
    let case = client_case("client-back-retry");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "fail-banner", "ok", "ok"]);

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "mark", "made-on-box.txt"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 1, "{outcome:?}");
    assert!(outcome.stderr.contains("gate: synced back 1 file(s): made-on-box.txt"), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(case.folder().join("made-on-box.txt")).expect("the box's file is here"),
        "run\n"
    );
    assert_eq!(
        calls(&steps),
        [
            "preflight",
            "git-receive-pack",
            "run",
            "git-upload-pack",
            "git-upload-pack",
            "git-upload-pack"
        ]
    );
}

/// The stall window or ceiling a watchdog case gives the one call `--test-remote-watch-call` names, which sends nothing:
/// every other call keeps the run's own windows, which no case reaches.
const WATCHED_MS: &str = "300";
/// The line of a run call the watchdog killed for a stall of [`WATCHED_MS`], in whole seconds rounded up.
const STALLED_LINE: &str = "gate: STALLED: no output or remote CPU for 1 s";

/// Whether the stand-in's calls `made` are `expected`, or `expected` without the call `watched`: a call killed
/// [`WATCHED_MS`] after its start may be killed on a busy machine before the stand-in has logged it, and the next call
/// then takes its step.
fn made_but_maybe(made: &[String], expected: &[&str], watched: &str) -> bool {
    let without: Vec<&str> = expected.iter().copied().filter(|verb| *verb != watched).collect();
    made == expected || made == without.as_slice()
}

/// A path no case creates: a `hold` on it parks the stand-in until the client kills it.
fn never(case: &Case) -> String {
    shown(&case.folder().join("ssh").join("never"))
}

/// The verdict the run's release journalled.
fn verdict(case: &Case) -> String {
    release(case)["verdict"].as_str().expect("the release names its verdict").to_owned()
}

/// Through `local-ssh`, so a sync-back would show as a call of the stand-in.
#[test]
fn a_stalled_run_call_is_killed_and_the_command_stopped() {
    let case = client_case("client-stalled");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("hold {}", never(&case)), "ok"]);
    let tuned = Tuned {
        kind: "local-ssh",
        extra: &["--test-remote-watch-call", "run", "--test-stall-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains(STALLED_LINE), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.contains(STALLED_LINE), "the kill line is in the log: {log}");
    assert_eq!(verdict(&case), "stalled");
    let made = calls(&steps);
    assert!(
        made_but_maybe(&made, &["preflight", "git-receive-pack", "run", "stop"], "run"),
        "a stop, and no sync-back: {made:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_run_past_its_ceiling_is_killed_with_a_timed_out_line() {
    let case = client_case("client-timed-out");
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", never(&case)), "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "run", "--test-remote-timeout-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: TIMED OUT after 1 s"), "{outcome:?}");
    assert_eq!(verdict(&case), "timed-out");
    let made = calls(&steps);
    assert!(made_but_maybe(&made, &["preflight", "run", "stop"], "run"), "{made:?}");
}

#[test]
fn a_stalled_preflight_falls_back_unreachable_and_runs_here() {
    let case = client_case("client-stalled-preflight");
    let steps = ssh_steps(&case, "ssh", &[&format!("hold {}", never(&case))]);
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "preflight", "--test-stall-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: remote builder unreachable: the preflight stalled"),
        "{outcome:?}"
    );
    assert!(outcome.stderr.contains(UNREACHABLE_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    let made = calls(&steps);
    assert!(
        made_but_maybe(&made, &["preflight"], "preflight"),
        "no stop: the command never started: {made:?}"
    );
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

/// Through `local-ssh`, the stand-in is git's ssh for the push, whose standard error git passes on as its own.
#[test]
fn a_push_whose_progress_updates_pass_64_kib_together_is_not_cut() {
    let case = client_case("client-push-progress");
    let steps = ssh_steps(&case, "ssh", &["ok", "progress-ok 70000", "ok", "ok"]);

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "out", "ran there"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("longer than 64 KiB"), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "routed, not fallen back");
    let made = calls(&steps);
    assert_eq!(
        made.get(..3),
        Some(&["preflight".to_owned(), "git-receive-pack".to_owned(), "run".to_owned()][..]),
        "{made:?}"
    );
}

#[test]
fn a_preflight_line_longer_than_64_kib_falls_back_unreachable_and_runs_here() {
    let case = client_case("client-preflight-flood");
    let steps = ssh_steps(&case, "ssh", &["flood 70000"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("gate: remote builder unreachable: the preflight sent a line longer than 64 KiB"),
        "{outcome:?}"
    );
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "no stop: the command never started");
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

#[test]
fn a_stall_window_under_15_s_is_raised_to_15() {
    let case = client_case("client-stall-floor");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--stall", "3"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let raised = "gate: --stall 3 raised to 15 for a remote run (heartbeats come every 5 s)";
    assert_eq!(outcome.stderr.matches(raised).count(), 1, "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
}

#[test]
fn a_stop_that_does_not_finish_is_given_up() {
    let case = client_case("client-stop-hangs");
    let hold = format!("hold {}", never(&case));
    let steps = ssh_steps(&case, "ssh", &["ok", &hold, &hold]);
    let tuned = Tuned {
        extra: &[
            "--test-remote-watch-call",
            "run",
            "--test-stall-ms",
            WATCHED_MS,
            "--test-remote-bound-ms",
            "200",
        ],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: stopping the command on builder did not finish in 1 s"),
        "{outcome:?}"
    );
    // The run call and the stop are each killed 300 or 200 ms after they start, either maybe before the stand-in logs it.
    let made = calls(&steps);
    let logged: Vec<&str> = ["preflight", "run", "stop"]
        .into_iter()
        .filter(|verb| made.iter().any(|call| call == verb))
        .collect();
    assert!(
        made.first().is_some_and(|call| call == "preflight") && made == logged.as_slice(),
        "{made:?}"
    );
}

/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

#[test]
fn a_ctrl_c_during_the_run_call_stops_the_command() {
    let case = client_case("client-ctrl-c-run");
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("say-hold started {}", never(&case)), "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "run"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    // The interrupt comes once the run call wrote its first byte, by when the stand-in has logged its call.
    assert_eq!(calls(&steps), ["preflight", "run", "stop"], "the command is stopped");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_ctrl_c_during_the_preflight_stops_nothing() {
    let case = client_case("client-ctrl-c-preflight");
    let steps = ssh_steps(&case, "ssh", &[&format!("say-hold started {}", never(&case))]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "preflight"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    // The interrupt comes once the preflight wrote its first byte, by when the stand-in has logged its call.
    assert_eq!(calls(&steps), ["preflight"], "no stop: the command never started");
    assert!(!outcome.stdout.contains("ran here"), "nothing ran here: {outcome:?}");
}

/// The preflight, the push's receive-pack and the run call through `local-ssh`, then the sync-back's `ls-remote` and,
/// as `last` says, its fetch: a run whose command made a file on the builder, ended by a sync-back call. A last call
/// that `--test-remote-watch-call` names may be killed before the stand-in has logged it.
fn a_failed_sync_back(name: &str, last: fn(&Case) -> Vec<String>, tuned: &Tuned<'_>) -> Outcome {
    let case = client_case(name);
    let mut all = vec!["ok".to_owned(), "ok".to_owned(), "ok".to_owned()];
    all.extend(last(&case));
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    let steps = ssh_steps(&case, "ssh", &all);
    let tuned = Tuned { kind: "local-ssh", ..*tuned };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "mark", "made-on-box.txt"],
    ));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(!case.folder().join("made-on-box.txt").exists(), "nothing was applied: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
    // Both sync-back calls, the `ls-remote` and the fetch, reach the stand-in as `git-upload-pack`.
    let mut expected = vec!["preflight", "git-receive-pack", "run"];
    expected.resize(all.len(), "git-upload-pack");
    let held = &expected[..expected.len() - 1];
    let made = calls(&steps);
    let watched = tuned.extra.contains(&"--test-remote-watch-call");
    assert!(made == expected || (watched && made == held), "{made:?}");
    outcome
}

#[test]
fn a_sync_back_fetch_that_sends_nothing_for_the_stall_window_fails_the_run() {
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "fetch", "--test-stall-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = a_failed_sync_back(
        "client-fetch-stalls",
        |case| vec!["ok".to_owned(), format!("hold {}", never(case))],
        &tuned,
    );

    assert!(
        outcome
            .stderr
            .contains("gate: the remote run failed: git fetch did not answer for 300 ms"),
        "{outcome:?}"
    );
}

#[test]
fn a_sync_back_ls_remote_past_its_deadline_fails_the_run() {
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "ls-remote", "--test-remote-timeout-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = a_failed_sync_back("client-ls-remote-late", |case| vec![format!("hold {}", never(case))], &tuned);

    assert!(
        outcome
            .stderr
            .contains("gate: the remote run failed: git ls-remote did not answer within 300 ms"),
        "{outcome:?}"
    );
}

#[test]
fn a_sync_back_call_that_sends_a_line_longer_than_64_kib_fails_the_run() {
    let outcome = a_failed_sync_back("client-ls-remote-flood", |_| vec!["flood 70000".to_owned()], &Tuned::default());

    assert!(
        outcome
            .stderr
            .contains("gate: the remote run failed: git ls-remote sent a line longer than 64 KiB"),
        "{outcome:?}"
    );
}

/// The bound of the `stop` and `boot` calls in a lost-connection case: 600 s, a hang guard, in place of their 30 s,
/// which a starved run could pass.
const HANG_GUARD_BOUND: [&str; 2] = ["--test-remote-bound-ms", "600000"];
/// A boot time a minute after [`BOOTED`]: a builder that restarted.
const REBOOTED: &str = "2026-10-10T08:01:00.0000000Z";

/// The gate of a lost-connection case logging to `out.log`: [`client_gate_tuned`] through the transport form `kind`,
/// its bounded calls bound by [`HANG_GUARD_BOUND`].
fn lost_gate(case: &Case, kind: &str, steps: &Path, command: &[&str]) -> Command {
    let tuned = Tuned {
        kind,
        extra: &HANG_GUARD_BOUND,
        ..Tuned::default()
    };
    client_gate_tuned(case, "out.log", steps, &tuned, command)
}

/// Through `local-ssh`, so a sync-back would show as a call of the stand-in.
#[test]
fn a_connection_lost_mid_run_is_stopped_and_syncs_nothing_back() {
    let case = client_case("client-lost");
    let booted = format!("boot {BOOTED}");
    let steps = ssh_steps(&case, "ssh", &[&booted, "ok", "drop", "ok", &booted]);

    let outcome = finish(lost_gate(&case, "local-ssh", &steps, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(255), "{outcome:?}");
    assert!(outcome.stderr.contains(LOST_LINE), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: synced back"), "{outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "git-receive-pack", "run", "stop", "boot"],
        "a stop and a boot query, and no sync-back"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_builder_that_restarted_is_a_rebooted_fallback() {
    let case = client_case("client-rebooted");
    let steps = ssh_steps(&case, "ssh", &[&format!("boot {BOOTED}"), "drop", "ok", &format!("boot {REBOOTED}")]);

    let outcome = finish(lost_gate(&case, "local", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = format!("gate: builder restarted during the run (booted {REBOOTED}); the run did not finish there");
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(!outcome.stderr.contains(LOST_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "it ran here: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop", "boot"], "the command is stopped first");
    assert_eq!(fallbacks(&events(&case)), ["rebooted"]);
}

#[test]
fn a_boot_time_within_the_slack_is_the_same_boot() {
    let case = client_case("client-same-boot");
    let steps = ssh_steps(
        &case,
        "ssh",
        &[&format!("boot {BOOTED}"), "drop", "ok", "boot 2026-10-10T08:00:05.0000000Z"],
    );

    let outcome = finish(lost_gate(&case, "local", &steps, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(255), "{outcome:?}");
    assert!(outcome.stderr.contains(LOST_LINE), "{outcome:?}");
    assert!(!outcome.stderr.contains("restarted during the run"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop", "boot"]);
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The run call writes a line before its connection is lost, and the boot query fails after the banner, so it is not
/// made again.
#[test]
fn a_builder_unreachable_after_the_loss_falls_back_unreachable() {
    let case = client_case("client-lost-unreachable");
    let steps = ssh_steps(&case, "ssh", &[&format!("boot {BOOTED}"), "out-then-drop", "ok", "fail-other"]);

    let outcome = finish(lost_gate(&case, "local", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: asking builder for its boot time failed (status 255)"),
        "{outcome:?}"
    );
    let line = "gate: builder unreachable after the connection was lost; the run did not finish there";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(!outcome.stderr.contains(LOST_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "it ran here: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop", "boot"]);
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

#[test]
fn a_lost_connection_with_no_boot_at_preflight_is_stopped() {
    let case = client_case("client-lost-no-boot");
    let steps = ssh_steps(&case, "ssh", &["no-boot", "drop", "ok"]);

    let outcome = finish(lost_gate(&case, "local", &steps, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(255), "{outcome:?}");
    assert!(outcome.stderr.contains(LOST_LINE), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop"], "no boot query with no boot time to compare");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The run call writes a failure marker before its connection is lost; the run that falls back here passes.
#[test]
fn a_run_that_falls_back_after_a_loss_reads_none_of_the_builder_s_output() {
    let case = client_case("client-lost-marker");
    let steps = ssh_steps(
        &case,
        "ssh",
        &[
            &format!("boot {BOOTED}"),
            "say-exit 255 Program.cs(1,1): error CS1002: ; expected",
            "ok",
            &format!("boot {REBOOTED}"),
        ],
    );

    let outcome = finish(lost_gate(&case, "local", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "it ran here: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop", "boot"]);
    assert_eq!(fallbacks(&events(&case)), ["rebooted"]);
}

/// The `stop` and the `boot` call each hold until the 200 ms bound kills them; either may be killed before the
/// stand-in has logged it, and the next call then takes its step, which holds too.
#[test]
fn a_boot_call_that_does_not_finish_falls_back_unreachable() {
    let case = client_case("client-boot-hangs");
    let hold = format!("hold {}", never(&case));
    let steps = ssh_steps(&case, "ssh", &[&format!("boot {BOOTED}"), "drop", &hold, &hold]);
    let tuned = Tuned {
        extra: &["--test-remote-bound-ms", "200"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: asking builder for its boot time did not finish in 1 s"),
        "{outcome:?}"
    );
    let line = "gate: builder unreachable after the connection was lost; the run did not finish there";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "it ran here: {outcome:?}");
    let made = calls(&steps);
    let logged: Vec<&str> = ["preflight", "run", "stop", "boot"]
        .into_iter()
        .filter(|verb| made.iter().any(|call| call == verb))
        .collect();
    assert!(
        made.starts_with(&["preflight".to_owned(), "run".to_owned()]) && made == logged.as_slice(),
        "{made:?}"
    );
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

#[test]
fn a_boot_call_dropped_before_the_banner_is_retried_once() {
    let case = client_case("client-boot-retry");
    let booted = format!("boot {BOOTED}");
    let steps = ssh_steps(&case, "ssh", &[&booted, "drop", "ok", "fail-banner", &booted]);

    let outcome = finish(lost_gate(&case, "local", &steps, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(255), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 1, "{outcome:?}");
    assert!(outcome.stderr.contains(LOST_LINE), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop", "boot", "boot"]);
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The stand-in copied into the case folder under `name`, so a relative `--test-remote-ssh` names it there.
fn copy_ssh(case: &Case, name: &str) {
    fs::copy(FAKE_SSH, case.folder().join(name)).expect("fake-ssh can be copied into the case folder");
}

/// `--test-remote-ssh .\fake-ssh.exe`, a name with a folder part, is read against the working folder as the gate reads
/// its own programs, so the stand-in copied into the case folder runs and the run reaches the builder.
#[test]
fn a_relative_ssh_stand_in_is_found_the_way_the_gate_finds_a_program() {
    let case = client_case("client-relative-ssh");
    copy_ssh(&case, "fake-ssh.exe");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        ssh: Some(r".\fake-ssh.exe"),
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: pushed snapshot "), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
}

/// A bare `--test-remote-ssh` name is searched along `PATH` alone, never the working folder: a copy under a name no
/// `PATH` folder holds — the suite's own `PATH` holds the built `fake-ssh.exe` — is not found, and the run is refused
/// with a clear line before its first ssh call.
#[test]
fn a_bare_ssh_stand_in_name_is_not_searched_in_the_working_folder() {
    let case = client_case("client-bare-ssh");
    copy_ssh(&case, "stand-in-ssh.exe");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        ssh: Some("stand-in-ssh.exe"),
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    let transport = format!("local:{}", shown(&case.folder().join("builder")));
    // The win32 error `ERROR_FILE_NOT_FOUND`, as `find_program` reports a word it does not find.
    let line = format!(
        "gate: cannot use the remote transport '{transport}': cannot find --test-remote-ssh stand-in-ssh.exe: {}",
        gate_win32::win32_message(2)
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no ssh call is made");
}

/// The transport's form is refused before the stand-in is looked for: `remote:x`, no local transport at all, with a
/// stand-in the gate cannot find reports the form, not the stand-in.
#[test]
fn a_transport_s_form_is_refused_before_the_stand_in_is_looked_for() {
    let case = client_case("client-form-first");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        kind: "remote",
        ssh: Some("stand-in-ssh.exe"),
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    let transport = format!("remote:{}", shown(&case.folder().join("builder")));
    let line = format!("gate: cannot use the remote transport '{transport}': it is not local:<folder>");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no ssh call is made");
}

/// The PC holds `bin/same.txt` as the command writes it on the builder and `bin/differs.txt` otherwise: only the second
/// comes back, which the box can tell only from the manifest the run call carried on its standard input.
#[test]
fn a_routed_run_sends_its_outputs_manifest() {
    let case = outputs_case("client-outputs-manifest", &BIN);
    write(&case, r"bin\same.txt", "same");
    write(&case, r"bin\differs.txt", "pc");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\same.txt", "same", "write", r"bin\differs.txt", "box"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 1 "), "one file comes back: {outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.contains("gate-remote-outputs: 1 "), "the box's line is in the log: {log}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"]);
    let differs = fs::read_to_string(case.folder().join("bin").join("differs.txt")).expect("the file is there");
    assert_eq!(differs, "box", "the file that came back is installed: {outcome:?}");
    assert!(!pending_marker(&case).exists(), "the outputs step completed: {outcome:?}");
}

#[test]
fn outputs_zero_clears_the_pending_marker() {
    let case = outputs_case("client-outputs-zero", &BIN);
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there"]));
    wait_until("the run call", || calls(&steps).len() == 2);
    let marker = pending_marker(&case);
    assert!(marker.exists(), "the marker is written before the run call");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 0"), "{outcome:?}");
    assert!(!marker.exists(), "nothing to bring back completes the outputs step: {outcome:?}");
}

/// The first run builds `bin/built.txt` on the builder and fails, so nothing comes back, and the builder's work folder
/// keeps the ignored file. The second run leaves it as it is, so only `OutputsFull` brings it back: the PC holds another.
#[test]
fn a_failing_run_leaves_the_marker_and_the_next_run_sends_outputs_full() {
    let case = outputs_case("client-outputs-full", &BIN);
    write(&case, r"bin\built.txt", "pc");
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let failed = finish(client_gate(
        &case,
        "first.log",
        &first,
        &[r".\tool.exe", "write", r"bin\built.txt", "box", "exit", "3"],
    ));
    assert_eq!(failed.code, Some(3), "{failed:?}");
    assert!(!failed.stderr.contains("gate-remote-outputs:"), "a failing run packs nothing: {failed:?}");
    assert!(pending_marker(&case).exists(), "a failing run leaves the marker: {failed:?}");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate-remote-outputs: 1 "),
        "the file this run left alone comes back: {outcome:?}"
    );
}

/// A routed run with output globs that the parity check refuses is refused before the push, so the builder's work
/// folder is never reached: it must leave no outputs pending marker, which would make the next run of the worktree ask
/// for every output that differs from the PC's (`OutputsFull`). That next run carries the PC's manifest as `Outputs`: a
/// file under the globs the builder's work folder held from an earlier run and this run leaves alone comes back only
/// under `OutputsFull`, so it stays where it is.
#[test]
fn a_parity_refusal_leaves_no_outputs_pending_marker_and_the_next_run_sends_outputs() {
    let case = outputs_case("client-outputs-parity-refusal", &BIN);
    write(&case, "global.json", r#"{"sdk": {"version": "999.0.100"}}"#);
    write(&case, "src/App/App.csproj", "<Project />\n");
    write(&case, r"bin\left.txt", "pc");
    let refused_steps = ssh_steps(&case, "refused", &["ok"]);
    let mut refused_gate = client_gate(&case, "refused.log", &refused_steps, &[r".\tool.exe", "out", "ran here"]);
    refused_gate.env("PATH", box_path_with_dotnet(&case, &["8.0.404"]));

    let (pid, refused) = finish_with_pid(refused_gate);

    assert_eq!(refused.code, Some(0), "the run here passed: {refused:?}");
    assert_eq!(fallbacks(&own(&case, pid)), ["parity"], "{refused:?}");
    assert!(refused.stderr.contains(PARITY_LINE), "{refused:?}");
    assert_eq!(calls(&refused_steps), ["preflight"], "the preflight alone reached the builder");
    // Read now and asserted at the end, so the red case shows the marker the refusal leaves and the run it makes.
    let refused_left_the_marker = pending_marker(&case).exists();
    // The builder's work folder keeps an earlier run's build output, as `hold_box_lock` and `plant_back` stand in for
    // an earlier run's files on the builder: the next run leaves it alone, and only the pre-command listing tells such
    // a file from one the run made.
    let key = key_of(&events(&case));
    write(&case, &format!(r"builder\work\{key}\bin\left.txt"), "box");
    let served_steps = ssh_steps(&case, "served", &["ok", "ok", "ok"]);
    let mut served_gate = client_gate(&case, "served.log", &served_steps, &[r".\tool.exe", "out", "ran there"]);
    served_gate.env("PATH", box_path_with_dotnet(&case, &["999.0.100"]));

    let outcome = finish(served_gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("gate-remote-outputs: 1 "),
        "the file this run left alone comes back only under OutputsFull: {outcome:?}"
    );
    assert!(
        outcome.stderr.contains("gate-remote-outputs: 0"),
        "the run brought nothing back: {outcome:?}"
    );
    assert!(!refused_left_the_marker, "a parity refusal leaves no marker: {refused:?}");
}

/// A routed run saves the hash of each file under its globs last written well before it: one line in the state root, no
/// temporary file left beside it. A file written ahead of the run's start is not saved, and one the run brought back
/// holds the hash of the file the outputs step put in place, not the PC's hash of the file it replaced. The builder's
/// file was written an hour before the run, after a first run named its work folder, so the install keeps its hash; the
/// pending marker has the box bring it back though the run did not write it.
#[test]
fn a_routed_run_saves_the_hashes_of_its_stable_files() {
    let case = outputs_case("client-hashes-saved", &BIN);
    write(&case, r"bin\same.txt", "same");
    write(&case, r"bin\differs.txt", "pc");
    write(&case, r"bin\ahead.txt", "ahead");
    backdate(&case, r"bin\same.txt", HOUR);
    backdate(&case, r"bin\differs.txt", HOUR);
    written_at(&case, r"bin\ahead.txt", SystemTime::now() + HOUR);
    first_run(&case);
    install_from_the_box(&case, "bin/differs.txt", "box");
    fs::write(pending_marker(&case), "").expect("the pending marker can be made");
    let steps = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\same.txt", "same"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 1 "), "one file comes back: {outcome:?}");
    let text = fs::read_to_string(hashes_file(&case)).expect("the hash cache was saved");
    assert!(text.ends_with('\n') && text.matches('\n').count() == 1, "one line: {text}");
    let files = &hashes(&case)["files"];
    assert_eq!(files["bin/same.txt"]["size"], 4, "{text}");
    assert_eq!(files["bin/same.txt"]["sha256"], sha256_hex("same"), "{text}");
    assert_eq!(
        files["bin/differs.txt"]["sha256"],
        sha256_hex("box"),
        "the file the outputs step put in place: {text}"
    );
    assert!(files.get("bin/ahead.txt").is_none(), "a file written ahead of the run: {text}");
    let temps: Vec<PathBuf> = fs::read_dir(case.root().join("remote"))
        .expect("the state root's remote folder can be listed")
        .map(|entry| entry.expect("an entry can be read").path())
        .filter(|path| path.extension().is_some_and(|kind| kind.eq_ignore_ascii_case("tmp")))
        .collect();
    assert_eq!(temps, Vec::<PathBuf>::new(), "no temporary file is left");
}

/// The second run takes the PC's hash of a file whose size and last write match the saved ones from the cache, not
/// from the file: a cache saying the PC holds what the builder writes makes the box bring nothing back.
#[test]
fn a_second_run_uses_the_cached_hash_not_the_file() {
    let case = outputs_case("client-hashes-used", &BIN);
    write(&case, r"bin\differs.txt", "pc");
    backdate(&case, r"bin\differs.txt", HOUR);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let seeded = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(seeded.code, Some(0), "{seeded:?}");
    plant(&case, "bin/differs.txt", &sha256_hex("box"));
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(
        &case,
        "second.log",
        &second,
        &[r".\tool.exe", "write", r"bin\differs.txt", "box"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate-remote-outputs: 0"),
        "the manifest carried the cached hash: {outcome:?}"
    );
    let differs = fs::read_to_string(case.folder().join("bin").join("differs.txt")).expect("the file is there");
    assert_eq!(differs, "pc", "{outcome:?}");
}

/// A file whose size or last write differs from the saved stamp is read again, its cached hash not taken: with the
/// cache saying the PC holds what the builder writes, both files still come back.
#[test]
fn a_rewritten_file_is_hashed_again() {
    let case = outputs_case("client-hashes-stale", &BIN);
    write(&case, r"bin\longer.txt", "pc");
    write(&case, r"bin\older.txt", "pc");
    backdate(&case, r"bin\longer.txt", HOUR);
    backdate(&case, r"bin\older.txt", HOUR);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let seeded = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(seeded.code, Some(0), "{seeded:?}");
    plant(&case, "bin/longer.txt", &sha256_hex("box"));
    plant(&case, "bin/older.txt", &sha256_hex("box"));
    write(&case, r"bin\longer.txt", "pc!");
    backdate(&case, r"bin\longer.txt", HOUR);
    backdate(&case, r"bin\older.txt", 2 * HOUR);
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\longer.txt", "box", "write", r"bin\older.txt", "box"];

    let outcome = finish(client_gate(&case, "second.log", &second, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 2 "), "both files come back: {outcome:?}");
    for name in ["longer.txt", "older.txt"] {
        let text = fs::read_to_string(case.folder().join("bin").join(name)).expect("the file is there");
        assert_eq!(text, "box", "{name}: {outcome:?}");
    }
}

/// A hash cache that is not an outputs manifest counts as empty, says nothing, and the run saves a good one over it,
/// even one that keeps no hash: the only file under the globs is written ahead of the run.
#[test]
fn a_corrupt_hashes_file_is_ignored_and_replaced() {
    let case = outputs_case("client-hashes-corrupt", &BIN);
    write(&case, r"bin\ahead.txt", "ahead");
    written_at(&case, r"bin\ahead.txt", SystemTime::now() + HOUR);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let seeded = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(seeded.code, Some(0), "{seeded:?}");
    assert!(
        !hashes_file(&case).exists(),
        "a run that keeps no hash saves no cache over none: {seeded:?}"
    );
    fs::write(hashes_file(&case), "not json").expect("the hash cache can be spoiled");
    let second = ssh_steps(&case, "second", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 0"), "{outcome:?}");
    assert!(!outcome.stderr.contains("outputs-hashes"), "the cache is not said: {outcome:?}");
    assert_eq!(hashes(&case)["files"], json!({}), "{outcome:?}");
}

/// While the pending marker stands the manifest takes no hash from the cache, and the cache is saved from the hashes
/// the build read: a cache saying the PC holds what the builder writes still sees the file come back, and then holds
/// the hash of the file the outputs step put in place, which the builder wrote an hour before the run.
#[test]
fn a_pending_marker_builds_the_manifest_without_the_cache_and_saves_it() {
    let case = outputs_case("client-hashes-full", &BIN);
    write(&case, r"bin\differs.txt", "pc");
    backdate(&case, r"bin\differs.txt", HOUR);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let seeded = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(seeded.code, Some(0), "{seeded:?}");
    plant(&case, "bin/differs.txt", &sha256_hex("box"));
    fs::write(pending_marker(&case), "").expect("the pending marker can be made");
    install_from_the_box(&case, "bin/differs.txt", "box");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "out", "ran there"];

    let outcome = finish(client_gate(&case, "second.log", &second, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate-remote-outputs: 1 "),
        "the manifest carried the file's own hash: {outcome:?}"
    );
    let differs = fs::read_to_string(case.folder().join("bin").join("differs.txt")).expect("the file is there");
    assert_eq!(differs, "box", "{outcome:?}");
    assert_eq!(
        hashes(&case)["files"]["bin/differs.txt"]["sha256"],
        sha256_hex("box"),
        "the cache holds the hash of the file the outputs step put in place: {outcome:?}"
    );
}

/// A build that fails saves no hash cache, not even the hash of a settled file it read before the file it could not.
#[test]
fn a_manifest_that_cannot_be_built_saves_no_hashes() {
    let case = outputs_case("client-hashes-unbuilt", &BIN);
    write(&case, r"bin\kept.txt", "kept");
    write(&case, r"bin\locked.txt", "pc");
    backdate(&case, r"bin\kept.txt", HOUR);
    backdate(&case, r"bin\locked.txt", HOUR);
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(case.folder().join("bin").join("locked.txt"))
        .expect("the file can be held");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there"]));
    drop(held);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome.stderr.contains("the manifest could not be built: "),
        "the build failed: {outcome:?}"
    );
    assert!(!hashes_file(&case).exists(), "no cache is saved: {outcome:?}");
}

/// A run whose build finds every file as the cache holds it does not write the cache again: the file keeps the last
/// write the test gave it.
#[test]
fn a_run_that_changes_no_hash_leaves_the_cache_file_alone() {
    let case = outputs_case("client-hashes-unchanged", &BIN);
    write(&case, r"bin\same.txt", "same");
    backdate(&case, r"bin\same.txt", HOUR);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let seeded = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(seeded.code, Some(0), "{seeded:?}");
    let path = hashes_file(&case);
    let saved = fs::read_to_string(&path).expect("the first run saves the cache");
    fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_modified(SystemTime::now() - HOUR))
        .expect("the cache's last write can be set");
    let aged = fs::metadata(&path).and_then(|held| held.modified()).expect("the cache's last write");
    let second = ssh_steps(&case, "second", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let now = fs::metadata(&path).and_then(|held| held.modified()).expect("the cache's last write");
    assert_eq!(now, aged, "the cache is not written again: {outcome:?}");
    assert_eq!(fs::read_to_string(&path).expect("the cache can be read"), saved, "{outcome:?}");
}

/// The hash cache is saved once the manifest is built, whatever the run's end: a failing run leaves it beside the
/// pending marker, and the next run's `OutputsFull` still brings back the file that run left alone.
#[test]
fn a_failed_run_still_leaves_the_pcs_valid_hashes() {
    let case = outputs_case("client-hashes-failed", &BIN);
    write(&case, r"bin\built.txt", "pc");
    backdate(&case, r"bin\built.txt", HOUR);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let failed = finish(client_gate(
        &case,
        "first.log",
        &first,
        &[r".\tool.exe", "write", r"bin\built.txt", "box", "exit", "3"],
    ));
    assert_eq!(failed.code, Some(3), "{failed:?}");
    assert!(pending_marker(&case).exists(), "a failing run leaves the marker: {failed:?}");
    assert_eq!(hashes(&case)["files"]["bin/built.txt"]["sha256"], sha256_hex("pc"), "{failed:?}");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate-remote-outputs: 1 "),
        "the file this run left alone comes back: {outcome:?}"
    );
    let built = fs::read_to_string(case.folder().join("bin").join("built.txt")).expect("the file is there");
    assert_eq!(built, "box", "{outcome:?}");
}

/// A file gone from the worktree drops out of the hash cache at its next save.
#[test]
fn a_deleted_file_drops_out_of_the_hashes() {
    let case = outputs_case("client-hashes-deleted", &BIN);
    write(&case, r"bin\same.txt", "same");
    write(&case, r"bin\kept.txt", "kept");
    backdate(&case, r"bin\same.txt", HOUR);
    backdate(&case, r"bin\kept.txt", HOUR);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let seeded = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(seeded.code, Some(0), "{seeded:?}");
    assert!(hashes(&case)["files"].get("bin/same.txt").is_some(), "the first run saves it: {seeded:?}");
    fs::remove_file(case.folder().join("bin").join("same.txt")).expect("the file can be removed");
    let second = ssh_steps(&case, "second", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let files = &hashes(&case)["files"];
    assert!(files.get("bin/same.txt").is_none(), "{files}");
    assert_eq!(files["bin/kept.txt"]["sha256"], sha256_hex("kept"), "{files}");
}

/// Three worktrees: a run the box refuses, as `a_refused_run_call_syncs_nothing_back` does once a first run named the
/// key; one the watchdog kills; and one whose connection is lost.
#[test]
fn a_refused_killed_or_lost_run_leaves_the_marker() {
    let refused = outputs_case("client-outputs-refused", &BIN);
    let first = ssh_steps(&refused, "first", &["ok", "ok"]);
    let cleared = finish(client_gate(&refused, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(cleared.code, Some(0), "{cleared:?}");
    let marker = pending_marker(&refused);
    assert!(!marker.exists(), "the first run brought nothing back: {cleared:?}");
    let held = hold_box_lock(&refused, &key_of(&events(&refused)));
    let second = ssh_steps(&refused, "second", &["ok", "ok"]);
    let outcome = finish(client_gate(&refused, "second.log", &second, &[r".\tool.exe", "out", "ran here"]));
    drop(held);
    assert!(outcome.stderr.contains(BUSY_LINE), "{outcome:?}");
    assert!(marker.exists(), "a refused run leaves the marker: {outcome:?}");

    let killed = outputs_case("client-outputs-killed", &BIN);
    let steps = ssh_steps(&killed, "ssh", &["ok", &format!("hold {}", never(&killed)), "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "run", "--test-stall-ms", WATCHED_MS],
        ..Tuned::default()
    };
    let outcome = finish(client_gate_tuned(
        &killed,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there"],
    ));
    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(pending_marker(&killed).exists(), "a killed run leaves the marker: {outcome:?}");

    let lost = outputs_case("client-outputs-lost", &BIN);
    let booted = format!("boot {BOOTED}");
    let steps = ssh_steps(&lost, "ssh", &[&booted, "drop", "ok", &booted]);
    let outcome = finish(lost_gate(&lost, "local", &steps, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(outcome.code, Some(255), "{outcome:?}");
    assert!(outcome.stderr.contains(LOST_LINE), "{outcome:?}");
    assert!(pending_marker(&lost).exists(), "a lost run leaves the marker: {outcome:?}");
}

#[test]
fn a_run_whose_worktree_changed_says_outputs_were_not_brought_back() {
    let case = outputs_case("client-outputs-worktree-changed", &BIN);
    write(&case, "a.txt", "before\n");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", "made-on-box.txt"]));
    wait_until("the run call", || calls(&steps).len() == 2);
    write(&case, "a.txt", "after\n");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let line = "gate: remote outputs not brought back: the worktree changed during the run";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(
        pending_marker(&case).exists(),
        "the box packed nothing, but the run did not pass: {outcome:?}"
    );
}

/// The PC's `bin/locked.txt` is held open, sharing nothing, while the run hashes it.
#[test]
fn a_manifest_that_cannot_be_built_still_runs_and_fails_a_pass() {
    let case = outputs_case("client-outputs-unbuilt", &BIN);
    write(&case, r"bin\locked.txt", "pc");
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(case.folder().join("bin").join("locked.txt"))
        .expect("the file can be held");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there"]));
    drop(held);

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "the run still runs: {outcome:?}");
    let line = "gate: remote outputs not brought back: the manifest could not be built: ";
    assert!(outcome.stderr.contains(line) && outcome.stderr.contains("locked.txt"), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate-remote-outputs:"), "no globs were sent: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert!(pending_marker(&case).exists(), "the marker is still written: {outcome:?}");
}

/// The command prints `gate-remote-outputs: 0` on both its streams, then builds a file the PC lacks, so the box's own
/// line, naming its archive, comes last.
#[test]
fn a_forged_outputs_line_from_the_command_is_not_accepted() {
    let case = outputs_case("client-outputs-forged", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let forged = "gate-remote-outputs: 0";
    let command = [r".\tool.exe", "err", forged, "out", forged, "write", r"bin\new.txt", "built"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(forged), "the command's line is passed on: {outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 1 "), "the box's own line: {outcome:?}");
    let installed = fs::read_to_string(case.folder().join("bin").join("new.txt"));
    assert_eq!(installed.ok().as_deref(), Some("built"), "only the box's last line counts: {outcome:?}");
}

/// The stand-in writes `gate-remote-outputs: 0` on standard output once the box has said its own line on standard error.
#[test]
fn an_outputs_line_on_stdout_after_the_box_is_ignored() {
    let case = outputs_case("client-outputs-stdout", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok-then-out gate-remote-outputs: 0", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 1 "), "the box's own line: {outcome:?}");
    assert!(outcome.stdout.contains("gate-remote-outputs: 0"), "the stand-in's line: {outcome:?}");
    let installed = fs::read_to_string(case.folder().join("bin").join("new.txt"));
    assert_eq!(
        installed.ok().as_deref(),
        Some("built"),
        "a line on standard output never counts: {outcome:?}"
    );
}

/// A dotnet `--no-build` command brings no outputs back, though its repo has globs and it builds a file under them, and
/// leaves the pending marker for the outputs it left on the builder.
#[test]
fn a_no_build_command_sends_no_manifest() {
    let case = outputs_case("client-outputs-no-build", &BIN);
    copy_tool(&case, "dotnet.exe");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let command = [
        r".\dotnet.exe",
        "test",
        "out",
        "ran there",
        "write",
        r"bin\new.txt",
        "built",
        "--no-build",
    ];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: outputs: none brought back for a --no-build command"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("gate-remote-outputs:"), "no globs were sent: {outcome:?}");
    assert!(!outcome.stderr.contains("gate: remote outputs not brought back"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert!(pending_marker(&case).exists(), "the next run asks for every output: {outcome:?}");
}

/// The run call runs nothing and exits 0 (`exit 0`), so no box says an outputs line.
#[test]
fn a_pass_without_an_outputs_line_fails() {
    let case = outputs_case("client-outputs-no-line", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "exit 0"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let line = "gate: remote outputs not brought back: the box printed no 'gate-remote-outputs: <n> <archive>' line";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_eq!(outcome.stderr.matches("gate: remote outputs not brought back").count(), 1, "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert!(pending_marker(&case).exists(), "the marker stays: {outcome:?}");
}

/// The run call's first attempt fails before the banner; the second is fed the manifest again, so the PC's identical
/// `bin/same.txt` stays out of the archive and only `bin/differs.txt` comes back.
#[test]
fn a_pre_banner_retry_feeds_the_manifest_again() {
    let case = outputs_case("client-outputs-retry", &BIN);
    write(&case, r"bin\same.txt", "same");
    write(&case, r"bin\differs.txt", "pc");
    let steps = ssh_steps(&case, "ssh", &["ok", "fail-banner", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\same.txt", "same", "write", r"bin\differs.txt", "box"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 1, "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 1 "), "one file comes back: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "run", "outputs"]);
}

/// The outputs archives the client fetched into the case's `.tmp` folder: files named for the case's work-folder key,
/// which its first run's queue line names, `<key>-<nonce>.tar.zst`.
fn fetched_archives(case: &Case) -> Vec<PathBuf> {
    let key = key_of(&events(case));
    let Ok(entries) = fs::read_dir(case.folder().join(".tmp")) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.expect("an entry of .tmp can be read").path())
        .filter(|path| {
            let name = path.file_name().expect("an entry has a name").to_string_lossy();
            name.starts_with(&format!("{key}-")) && name.ends_with(".tar.zst")
        })
        .collect()
}

/// The archive the box's own `gate-remote-outputs: <n> <archive>` line names in `stderr`.
fn named_archive(stderr: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .find(|line| line.starts_with("gate-remote-outputs: "))
        .expect("the box said its outputs line");
    line.rsplit(' ').next().expect("the line names its archive").to_owned()
}

/// The line of a fetch the watchdog stopped.
const FETCH_STOPPED_LINE: &str = "gate: remote outputs not brought back: the fetch was stopped by the watchdog";

/// What the case's `.tmp` folder holds of its outputs steps: the archives and the stage folders beside them, each named
/// for the case's work-folder key, `<key>-<nonce>` and `<key>-<nonce>.tar.zst`.
fn outputs_leftovers(case: &Case) -> Vec<PathBuf> {
    let key = key_of(&events(case));
    let Ok(entries) = fs::read_dir(case.folder().join(".tmp")) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.expect("an entry of .tmp can be read").path())
        .filter(|path| {
            let name = path.file_name().expect("an entry has a name").to_string_lossy();
            name.starts_with(&format!("{key}-"))
        })
        .collect()
}

/// The success line of an outputs step that installed one file, its figures any.
const ONE_INSTALLED: &str = r"(?m)^gate: outputs: 1 file\(s\), \d+\.\d MB \(\d+\.\d MB compressed\) in \d+\.\d s\r?$";

#[test]
fn a_passing_run_installs_its_fetched_outputs() {
    let case = outputs_case("client-outputs-fetch", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"]);
    let installed = fs::read_to_string(case.folder().join("bin").join("new.txt")).expect("the built file is installed");
    assert_eq!(installed, "built", "the box's content");
    let line = regex::Regex::new(ONE_INSTALLED).expect("the line's pattern");
    assert!(line.is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(!pending_marker(&case).exists(), "the outputs step completed: {outcome:?}");
    assert!(!outcome.stderr.contains("not brought back"), "{outcome:?}");
}

/// The builder streams more than the fetch cap the lowered outputs budget gives (`--test-remote-outputs-budget-bytes
/// 1 MiB`, so 1 MiB + 1 MiB/64 + 1 MiB of slack, said as `2.0 MiB`): a cut, but the run keeps its passing status, the
/// partial archive is removed and nothing is installed.
#[test]
fn an_outputs_call_past_the_fetch_cap_keeps_the_pass_and_brings_nothing_back() {
    let case = outputs_case("client-outputs-fetch-cap", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "garbage 3000000"]);
    let tuned = Tuned {
        extra: &["--test-remote-outputs-budget-bytes", "1048576"],
        ..Tuned::default()
    };
    let command = [r".\tool.exe", "write", r"bin\new.txt", "built"];

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &command));

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    let line = "gate: remote outputs not brought back: the archive passed 2.0 MiB";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"], "{outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and no stage are left");
    assert!(!case.folder().join("bin").join("new.txt").exists(), "nothing is installed");
    assert!(pending_marker(&case).exists(), "the next run asks for every output: {outcome:?}");
    assert_eq!(cap_record_text(&case).as_deref(), Some(r#"{"v":1,"hits":1}"#), "the hit is counted");
}

/// The seam that lowers the outputs budget to 1 MiB, so a `garbage 3000000` outputs call passes the fetch cap.
const LOW_BUDGET: [&str; 2] = ["--test-remote-outputs-budget-bytes", "1048576"];
/// The outputs call step that streams past the fetch cap [`LOW_BUDGET`] gives.
const PAST_THE_CAP: &str = "garbage 3000000";
/// The line of an outputs call cut for the archive it passed, at the cap [`LOW_BUDGET`] gives.
const CAPPED_LINE: &str = "gate: remote outputs not brought back: the archive passed 2.0 MiB";
/// A passing command that writes one output file.
const WRITE_BUILT: [&str; 4] = [r".\tool.exe", "write", r"bin\new.txt", "built"];

/// The line of the `hits`-th fetch in a row past the cap, which fails its run.
fn capped_again(hits: u64) -> String {
    format!("gate: the builder's outputs passed the cap again, {hits} runs in a row; this run fails")
}

/// The case's outputs cap record, as text; none when there is none.
fn cap_record_text(case: &Case) -> Option<String> {
    let record = case.root().join("remote").join(format!("{}.outputs-capped", key_of(&events(case))));
    let read = fs::read_to_string(&record);
    if matches!(&read, Err(error) if error.kind() == ErrorKind::NotFound) {
        return None;
    }
    Some(read.expect("the record, when there, can be read"))
}

/// One routed run of `command` under [`LOW_BUDGET`], logging to `log`, its calls taking the steps of `steps` after
/// those the case's earlier runs took.
fn low_budget_run(case: &Case, steps: &Path, log: &str, command: &[&str]) -> Outcome {
    low_budget_run_with(case, steps, log, &[], command)
}

/// [`low_budget_run`] with the seams `more` added.
fn low_budget_run_with(case: &Case, steps: &Path, log: &str, more: &[&str], command: &[&str]) -> Outcome {
    let extra: Vec<&str> = LOW_BUDGET.iter().chain(more).copied().collect();
    let tuned = Tuned {
        extra: &extra,
        ..Tuned::default()
    };
    finish(client_gate_tuned(case, log, steps, &tuned, command))
}

/// A run between two fetches past the cap: its calls take `steps`, under the seams `more`, for `command`.
struct Between<'b> {
    steps: &'b [&'b str],
    more: &'b [&'b str],
    command: &'b [&'b str],
}

/// A fetch past the cap that keeps its pass, then the run `between` once `prepare` has readied the case, then another
/// fetch past the cap of an output that differs from any the PC holds: the run between, the case's outputs cap record
/// after it, and the last run.
fn between_two_hits(case: &Case, prepare: impl FnOnce(&Case), between: &Between<'_>) -> (Outcome, Option<String>, Outcome) {
    let hit = ["ok", "ok", PAST_THE_CAP];
    let all: Vec<&str> = hit.iter().chain(between.steps).chain(&hit).copied().collect();
    let steps = ssh_steps(case, "ssh", &all);
    let first = low_budget_run(case, &steps, "first.log", &WRITE_BUILT);
    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(cap_record_text(case).as_deref(), Some(r#"{"v":1,"hits":1}"#), "{first:?}");
    prepare(case);
    let middle = low_budget_run_with(case, &steps, "second.log", between.more, between.command);
    let after = cap_record_text(case);
    let last = low_budget_run(case, &steps, "third.log", &[r".\tool.exe", "write", r"bin\new.txt", "rebuilt"]);
    assert!(last.stderr.contains(CAPPED_LINE), "the last run's fetch passed the cap: {last:?}");
    (middle, after, last)
}

/// The PC already holds the file the first run built on the box, and the run between builds nothing, so the box packs
/// nothing.
#[test]
fn a_box_that_packs_nothing_between_two_fetches_past_the_cap_clears_the_count() {
    let case = outputs_case("client-outputs-cap-then-nothing", &BIN);
    let between = Between {
        steps: &["ok", "ok"],
        more: &[],
        command: &[r".\tool.exe", "out", "ran there"],
    };

    let (middle, after, last) = between_two_hits(&case, |case| write(case, r"bin\new.txt", "built"), &between);

    assert_eq!(middle.code, Some(0), "{middle:?}");
    assert!(middle.stderr.contains("gate-remote-outputs: 0"), "the box packed nothing: {middle:?}");
    assert_eq!(after, None, "a box that packed nothing clears the count: {middle:?}");
    assert_eq!(last.code, Some(0), "a hit after it is the first in a row: {last:?}");
}

#[test]
fn a_ctrl_c_during_a_fetch_between_two_fetches_past_the_cap_leaves_the_count() {
    let case = outputs_case("client-outputs-cap-then-ctrl-c", &BIN);
    let hold = format!("bytes-hold 100 {}", never(&case));
    let between = Between {
        steps: &["ok", "ok", &hold],
        more: &["--test-remote-interrupt-at", "outputs"],
        command: &WRITE_BUILT,
    };

    let (middle, after, last) = between_two_hits(&case, |_| {}, &between);

    assert_eq!(middle.code, Some(CONTROL_C_EXIT), "{middle:?}");
    assert_eq!(
        after.as_deref(),
        Some(r#"{"v":1,"hits":1}"#),
        "a Ctrl+C during the fetch leaves the count: {middle:?}"
    );
    assert_eq!(last.code, Some(1), "{last:?}");
    assert!(last.stderr.contains(&capped_again(2)), "{last:?}");
}

#[test]
fn a_fetch_the_watchdog_stops_between_two_fetches_past_the_cap_clears_the_count() {
    let case = outputs_case("client-outputs-cap-then-stalled", &BIN);
    let hold = format!("bytes-hold 100 {}", never(&case));
    let between = Between {
        steps: &["ok", "ok", &hold],
        more: &["--test-remote-watch-call", "outputs", "--test-stall-ms", WATCHED_MS],
        command: &WRITE_BUILT,
    };

    let (middle, after, last) = between_two_hits(&case, |_| {}, &between);

    assert_eq!(middle.code, Some(124), "{middle:?}");
    assert!(middle.stderr.contains(FETCH_STOPPED_LINE), "{middle:?}");
    assert_eq!(after, None, "a fetch the watchdog stopped clears the count: {middle:?}");
    assert_eq!(last.code, Some(0), "a hit after it is the first in a row: {last:?}");
}

/// The fetch between ends whole, under the cap; the Ctrl+C comes during its install.
#[test]
fn a_ctrl_c_during_an_install_between_two_fetches_past_the_cap_clears_the_count() {
    let case = outputs_case("client-outputs-cap-then-install-ctrl-c", &BIN);
    let between = Between {
        steps: &["ok", "ok", "ok"],
        more: &["--test-remote-interrupt-at", "install"],
        command: &[r".\tool.exe", "write", r"bin\a.txt", "a", "write", r"bin\b.txt", "b"],
    };

    let (middle, after, last) = between_two_hits(&case, |_| {}, &between);

    assert_eq!(middle.code, Some(CONTROL_C_EXIT), "{middle:?}");
    assert!(middle.stderr.contains("files were installed before the interrupt"), "{middle:?}");
    assert_eq!(after, None, "a fetch that ended whole clears the count: {middle:?}");
    assert_eq!(last.code, Some(0), "a hit after it is the first in a row: {last:?}");
}

/// The second fetch in a row past the cap fails the run, saying so after the cut's own line, and the record is kept, so
/// the third fails too.
#[test]
fn a_second_outputs_call_past_the_fetch_cap_in_a_row_fails_the_run_and_so_does_a_third() {
    let case = outputs_case("client-outputs-cap-twice", &BIN);
    let steps = ssh_steps(
        &case,
        "ssh",
        &["ok", "ok", PAST_THE_CAP, "ok", "ok", PAST_THE_CAP, "ok", "ok", PAST_THE_CAP],
    );

    let first = low_budget_run(&case, &steps, "first.log", &WRITE_BUILT);
    let second = low_budget_run(&case, &steps, "second.log", &WRITE_BUILT);
    let after_second = cap_record_text(&case);
    let third = low_budget_run(&case, &steps, "third.log", &WRITE_BUILT);

    assert_eq!(first.code, Some(0), "the first hit keeps the pass: {first:?}");
    assert!(!first.stderr.contains("passed the cap again"), "{first:?}");
    assert_eq!(second.code, Some(1), "{second:?}");
    let cut = second.stderr.find(CAPPED_LINE).expect("the cut's line");
    let again = second.stderr.find(&capped_again(2)).expect("the line saying the cap was passed again");
    assert!(cut < again, "the cut's line comes first: {second:?}");
    assert_eq!(after_second.as_deref(), Some(r#"{"v":1,"hits":2}"#), "{second:?}");
    assert_eq!(third.code, Some(1), "{third:?}");
    assert!(third.stderr.contains(&capped_again(3)), "{third:?}");
    assert_eq!(
        outputs_leftovers(&case),
        Vec::<PathBuf>::new(),
        "the archive and no stage are left: {third:?}"
    );
    assert!(!case.folder().join("bin").join("new.txt").exists(), "nothing is installed: {third:?}");
    assert!(pending_marker(&case).exists(), "the next run asks for every output: {third:?}");
    assert_eq!(cap_record_text(&case).as_deref(), Some(r#"{"v":1,"hits":3}"#), "{third:?}");
}

#[test]
fn outputs_brought_back_between_two_fetches_past_the_cap_clear_the_count() {
    let case = outputs_case("client-outputs-cap-then-back", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", PAST_THE_CAP, "ok", "ok", "ok", "ok", "ok", PAST_THE_CAP]);

    let first = low_budget_run(&case, &steps, "first.log", &WRITE_BUILT);
    let back = low_budget_run(&case, &steps, "second.log", &WRITE_BUILT);
    let after_back = cap_record_text(&case);
    // Another content, so the box has an output that differs from the one brought back to pack.
    let third = low_budget_run(&case, &steps, "third.log", &[r".\tool.exe", "write", r"bin\new.txt", "rebuilt"]);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(back.code, Some(0), "{back:?}");
    assert_eq!(worktree_text(&case, r"bin\new.txt"), "built", "the outputs came back");
    assert_eq!(after_back, None, "outputs brought back clear the count");
    assert_eq!(third.code, Some(0), "a hit after them is the first in a row: {third:?}");
    assert!(third.stderr.contains(CAPPED_LINE), "{third:?}");
    assert_eq!(cap_record_text(&case).as_deref(), Some(r#"{"v":1,"hits":1}"#));
}

#[test]
fn a_fetch_failing_another_way_between_two_fetches_past_the_cap_clears_the_count() {
    let case = outputs_case("client-outputs-cap-then-failed-fetch", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", PAST_THE_CAP, "ok", "ok", "exit 3", "ok", "ok", PAST_THE_CAP]);

    let first = low_budget_run(&case, &steps, "first.log", &WRITE_BUILT);
    let failed = low_budget_run(&case, &steps, "second.log", &WRITE_BUILT);
    let after_failed = cap_record_text(&case);
    let third = low_budget_run(&case, &steps, "third.log", &WRITE_BUILT);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(failed.code, Some(1), "{failed:?}");
    assert!(
        failed
            .stderr
            .contains("gate: remote outputs not brought back: the outputs call failed (status 3)"),
        "{failed:?}"
    );
    assert_eq!(after_failed, None, "a fetch that failed another way clears the count");
    assert_eq!(third.code, Some(0), "a hit after it is the first in a row: {third:?}");
    assert_eq!(cap_record_text(&case).as_deref(), Some(r#"{"v":1,"hits":1}"#));
}

/// A failing run makes no outputs call, so it leaves the count: the hits on either side of it are in a row.
#[test]
fn a_failing_run_between_two_fetches_past_the_cap_leaves_the_count() {
    let case = outputs_case("client-outputs-cap-then-failing-run", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", PAST_THE_CAP, "ok", "ok", "ok", "ok", PAST_THE_CAP]);
    let failing = [r".\tool.exe", "write", r"bin\new.txt", "built", "exit", "3"];

    let first = low_budget_run(&case, &steps, "first.log", &WRITE_BUILT);
    let failed = low_budget_run(&case, &steps, "second.log", &failing);
    let after_failed = cap_record_text(&case);
    let third = low_budget_run(&case, &steps, "third.log", &WRITE_BUILT);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(failed.code, Some(3), "{failed:?}");
    assert_eq!(after_failed.as_deref(), Some(r#"{"v":1,"hits":1}"#), "the failing run leaves the count");
    assert_eq!(third.code, Some(1), "{third:?}");
    assert!(third.stderr.contains(&capped_again(2)), "{third:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "run", "outputs", "preflight", "run", "preflight", "run", "outputs"]
    );
}

#[test]
fn a_failing_run_makes_no_outputs_call() {
    let case = outputs_case("client-outputs-failing", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\built.txt", "box", "exit", "3"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new());
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

/// The outputs call sends a few bytes, then nothing: the watchdog of `--test-remote-watch-call outputs` stops it.
#[test]
fn a_fetch_the_watchdog_stops_keeps_124_and_says_so() {
    let case = outputs_case("client-outputs-fetch-stalled", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("bytes-hold 100 {}", never(&case))]);
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "outputs", "--test-stall-ms", WATCHED_MS],
        ..Tuned::default()
    };
    let command = [r".\tool.exe", "write", r"bin\new.txt", "built"];

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &command));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let stalled = outcome.stderr.find(STALLED_LINE).expect("the STALLED line");
    let stopped = outcome.stderr.find(FETCH_STOPPED_LINE).expect("the outputs line");
    assert!(stalled < stopped, "the kill line comes first: {outcome:?}");
    let made = calls(&steps);
    assert!(made_but_maybe(&made, &["preflight", "run", "outputs"], "outputs"), "no stop: {made:?}");
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new(), "the partial archive is removed");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

#[test]
fn ctrl_c_during_the_fetch_ends_as_an_interrupt() {
    let case = outputs_case("client-outputs-fetch-ctrl-c", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("bytes-hold 100 {}", never(&case))]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "outputs"],
        ..Tuned::default()
    };
    let command = [r".\tool.exe", "write", r"bin\new.txt", "built"];

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &command));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    // The interrupt comes once the outputs call wrote its first byte, by when the stand-in has logged its call.
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"], "no stop: the command had ended");
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new(), "the partial archive is removed");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

#[test]
fn a_fetch_dropped_before_any_byte_is_retried_once() {
    let case = outputs_case("client-outputs-fetch-banner", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "fail-banner", "fail-banner"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 1, "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs", "outputs"]);
    let line = "gate: remote outputs not brought back: Connection timed out during banner exchange";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new(), "nothing is left of either attempt");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

/// The stand-in passes the archive's first 8 bytes, then says ssh's pre-banner line and exits 255: only the bytes it
/// sent tell the drop from one before the banner.
#[test]
fn a_fetch_dropped_after_bytes_is_not_retried() {
    let case = outputs_case("client-outputs-fetch-truncated", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "truncate 8", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    assert!(!outcome.stderr.contains(RETRY_LINE), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"]);
    let line = "gate: remote outputs not brought back: Connection timed out during banner exchange";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new(), "the partial archive is removed");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

/// The outputs call sends a few bytes, then nothing, past the ceiling `--test-remote-watch-call outputs` gives it alone.
#[test]
fn a_fetch_past_its_ceiling_keeps_124_and_says_so() {
    let case = outputs_case("client-outputs-fetch-ceiling", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("bytes-hold 100 {}", never(&case))]);
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "outputs", "--test-remote-timeout-ms", WATCHED_MS],
        ..Tuned::default()
    };
    let command = [r".\tool.exe", "write", r"bin\new.txt", "built"];

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &command));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let timed_out = outcome.stderr.find("gate: TIMED OUT after 1 s").expect("the TIMED OUT line");
    let stopped = outcome.stderr.find(FETCH_STOPPED_LINE).expect("the outputs line");
    assert!(timed_out < stopped, "the kill line comes first: {outcome:?}");
    let made = calls(&steps);
    assert!(made_but_maybe(&made, &["preflight", "run", "outputs"], "outputs"), "no stop: {made:?}");
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new(), "the partial archive is removed");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

/// The outputs call writes a standard error line past 64 KiB: a cut, but not the watchdog's.
#[test]
fn an_outputs_call_with_a_line_past_the_cap_fails_a_pass() {
    let case = outputs_case("client-outputs-fetch-long-line", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "flood 70000"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let line = "gate: remote outputs not brought back: the outputs call sent a line longer than 64 KiB";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("TIMED OUT") && !outcome.stderr.contains("STALLED"),
        "{outcome:?}"
    );
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new(), "the empty archive file is removed");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

/// What the fetch wrote, ending 0, is no archive at all: the install cannot open it.
#[test]
fn a_garbage_fetch_cannot_be_opened_and_fails_a_pass() {
    let case = outputs_case("client-outputs-fetch-garbage", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "garbage 64"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let name = named_archive(&outcome.stderr);
    let line = format!("gate: remote outputs not brought back: the outputs archive {name} cannot be opened: ");
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(!case.folder().join("bin").join("new.txt").exists(), "nothing is installed");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

/// The builder's own archive of the case's work folder: the one file ending `.zst` in the box's outputs folder, beside
/// its work folders.
fn box_output_archive(case: &Case) -> PathBuf {
    let folder = case.folder().join("builder").join("outputs");
    let archives: Vec<PathBuf> = fs::read_dir(&folder)
        .expect("the box's outputs folder can be listed")
        .map(|entry| entry.expect("an entry can be read").path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "zst"))
        .collect();
    assert_eq!(archives.len(), 1, "the box left one archive of its own: {archives:?}");
    archives.into_iter().next().expect("the archive is there")
}

/// A `.tar.zst` outputs archive holding one member `path` of `text` and the `.gate-outputs.json` list agreeing with it,
/// as a broken or hostile builder would send.
fn crafted_archive(path: &str, text: &str) -> Vec<u8> {
    let sha256 = sha256_hex(text);
    let list = format!(r#"[{{"path":"{path}","size":{},"sha256":"{sha256}"}}]"#, text.len());
    let mut tar = tar::Builder::new(Vec::new());
    for (name, data) in [(path.as_bytes(), text.as_bytes()), (".gate-outputs.json".as_bytes(), list.as_bytes())] {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().expect("a GNU header").name[..name.len()].copy_from_slice(name);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, data).expect("the member can be appended");
    }
    zstd::encode_all(&tar.into_inner().expect("the tar ends")[..], 3).expect("the tar compresses")
}

/// A hostile builder's archive carries a root `.lfsconfig`, which a sync-back never writes: the install refuses it, so
/// nothing of it reaches the worktree, and the run becomes 1.
#[test]
fn an_archive_member_that_is_an_lfsconfig_is_refused_by_the_install() {
    let case = outputs_case("client-outputs-lfsconfig", &["**"]);
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\a.txt", "built"]));
    wait_until("the outputs call", || calls(&steps).len() == 3);
    // The box's own archive is swapped for one carrying the `.lfsconfig`, before the call reads it.
    fs::write(box_output_archive(&case), crafted_archive(".lfsconfig", "lfs")).expect("the archive can be replaced");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let line = "gate: remote outputs not brought back: the archive member .lfsconfig is not under the output globs";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(!case.folder().join(".lfsconfig").exists(), "nothing of it is installed: {outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

/// A routed run, the gate given `extra` before the command, whose PC's `bin/kept.txt` (`pc`) is committed though `bin/`
/// is ignored, and whose box's archive is swapped, before the outputs call reads it, for one carrying `bin/kept.txt` as
/// `box`; its case and outcome.
fn tracked_member_run(name: &str, extra: &[&str]) -> (Case, Outcome) {
    let case = outputs_case(name, &BIN);
    write(&case, r"bin\kept.txt", "pc");
    git(&case, &["add", "--force", "bin/kept.txt"]);
    git(&case, &["commit", "--quiet", "-m", "check in an output"]);
    let outcome = swapped_archive_run(&case, extra, "bin/kept.txt");
    (case, outcome)
}

/// A routed run of `case`, the gate given `extra` before the command, whose box's archive is swapped, before the
/// outputs call reads it, for one carrying `member` as `box`.
fn swapped_archive_run(case: &Case, extra: &[&str], member: &str) -> Outcome {
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(case, "ssh", &["ok", "ok", &format!("hold {}", shown(go.path()))]);
    let tuned = Tuned { extra, ..Tuned::default() };
    let run = start(client_gate_tuned(
        case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "write", r"bin\new.txt", "built"],
    ));
    wait_until("the outputs call", || calls(&steps).len() == 3);
    fs::write(box_output_archive(case), crafted_archive(member, "box")).expect("the archive can be replaced");
    fs::write(go.path(), "").expect("the go file can be made");
    run.finish()
}

/// The PC's index holds a submodule `lib`, never initialised (a gitlink and an empty folder), whose files no index the
/// install reads lists: a member under it is not installed.
#[test]
fn an_archive_member_under_a_submodule_is_not_installed() {
    let case = outputs_case("client-outputs-submodule", &BIN);
    fs::create_dir_all(case.folder().join("lib")).expect("the submodule's folder can be made");
    let gitlink = format!("160000,{},lib", "1".repeat(40));
    git(&case, &["update-index", "--add", "--cacheinfo", &gitlink]);
    git(&case, &["commit", "--quiet", "-m", "add a submodule"]);

    let outcome = swapped_archive_run(&case, &[], "lib/bin/a.txt");

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    let skipped = "gate: remote outputs not fully brought back: lib/bin/a.txt is tracked by git";
    assert!(outcome.stderr.contains(skipped), "{outcome:?}");
    assert!(!case.folder().join("lib").join("bin").exists(), "nothing is installed: {outcome:?}");
    assert!(pending_marker(&case).exists(), "the marker stays: {outcome:?}");
}

/// A builder's archive carries a file the worktree's git index holds: the install skips it and says so, the pass
/// stands, and the marker stays.
#[test]
fn an_archive_member_git_tracks_is_not_installed() {
    let (case, outcome) = tracked_member_run("client-outputs-tracked", &[]);

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    let skipped = "gate: remote outputs not fully brought back: bin/kept.txt is tracked by git";
    assert!(outcome.stderr.contains(skipped), "{outcome:?}");
    assert!(!outcome.stderr.contains("not brought back:"), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: outputs: 0 file(s)"), "nothing is counted: {outcome:?}");
    assert_eq!(worktree_text(&case, r"bin\kept.txt"), "pc", "the tracked file is not overwritten");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(pending_marker(&case).exists(), "the marker stays: {outcome:?}");
}

/// `--bring-tracked BIN/**` matches the tracked file's folder, though its letter case differs: the file is installed as
/// before, and the marker cleared.
#[test]
fn a_tracked_member_a_bring_tracked_glob_matches_is_installed() {
    let (case, outcome) = tracked_member_run("client-outputs-bring-tracked", &["--bring-tracked", "BIN/**"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("brought back"), "{outcome:?}");
    let line = regex::Regex::new(ONE_INSTALLED).expect("the line's pattern");
    assert!(line.is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(worktree_text(&case, r"bin\kept.txt"), "box", "the tracked file is installed");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(!pending_marker(&case).exists(), "the marker is cleared: {outcome:?}");
}

/// The text of the case's worktree file `name`, which must be there.
fn worktree_text(case: &Case, name: &str) -> String {
    fs::read_to_string(case.folder().join(name)).expect("the worktree file is there")
}

/// Asserts an install that refused, `outcome`, saying `reason`: a passing run becomes 1, the PC's `bin/both.txt` keeps
/// `kept`, `bin/new.txt`, which the archive held after it, is not moved, the archive and the stage are removed, and the
/// marker stays.
fn assert_refused_install(case: &Case, outcome: &Outcome, reason: &str, kept: &str) {
    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let line = format!("gate: remote outputs not brought back: {reason}");
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert_eq!(worktree_text(case, r"bin\both.txt"), kept);
    assert!(!case.folder().join("bin").join("new.txt").exists(), "nothing is moved: {outcome:?}");
    assert_eq!(outputs_leftovers(case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(pending_marker(case).exists(), "{outcome:?}");
}

/// The command on the builder writes `bin/both.txt`, which the PC holds too, and `bin/new.txt`.
const BOTH_AND_NEW: [&str; 7] = [r".\tool.exe", "write", r"bin\both.txt", "box", "write", r"bin\new.txt", "built"];

/// The PC's `bin/both.txt` is edited while the run call is held, after the manifest recorded it.
#[test]
fn a_pc_output_edited_during_the_run_is_not_overwritten() {
    let case = outputs_case("client-outputs-install-changed", &BIN);
    write(&case, r"bin\both.txt", "pc");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path())), "ok"]);
    let run = start(client_gate(&case, "out.log", &steps, &BOTH_AND_NEW));
    wait_until("the run call", || calls(&steps).len() == 2);
    write(&case, r"bin\both.txt", "edited during the run");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_refused_install(&case, &outcome, "bin/both.txt changed during the run", "edited during the run");
}

/// The PC's `bin/both.txt` is held open, sharing nothing, from the run call to the run's end: it alone is skipped, the
/// pass stands, and the marker stays for the next run to ask for every output.
#[test]
fn a_pc_output_held_open_is_skipped_and_the_rest_installed() {
    let case = outputs_case("client-outputs-install-in-use", &BIN);
    write(&case, r"bin\both.txt", "pc");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path())), "ok"]);
    let run = start(client_gate(&case, "out.log", &steps, &BOTH_AND_NEW));
    wait_until("the run call", || calls(&steps).len() == 2);
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(case.folder().join("bin").join("both.txt"))
        .expect("the file can be held");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();
    drop(held);

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    let skipped = "gate: remote outputs not fully brought back: bin/both.txt is in use or read-only";
    assert!(outcome.stderr.contains(skipped), "{outcome:?}");
    assert!(!outcome.stderr.contains("not brought back:"), "{outcome:?}");
    assert_eq!(worktree_text(&case, r"bin\both.txt"), "pc", "the held file is not overwritten");
    assert_eq!(worktree_text(&case, r"bin\new.txt"), "built", "the other file is installed");
    let line = regex::Regex::new(ONE_INSTALLED).expect("the line's pattern");
    assert!(line.is_match(&outcome.stderr), "only the installed file is counted: {outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(pending_marker(&case).exists(), "the marker stays: {outcome:?}");
}

/// The nothing-back record a routed `-c Release` dotnet build on [`HOST`] leaves, planted by [`record_case`].
const RELEASE_RECORD: &str = r#"{"v":1,"host":"builder","configuration":"Release"}"#;
/// The settings of a routed build [`RELEASE_RECORD`] stands for.
const RELEASE: [&str; 2] = ["-c", "Release"];
/// The steps of a routed build that writes `bin/new.txt`, which the PC lacks.
const WRITE_NEW: [&str; 3] = ["write", r"bin\new.txt", "built"];
/// The steps of a routed build that writes `bin/same.txt` as the PC holds it ([`with_same`]).
const WRITE_SAME: [&str; 3] = ["write", r"bin\same.txt", "same"];

/// An [`outputs_case`] with the output globs [`BIN`] holding fake-tool as `pwsh.exe` too, and its `.gitignore`
/// committed so a routed build has a `HEAD` to pin.
fn build_case(name: &str) -> Case {
    let case = outputs_case(name, &BIN);
    copy_tool(&case, "pwsh.exe");
    git(&case, &["add", ".gitignore"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    case
}

/// A [`build_case`] with `record` planted as its worktree's nothing-back record.
fn planted_case(name: &str, record: &str) -> Case {
    let case = build_case(name);
    let key = work_folder_key(&git(&case, &["rev-parse", "--show-toplevel"]));
    let path = case.root().join("remote").join(format!("{key}.nothing-back"));
    fs::create_dir_all(path.parent().expect("the record has a folder")).expect("the record's folder can be made");
    fs::write(path, record).expect("the record can be planted");
    case
}

/// A [`planted_case`] whose record is [`RELEASE_RECORD`].
fn record_case(name: &str) -> Case {
    planted_case(name, RELEASE_RECORD)
}

/// [`record_case`] whose PC holds `bin/same.txt` as [`WRITE_SAME`] writes it, so its manifest is not empty.
fn with_same(name: &str) -> Case {
    let case = record_case(name);
    write(&case, r"bin\same.txt", "same");
    case
}

/// The words of a routed dotnet build that fake-tool, as `pwsh.exe`, runs: a wrapper that takes `steps`, then says the
/// words after them, and whose words after `--` are the dotnet build the gate reads: `dotnet build` and `settings`.
fn wrapped_build<'a>(steps: &[&'a str], settings: &[&'a str]) -> Vec<&'a str> {
    let mut dotnet = vec!["build"];
    dotnet.extend_from_slice(settings);
    wrapped_dotnet(steps, &dotnet)
}

/// The words of a routed dotnet command that fake-tool, as `pwsh.exe`, runs: a wrapper that takes `steps`, then says
/// the words after them, and whose words after `--` are the dotnet command the gate reads: `dotnet` and `dotnet_words`.
fn wrapped_dotnet<'a>(steps: &[&'a str], dotnet_words: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec![r".\pwsh.exe"];
    words.extend_from_slice(steps);
    words.extend_from_slice(&["argv", "--", "dotnet"]);
    words.extend_from_slice(dotnet_words);
    words
}

/// A dotnet test of the `-c Release` build that asks for no build: the builder runs it with `--no-build` dropped and
/// brings no outputs back.
const NO_BUILD_RELEASE_TEST: [&str; 4] = ["test", "--no-build", "-c", "Release"];

/// A routed run of `case`'s `command` whose ssh calls are the stand-in's `steps`, passing; its outcome.
fn passing_build(case: &Case, steps: &[&str], command: &[&str]) -> Outcome {
    let steps = ssh_steps(case, "ssh", steps);
    let outcome = finish(client_gate(case, "out.log", &steps, command));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    outcome
}

#[test]
fn a_routed_build_that_installs_all_its_outputs_clears_the_nothing_back_record() {
    let case = record_case("client-record-installed");

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &wrapped_build(&WRITE_NEW, &RELEASE));

    assert_eq!(worktree_text(&case, r"bin\new.txt"), "built", "{outcome:?}");
    assert_eq!(nothing_back(&case), None, "{outcome:?}");
}

#[test]
fn a_routed_build_that_packs_nothing_clears_the_nothing_back_record() {
    let case = with_same("client-record-packs-nothing");

    let outcome = passing_build(&case, &["ok", "ok"], &wrapped_build(&WRITE_SAME, &RELEASE));

    assert!(outcome.stderr.contains("gate-remote-outputs: 0"), "{outcome:?}");
    assert_eq!(nothing_back(&case), None, "{outcome:?}");
}

/// The PC holds no output, so the box packing nothing does not say the PC is up to date.
#[test]
fn a_routed_build_whose_pc_had_no_outputs_and_packs_nothing_leaves_the_nothing_back_record() {
    let case = record_case("client-record-no-outputs");

    let outcome = passing_build(&case, &["ok", "ok"], &wrapped_build(&["out", "ran there"], &RELEASE));

    assert!(outcome.stderr.contains("gate-remote-outputs: 0"), "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{outcome:?}");
}

/// The box packs nothing the PC lacks but skips `bin/m~10.txt`, a name this PC cannot take.
#[test]
fn a_routed_build_that_skips_a_name_leaves_the_nothing_back_record() {
    let case = with_same("client-record-names-skip");
    let steps = ["write", r"bin\same.txt", "same", "write", r"bin\m~10.txt", "x"];

    let outcome = passing_build(&case, &["ok", "ok"], &wrapped_build(&steps, &RELEASE));

    let skipped = "gate: remote outputs: skipped 1 file(s) with names this PC cannot take: bin/m~10.txt";
    assert!(outcome.stderr.contains(skipped), "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 0"), "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{outcome:?}");
}

/// The PC's `bin/both.txt` is held open from the run call to the run's end, so the install skips it.
#[test]
fn a_routed_build_with_an_output_in_use_leaves_the_nothing_back_record() {
    let case = record_case("client-record-in-use");
    write(&case, r"bin\both.txt", "pc");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path())), "ok"]);
    let command = wrapped_build(&BOTH_AND_NEW[1..], &RELEASE);
    let run = start(client_gate(&case, "out.log", &steps, &command));
    wait_until("the run call", || calls(&steps).len() == 2);
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(case.folder().join("bin").join("both.txt"))
        .expect("the file can be held");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();
    drop(held);

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    let skipped = "gate: remote outputs not fully brought back: bin/both.txt is in use or read-only";
    assert!(outcome.stderr.contains(skipped), "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{outcome:?}");
}

#[test]
fn a_routed_build_past_the_fetch_cap_leaves_the_nothing_back_record() {
    let case = record_case("client-record-fetch-cap");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", PAST_THE_CAP]);
    let tuned = Tuned {
        extra: &LOW_BUDGET,
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &wrapped_build(&WRITE_NEW, &RELEASE)));

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    assert!(outcome.stderr.contains(CAPPED_LINE), "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{outcome:?}");
}

/// A Debug build that brought everything back says nothing of the Release build the record stands for.
#[test]
fn a_routed_debug_build_that_brings_everything_back_leaves_a_release_record() {
    let case = record_case("client-record-debug");

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &wrapped_build(&WRITE_NEW, &["-c", "Debug"]));

    assert_eq!(worktree_text(&case, r"bin\new.txt"), "built", "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{outcome:?}");
}

/// A build of one project says nothing of the others the record may stand for. The record names a framework the build
/// does not set, so a record the run rewrote would differ from it.
#[test]
fn a_routed_build_of_one_project_that_brings_everything_back_leaves_the_record() {
    let planted = r#"{"v":1,"host":"builder","configuration":"Release","framework":"net8.0"}"#;
    let case = planted_case("client-record-one-project", planted);
    let settings = ["app.csproj", "-c", "Release"];

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &wrapped_build(&WRITE_NEW, &settings));

    assert_eq!(worktree_text(&case, r"bin\new.txt"), "built", "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(planted), "{outcome:?}");
}

/// The box installs `bin/new.txt` but skips `bin/m~10.txt`, a name this PC cannot take: the install is complete, but
/// the PC still lacks an output the build made.
#[test]
fn a_routed_build_whose_archive_installs_but_a_name_was_skipped_leaves_the_record() {
    let case = record_case("client-record-archive-names-skip");
    let steps = ["write", r"bin\new.txt", "built", "write", r"bin\m~10.txt", "x"];

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &wrapped_build(&steps, &RELEASE));

    let skipped = "gate: remote outputs: skipped 1 file(s) with names this PC cannot take: bin/m~10.txt";
    assert!(outcome.stderr.contains(skipped), "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 1 "), "{outcome:?}");
    assert_eq!(worktree_text(&case, r"bin\new.txt"), "built", "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{outcome:?}");
}

/// The builder builds `bin/new.txt` for a dotnet test whose `--no-build` it dropped, and brings no outputs back.
#[test]
fn a_routed_no_build_test_whose_outputs_are_withheld_writes_the_pending_marker_and_the_record() {
    let case = build_case("client-record-withheld");
    let command = wrapped_dotnet(&WRITE_NEW, &NO_BUILD_RELEASE_TEST);

    let outcome = passing_build(&case, &["ok", "ok"], &command);

    assert!(outcome.stderr.contains("gate: dropped --no-build"), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate-remote-outputs:"), "no outputs are asked for: {outcome:?}");
    assert!(!case.folder().join("bin").join("new.txt").exists(), "nothing comes back: {outcome:?}");
    assert!(pending_marker(&case).exists(), "the next run asks for every output: {outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{outcome:?}");
}

/// A record already stands for a Debug build: the builder built a Release one for the withheld run, but the record is
/// never rewritten.
#[test]
fn a_routed_no_build_test_leaves_a_record_that_stands_unchanged() {
    let debug = r#"{"v":1,"host":"builder","configuration":"Debug"}"#;
    let case = planted_case("client-record-withheld-stands", debug);

    let outcome = passing_build(&case, &["ok", "ok"], &wrapped_dotnet(&WRITE_NEW, &NO_BUILD_RELEASE_TEST));

    assert!(outcome.stderr.contains("gate: dropped --no-build"), "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(debug), "{outcome:?}");
}

/// A repo with no output globs: the builder still builds once `--no-build` is dropped, so the record is written, but
/// nothing is withheld and no pending marker is needed.
#[test]
fn a_routed_no_build_test_in_a_repo_without_output_globs_writes_the_record() {
    let case = build_case("client-record-no-globs");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: ["*tool*", "*dotnet*"] }, "outputs": { &repo: [] } });
    write_remote_json(&case, &routing.to_string());

    let outcome = passing_build(&case, &["ok", "ok"], &wrapped_dotnet(&WRITE_NEW, &NO_BUILD_RELEASE_TEST));

    assert!(outcome.stderr.contains("gate: dropped --no-build"), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("gate: outputs: none brought back"),
        "nothing is withheld: {outcome:?}"
    );
    assert!(!pending_marker(&case).exists(), "{outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{outcome:?}");
}

/// The withheld run rebuilds `bin/app.txt` in the builder's work folder only, leaving the PC's stale. The next
/// `-c Release` build has nothing to rebuild, so only a full manifest finds the PC's copy differs: without one the box
/// compares only the files the command changed, packs nothing for a PC that holds outputs, and the record would be
/// cleared over the stale file. The fetch passes the cap, so nothing comes back and the record stands.
#[test]
fn a_routed_build_after_a_withheld_run_sends_a_full_manifest_and_keeps_the_record_if_nothing_comes_back() {
    let case = build_case("client-record-withheld-then-build");
    write(&case, r"bin\app.txt", "stale");
    let withheld_steps = ["write", r"bin\app.txt", "rebuilt"];
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let withheld = finish(client_gate(
        &case,
        "first.log",
        &first,
        &wrapped_dotnet(&withheld_steps, &NO_BUILD_RELEASE_TEST),
    ));
    assert_eq!(withheld.code, Some(0), "{withheld:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{withheld:?}");
    let second = ssh_steps(&case, "second", &["ok", "ok", PAST_THE_CAP]);
    let tuned = Tuned {
        extra: &LOW_BUDGET,
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "second.log", &second, &tuned, &wrapped_build(&[], &RELEASE)));

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    assert!(
        outcome.stderr.contains("gate-remote-outputs: 1 "),
        "the box packs the output the PC holds stale: {outcome:?}"
    );
    assert!(outcome.stderr.contains(CAPPED_LINE), "{outcome:?}");
    assert_eq!(worktree_text(&case, r"bin\app.txt"), "stale", "nothing comes back: {outcome:?}");
    assert!(pending_marker(&case).exists(), "the next run asks for every output again: {outcome:?}");
    assert_eq!(nothing_back(&case).as_deref(), Some(RELEASE_RECORD), "{outcome:?}");
}

#[test]
fn ctrl_c_during_the_install_says_how_many_files_were_installed() {
    let case = outputs_case("client-outputs-install-ctrl-c", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "install"],
        ..Tuned::default()
    };
    let command = [r".\tool.exe", "write", r"bin\a.txt", "a", "write", r"bin\b.txt", "b"];

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &command));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    let line = "gate: remote outputs not brought back: 1 of 2 files were installed before the interrupt";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"], "no stop: the command had ended");
    assert_eq!(worktree_text(&case, r"bin\a.txt"), "a", "the file moved before the interrupt stays");
    assert!(!case.folder().join("bin").join("b.txt").exists(), "{outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

/// A first run names the work-folder key, so the second's command can write the builder's work folder, in another
/// case, into the test host's manifest it builds.
#[test]
fn an_installed_test_host_manifest_names_the_worktree() {
    let case = outputs_case("client-outputs-install-rewrite", &BIN);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let cleared = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(cleared.code, Some(0), "{cleared:?}");
    let builder = std::path::absolute(case.folder().join("builder")).expect("the builder root has a full path");
    let work = format!(r"{}\work\{}", builder.display(), key_of(&events(&case)));
    // After a `_`, the path is not one the router rewrites into the work folder: the builder sits in the worktree here.
    let recorded = format!(r"Root_{}\src\App", work.to_uppercase());
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\MvcTestingAppManifest.json", &recorded];

    let outcome = finish(client_gate(&case, "second.log", &second, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let root = std::path::absolute(case.folder()).expect("the case folder has a full path");
    let expected = format!(r"Root_{}\src\App", root.display());
    assert_eq!(worktree_text(&case, r"bin\MvcTestingAppManifest.json"), expected, "{outcome:?}");
}

/// A first run of `case`, which names the work-folder key, so a builder file can be put in its work folder after it.
fn first_run(case: &Case) {
    let steps = ssh_steps(case, "first", &["ok", "ok"]);
    let cleared = finish(client_gate(case, "first.log", &steps, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(cleared.code, Some(0), "{cleared:?}");
}

/// Writes `text` to the builder's work folder file `path`, its folders made, its last write [`HOUR`] ago: the install
/// keeps no hash of a file written inside the 2 s window of its moves.
fn install_from_the_box(case: &Case, path: &str, text: &str) {
    let work = case.folder().join("builder").join("work").join(key_of(&events(case)));
    let full = work.join(path.replace('/', "\\"));
    fs::create_dir_all(full.parent().expect("a file has a folder")).expect("the folder can be made");
    fs::write(&full, text).expect("the builder's file can be written");
    fs::File::options()
        .write(true)
        .open(&full)
        .and_then(|file| file.set_modified(SystemTime::now() - HOUR))
        .expect("the file's last write can be set");
}

/// A run that installs a file the builder wrote an hour ago keeps its hash at the stamp the installed file's own
/// handle reads: the next run then takes that hash instead of reading the file.
#[test]
fn an_installed_file_is_cached_at_its_installed_stamp() {
    let case = outputs_case("client-outputs-cached", &BIN);
    first_run(&case);
    install_from_the_box(&case, "bin/x.dll", "built");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let installed = case.folder().join("bin").join("x.dll");
    assert_eq!(
        fs::read_to_string(&installed).expect("the built file is installed"),
        "built",
        "{outcome:?}"
    );
    let held = hashes(&case)["files"]
        .get("bin/x.dll")
        .cloned()
        .expect("the install caches what it installed");
    let stamp = fs::metadata(&installed).expect("the installed file is there");
    assert_eq!(held["sha256"], json!(sha256_hex("built")), "the box's hash: {outcome:?}");
    assert_eq!(held["size"], json!(stamp.len()), "the installed file's size: {outcome:?}");
    assert_eq!(
        held["modified"],
        json!(stamp.last_write_time()),
        "the installed file's own stamp: {outcome:?}"
    );
}

/// The run after an install takes the hash the cache holds at the installed file's stamp, not the file's: a hash
/// planted in the cache changes what the box brings back, where reading the file again would bring nothing.
#[test]
fn a_run_after_an_install_does_not_rehash_what_it_installed() {
    let case = outputs_case("client-outputs-installed-hash", &BIN);
    first_run(&case);
    install_from_the_box(&case, "bin/x.dll", "built");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let installed = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(installed.code, Some(0), "{installed:?}");
    plant(&case, "bin/x.dll", &sha256_hex("sentinel"));
    let third = ssh_steps(&case, "third", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\x.dll", "built"];

    let outcome = finish(client_gate(&case, "third.log", &third, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate-remote-outputs: 1 "),
        "the manifest carried the cache's hash, not the file's: {outcome:?}"
    );
}

/// An install the Ctrl+C stops keeps no hash, not even of the settled file it moved before the interrupt.
#[test]
fn an_interrupted_install_caches_nothing() {
    let case = outputs_case("client-outputs-install-ctrl-c-cache", &BIN);
    first_run(&case);
    install_from_the_box(&case, "bin/a.txt", "a");
    install_from_the_box(&case, "bin/b.txt", "b");
    let steps = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "install"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "second.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there"],
    ));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_eq!(
        worktree_text(&case, r"bin\a.txt"),
        "a",
        "the file moved before the interrupt stays: {outcome:?}"
    );
    let cache = fs::read_to_string(hashes_file(&case)).map_or(Value::Null, |text| serde_json::from_str(&text).expect("the hash cache is JSON"));
    assert!(cache["files"].get("bin/a.txt").is_none(), "an interrupted install keeps no hash: {cache}");
}
