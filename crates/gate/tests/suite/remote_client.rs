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
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_router::{DROPPED_NO_BUILD_LINE, DROPPED_UNSENT_LINE, repo_name, work_folder_key};
use gate_wire::stamp::{Tag, is_process_line, unstamped};
use proc_table::{ProcessTable as _, ProcessTree, Win32ProcessTable};
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::durations::seed;
use crate::remote_inputs::inputs_case;
use crate::support::{
    ANSWERING, Case, FAKE_SSH, FAKE_TOOL, GATE, GATE_VARIABLE, GIT_REDIRECTS, Go, Junction, NetfxKey, Outcome, PREFLIGHT_SEAMS_VARIABLE, Running,
    STEPS_VARIABLE, allow_breakaway, assert_in_order, beside, build_version, calls, copy_tool, events, finish, git, git_init, heavy_words, log_texts,
    nothing_back, refuse_breakaway, release, run_words, shown, ssh_steps, start, wait_for_journal_line, wait_until, wait_until_closed,
    write_remote_json,
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
    let mut before = vec!["--test-remote-snapshot"];
    before.extend(GIT_HANG_GUARDS);
    before.extend(extra);
    heavy_words(log, &before, &[r".\tool.exe", "out", "hi"])
}

/// Writes `text` to the case folder's file `name`, making its folder.
pub(super) fn write(case: &Case, name: &str, text: &str) {
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
    crate::support::git_home_for(&mut command);
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
    let logged = log_texts(&case.folder().join(log));
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
    let mut holder_seams = vec!["--test-remote-client", steps.as_str()];
    holder_seams.extend(GIT_HANG_GUARDS);
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

/// What a client case's snapshot leaves out: the gate's state root, LOCALAPPDATA and logs, the builder root and the
/// stand-in's files, and `.tmp/`, which the client writes in. `tool.exe` is sent, so the builder runs it.
pub(super) const CLIENT_IGNORED: &str = "state/\nappdata/\n*.log\nbuilder/\nssh/\n.tmp/\n";
/// The start of every retry line of a builder that did not answer the ssh banner; its `n of 3` follows it.
const RETRY_LINE: &str = "gate: builder did not answer the ssh banner; trying again (";
const BUSY_LINE: &str = "gate: remote builder busy; running here";
const UNREACHABLE_LINE: &str = "gate: remote builder unreachable; running here";
/// The line of a run call whose connection was lost on a builder that did not restart.
const LOST_LINE: &str = "gate: lost the connection to builder";
/// The boot time a case's preflight says the builder booted at.
const BOOTED: &str = "2026-10-10T08:00:00.0000000Z";

/// A case folder made a checkout of its own holding fake-tool as `tool.exe`, which its snapshot sends, with a routing
/// file that sends its `*tool*` commands to the builder with no output globs and gives the remote pool two slots. The
/// suite enters a job that allows breakaway first, as the box's command needs.
pub(super) fn client_case(name: &str) -> Case {
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

/// Plants the case's record of the work-folder key's last complete outputs pull, naming `builder`: a run of the worktree
/// on that builder then sends only what changed, while a worktree with no record sends every output that differs.
fn plant_outputs_from(case: &Case, builder: &str) {
    let record = case.root().join("remote").join(format!("{}.outputs-from", key_of(&events(case))));
    fs::create_dir_all(record.parent().expect("the record has a folder")).expect("the remote folder can be made");
    fs::write(&record, builder).expect("the record can be written");
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

/// Each call the stand-in of the steps file `steps` was given, in order: its verb, then the words before the host,
/// one line a call; none before its first call.
fn call_words(steps: &Path) -> Vec<String> {
    fs::read_to_string(beside(steps, ".words")).map_or_else(|_| Vec::new(), |text| text.lines().map(str::to_owned).collect())
}

/// The gate of a routed run logging to `log` through the Rust client: the builder root `builder` in the case folder,
/// each ssh call made through the stand-in of the steps file `steps`, the retry pause 1 ms.
pub(super) fn client_gate(case: &Case, log: &str, steps: &Path, command: &[&str]) -> Command {
    client_gate_through(case, "local", log, steps, command)
}

/// [`client_gate`] through the local transport of the form `kind`: `local`, or `local-ssh`, which pushes and fetches
/// through the stand-in as git's own ssh, so each of git's calls to the builder is a call of the stand-in too.
fn client_gate_through(case: &Case, kind: &str, log: &str, steps: &Path, command: &[&str]) -> Command {
    let tuned = Tuned { kind, ..Tuned::default() };
    client_gate_tuned(case, log, steps, &tuned, command)
}

/// The `--stall` of a client run: a hang guard, so a call no seam watches, such as the preflight, is never cut for a
/// silence a busy machine gives it. The default 120 s is a bound a starved gate and its stand-in can cross before the
/// first call has said anything.
const CLIENT_STALL: &str = "600";

/// The git seams of a client run: hang guards on each git call and on its readers once git has exited, so a starved
/// gate is late, never failed.
pub(crate) const GIT_HANG_GUARDS: [&str; 4] = ["--test-git-limit-ms", "3600000", "--test-git-drain-ms", "600000"];

/// The bound of this PC's `dotnet --list-sdks` read, for a case that reads it: a hang guard. Given by the case or its
/// helper, never by [`client_gate_tuned`], since a case about the bound gives its own and the seam is refused given
/// twice.
const PC_SDKS_ANSWERING: [&str; 2] = ["--test-dotnet-limit-ms", "3600000"];

/// [`client_gate`] for a case whose tree is a .NET one with no SDK pin, so the run reads this PC's SDK list: with
/// [`PC_SDKS_ANSWERING`].
fn dotnet_gate(case: &Case, log: &str, steps: &Path, command: &[&str]) -> Command {
    let tuned = Tuned {
        extra: &PC_SDKS_ANSWERING,
        ..Tuned::default()
    };
    client_gate_tuned(case, log, steps, &tuned, command)
}

/// What a case changes in the words of a client run: the transport's form, the ceiling, the stall window, and more
/// options before the command.
pub(super) struct Tuned<'a> {
    pub(super) kind: &'a str,
    pub(super) timeout: &'a str,
    /// The run's `--stall`, [`CLIENT_STALL`] unless the case is about the window itself.
    pub(super) stall: &'a str,
    pub(super) extra: &'a [&'a str],
    /// The stand-in `--test-remote-ssh` names, [`FAKE_SSH`] when none.
    pub(super) ssh: Option<&'a str>,
    /// The builder's preflight seams, [`ANSWERING`] unless the case is about a probe's bound or the SDKs it lists.
    pub(super) preflight_seams: &'a str,
    /// The folders part after the transport's form, `alias=folder|alias=folder` giving each builder of a map its own
    /// folder; the case's `builder` folder for every builder when `None`.
    pub(super) folders: Option<&'a str>,
    /// The run's `--test-remote-retry-pause-ms`, `1` unless the case is about the time a retry pauses.
    pub(super) pause: &'a str,
}

impl Default for Tuned<'_> {
    fn default() -> Self {
        Tuned {
            kind: "local",
            timeout: "3600",
            stall: CLIENT_STALL,
            extra: &[],
            ssh: None,
            preflight_seams: ANSWERING,
            folders: None,
            pause: "1",
        }
    }
}

/// [`client_gate`] with the words `tuned` changes.
pub(super) fn client_gate_tuned(case: &Case, log: &str, steps: &Path, tuned: &Tuned<'_>, command: &[&str]) -> Command {
    let folders = tuned.folders.map_or_else(|| shown(&case.folder().join("builder")), str::to_owned);
    let transport = format!("{}:{folders}", tuned.kind);
    let ssh = tuned.ssh.unwrap_or(FAKE_SSH);
    let mut seams = vec![
        "--test-remote-transport",
        transport.as_str(),
        "--test-remote-ssh",
        ssh,
        "--test-remote-retry-pause-ms",
        tuned.pause,
        "--stall",
        tuned.stall,
    ];
    seams.extend(GIT_HANG_GUARDS);
    seams.extend(tuned.extra);
    let mut words = heavy_words(log, &seams, command);
    assert_eq!(words[3], "--timeout", "the ceiling's value follows it");
    words[4] = tuned.timeout;
    let mut gate = case.gate(&words);
    gate.env(STEPS_VARIABLE, steps)
        .env(GATE_VARIABLE, GATE)
        .env(PREFLIGHT_SEAMS_VARIABLE, tuned.preflight_seams)
        .env(crate::support::ROOT_WALK_STOP_VARIABLE, case.folder());
    gate
}

/// The journal lines of the gate `pid`.
fn own(case: &Case, pid: u32) -> Vec<Value> {
    events(case).into_iter().filter(|event| event["pid"] == pid).collect()
}

/// The reasons of the `fallback` lines among `events`.
pub(super) fn fallbacks(events: &[Value]) -> Vec<&str> {
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
    let text = log_texts(&case.folder().join(log));
    let line = text
        .lines()
        .find(|line| line.starts_with("gate: pushed snapshot "))
        .expect("the run pushed a snapshot");
    line["gate: pushed snapshot ".len()..].chars().take(8).collect()
}

/// The bare repository of the case's repo that the builder laid out under the case's `builder\<letter>` folder
/// ([`builder_folder`]).
fn bare_in(case: &Case, letter: &str) -> String {
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    shown(&builder_folder(case, letter).join(format!("{repo}.git")))
}

/// The commit `name` names in the bare repository of the builder of `letter` ([`bare_in`]), empty when it names none.
fn ref_in(case: &Case, letter: &str, name: &str) -> String {
    git_out(
        case,
        &[],
        &["--git-dir", &bare_in(case, letter), "for-each-ref", "--format=%(objectname)", name],
    )
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
    let log = log_texts(&case.folder().join("out.log"));
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

/// The words of each `gat` process line of `log` that starts with `start`, its text split at spaces.
fn process_words<'l>(log: &'l str, start: &str) -> Vec<Vec<&'l str>> {
    log.lines()
        .map(unstamped)
        .filter(|line| line.tag == Some(Tag::Gat) && is_process_line(line.text) && line.text.starts_with(start))
        .map(|line| line.text.split(' ').collect())
        .collect()
}

/// The box stamps its lines at the source and the PC writes them to its log as they came: the command's own as
/// `out` and `err`, which no PC stamp would tag, and the box's process lines, its root and the command's spawn naming
/// the box as its parent, then the command's exit.
#[test]
fn a_routed_run_logs_the_box_s_stamped_lines_and_its_process_lines_as_they_came() {
    let case = client_case("client-stamped");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(
        &case,
        "out.log",
        &steps,
        &[r".\tool.exe", "out", "ran there", "err", "said there"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    let read: Vec<_> = log.lines().map(unstamped).collect();
    assert!(read.iter().all(|line| line.stamp.is_some()), "every line is stamped: {log}");
    assert!(read.iter().any(|line| line.tag == Some(Tag::Out) && line.text == "ran there"), "{log}");
    assert!(read.iter().any(|line| line.tag == Some(Tag::Err) && line.text == "said there"), "{log}");
    let spawns = process_words(&log, "spawn pid=");
    let tool = spawns
        .iter()
        .find(|words| words.last() == Some(&"tool.exe"))
        .expect("the command's spawn is logged");
    let parent = tool[2].strip_prefix("ppid=").expect("a spawn names its parent");
    assert!(
        process_words(&log, "gate pid=").iter().any(|words| words[1] == format!("pid={parent}")),
        "the command's parent is the box, whose root line is logged: {log}"
    );
    assert!(
        process_words(&log, "exit pid=")
            .iter()
            .any(|words| words[1] == tool[1] && words[2] == "code=0"),
        "the command's exit is logged: {log}"
    );
}

/// What a routed run says on the console, the log's tail of a failed run among it, carries no stamp and no process
/// line.
#[test]
fn a_routed_run_s_console_shows_no_stamp_and_no_process_line() {
    let case = client_case("client-stamped-console");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(
        &case,
        "out.log",
        &steps,
        &[r".\tool.exe", "out", "ran there", "err", "said there", "exit", "3"],
    ));

    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    assert!(outcome.stdout.lines().any(|line| line == "ran there"), "{outcome:?}");
    assert!(outcome.stderr.lines().any(|line| line == "said there"), "{outcome:?}");
    for line in outcome.stdout.lines().chain(outcome.stderr.lines()) {
        assert_eq!(unstamped(line).stamp, None, "no stamp on the console: {line}");
        assert!(!is_process_line(line), "no process line on the console: {line}");
    }
}

/// A routed run shows its command's last line once: what the box streamed live is not said again as the verdict's tail,
/// even when `--tail` names enough lines to hold the command's own.
#[test]
fn a_routed_run_shows_its_command_s_output_once_with_a_tail() {
    let case = client_case("client-routed-tail-once");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--tail", "15"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there", "exit", "0"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let shown = outcome.stdout.lines().filter(|line| *line == "ran there").count();
    assert_eq!(shown, 1, "the command's line is shown once: {outcome:?}");
}

/// A failed routed run shows its command's last line once too, and still says the verdict line.
#[test]
fn a_failed_routed_run_shows_its_command_s_output_once_with_a_tail() {
    let case = client_case("client-routed-failed-tail-once");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--tail", "15"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there", "exit", "3"],
    ));

    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    let shown = outcome.stdout.lines().filter(|line| *line == "ran there").count();
    assert_eq!(shown, 1, "the command's line is shown once: {outcome:?}");
    assert!(outcome.stderr.contains("gate: FAILED (status 3)"), "{outcome:?}");
}

/// The case's worktree root as git gives it, which routing hashes into the work-folder key.
fn git_root(case: &Case) -> String {
    git_out(case, &[], &["rev-parse", "--show-toplevel"])
}

/// A routed run leaves `remote\<key>.worktree` in the state root: the worktree root the key was made from, the repo's
/// git common folder, the repo name and when it was routed.
#[test]
fn a_routed_run_records_which_worktree_its_key_came_from() {
    let case = client_case("client-worktree-record");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there", "exit", "0"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let root = git_root(&case);
    let key = key_of(&events(&case));
    assert_eq!(key, work_folder_key(&root), "the key is the root's");
    let path = case.root().join("remote").join(format!("{key}.worktree"));
    let text = fs::read_to_string(&path).expect("the worktree record was written");
    let record: Value = serde_json::from_str(&text).expect("the record is JSON");
    assert_eq!(record["v"], 1, "{text}");
    assert_eq!(record["root"], root.as_str(), "{text}");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    assert_eq!(record["repo"], repo.as_str(), "{text}");
    let common = record["commonDir"].as_str().expect("the common folder is text");
    assert!(Path::new(common).ends_with(".git"), "{text}");
    assert_eq!(
        fs::canonicalize(common).expect("the common folder exists"),
        fs::canonicalize(case.folder().join(".git")).expect("the case's git folder exists"),
        "{text}"
    );
    let routed = record["lastRouted"].as_str().expect("the time is text");
    routed.parse::<jiff::Timestamp>().expect("lastRouted is a time");
}

/// A worktree record that cannot be written is said once, and the routed run ends as it would have.
#[test]
fn a_record_that_cannot_be_written_leaves_the_routed_run_as_it_was() {
    let case = client_case("client-worktree-unwritable");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let blocker = case.root().join("remote").join(format!("{}.worktree", work_folder_key(&git_root(&case))));
    fs::create_dir_all(&blocker).expect("a folder can stand at the record's path");

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there", "exit", "0"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(record_unwritten_lines(&outcome), 1, "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
    assert!(blocker.is_dir(), "the folder is left as it was");
}

/// How many of the run's error lines say a worktree record could not be written.
fn record_unwritten_lines(outcome: &Outcome) -> usize {
    outcome
        .stderr
        .lines()
        .filter(|line| line.starts_with("gate: cannot write ") && line.contains(".worktree"))
        .count()
}

/// A routed run of a linked worktree records the linked worktree's root and the main checkout's git folder, which
/// every worktree of the clone shares.
#[test]
fn a_routed_run_from_a_linked_worktree_records_its_main_checkouts_git_folder() {
    let case = client_case("client-worktree-linked");
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    let mut name = case.folder().file_name().expect("the case folder has a name").to_owned();
    name.push("-wt2");
    // Outside the case folder, so the main checkout's snapshot does not take it for a repository inside it.
    let linked = Removed(case.folder().with_file_name(name));
    git(&case, &["worktree", "add", "--quiet", "--detach", &shown(&linked.0)]);
    let root = git_out(&case, &[], &["-C", &shown(&linked.0), "rev-parse", "--show-toplevel"]);
    let log = linked.0.join("out.log");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let mut gate = client_gate(&case, &shown(&log), &steps, &[r".\tool.exe", "out", "ran there"]);
    gate.current_dir(&linked.0);

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let key = key_of(&own(&case, pid));
    assert_eq!(key, work_folder_key(&root), "the key is the linked worktree's");
    let text = fs::read_to_string(case.root().join("remote").join(format!("{key}.worktree"))).expect("the record was written");
    let record: Value = serde_json::from_str(&text).expect("the record is JSON");
    assert_eq!(record["root"], root.as_str(), "{text}");
    let common = record["commonDir"].as_str().expect("the common folder is text");
    assert_eq!(
        fs::canonicalize(common).expect("the common folder exists"),
        fs::canonicalize(case.folder().join(".git")).expect("the main checkout's git folder exists"),
        "{text}"
    );
}

/// A second routed run of a worktree within the hour of the first, with nothing changed, leaves the record's bytes, its
/// `lastRouted` among them, as the first wrote them.
#[test]
fn a_second_routed_run_within_the_hour_leaves_the_record_as_it_was() {
    let case = client_case("client-worktree-fresh");
    let first_steps = ssh_steps(&case, "first", &["ok", "ok"]);
    let first = finish(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "a"]));
    assert_eq!(first.code, Some(0), "{first:?}");
    let path = case.root().join("remote").join(format!("{}.worktree", work_folder_key(&git_root(&case))));
    let written = fs::read(&path).expect("the first run wrote the record");
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);

    let second = finish(client_gate(&case, "second.log", &second_steps, &[r".\tool.exe", "out", "b"]));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(calls(&second_steps), ["preflight", "run"], "the second run was routed");
    assert_eq!(
        fs::read(&path).expect("the record is still there"),
        written,
        "the record is as the first run wrote it"
    );
}

/// A run whose first builder turns it away and whose second runs it says once that the record cannot be written:
/// the record is the worktree's, written once a run, not once a builder.
#[test]
fn an_unwritable_record_is_said_once_when_the_run_tries_two_builders() {
    let case = next_builder_case("client-worktree-two-builders");
    let blocker = case.root().join("remote").join(format!("{}.worktree", work_folder_key(&git_root(&case))));
    fs::create_dir_all(&blocker).expect("a folder can stand at the record's path");
    let steps = ssh_steps(&case, "ssh", &["on builder-a", "version 0.0.0-other", "on builder", "ok", "ok"]);
    let folders = map_folders(&case);
    let tuned = Tuned {
        folders: Some(&folders),
        ..Tuned::default()
    };

    let (pid, outcome) = finish_with_pid(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_handed_on(&case, pid, &outcome, &steps, "version");
    assert_eq!(record_unwritten_lines(&outcome), 1, "{outcome:?}");
}

/// A routed run's `--env` pairs travel in the run call's `Env`: the command on the box sees the value the PC gave, a
/// variable the box's own environment does not hold.
#[test]
fn a_routed_run_sends_its_env_pairs() {
    let case = client_case("client-env");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--env", "ZZ_GATE_ENV_SENT=from the PC", "--env", "ZZ_GATE_ENV_EMPTY="],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "env", "ZZ_GATE_ENV_SENT"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert!(outcome.stdout.lines().any(|line| line == "from the PC"), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
}

/// The box's command sees the worktree's HEAD as `SourceRevisionId`, not the snapshot commit the box checks out.
#[test]
fn a_routed_command_sees_the_worktree_head_as_its_source_revision() {
    let case = client_case("client-env-source-revision");
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let head = git(&case, &["rev-parse", "HEAD"]);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "env", "SourceRevisionId"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert!(
        outcome.stdout.lines().any(|line| line == head),
        "the command on the box sees the worktree's HEAD {head}: {outcome:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
}

/// A routed dotnet build gets its source revision, then the path map of the work folder the box checked it out in, on
/// the transport's builder root, to the worktree root as this PC spells it.
#[test]
fn a_routed_dotnet_build_maps_the_box_work_folder_to_the_worktree_root() {
    let case = outputs_case("client-path-map", &[]);
    copy_tool(&case, "dotnet.exe");
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let head = git(&case, &["rev-parse", "HEAD"]);
    let root = std::path::absolute(git_out(&case, &[], &["rev-parse", "--show-toplevel"])).expect("git names the worktree by a full path");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\dotnet.exe", "test", "argv"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let path_map = format!(r"-p:PathMap={}\={}", work_folder(&case).display(), root.display());
    let words: Vec<&str> = outcome.stdout.lines().filter(|line| line.starts_with("-p:")).take(2).collect();
    assert_eq!(words, [format!("-p:SourceRevisionId={head}"), path_map], "{outcome:?}");
}

/// A forwarded `--env NAME`: the box's command sees the value the PC's environment held, and no line of the gate's or
/// the journal says it.
#[test]
fn a_routed_run_sends_a_forwarded_env_pair() {
    let case = client_case("client-env-forwarded");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--env", "ZZ_GATE_ENV_FORWARDED"],
        ..Tuned::default()
    };
    let mut gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "env", "ZZ_GATE_ENV_FORWARDED"]);
    gate.env("ZZ_GATE_ENV_FORWARDED", "from the caller's environment");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert!(
        outcome.stdout.lines().any(|line| line == "from the caller's environment"),
        "the command on the box sees the value: {outcome:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
    assert!(
        !outcome
            .stderr
            .lines()
            .any(|line| line.starts_with("gate: ") && line.contains("from the caller's environment")),
        "no line of the gate's says the value: {outcome:?}"
    );
    let journal = fs::read_to_string(case.root().join("events.jsonl")).expect("the journal was written");
    assert!(
        !journal.contains("from the caller's environment"),
        "the journal holds no value: {journal}"
    );
}

/// The builder's command gets the gate's `UseSharedCompilation=false`, whatever the PC's own environment holds
/// (GATE-383): a command that waits for every process in its job never ends while the run's own compiler server idles in
/// it.
#[test]
fn a_routed_run_gives_the_builder_s_command_shared_compilation_off() {
    let case = client_case("client-shared-compilation");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "env", "UseSharedCompilation"]);
    gate.env("UseSharedCompilation", "true");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "the run went to the builder: {outcome:?}");
    assert!(
        outcome.stdout.lines().any(|line| line == "false"),
        "the builder's command read the gate's value, not the PC's `true`: {outcome:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
}

/// The most git processes the simple pass below starts, on the PC and the box together, children of git counted. Among
/// them are the PC's `rev-parse --verify -q HEAD` that every routed run asks for its `SourceRevisionId`, and the box's
/// `git grep` of the snapshot's `.gitattributes` files for the LFS filter, before the checkout.
const SIMPLE_PASS_GIT_STARTS: usize = 32;

/// The `argv` of each git process that wrote a trace into the folder `traces`, the `GIT_TRACE2_EVENT` of the gate that
/// started them: one `"event":"start"` record a process.
fn git_starts(traces: &Path) -> Vec<String> {
    let mut starts = Vec::new();
    for entry in fs::read_dir(traces).expect("the trace folder can be listed") {
        let text = fs::read_to_string(entry.expect("a trace file can be named").path()).expect("a trace file can be read");
        for line in text.lines() {
            let record: Value = serde_json::from_str(line).expect("a trace record is JSON");
            if record["event"] == "start" {
                starts.push(record["argv"].to_string());
            }
        }
    }
    starts.sort();
    starts
}

/// The simple pass of a single repository with no outputs, its git processes counted through `GIT_TRACE2_EVENT`, which
/// the stand-in passes on to the box's gate and every git it starts: a merged call that comes apart shows here.
#[test]
fn a_simple_routed_run_starts_at_most_its_counted_git_processes() {
    let case = client_case("client-git-starts");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let traces = case.folder().join(".tmp").join("git-trace2");
    fs::create_dir_all(&traces).expect("the trace folder can be made");
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there", "exit", "3"]);
    gate.env("GIT_TRACE2_EVENT", &traces);

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    let starts = git_starts(&traces);
    let listed = starts.join("\n");
    assert!(
        starts.iter().any(|argv| argv.contains(r#""push","--no-verify""#)),
        "the PC's push is counted:\n{listed}"
    );
    assert!(
        starts
            .iter()
            .any(|argv| argv.contains(r#""init","-q""#) || argv.contains(r#""cat-file","-e""#)),
        "the box's git is counted:\n{listed}"
    );
    assert!(
        starts.len() <= SIMPLE_PASS_GIT_STARTS,
        "{} git processes, more than {SIMPLE_PASS_GIT_STARTS}:\n{listed}",
        starts.len()
    );
}

/// The command makes a file the sync-back brings and one the outputs fetch brings, so each transfer is named.
#[test]
fn a_routed_pass_splits_its_time() {
    let case = outputs_case("client-pass-split", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\new.txt", "built", "mark", "made-on-box.txt"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"]);
    let line = Regex::new(concat!(
        r"(?m)^gate: passed in \d+ s on builder ",
        // The case's first run meets a new work folder: its 3600 s ceiling is raised to 5400 s.
        r"\(load-adjusted \d+ s, ceiling 5400 s; command \d+ s, push \d+ s, sync-back \d+ s, outputs \d+ s\)\. ",
        r"Full output: .*out\.log\r?$",
    ))
    .expect("the pattern is valid");
    assert!(line.is_match(&outcome.stdout), "{outcome:?}");
    assert!(!outcome.stdout.contains("wall clock"), "{outcome:?}");
}

/// The request the stand-in of the steps file `steps` was fed by its last call that kept it (`fed-hold`), decoded: its
/// first line, base64 JSON.
fn last_request(steps: &Path) -> Value {
    let input = fs::read_to_string(beside(steps, ".input")).expect("the stand-in kept the call's input");
    let word = input.lines().next().expect("the input has a request line");
    let json = STANDARD.decode(word).expect("the request is base64");
    serde_json::from_slice(&json).expect("the request is JSON")
}

/// The cold note of a run whose `--timeout 60` was raised, in a repo with no warm-up.
const COLD_NOTE: &str = "gate: remote cold: this worktree's work folder on the builder is new or was cleared; \
             the ceiling is raised from 60 s to 120 s; \
                         a .gate/warmup.toml lets gate warm prewarm the next new worktree";

/// The first routed run of a worktree meets a work folder the preflight lays out now: the box is sent twice the
/// ceiling, and the run says so once, naming the warm-up file it lacks. The second run of the worktree is sent its own.
#[test]
fn a_routed_run_in_a_new_work_folder_is_given_a_raised_ceiling_and_told_to_prewarm() {
    let case = client_case("client-cold-ceiling");
    // Each run call keeps its request: it waits on the case folder, which is there already.
    let fed = format!("fed-hold {}", shown(&case.folder()));
    let steps = ssh_steps(&case, "ssh", &["ok", &fed, "ok", &fed]);
    let tuned = Tuned {
        timeout: "60",
        ..Tuned::default()
    };
    let notes = |outcome: &Outcome| outcome.stderr.lines().filter(|line| *line == COLD_NOTE).count();

    let (first_pid, first) = finish_with_pid(client_gate_tuned(
        &case,
        "first.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there"],
    ));
    let first_request = last_request(&steps);
    let (second_pid, second) = finish_with_pid(client_gate_tuned(
        &case,
        "second.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there"],
    ));
    let release_of = |pid: u32| {
        own(&case, pid)
            .into_iter()
            .find(|event| event["event"] == "release")
            .expect("the run journalled its release")
    };

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "preflight", "run"]);
    assert_eq!(notes(&first), 1, "said once: {first:?}");
    let log = log_texts(&case.folder().join("first.log"));
    assert!(log.lines().any(|line| line == COLD_NOTE), "{log}");
    assert_eq!(first_request["Timeout"], 120, "the box is sent the raised ceiling: {first_request}");
    assert_eq!(notes(&second), 0, "the work folder is there now: {second:?}");
    assert_eq!(last_request(&steps)["Timeout"], 60, "the second run is sent its own ceiling");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
    assert!(
        first.stdout.contains(", ceiling 120 s; command "),
        "the passed line names the raised ceiling: {first:?}"
    );
    assert!(second.stdout.contains(", ceiling 60 s; command "), "{second:?}");
    let (cold, warm) = (release_of(first_pid), release_of(second_pid));
    assert_eq!(cold["cold"], true, "{cold}");
    assert!(warm.get("cold").is_none(), "{warm}");
}

/// A run after one whose command was ended mid-run meets a work folder holding the interrupted-command record, which
/// the box clears what that command wrote from: the run is given the raised ceiling, as in a new work folder.
#[test]
fn a_routed_run_after_an_interrupted_one_gets_the_cold_ceiling() {
    let case = client_case("client-interrupted-cold");
    let fed = format!("fed-hold {}", shown(&case.folder()));
    let steps = ssh_steps(&case, "ssh", &["ok", &fed, "ok", &fed]);
    let tuned = Tuned {
        timeout: "60",
        ..Tuned::default()
    };
    let notes = |outcome: &Outcome| outcome.stderr.lines().filter(|line| *line == COLD_NOTE).count();
    let first = finish(client_gate_tuned(
        &case,
        "first.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there"],
    ));
    assert_eq!(first.code, Some(0), "{first:?}");
    let first_request = last_request(&steps);
    let work = first_request["Work"].as_str().expect("the request names its work folder");
    let job = first_request["Job"].as_str().expect("the request names its job");
    // As a command the box's ceiling killed leaves it: written just before the command, of the run's job.
    fs::write(format!("{work}.command"), format!("{} {job}", jiff::Timestamp::now())).expect("the record can be written");

    let second = finish(client_gate_tuned(
        &case,
        "second.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there"],
    ));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "preflight", "run"]);
    assert_eq!(notes(&second), 1, "said once: {second:?}");
    assert_eq!(last_request(&steps)["Timeout"], 120, "the box is sent the raised ceiling");
    assert!(second.stdout.contains(", ceiling 120 s; command "), "{second:?}");
    assert!(
        !Path::new(&format!("{work}.command")).exists(),
        "the run cleared the folder and ended clean"
    );
}

/// A folder a case made outside its own, removed with all it holds when it drops; a removal that fails is said.
struct Removed(PathBuf);

impl Drop for Removed {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && self.0.exists()
        {
            let _ = writeln!(io::stderr(), "the folder {} cannot be removed: {error}", self.0.display());
        }
    }
}

/// `count` bytes no compression shrinks, from a fixed linear congruential sequence.
fn incompressible(count: usize) -> Vec<u8> {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    (0..count)
        .map(|_| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            state.to_be_bytes()[0]
        })
        .collect()
}

/// The bytes the run logging to `log` says its push wrote, from its `gate: pushed snapshot <8> in <s> s (<size>)` line
/// in git's own units: 0 for `up to date`.
fn pushed_bytes(log: &Path) -> f64 {
    let text = log_texts(log);
    let line = text
        .lines()
        .find(|line| line.starts_with("gate: pushed snapshot "))
        .expect("the run pushed a snapshot");
    let size = line
        .rsplit_once(" (")
        .and_then(|(_, size)| size.strip_suffix(')'))
        .expect("the line ends with its size");
    if size == "up to date" {
        return 0.0;
    }
    let unit_at = size.find(|c: char| !(c.is_ascii_digit() || c == '.')).expect("a unit follows the number");
    let number: f64 = size[..unit_at].parse().expect("the size starts with a number");
    let unit = size[unit_at..].trim();
    let scale = match unit {
        "byte" | "bytes" | "B" => Some(1.0),
        "KiB" => Some(1024.0),
        "MiB" => Some(1024.0 * 1024.0),
        "GiB" => Some(1024.0 * 1024.0 * 1024.0),
        _ => None,
    };
    assert!(scale.is_some(), "an unknown unit {unit:?} in {line:?}");
    number * scale.expect("checked above")
}

/// A second worktree of one clone is a key of its own, but its snapshot's bridge is a child of the first worktree's
/// snapshot, which the builder's bare repository, one for the repo, already holds: only the difference travels.
#[test]
fn a_new_worktree_s_first_push_sends_only_its_difference() {
    let case = client_case("client-second-worktree");
    fs::write(case.folder().join("big.bin"), incompressible(3 << 20)).expect("the big file can be written");
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    let first_log = case.folder().join("first.log");
    let first_steps = ssh_steps(&case, "first", &["ok", "ok", "ok"]);
    let (first_pid, first) = finish_with_pid(client_gate(&case, &shown(&first_log), &first_steps, &[r".\tool.exe", "out", "a"]));
    assert_eq!(first.code, Some(0), "{first:?}");
    let key1 = key_of(&own(&case, first_pid));
    let mut name = case.folder().file_name().expect("the case folder has a name").to_owned();
    name.push("-wt2");
    // Outside the case folder, so the first worktree's snapshot does not take it for a repository inside it.
    let other = Removed(case.folder().with_file_name(name));
    git(&case, &["worktree", "add", "--quiet", "--detach", &shown(&other.0)]);
    fs::write(other.0.join("new.txt"), "only here\n").expect("the new file can be written");
    let second_log = other.0.join("second.log");
    let second_steps = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let mut gate = client_gate(&case, &shown(&second_log), &second_steps, &[r".\tool.exe", "out", "b"]);
    gate.current_dir(&other.0);

    let (second_pid, second) = finish_with_pid(gate);

    assert_eq!(second.code, Some(0), "{second:?}");
    let key2 = key_of(&own(&case, second_pid));
    assert_ne!(key2, key1, "a worktree of its own is a key of its own");
    let first_snapshot = builder_ref(&case, &format!("refs/gate/{key1}"));
    let bridge = builder_ref(&case, &format!("refs/gate-bridge/{key2}"));
    assert!(is_sha(&first_snapshot) && is_sha(&bridge), "{first_snapshot:?} {bridge:?}");
    let parent = git_out(&case, &[], &["--git-dir", &bare_of(&case), "rev-parse", &format!("{bridge}^")]);
    assert_eq!(parent, first_snapshot, "the bridge's parent is the first worktree's snapshot");
    let (sent_first, sent_second) = (pushed_bytes(&first_log), pushed_bytes(&second_log));
    assert!(sent_first >= 1024.0 * 1024.0, "the first push sends the big file: {sent_first} bytes");
    assert!(sent_second < sent_first, "{sent_second} bytes after {sent_first}");
    assert!(
        sent_second < 64.0 * 1024.0,
        "the second push sends only its difference: {sent_second} bytes"
    );
}

/// A worktree whose runs alternate between `builder-a` and `host` bridges each push on the snapshot that builder last
/// received, kept at a ref of its own: going back to `builder-a` after `host` sends only the difference, never host's
/// snapshot and its whole tree.
#[test]
fn alternating_builders_each_bridge_on_their_own_last_snapshot() {
    let case = next_builder_case("client-alternating-builders");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": HOST,
        "repos": { &repo: ["*tool* out b*"] },
        "outputs": { &repo: [] },
        "builders": [{ "host": FIRST_BUILDER, "os": "windows", "repos": { &repo: ["*tool* out a*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    fs::write(case.folder().join("big.bin"), incompressible(3 << 20)).expect("the big file can be written");
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    let folders = map_folders(&case);
    let tuned = Tuned {
        folders: Some(&folders),
        ..Tuned::default()
    };
    let run = |name: &str, letter: &str| {
        let log = case.folder().join(format!("{name}.log"));
        let steps = ssh_steps(&case, name, &["ok", "ok", "ok"]);
        let outcome = finish(client_gate_tuned(&case, &shown(&log), &steps, &tuned, &[r".\tool.exe", "out", letter]));
        assert_eq!(outcome.code, Some(0), "{name}: {outcome:?}");
        pushed_bytes(&log)
    };

    let first = run("first", "a");
    let key = key_of(&events(&case));
    let snapshot = format!("refs/gate/{key}");
    let first_snapshot = ref_in(&case, "a", &snapshot);
    run("second", "b");
    let third = run("third", "a");

    assert!(first >= 1024.0 * 1024.0, "the first push sends the big file: {first} bytes");
    assert!(
        third < 64.0 * 1024.0,
        "back on builder-a, the push sends only the difference from its own last snapshot: {third} bytes"
    );
    let bridge = ref_in(&case, "a", &format!("refs/gate-bridge/{key}"));
    assert!(is_sha(&first_snapshot) && is_sha(&bridge), "{first_snapshot:?} {bridge:?}");
    let parent = git_out(&case, &[], &["--git-dir", &bare_in(&case, "a"), "rev-parse", &format!("{bridge}^")]);
    assert_eq!(parent, first_snapshot, "the bridge's parent is builder-a's own first snapshot");
    let id = gate_router::builder_id(FIRST_BUILDER);
    let local = |name: &str| git_out(&case, &[], &["rev-parse", "--verify", name]);
    assert_eq!(
        local(&format!("refs/gate-remote/{id}/{key}")),
        ref_in(&case, "a", &snapshot),
        "builder-a's ref names the snapshot builder-a last received"
    );
    assert_eq!(
        local(&format!("refs/gate-remote/{key}")),
        ref_in(&case, "b", &snapshot),
        "host's ref names the snapshot host last received"
    );
}

#[test]
fn a_routed_run_past_3x_its_usual_p90_is_hinted_once() {
    let case = client_case("client-hang-hint");
    // The command waits for `go` in the folder it runs in: the snapshot of the first five runs sends it, so they pass
    // at once; the held run's snapshot has none, and the case writes it into the builder's work folder.
    let command = [r".\tool.exe", "wait", "go", "out", "ran"];
    let steps = ssh_steps(&case, "ssh", &["ok"; 12]);
    write(&case, "go", "");
    for _ in 0..5 {
        let outcome = finish(client_gate(&case, "history.log", &steps, &command));
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
    }
    fs::remove_file(case.folder().join("go")).expect("the go file can be removed");
    // The builder key's five runs, rewritten to a wall p90 of 1 s.
    seed(&case, " out ran", 1.0, None);
    // Each span in seconds or, for a hint a starved watch says 90 s or more into the run, in minutes.
    let hint = Regex::new(r"gate: still running after (\d+ (?:s|min)), \d+x its usual p90 of 1 s; running on builder for (\d+ (?:s|min))\n")
        .expect("the pattern is valid");
    // Written as it drops, the go file lets the command go when the case fails before its own write.
    let go = Go(case.folder().join("builder").join("work").join(key_of(&events(&case))).join("go"));

    let mut running = start(client_gate(&case, "out.log", &steps, &command));
    running.wait_for_stderr_while_running("the hang hint", |stderr| hint.is_match(stderr));
    fs::write(go.path(), "").expect("the go file can be made");
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "the hint kills nothing: {outcome:?}");
    assert_eq!(hint.find_iter(&outcome.stderr).count(), 1, "{outcome:?}");
    let said = hint.captures(&outcome.stderr).expect("the hint");
    assert_eq!(
        said[1], said[2],
        "the run call's phase counts from the command's start, as the hint does: {outcome:?}"
    );
    let log = log_texts(&case.folder().join("out.log"));
    assert_eq!(hint.find_iter(&log).count(), 1, "{log}");
    let events = events(&case);
    assert_eq!(fallbacks(&events), Vec::<&str>::new(), "{events:#?}");
    assert!(events.iter().all(|event| event["remote"] == true), "every run was routed: {events:#?}");
}

/// The first line of a routed run waiting for its worktree's work folder: the slot wait's line for the key mutex.
const KEY_WAITING: &str = "gate: waiting for this worktree's work folder on builder, which another of its runs holds";
/// The line a routed run says when a Ctrl+C ends its wait for the work folder.
const KEY_INTERRUPTED: &str = "gate: interrupted while waiting for this worktree's work folder on builder";

#[test]
fn a_second_routed_run_of_one_worktree_falls_back_busy_past_the_bound() {
    // One slot, which the first run holds: the second waits for the key's lock up to a bound this run crosses at once,
    // then falls back as it did before there was a wait. The holder ran the command once, so its claim carries no usual
    // run time and the wait's first line names none.
    let case = client_case_of("client-busy", 1);
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "100"],
        ..Tuned::default()
    };

    let (pid, second) = finish_with_pid(client_gate_tuned(
        &case,
        "second.log",
        &second_steps,
        &tuned,
        &[r".\tool.exe", "out", "ran here"],
    ));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_in_order(&second.stderr, &[KEY_WAITING, BUSY_LINE]);
    assert!(
        second.stderr.lines().any(|line| line == KEY_WAITING),
        "a holder with no usual run time leaves the line without a time left: {second:?}"
    );
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
fn a_second_routed_run_of_one_worktree_waits_for_its_holder_and_routes() {
    // The bound is far longer than the wait: once the first run lets the key go, the second takes it and routes.
    let case = client_case_of("client-key-waits", 1);
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "600000", "--test-remote-wait-ms", "3600000"],
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
    second.wait_for_stderr(KEY_WAITING);

    fs::write(go.path(), "").expect("the go file can be made");
    let first = first.finish();
    let second = second.finish();

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(
        !second.stderr.contains(BUSY_LINE),
        "the wait outlived the holder, so it did not fall back: {second:?}"
    );
    assert!(second.stdout.contains("ran here"), "it ran there: {second:?}");
    assert_eq!(calls(&second_steps), ["preflight", "run"]);
    assert_eq!(fallbacks(&own(&case, pid)), Vec::<&str>::new(), "the second run fell back for nothing");
}

#[test]
fn a_second_routed_run_of_one_worktree_says_the_holders_time_left() {
    // Five runs file the command's duration and `seed` makes its usual 600 s, so the holder's remote claim carries that
    // expected time and the wait's line says what is left of it.
    let case = client_case_of("client-key-eta", 1);
    let steps = ssh_steps(&case, "seed", &["ok"; 12]);
    let command = [r".\tool.exe", "out", "ran"];
    for _ in 0..5 {
        let outcome = finish(client_gate(&case, "history.log", &steps, &command));
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
    }
    seed(&case, " out ran", 600.0, None);
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &command));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "600000", "--test-remote-wait-ms", "3600000"],
        ..Tuned::default()
    };
    let second = start(client_gate_tuned(&case, "second.log", &second_steps, &tuned, &command));
    let left = Regex::new(r" \(about (\d+) (s|min) left\)$").expect("the pattern is valid");
    let said = second.wait_for_stderr_where("the key wait's line with the time left", |stderr| {
        stderr.lines().any(|line| line.starts_with(KEY_WAITING) && left.is_match(line))
    });

    let line = said
        .lines()
        .find(|line| line.starts_with(KEY_WAITING))
        .expect("the key wait's line was said");
    let found = left.captures(line).expect("the line says the holder's time left");
    let figure: u64 = found[1].parse().expect("the time left is a whole number");
    let seconds = if &found[2] == "min" { figure * 60 } else { figure };
    assert!((1..=600).contains(&seconds), "what is left of the holder's seeded usual 600 s: {line:?}");
    fs::write(go.path(), "").expect("the go file can be made");
    let first = first.finish();
    let second = second.finish();
    assert_eq!((first.code, second.code), (Some(0), Some(0)), "{first:?}\n{second:?}");
}

#[test]
fn a_second_routed_run_of_one_worktree_says_it_still_waits() {
    let case = client_case_of("client-key-still-waits", 1);
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &[
            "--test-key-wait-ms",
            "600000",
            "--test-heartbeat-ms",
            "50",
            "--test-remote-wait-ms",
            "3600000",
        ],
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
    second.wait_for_stderr(KEY_WAITING);
    second.wait_for_stderr("gate: still waiting for this worktree's work folder on builder (");

    fs::write(go.path(), "").expect("the go file can be made");
    let first = first.finish();
    let second = second.finish();

    assert_eq!((first.code, second.code), (Some(0), Some(0)), "{first:?}\n{second:?}");
    assert!(!second.stderr.contains(BUSY_LINE), "{second:?}");
    assert_eq!(calls(&second_steps), ["preflight", "run"]);
    assert_eq!(fallbacks(&own(&case, pid)), Vec::<&str>::new());
}

#[test]
fn a_ctrl_c_while_waiting_for_the_work_folder_abandons_the_wait() {
    let case = client_case_of("client-key-ctrl-c", 1);
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    // The seam raises the console interrupt once the gate has said it waits, the key wait's line included.
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "3600000", "--test-interrupt-queued"],
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

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[KEY_WAITING, KEY_INTERRUPTED]);
    let own = own(&case, pid);
    let names: Vec<&str> = own.iter().map(|event| event["event"].as_str().expect("an event has a name")).collect();
    assert_eq!(names, ["queue", "abandon"], "{own:#?}");
    assert!(own.iter().all(|event| event["remote"] == true), "{own:#?}");
    assert!(own[1]["waitedSeconds"].is_number(), "{own:#?}");
    assert_eq!(calls(&second_steps), Vec::<String>::new(), "the second run made no ssh call");
    fs::write(go.path(), "").expect("the go file can be made");
    assert_eq!(first.finish().code, Some(0));
}

#[test]
fn a_ctrl_c_already_raised_at_the_key_take_abandons_before_busy() {
    let case = client_case_of("client-key-ctrl-c-before", 1);
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    // The seam raises the console interrupt before the zero-wait take, which reads the taken lock as busy: the run must
    // end as an interrupted key wait, not fall back `busy`, and without a waiting line it never said.
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "3600000", "--test-interrupt-before-key"],
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

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(outcome.stderr.contains(KEY_INTERRUPTED), "{outcome:?}");
    assert!(!outcome.stderr.contains(KEY_WAITING), "it never waited: {outcome:?}");
    assert!(!outcome.stderr.contains(BUSY_LINE), "an interrupt is not a busy fallback: {outcome:?}");
    let own = own(&case, pid);
    let names: Vec<&str> = own.iter().map(|event| event["event"].as_str().expect("an event has a name")).collect();
    assert_eq!(names, ["queue", "abandon"], "{own:#?}");
    assert!(own.iter().all(|event| event["remote"] == true), "{own:#?}");
    assert_eq!(calls(&second_steps), Vec::<String>::new(), "the second run made no ssh call");
    fs::write(go.path(), "").expect("the go file can be made");
    assert_eq!(first.finish().code, Some(0));
}

#[test]
fn a_routed_run_behind_a_warm_up_falls_back_busy_without_waiting() {
    // A work folder with no warm-up consent of its own is warmed by a warm-up the route never probes the lock for, and
    // the run reaches the key with a warm-up holding it. The warm-up lock is held here as a warm-up holds it, and the
    // run falls back at once rather than wait a warm-up out.
    let case = client_case_of("client-warm-holder", 1);
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let lock = gate_state::v1::warm_lock_path(&case.root(), &key_of(&events(&case)));
    fs::create_dir_all(lock.parent().expect("the lock has a folder")).expect("the warm-up folder can be made");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&lock)
        .expect("the warm-up lock can be held");
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "3600000"],
        ..Tuned::default()
    };

    let (pid, second) = finish_with_pid(client_gate_tuned(
        &case,
        "second.log",
        &second_steps,
        &tuned,
        &[r".\tool.exe", "out", "ran here"],
    ));

    drop(held);
    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(
        !second
            .stderr
            .lines()
            .any(|line| line.starts_with("gate: waiting for this worktree's work folder")),
        "a warm-up's work folder is not waited for: {second:?}"
    );
    assert!(second.stderr.contains(BUSY_LINE), "{second:?}");
    assert_eq!(calls(&second_steps), Vec::<String>::new(), "the second run made no ssh call");
    assert_eq!(fallbacks(&own(&case, pid)), ["busy"]);
    fs::write(go.path(), "").expect("the go file can be made");
    assert_eq!(first.finish().code, Some(0));
}

/// The running stand-in whose parent is the gate `pid`, with its start, once there is exactly one.
fn ssh_child_of(pid: u32) -> (u32, jiff::Timestamp) {
    let mut found = None;
    wait_until(&format!("one fake-ssh under gate {pid}"), || {
        let children: Vec<u32> = ProcessTree::new(Win32ProcessTable.snapshot())
            .iter()
            .filter(|process| process.parent_pid == pid && process.name.eq_ignore_ascii_case("fake-ssh.exe"))
            .map(|process| process.pid)
            .collect();
        found = match children[..] {
            [child] => gate_win32::process_start_of(child).ok().flatten().map(|at| (child, at)),
            _ => None,
        };
        found.is_some()
    });
    found.expect("checked by the wait")
}

#[test]
fn gate_stop_tree_reaches_the_ssh_client() {
    let case = client_case("client-stop-tree");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path()))]);
    let running = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));
    running.wait_until("the run call", || calls(&steps).len() == 2);
    let gate_pid = running.id();
    let (ssh, at) = ssh_child_of(gate_pid);

    let stopped = case.run(&["stop-tree", &gate_pid.to_string()]);
    let outcome = running.finish();

    assert_eq!((stopped.code, stopped.stderr.as_str()), (Some(0), ""), "{stopped:?}");
    // The ssh call's job is anonymous, so stop-tree finds no gate job: it ends the tree under the gate, the gate and the
    // ssh it holds, by their pids.
    assert_eq!(
        stopped.stdout,
        format!("gate: stopped 0 gate job(s) and 2 process(es) under {gate_pid}\n"),
        "{stopped:?}"
    );
    // A gate whose ssh ended before it did ends its run with status 1, never here.
    assert!(matches!(outcome.code, Some(124 | 1)), "the gate ends stopped: {outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "the command never ran here: {outcome:?}");
    let takes = events(&case).iter().filter(|event| event["event"] == "take").count();
    assert_eq!(takes, 1, "no take after the remote one: {:?}", events(&case));
    wait_until(
        &format!("fake-ssh {ssh} ending"),
        || !matches!(gate_win32::process_start_of(ssh), Ok(Some(start)) if start == at),
    );
    assert_eq!(calls(&steps), ["preflight", "run"], "the held call was the last");
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
fn a_dropped_preflight_logs_no_line_saying_its_input_was_not_sent() {
    let case = client_case("client-preflight-drop-fed");
    let steps = ssh_steps(&case, "ssh", &["drop"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    let log = log_texts(&case.folder().join("out.log"));
    assert!(!log.contains("input was not sent"), "the call's own end says why: {log}");
}

#[test]
fn a_preflight_that_never_answers_before_the_banner_falls_back_unreachable() {
    let case = client_case("client-preflight-drops");
    let steps = ssh_steps(&case, "ssh", &["fail-banner", "fail-banner", "fail-banner", "fail-banner"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 3, "{outcome:?}");
    assert!(outcome.stderr.contains(UNREACHABLE_LINE), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "preflight", "preflight", "preflight"]);
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

#[test]
fn a_run_call_that_never_answers_before_the_banner_falls_back_unreachable() {
    let case = client_case("client-run-drops");
    let steps = ssh_steps(&case, "ssh", &["ok", "fail-banner", "fail-banner", "fail-banner", "fail-banner"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 3, "{outcome:?}");
    assert!(outcome.stderr.contains(UNREACHABLE_LINE), "{outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.contains("ran here"), "it ran here: {log}");
    assert_eq!(calls(&steps), ["preflight", "run", "run", "run", "run"]);
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

/// The box cannot clear the work folder, whose record says another layout, for a file held open in it, so it refuses
/// the run `layout`.
#[test]
fn a_layout_refusal_of_the_box_falls_back_with_its_line_and_runs_the_command_here_once() {
    let case = client_case("client-layout-refused");
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let named = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(named.code, Some(0), "{named:?}");
    let work = case.folder().join("builder").join("work").join(key_of(&events(&case)));
    fs::write(format!("{}.layout", shown(&work)), "pair app lib").expect("the record can be written");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(work.join("held.txt"))
        .expect("a file in the work folder can be held");
    let second = ssh_steps(&case, "second", &["ok", "ok"]);

    let (pid, outcome) = finish_with_pid(client_gate(&case, "second.log", &second, &[r".\tool.exe", "mark", RAN, "out", "ran"]));
    drop(held);

    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    assert_eq!(fallbacks(&own(&case, pid)), ["layout"], "{outcome:?}");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let line = format!("gate: not routed: the builder could not clear {repo}'s work folder: ");
    assert!(
        outcome.stderr.lines().any(|said| said.starts_with(&line) && said.contains("held.txt")),
        "the box's line is passed on: {outcome:?}"
    );
    assert!(outcome.stderr.contains("gate: remote builder layout; running here"), "{outcome:?}");
    assert_eq!(runs(&case), (1, 0), "the command ran once, here: {outcome:?}");
    assert_eq!(calls(&second), ["preflight", "run"]);
}

#[test]
fn a_routed_run_whose_command_is_longer_than_cmd_s_line_runs_on_the_box() {
    let case = client_case("client-long-command");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let long = "x".repeat(9000);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", &long, "out", "end"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stdout.contains(&format!("{long}\n")),
        "the word reached the tool whole: {outcome:?}"
    );
    assert!(outcome.stdout.contains("end"), "the word after it too: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "the command ran on the box");
    let events = events(&case);
    assert_eq!(fallbacks(&events), Vec::<&str>::new(), "{events:#?}");
    assert!(events.iter().all(|event| event["remote"] == true), "a remote run: {events:#?}");
}

/// A builder whose preflight ends with cmd's 9009, or a Unix shell's 126 or 127 with no line of the box's own, has no
/// gate box that takes the run: the run is not routed and falls back, here to the PC. A shell's line of its own, which
/// says why it could not run the box, is kept after the status.
#[test]
fn a_builder_without_a_usable_gate_box_is_not_routed() {
    let cases = [
        (
            "client-no-box",
            "exit 9009",
            "gate: not routed: builder has no gate box that takes this run (preflight status 9009)",
        ),
        (
            "client-no-box-127",
            "say-exit 127 zsh: command not found: gate-box",
            "gate: not routed: builder has no gate box that takes this run (preflight status 127): zsh: command not found: gate-box",
        ),
        (
            "client-no-box-126",
            "say-exit 126 zsh: permission denied: gate-box",
            "gate: not routed: builder has no gate box that takes this run (preflight status 126): zsh: permission denied: gate-box",
        ),
    ];
    for (name, step, line) in cases {
        let case = client_case(name);
        let steps = ssh_steps(&case, "ssh", &[step]);

        let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

        assert_eq!(outcome.code, Some(0), "{name}: {outcome:?}");
        assert!(outcome.stderr.contains(line), "{name}: {outcome:?}");
        assert!(
            outcome.stderr.contains("gate: remote builder version; running here"),
            "{name}: {outcome:?}"
        );
        assert!(outcome.stdout.contains("ran here"), "{name}: {outcome:?}");
        assert_eq!(calls(&steps), ["preflight"], "{name}");
        assert_eq!(fallbacks(&events(&case)), ["version"], "{name}");
        assert!(!case.folder().join("builder").exists(), "{name}: nothing reached the builder");
    }
}

/// A preflight that ended with a shell's status after the box said its own `gate: remote … failed (status …)` line on
/// standard error, as both boxes do, has a gate box that takes the run: the run ends with that status, and the PC does
/// not run the command instead.
#[test]
fn a_builder_whose_box_said_a_git_step_failed_is_not_fallen_back_from() {
    let case = client_case("client-box-said-127");
    let steps = ssh_steps(&case, "ssh", &["say-exit 127 gate: remote git init failed (status 127)"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(127), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: preparing "), "{outcome:?}");
    assert!(outcome.stderr.contains("failed (status 127)"), "{outcome:?}");
    assert!(!outcome.stderr.contains("not routed"), "{outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "{:?}", events(&case));
}

/// The same when the box said its line on standard output instead: a `gate:` line on either stream is the box speaking.
#[test]
fn a_builder_whose_box_said_a_git_step_failed_on_its_output_is_not_fallen_back_from() {
    let case = client_case("client-box-said-127-out");
    let steps = ssh_steps(&case, "ssh", &["out-exit 127 gate: remote git init failed (status 127)"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(127), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: preparing "), "{outcome:?}");
    assert!(outcome.stderr.contains("failed (status 127)"), "{outcome:?}");
    assert!(!outcome.stderr.contains("not routed"), "{outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "{:?}", events(&case));
}

/// The id of the `builders` entry `mac-1`: the first 8 hex digits of the SHA-256 of its alias.
const MAC_ID: &str = "1f0efc62";

/// A command a macOS `builders` entry picks is sent that builder's Unix wire: its `gate-box` line under its root, a
/// payload laid out with `/` under that root and a job with no `Local\`; the run's per-worktree caches go in the
/// builder's own PC folder, `remote\<id>`. The PC's stand-in box reads Windows paths only, so it refuses the preflight
/// and the run falls back here.
#[test]
fn a_command_a_macos_builder_picks_is_sent_its_unix_wire_and_caches_in_its_own_folder() {
    let case = client_case("client-macos-wire");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": HOST,
        "repos": { &repo: ["*never*"] },
        "builders": [{
            "host": "mac-1",
            "os": "macos",
            "root": "/Users/b/gate-remote",
            "repos": { &repo: ["*tool*"] },
            "outputs": { &repo: BIN },
        }],
    });
    write_remote_json(&case, &routing.to_string());
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\n"));
    write(&case, r"bin\old.txt", "built before");
    backdate(&case, r"bin\old.txt", HOUR);
    let fed = format!("fed-hold {}", shown(&case.folder()));
    let steps = ssh_steps(&case, "ssh", &[&fed]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    let refused = "gate: not routed: mac-1 has no gate box that takes this run (preflight status 2)";
    assert!(outcome.stderr.contains(refused), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    let lines = fs::read_to_string(beside(&steps, ".lines")).expect("the stand-in kept the call's line");
    assert_eq!(
        lines.lines().collect::<Vec<_>>(),
        ["mac-1 /Users/b/gate-remote/bin/gate-box box preflight --stdin"]
    );
    let key = key_of(&events(&case));
    let sent = last_request(&steps);
    let work = format!("/Users/b/gate-remote/work/{key}");
    assert_eq!(sent["Bare"], json!(format!("/Users/b/gate-remote/{repo}.git")), "{sent}");
    assert_eq!(sent["Work"], json!(work), "{sent}");
    assert_eq!(sent["Lock"], json!(format!("{work}.lock")), "{sent}");
    assert_eq!(sent["OutputFolder"], json!("/Users/b/gate-remote/outputs"), "{sent}");
    let job = Regex::new(&format!("^gate-remote-{key}-[0-9a-f]{{12}}$")).expect("the pattern is valid");
    assert!(sent["Job"].as_str().is_some_and(|said| job.is_match(said)), "{sent}");
    let remote = case.root().join("remote");
    let hashes = format!("{key}.outputs-hashes");
    let listed = |folder: &Path| fs::read_dir(folder).map(|entries| entries.flatten().map(|entry| entry.file_name()).collect::<Vec<_>>());
    assert!(
        remote.join(MAC_ID).join(&hashes).is_file(),
        "the cache is in mac-1's folder: {:?} {:?}",
        listed(&remote),
        listed(&remote.join(MAC_ID))
    );
    assert!(!remote.join(&hashes).exists(), "host's folder holds no cache of mac-1's");
}

/// The record of a work folder's last complete outputs pull names the builder that made it, so a run on another builder
/// asks for every output that differs from the PC's (`OutputsFull`), and the next run on that same builder does not: a
/// worktree whose runs go host, `builder-a`, `builder-a`, host sends full on both switches.
#[test]
fn a_run_after_another_builder_s_run_sends_outputs_full_and_the_next_on_the_same_builder_does_not() {
    let case = outputs_case("client-outputs-builder-switch", &BIN);
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": HOST,
        "repos": { &repo: ["*tool* out b*"] },
        "outputs": { &repo: BIN },
        "builders": [{ "host": FIRST_BUILDER, "os": "windows", "repos": { &repo: ["*tool* out a*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    // The builders' own folders are laid out under the case's, which must be there for them to be made in.
    fs::create_dir_all(case.folder().join("builder")).expect("the builders' folder can be made");
    let folders = map_folders(&case);
    let tuned = Tuned {
        folders: Some(&folders),
        ..Tuned::default()
    };
    // The run call's request is kept for the assertions below, which only `fed-hold` does: it feeds the call its
    // standard input and returns at once, the case folder being there already.
    let fed = format!("fed-hold {}", shown(&case.folder()));
    let run = |name: &str, letter: &str| {
        let log = case.folder().join(format!("{name}.log"));
        let steps = ssh_steps(&case, name, &["ok", &fed, "ok"]);
        let outcome = finish(client_gate_tuned(&case, &shown(&log), &steps, &tuned, &[r".\tool.exe", "out", letter]));
        assert_eq!(outcome.code, Some(0), "{name}: {outcome:?}");
        last_request(&steps)
    };

    let first = run("first", "b");
    assert_eq!(
        first["OutputsFull"],
        json!(true),
        "a worktree with no record sends full on its first run: {first}"
    );
    let switched = run("second", "a");
    assert_eq!(
        switched["OutputsFull"],
        json!(true),
        "a run after another builder's complete pull asks for every output: {switched}"
    );
    let same = run("third", "a");
    assert_eq!(
        same["OutputsFull"],
        json!(false),
        "the next run on that builder sends changed only: {same}"
    );
    let back = run("fourth", "b");
    assert_eq!(
        back["OutputsFull"],
        json!(true),
        "back on the first builder, the switch asks again: {back}"
    );
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

/// The alias of the `builders` entry a run of [`next_builder_case`] tries before `host`.
const FIRST_BUILDER: &str = "builder-a";

/// A [`client_case`] whose routing file lists the one `builders` entry `builder-a`, in file order before `host`, both
/// matching the case's `*tool*` commands: a run that builder turns away may try `host` next.
fn next_builder_case(name: &str) -> Case {
    let case = client_case(name);
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": HOST,
        "repos": { &repo: ["*tool*"] },
        "outputs": { &repo: [] },
        "builders": [{ "host": FIRST_BUILDER, "os": "windows", "repos": { &repo: ["*tool*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    // Each builder's folder is a child of the case folder's own `builder` folder, so the box's preflight finds its parent.
    for letter in ["a", "b"] {
        fs::create_dir_all(builder_folder(&case, letter)).expect("a builder's folder can be made");
    }
    case
}

/// The folders part of a map transport: `builder-a` laid out under `builder\a`, `host` under `builder\b`, both in the
/// case folder, which [`CLIENT_IGNORED`]'s `builder/` leaves out of every snapshot.
fn map_folders(case: &Case) -> String {
    let folder = |letter: &str| shown(&case.folder().join("builder").join(letter));
    format!("{FIRST_BUILDER}={}|{HOST}={}", folder("a"), folder("b"))
}

/// The folder the run of a [`next_builder_case`] lays `builder-a` (`"a"`) or `host` (`"b"`) out under.
fn builder_folder(case: &Case, letter: &str) -> PathBuf {
    case.folder().join("builder").join(letter)
}

/// Asserts a run that `builder-a` turned away for `reason` went on to `host` and ran the command there: the line saying
/// so, no `running here`, the command's own output, one attempt of each builder, and host's folder holding the run.
fn assert_handed_on(case: &Case, pid: u32, outcome: &Outcome, steps: &Path, reason: &str) {
    let line = format!("gate: remote {FIRST_BUILDER} {reason}; trying {HOST}");
    assert!(
        outcome.stderr.lines().any(|said| said == line),
        "the line names the next builder: {outcome:?}"
    );
    assert!(!outcome.stderr.contains("running here"), "nothing ran here: {outcome:?}");
    assert!(
        outcome.stdout.contains("ran there"),
        "the command's own output came back from host: {outcome:?}"
    );
    assert_eq!(calls(steps), ["preflight", "preflight", "run"], "builder-a's one call, then host's two");
    let own = own(case, pid);
    let tagged = |event: &str, host: &str| own.iter().filter(|said| said["event"] == event && said["host"] == host).count();
    assert_eq!(tagged("take", FIRST_BUILDER), 1, "{own:#?}");
    assert_eq!(tagged("fallback", FIRST_BUILDER), 1, "{own:#?}");
    assert_eq!(tagged("take", HOST), 1, "{own:#?}");
    assert_eq!(fallbacks(&own), [reason], "{own:#?}");
    let key = key_of(&events(case));
    assert!(
        builder_folder(case, "b").join("work").join(&key).is_dir(),
        "host laid the run out in its own work folder"
    );
}

/// A `builders` entry of another gate version is tried first, refuses, and the run goes on to `host` and runs there.
#[test]
fn a_builder_of_another_version_hands_the_run_to_the_next_builder() {
    let case = next_builder_case("client-next-builder-version");
    let steps = ssh_steps(&case, "ssh", &["on builder-a", "version 0.0.0-other", "on builder", "ok", "ok"]);
    let folders = map_folders(&case);
    let tuned = Tuned {
        folders: Some(&folders),
        ..Tuned::default()
    };

    let (pid, outcome) = finish_with_pid(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_handed_on(&case, pid, &outcome, &steps, "version");
}

/// A `builders` entry whose connection drops is tried first, is unreachable, and the run goes on to `host` and runs
/// there.
#[test]
fn an_unreachable_builder_hands_the_run_to_the_next_builder() {
    let case = next_builder_case("client-next-builder-unreachable");
    let steps = ssh_steps(&case, "ssh", &["on builder-a", "drop", "on builder", "ok", "ok"]);
    let folders = map_folders(&case);
    let tuned = Tuned {
        folders: Some(&folders),
        ..Tuned::default()
    };

    let (pid, outcome) = finish_with_pid(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_handed_on(&case, pid, &outcome, &steps, "unreachable");
}

/// The real box, judging its own image under `GATE_TEST_BOX_TRUST=check`, refuses the `target\` gate this user owns;
/// the run falls back to local and says why.
#[test]
fn a_builder_whose_gate_is_not_admin_installed_falls_back_to_local() {
    let case = client_case("client-untrusted-box");
    let steps = ssh_steps(&case, "ssh", &["ok"]);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here", "exit", "3"]);
    gate.env("GATE_TEST_BOX_TRUST", "check");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(3), "the local command's status: {outcome:?}");
    let refusal = "gate: not routed: builder's gate is not installed where only administrators can change it (";
    let said = outcome.stderr.lines().find(|line| line.starts_with(refusal));
    assert!(said.is_some_and(|line| line.ends_with("); run gate check --builder")), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder install; running here"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    assert_eq!(fallbacks(&events(&case)), ["install"]);
}

/// Grants the NULL SID, which has no members and so stands for another account, the right to change `folder` and what
/// it holds: a box whose work root is `folder` refuses it as not locked down.
pub(super) fn open_to_another_account(folder: &Path) {
    let output = Command::new("icacls")
        .arg(folder)
        .args(["/grant", "*S-1-0-0:(OI)(CI)M", "/q"])
        .stdin(Stdio::null())
        .output()
        .expect("icacls starts");
    assert!(output.status.success(), "icacls grants the NULL SID {}: {output:?}", folder.display());
}

/// The real box refuses a work root another account may write in; the run falls back to local, saying the box's why
/// once, with the installer step that locks the root down.
#[test]
fn a_builder_whose_work_root_is_open_falls_back_root() {
    let case = client_case("client-open-root");
    let root = case.folder().join("builder");
    fs::create_dir_all(&root).expect("the builder root can be made");
    open_to_another_account(&root);
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here", "exit", "3"]));

    assert_eq!(outcome.code, Some(3), "the local command's status: {outcome:?}");
    let line = format!(
        "gate: not routed: builder's work root is not locked down: {} lets S-1-0-0 add files to it; run the installer's \
         \"Gate work root\" step (install-toolchains.ps1 -Only \"Gate work root\")",
        shown(&root)
    );
    assert_eq!(outcome.stderr.lines().filter(|said| *said == line).count(), 1, "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder root; running here"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    assert_eq!(fallbacks(&events(&case)), ["root"]);
    assert!(!root.join("work").exists(), "no work folder is made: {outcome:?}");
}

/// A locked work root another process holds open with delete access is busy, not unsafe: the run falls back to local
/// for the same reason, saying the box's busy line, which names no installer step.
#[test]
fn a_builder_whose_work_root_is_busy_falls_back_root() {
    let case = client_case("client-busy-root");
    let root = case.folder().join("builder");
    fs::create_dir_all(&root).expect("the builder root can be made");
    let user = gate_win32::current_user_sid().expect("this user's SID can be read");
    let grant = format!("*{user}:(OI)(CI)F");
    let locked = Command::new("icacls")
        .arg(&root)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
            &grant,
            "/q",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("icacls starts");
    assert!(locked.status.success(), "the root is locked as the installer locks it: {locked:?}");
    let other = fs::File::options()
        .access_mode(DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&root)
        .expect("the root can be opened with delete access");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here", "exit", "3"]));

    drop(other);
    assert_eq!(outcome.code, Some(3), "the local command's status: {outcome:?}");
    let line = format!(
        "gate: not routed: builder's work root is busy: {} is open in another process; retry when it is closed",
        shown(&root)
    );
    assert_eq!(outcome.stderr.lines().filter(|said| *said == line).count(), 1, "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder root; running here"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"]);
    assert_eq!(fallbacks(&events(&case)), ["root"]);
}

/// A box that passed the preflight and refuses its install at the run call, before the command started, falls back to
/// local as at the preflight.
#[test]
fn a_box_that_refuses_its_install_at_the_run_call_falls_back_to_local() {
    let case = client_case("client-untrusted-run");
    let why = r"C:\gate\gate-desktop-helper.exe is owned by S-1-5-21-1, not SYSTEM, Administrators or TrustedInstaller";
    let refused = format!("say-exit 1 gate: the builder's gate install is not trusted: {why}");
    let steps = ssh_steps(&case, "ssh", &["ok", &refused]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here", "exit", "3"]));

    assert_eq!(outcome.code, Some(3), "the local command's status: {outcome:?}");
    let line = format!("gate: not routed: builder's gate is not installed where only administrators can change it ({why}); run gate check --builder");
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder install; running here"), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(fallbacks(&events(&case)), ["install"]);
}

/// A [`client_case`] whose repo declares one sibling the routing file allows, the folder `make` makes beside the case
/// folder, named after it, and whose routing file gives its repo the output globs `outputs`: the case, its folder name,
/// the sibling's name and its folder. A sibling an earlier run left is removed first.
pub(super) fn pair_case(name: &str, outputs: &[&str], make: impl FnOnce(&Case, &Path)) -> (Case, String, String, PathBuf) {
    let case = client_case(name);
    let leaf = case
        .folder()
        .file_name()
        .expect("the case folder has a name")
        .to_string_lossy()
        .into_owned();
    let (repo, sibling_name) = (repo_name(&leaf), format!("{leaf}-sibling"));
    let sibling = case.folder().parent().expect("the case folder has a parent").join(&sibling_name);
    if sibling.exists() {
        fs::remove_dir_all(&sibling).expect("a sibling left by an earlier run can be removed");
    }
    make(&case, &sibling);
    write(&case, ".gate/siblings.toml", &format!("siblings = [\"{sibling_name}\"]\n"));
    let routing = json!({
        "host": HOST,
        "slots": 2,
        "repos": { &repo: ["*tool*"] },
        "outputs": { &repo: outputs },
        "siblings": { &repo: [&sibling_name] },
    });
    write_remote_json(&case, &routing.to_string());
    (case, leaf, sibling_name, sibling)
}

/// Makes `sibling` a repository of its own holding `lib.txt`.
pub(super) fn sibling_repository(case: &Case, sibling: &Path) {
    fs::create_dir_all(sibling).expect("the sibling can be made");
    git_out(case, &[], &["-C", &shown(sibling), "init", "--quiet"]);
    fs::write(sibling.join("lib.txt"), "lib\n").expect("the sibling's file can be written");
}

/// The commit `name` names in the builder's bare repository of the sibling `sibling`, empty when it names none.
fn sibling_builder_ref(case: &Case, sibling: &str, name: &str) -> String {
    let bare = shown(&case.folder().join("builder").join(format!("{}.git", repo_name(sibling))));
    git_out(case, &[], &["--git-dir", &bare, "for-each-ref", "--format=%(objectname)", name])
}

/// The lines of `said` that say a pair's push.
fn pair_pushes(said: &str) -> Vec<&str> {
    said.lines().filter(|line| line.starts_with("gate: remote pushed snapshot ")).collect()
}

/// A run whose repo declares a sibling is sent as a pair: the main repository and the sibling are each snapshotted and
/// pushed to a bare of their own, the main one first, each push said; the run call carries the `Pair` and the sibling's
/// snapshot; and the command's changes in both worktrees come back.
#[test]
fn a_pair_run_pushes_each_repo_main_first_and_syncs_both_back() {
    let (case, leaf, name, sibling) = pair_case("client-pair", &[], sibling_repository);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let on_sibling = format!(r"..\{name}\made-on-sibling.txt");

    let outcome = finish(client_gate(
        &case,
        "out.log",
        &steps,
        &[r".\tool.exe", "mark", "made-on-box.txt", "mark", &on_sibling],
    ));
    let key = key_of(&events(&case));
    let saved = git_out(&case, &[], &["-C", &shown(&sibling), "rev-parse", &format!("refs/gate-remote/{key}")]);
    let synced = sibling.join("made-on-sibling.txt").exists();
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said = &outcome.stderr;
    let pushes = pair_pushes(said);
    assert_eq!(pushes.len(), 2, "one push line a repo: {outcome:?}");
    assert!(pushes[0].starts_with(&format!("gate: remote pushed snapshot {leaf} ")), "{pushes:?}");
    assert!(pushes[1].starts_with(&format!("gate: remote pushed snapshot {name} ")), "{pushes:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    let pushed = sibling_builder_ref(&case, &name, &format!("refs/gate/{key}"));
    assert_eq!(pushed, saved, "the sibling's saved ref is the snapshot its bare holds");
    // The box lays out a pair's work folder, each repository checked out at its snapshot, only from the run's `Pair`
    // and `SiblingSnapshots`.
    let laid_out = case.folder().join("builder").join("work").join(&key);
    assert!(
        laid_out.join(&name).join("lib.txt").exists(),
        "the sibling was checked out beside the main repository"
    );
    assert!(
        case.folder().join("made-on-box.txt").exists(),
        "the main repository's change came back: {outcome:?}"
    );
    assert!(synced, "the sibling's change came back: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
}

/// A pair run's box checks the main repository out in `{work}\{leaf}` and the sibling in `{work}\{name}`: a fetched
/// test host's manifest that records both trees, in either case, names the main worktree and the sibling's own root on
/// this PC, never the work folder's place under the main worktree. A first run names the work-folder key.
#[test]
fn a_pair_s_fetched_output_is_rewritten_to_the_main_s_pc_root() {
    let (case, leaf, name, sibling) = pair_case("client-pair-rewrite", &BIN, sibling_repository);
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\n"));
    first_run(&case);
    let builder = std::path::absolute(case.folder().join("builder")).expect("the builder root has a full path");
    let work = format!(r"{}\work\{}", builder.display(), key_of(&events(&case)));
    // After a `_`, neither path is one the router rewrites into the work folder: the builder sits in the worktree here.
    let recorded = format!(r"Root_{}\{leaf}\src\App;Lib_{work}\{name}\lib", work.to_uppercase());
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\MvcTestingAppManifest.json", &recorded];

    let outcome = finish(client_gate(&case, "second.log", &second, &command));
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let root = std::path::absolute(git_out(&case, &[], &["rev-parse", "--show-toplevel"])).expect("git names the worktree by a full path");
    let sibling_root = root.parent().expect("the worktree has a parent").join(&name);
    let expected = format!(r"Root_{}\src\App;Lib_{}\lib", root.display(), sibling_root.display());
    assert_eq!(worktree_text(&case, r"bin\MvcTestingAppManifest.json"), expected, "{outcome:?}");
}

/// A sibling whose push fails, through `local-ssh` after the main one's went through, keeps the run here as
/// `unreachable` with no run call: the main repository's ref is saved, the sibling's is not.
#[test]
fn a_pair_run_whose_sibling_push_fails_falls_back_with_no_run_call() {
    let (case, _, name, sibling) = pair_case("client-pair-push-fails", &[], sibling_repository);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "fail-other"]);

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "out", "ran here"],
    ));
    let sibling_refs = git_out(&case, &[], &["-C", &shown(&sibling), "for-each-ref", "refs/gate-remote/"]);
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    // The attempt's lines, the main push's progress among them, are noted after the command's output, so the log's
    // 20-line tail on stdout may not reach it: the log shows the command ran.
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.lines().any(|line| line == "ran here"), "the command ran here: {log}");
    let said = &outcome.stderr;
    assert!(
        said.contains(&format!("gate: pushing the snapshot of {name} to ")),
        "the failed push names the sibling: {outcome:?}"
    );
    assert_eq!(pair_pushes(said).len(), 1, "only the main repository's push is said: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "git-receive-pack", "git-receive-pack"], "no run call");
    let events = events(&case);
    assert_eq!(fallbacks(&events), ["unreachable"]);
    let main_saved = git_out(&case, &[], &["for-each-ref", "--format=%(refname)", "refs/gate-remote/"]);
    assert_eq!(
        main_saved,
        format!("refs/gate-remote/{}", key_of(&events)),
        "the main repository's ref is saved"
    );
    assert_eq!(sibling_refs, "", "the sibling's ref is not saved");
}

/// A pair run whose sibling's bare repository is new while the main one's is there: the preflight creates the
/// sibling's alone, so each push goes by its own bare's `created` flag: the sibling's sends no bridge, the main one's
/// does.
#[test]
fn a_pair_s_new_sibling_bare_takes_no_bridge_while_the_main_one_does() {
    let (case, _, name, sibling) = pair_case("client-pair-new-bare", &[], sibling_repository);
    let first_steps = ssh_steps(&case, "first", &["ok", "ok"]);
    let first = finish(client_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "first"]));
    assert_eq!(first.code, Some(0), "{first:?}");
    // The builder keeps the main repository's bare and loses the sibling's; its work folder, whose checkouts came from
    // the bare that is gone, goes too, as a builder laying the pair out anew has none.
    let key = key_of(&events(&case));
    let builder = case.folder().join("builder");
    fs::remove_dir_all(builder.join(format!("{}.git", repo_name(&name)))).expect("the sibling's bare can be removed");
    fs::remove_dir_all(builder.join("work").join(&key)).expect("the work folder can be removed");
    let steps = ssh_steps(&case, "second", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &steps, &[r".\tool.exe", "out", "second"]));
    let sibling_pushed = sibling_builder_ref(&case, &name, &format!("refs/gate/{key}"));
    let sibling_bridge = sibling_builder_ref(&case, &name, &format!("refs/gate-bridge/{key}"));
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let log = log_texts(&case.folder().join("second.log"));
    let created: Vec<&str> = log.lines().filter(|line| line.starts_with("gate: remote created ")).collect();
    assert_eq!(created.len(), 1, "the sibling's bare alone is created: {log}");
    assert!(created[0].ends_with(&format!("{}.git", repo_name(&name))), "{created:?}");
    assert_ne!(sibling_pushed, "", "the sibling's snapshot was pushed: {outcome:?}");
    assert_eq!(sibling_bridge, "", "no bridge to the new bare: {outcome:?}");
    assert_ne!(
        builder_ref(&case, &format!("refs/gate-bridge/{key}")),
        "",
        "the main one's bridge: {outcome:?}"
    );
}

/// `--test-remote-snapshot` snapshots every repository of a pair, the main one first: one line a repository, each
/// commit made in its own repository.
#[test]
fn the_snapshot_seam_snapshots_every_repo_of_a_pair() {
    let (case, _, _, sibling) = pair_case("snapshot-pair", &[], sibling_repository);

    let outcome = finish(case.gate(&snapshot_words("out.log", &[])));
    let commits: Vec<String> = outcome
        .stderr
        .lines()
        .filter_map(|line| line.strip_prefix(SAID))
        .map(|line| line.split_once(" bridge ").expect("the line names the bridge").0.to_owned())
        .collect();
    let in_sibling = commits
        .get(1)
        .map(|commit| git_out(&case, &[], &["-C", &shown(&sibling), "cat-file", "-t", commit]));
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(commits.len(), 2, "one snapshot line a repository: {outcome:?}");
    assert_eq!(git_out(&case, &[], &["cat-file", "-t", &commits[0]]), "commit", "the main one's first");
    assert_eq!(in_sibling.as_deref(), Some("commit"), "the sibling's second, in its own repository");
}

/// A pair whose sibling alone pins an SDK the builder lacks is refused `parity` before any push, the mismatch line
/// naming the sibling's repository: each repository's own `global.json` is checked.
#[test]
fn a_pair_whose_sibling_pins_an_sdk_the_builder_lacks_is_refused_parity() {
    let (case, _, name, sibling) = pair_case("client-pair-parity", &[], |case, sibling| {
        sibling_repository(case, sibling);
        fs::write(sibling.join("global.json"), r#"{"sdk": {"version": "999.0.0"}}"#).expect("the sibling's global.json can be written");
    });
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]);
    gate.env("PATH", box_path_with_dotnet(&case, &[BUILDER_SDK]));

    let (pid, outcome) = finish_with_pid(gate);
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    assert_eq!(fallbacks(&own(&case, pid)), ["parity"], "{outcome:?}");
    let mismatch = format!(
        "gate: remote toolchain mismatch: {} needs .NET SDK 999.0.0 (rollForward patch); builder has {BUILDER_SDK}",
        repo_name(&name)
    );
    assert!(outcome.stderr.lines().any(|said| said == mismatch), "{mismatch}: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "the preflight alone reached the builder");
    let key = key_of(&events(&case));
    assert_eq!(
        builder_ref(&case, &format!("refs/gate/{key}")),
        "",
        "nothing was pushed, the main repository first"
    );
}

/// A run of one repository whose push fails, through `local-ssh`, keeps the run here as `unreachable` with no run call,
/// and the command runs here, its output on standard output.
#[test]
fn a_run_whose_push_fails_falls_back_and_runs_here() {
    let case = client_case("client-push-fails");
    let steps = ssh_steps(&case, "ssh", &["ok", "fail-other"]);

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "out", "ran here"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "the command ran here: {outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.lines().any(|line| line == "ran here"), "the command ran here: {log}");
    assert!(outcome.stderr.contains("gate: pushing the snapshot to "), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "git-receive-pack"], "no run call");
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
}

/// A sibling that is a worktree of the main repository's own repository shares its refs, so the run stays here naming
/// it, before anything reaches the builder.
#[test]
fn a_pair_whose_sibling_shares_the_main_repository_stays_here() {
    let (case, leaf, name, sibling) = pair_case("client-pair-shared", &[], |case, sibling| {
        git(case, &["commit", "--quiet", "--allow-empty", "-m", "base"]);
        git(case, &["worktree", "add", "--quiet", "--detach", &shown(sibling)]);
    });
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));
    git(&case, &["worktree", "remove", "--force", &shown(&sibling)]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    let line = format!("gate: not routed: sibling {name} shares its git repository with {leaf}");
    assert_eq!(outcome.stderr.lines().filter(|said| *said == line).count(), 1, "said once: {outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no call reached the builder");
    assert_eq!(fallbacks(&events(&case)), ["client"]);
}

/// The folder beside the sibling `sibling` named as it is with `suffix` after its name, emptied of what an earlier run
/// left there.
fn beside_sibling(sibling: &Path, suffix: &str) -> PathBuf {
    let name = sibling.file_name().expect("the sibling has a name").to_string_lossy();
    let folder = sibling.with_file_name(format!("{name}{suffix}"));
    if folder.exists() {
        fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
    }
    folder
}

/// Asserts the command `out ran here` ran here, as the case's `out.log` shows: the attempt's lines, the pushes'
/// progress among them, come after the command's output, so the log's 20-line tail on stdout may not reach it.
fn assert_ran_here(case: &Case) {
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.lines().any(|line| line == "ran here"), "the command ran here: {log}");
}

/// The files of the commit `commit` in the builder's bare repository of the sibling `name`.
fn sibling_builder_files(case: &Case, name: &str, commit: &str) -> Vec<String> {
    let bare = shown(&case.folder().join("builder").join(format!("{}.git", repo_name(name))));
    let listed = git_out(case, &[], &["--git-dir", &bare, "ls-tree", "-r", "--name-only", commit]);
    listed.lines().map(str::to_owned).collect()
}

/// `core.worktree` written into a sibling's git config once the main repository's snapshot is taken, after the check
/// of where git finds it (`--test-remote-tamper after-first-snapshot:worktree`), moves none of the client's git calls:
/// the sibling's snapshot is of its own folder, so the canary folder's file is not pushed, and the check after its
/// save keeps the run here naming it, with no run call.
#[test]
fn a_sibling_whose_config_names_another_work_tree_after_its_check_pushes_none_of_it() {
    let (case, _, name, sibling) = pair_case("client-pair-worktree-later", &[], sibling_repository);
    let canary = beside_sibling(&sibling, "-canary");
    fs::create_dir_all(&canary).expect("the canary folder can be made");
    fs::write(canary.join("canary.txt"), "canary\n").expect("the canary's file can be written");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-tamper", "after-first-snapshot:worktree"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran here"]));
    let events = events(&case);
    let pushed = sibling_builder_ref(&case, &name, &format!("refs/gate/{}", key_of(&events)));
    let files = if pushed.is_empty() {
        Vec::new()
    } else {
        sibling_builder_files(&case, &name, &pushed)
    };
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");
    fs::remove_dir_all(&canary).expect("the canary folder can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_ran_here(&case);
    let line = format!("gate: not routed: sibling {name} changed its git location while it was held");
    assert!(outcome.stderr.lines().any(|said| said == line), "{line}: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "no run call");
    assert_eq!(fallbacks(&events), ["client"]);
    assert!(files.iter().any(|file| file == "lib.txt"), "the sibling's own file was pushed: {files:?}");
    assert!(
        !files.iter().any(|file| file == "canary.txt"),
        "the canary's file was not pushed: {files:?}"
    );
}

/// Writes a `reference-transaction` hook into the folder `hooks` that appends a line to the file `marker`.
fn marking_hook(hooks: &Path, marker: &Path) {
    fs::create_dir_all(hooks).expect("the hooks folder can be made");
    let marker = marker.to_string_lossy().replace('\\', "/");
    fs::write(hooks.join("reference-transaction"), format!("#!/bin/sh\necho ran >> '{marker}'\n")).expect("the hook can be written");
}

/// No hook of a pair's repository runs from the client's git calls: the main repository's `reference-transaction`
/// hook in its own hooks folder, and the sibling's in a folder its `core.hooksPath` names, each appending to a marker
/// file, leave no marker after a pair run that went remote and saved both repositories' refs.
#[test]
fn no_hook_of_a_pair_s_repositories_runs_from_the_client_s_git_calls() {
    let (case, _, _, sibling) = pair_case("client-pair-hooks", &[], sibling_repository);
    let markers = beside_sibling(&sibling, "-markers");
    let hooks = beside_sibling(&sibling, "-hooks");
    let (main_marker, sibling_marker) = (markers.join("main"), markers.join("sibling"));
    marking_hook(&case.folder().join(".git").join("hooks"), &main_marker);
    marking_hook(&hooks, &sibling_marker);
    git_out(&case, &[], &["-C", &shown(&sibling), "config", "core.hooksPath", &shown(&hooks)]);
    fs::create_dir_all(&markers).expect("the markers' folder can be made");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran there"]));
    let saved = git_out(&case, &[], &["-C", &shown(&sibling), "for-each-ref", "refs/gate-remote/"]);
    let marked = [main_marker.exists(), sibling_marker.exists()];
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");
    fs::remove_dir_all(&markers).expect("the markers' folder can be removed");
    fs::remove_dir_all(&hooks).expect("the hooks folder can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "{outcome:?}");
    assert_ne!(saved, "", "the sibling's ref was saved, a ref update its hook would see");
    assert_eq!(marked, [false, false], "no hook ran, the main one's then the sibling's: {outcome:?}");
}

/// A sibling whose ref cannot be saved after its push, its `refs/gate-remote` a file, keeps the run here naming it,
/// with no run call.
#[test]
fn a_sibling_whose_save_fails_stays_here_naming_it() {
    let (case, _, name, sibling) = pair_case("client-pair-save-fails", &[], |case, sibling| {
        sibling_repository(case, sibling);
        fs::write(sibling.join(".git").join("refs").join("gate-remote"), "").expect("the refs folder's place can be taken");
    });
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]));
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_ran_here(&case);
    let start = format!("gate: not routed: sibling {name} cannot be sent: ");
    assert!(outcome.stderr.lines().any(|said| said.starts_with(&start)), "{start}: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "no run call");
    assert_eq!(fallbacks(&events(&case)), ["client"]);
}

/// A pair whose main repository and sibling are each a .NET tree with no pin, so each needs this PC's newest major,
/// run with this PC's `PATH` `path`: the run falls back `parity`, the sibling's mismatch line naming 999, the major
/// this PC's stub lists. Its outcome.
fn a_pair_needing_the_pcs_major(case: &Case, sibling: &Path, name: &str, path: &OsString) -> Outcome {
    write(case, "src/App/App.csproj", "<Project />\n");
    fs::create_dir_all(sibling.join("src")).expect("the sibling's src folder can be made");
    fs::write(sibling.join("src").join("Lib.csproj"), "<Project />\n").expect("the sibling's project can be written");
    let steps = ssh_steps(case, "ssh", &["ok", "ok"]);
    let mut gate = dotnet_gate(case, "out.log", &steps, &[r".\tool.exe", "out", "ran here"]);
    gate.env("PATH", path);

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    assert_eq!(fallbacks(&own(case, pid)), ["parity"], "{outcome:?}");
    let mismatch = format!(
        "gate: remote toolchain mismatch: {} needs a .NET 999 SDK; builder has {BUILDER_SDK}",
        repo_name(name)
    );
    assert!(outcome.stderr.lines().any(|said| said == mismatch), "{mismatch}: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "the preflight alone reached the builder");
    outcome
}

/// This PC's SDK list is read once per run however many of a pair's repositories need its major: the stub that lists
/// it notes each run.
#[test]
fn a_pair_reads_the_pcs_sdk_list_once() {
    let (case, _, name, sibling) = pair_case("client-pair-sdks-once", &[], sibling_repository);
    let marker = case.folder().join("ssh").join("pc-dotnet-ran");
    let pc = format!(">>\"{}\" echo ran\r\necho 999.0.100 [C:\\stub]\r\n", marker.display());
    let dotnet = box_path_with_pc_dotnet(&case, &[BUILDER_SDK], &pc);

    a_pair_needing_the_pcs_major(&case, &sibling, &name, &dotnet.path);
    let ran = fs::read_to_string(&marker).expect("this PC's dotnet ran");
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(ran.lines().count(), 1, "this PC's SDK list was read once: {ran:?}");
}

/// A folder inside a pair's sibling that `PATH` names before this PC's dotnet is skipped for this PC's SDK list, as one
/// inside the main worktree is: the `dotnet.cmd` there, which lists the builder's SDK and notes each run, never runs.
#[test]
fn a_path_folder_inside_a_sibling_is_not_run_for_the_sdk_list() {
    let (case, _, name, sibling) = pair_case("client-pair-path-in-sibling", &[], sibling_repository);
    let inside = sibling.join("bin");
    fs::create_dir_all(&inside).expect("the folder inside the sibling can be made");
    let marker = case.folder().join("ssh").join("sibling-dotnet-ran");
    let decoy = format!("@echo off\r\n>>\"{}\" echo ran\r\n{}", marker.display(), listing(&[BUILDER_SDK]));
    fs::write(inside.join("dotnet.cmd"), decoy).expect("the dotnet inside the sibling can be written");
    let dotnet = box_path_with_pc_dotnet(&case, &[BUILDER_SDK], "echo 999.0.100 [C:\\stub]\r\n");
    let mut folders: Vec<PathBuf> = std::env::split_paths(&dotnet.path).collect();
    folders.insert(1, inside);
    let path = std::env::join_paths(folders).expect("the folders join into a PATH");

    a_pair_needing_the_pcs_major(&case, &sibling, &name, &path);
    let ran = marker.exists();
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert!(!ran, "the dotnet.cmd inside the sibling ran");
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
    let log = log_texts(&case.folder().join("out.log"));
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

    a_parity_refusal(&case, &PC_SDKS_ANSWERING, "a .NET 999 SDK", Some(&dotnet.path));

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

    a_parity_refusal(&case, &PC_SDKS_ANSWERING, "a .NET 999 SDK", Some(&path));

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

    a_parity_refusal(&case, &PC_SDKS_ANSWERING, "a .NET 999 SDK", Some(&path));

    let ran = fs::read_to_string(&marker).expect("the builder ran the dotnet.cmd behind the junction");
    assert_eq!(
        ran.lines().count(),
        1,
        "the dotnet.cmd behind the junction ran for the builder alone: {ran:?}"
    );
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
    let log = log_texts(&case.folder().join("out.log"));
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
    let mut gate = dotnet_gate(&case, "out.log", &steps, &[r".\tool.exe", "mark", RAN, "out", "ran there"]);
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

/// The repo's toolchain manifest, as a tool case writes it.
const MANIFEST: &str = ".gate/toolchain.toml";

/// Whether `folder` holds the program `program` under any of the extensions a `PATHEXT` search tries.
fn holds(folder: &Path, program: &str) -> bool {
    ["exe", "cmd", "bat", "com"]
        .iter()
        .any(|extension| folder.join(format!("{program}.{extension}")).is_file())
}

/// The `PATH` of a tool case's builder: a folder in the case's `ssh` folder first, holding a `dotnet.cmd` that runs
/// `dotnet` and a `<program>.cmd` for each `(program, body)` of `tools`, each after `@echo off`; then this PC's own
/// `PATH` without the folders holding a `dotnet` or a program of `missing`, so the builder finds none of those. A
/// folder it would drop for a missing tool that also holds a `git`, a `pwsh` or an `ssh` the run itself needs fails the
/// case with why, since taking the tool off the builder's `PATH` would take those too.
fn box_path_with_tools(case: &Case, dotnet: &str, tools: &[(&str, &str)], missing: &[&str]) -> OsString {
    let stub = case.folder().join("ssh").join("stub-tools");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    fs::write(stub.join("dotnet.cmd"), format!("@echo off\r\n{dotnet}")).expect("the stub dotnet can be written");
    for (program, body) in tools {
        fs::write(stub.join(format!("{program}.cmd")), format!("@echo off\r\n{body}")).expect("a tool stub can be written");
    }
    let own = std::env::var_os("PATH").expect("PATH is set");
    #[expect(
        clippy::panic,
        reason = "a folder that would take a git, a pwsh or an ssh off the builder's PATH fails the case saying so"
    )]
    let kept = std::env::split_paths(&own).filter(|folder| {
        if holds_dotnet(folder) {
            return false;
        }
        let Some(program) = missing.iter().find(|program| holds(folder, program)) else {
            return true;
        };
        if let Some(other) = ["git", "pwsh", "ssh"].into_iter().find(|other| holds(folder, other)) {
            panic!(
                "{} holds both {program} and {other}: the case cannot take {program} off the builder's PATH without taking {other}",
                folder.display()
            );
        }
        false
    });
    std::env::join_paths(std::iter::once(stub).chain(kept)).expect("the folders join into a PATH")
}

/// The mismatch line of a case whose builder lacks `need` and has `has` of it.
fn mismatch_of(case: &Case, need: &str, has: &str) -> String {
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    format!("gate: remote toolchain mismatch: {repo} needs {need}; builder has {has}")
}

/// Whether `line` is a mismatch line or the parity line.
fn is_parity_line(line: &str) -> bool {
    line.starts_with("gate: remote toolchain mismatch: ") || line == PARITY_LINE
}

/// A routed run of a case whose builder, given the `PATH` `box_path` and the `GATE_GODOT_ROOT` `godot`, lacks what the
/// case's tree needs: it falls back `parity` after the preflight alone, so nothing is pushed, says the mismatch lines
/// `said` and no other, in their order, then the one parity line, on the console and, but the parity line, in the log,
/// and runs the command here once.
fn a_toolchain_refusal(case: &Case, box_path: &OsString, said: &[String], godot: Option<&Path>) -> Outcome {
    a_toolchain_refusal_with(case, box_path, said, |gate| set_godot_root(gate, godot))
}

/// [`a_toolchain_refusal`] with the builder's install roots set by `roots` on the gate's command.
fn a_toolchain_refusal_with(case: &Case, box_path: &OsString, said: &[String], roots: impl FnOnce(&mut Command)) -> Outcome {
    let steps = ssh_steps(case, "ssh", &["ok", "ok"]);
    let mut gate = dotnet_gate(case, "out.log", &steps, &[r".\tool.exe", "mark", RAN, "out", "ran"]);
    gate.env("PATH", box_path);
    roots(&mut gate);

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    let events = own(case, pid);
    assert_eq!(fallbacks(&events), ["parity"], "{outcome:?}");
    let want: Vec<&str> = said.iter().map(String::as_str).chain([PARITY_LINE]).collect();
    let console: Vec<&str> = outcome.stderr.lines().filter(|line| is_parity_line(line)).collect();
    assert_eq!(console, want, "{outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    let logged: Vec<&str> = log.lines().filter(|line| line.starts_with("gate: remote toolchain mismatch: ")).collect();
    assert_eq!(logged, said, "{log}");
    assert_eq!(calls(&steps), ["preflight"], "the preflight alone reached the builder");
    assert_eq!(builder_ref(case, &format!("refs/gate/{}", key_of(&events))), "", "nothing was pushed");
    assert_eq!(runs(case), (1, 0), "the command ran once, here: {outcome:?}");
    outcome
}

/// A routed run of a case whose builder, given the `PATH` `box_path`, the preflight seams `seams` and the
/// `GATE_GODOT_ROOT` `godot`, serves what the case's tree needs: it goes remote with no mismatch line, and runs the
/// command once, on the builder.
fn a_toolchain_served(case: &Case, box_path: &OsString, seams: &str, godot: Option<&Path>) -> Outcome {
    let steps = ssh_steps(case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &PC_SDKS_ANSWERING,
        preflight_seams: seams,
        ..Tuned::default()
    };
    let mut gate = client_gate_tuned(case, "out.log", &steps, &tuned, &[r".\tool.exe", "mark", RAN, "out", "ran there"]);
    gate.env("PATH", box_path);
    set_godot_root(&mut gate, godot);

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.lines().any(is_parity_line), "{outcome:?}");
    assert_eq!(fallbacks(&own(case, pid)), Vec::<&str>::new(), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(runs(case), (0, 1), "the command ran once, on the builder: {outcome:?}");
    outcome
}

/// Sets the gate's `GATE_GODOT_ROOT` to `root` — which the builder's godot probe reads, and no run of this PC's own —
/// or takes it out of the environment when the case gives none, so a machine with one set cannot serve a case by
/// accident.
fn set_godot_root(gate: &mut Command, root: Option<&Path>) {
    if let Some(root) = root {
        gate.env("GATE_GODOT_ROOT", root);
    } else {
        gate.env_remove("GATE_GODOT_ROOT");
    }
}

/// A builder that finds no tool the manifest declares is refused `parity`, though the tree is no .NET tree, which the
/// SDK check skips.
#[test]
fn a_tool_the_builder_lacks_is_refused_parity_in_a_tree_that_is_not_dotnet() {
    let case = client_case("client-tool-missing");
    write(&case, MANIFEST, "[tools]\npnpm = \"10\"\n");
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &["pnpm"]);

    a_toolchain_refusal(&case, &box_path, &[mismatch_of(&case, "pnpm 10.0.0 or later", "none")], None);
}

#[test]
fn a_tool_older_than_the_manifest_s_minimum_is_refused_parity() {
    let case = client_case("client-tool-too-old");
    write(&case, MANIFEST, "[tools]\ncargo-nextest = \"0.9.100\"\n");
    let nextest = "echo cargo-nextest 0.9.80 (abc 2026-01-01)\r\n";
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[("cargo-nextest", nextest)], &[]);

    a_toolchain_refusal(&case, &box_path, &[mismatch_of(&case, "cargo-nextest 0.9.100 or later", "0.9.80")], None);
}

/// The SDK's mismatch line comes first, then each tool's in the manifest's order, not the names', then one parity line.
#[test]
fn the_sdk_s_mismatch_line_comes_before_the_tools_in_the_manifest_s_order() {
    let case = client_case("client-tool-order");
    write(&case, "global.json", r#"{"sdk": {"version": "999.0.0"}}"#);
    write(&case, "src/App/App.csproj", "<Project />\n");
    write(&case, MANIFEST, "[tools]\npnpm = \"10\"\ncargo-nextest = \"0.9.100\"\n");
    let nextest = "echo cargo-nextest 0.9.80 (abc 2026-01-01)\r\n";
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[("cargo-nextest", nextest)], &["pnpm"]);

    a_toolchain_refusal(
        &case,
        &box_path,
        &[
            mismatch_of(&case, ".NET SDK 999.0.0 (rollForward patch)", BUILDER_SDK),
            mismatch_of(&case, "pnpm 10.0.0 or later", "none"),
            mismatch_of(&case, "cargo-nextest 0.9.100 or later", "0.9.80"),
        ],
        None,
    );
}

#[test]
fn a_tool_the_builder_serves_goes_remote() {
    let case = client_case("client-tool-served");
    write(&case, MANIFEST, "[tools]\ncargo-nextest = \"0.9.100\"\npnpm = \"\"\n");
    let nextest = "echo cargo-nextest 0.9.148 (abc 2026-10-07)\r\n";
    let tools = [("cargo-nextest", nextest), ("pnpm", "echo 10.0.0\r\n")];
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &tools, &[]);

    let outcome = a_toolchain_served(&case, &box_path, ANSWERING, None);

    assert!(!outcome.stderr.contains("did not probe"), "{outcome:?}");
}

/// A tool the builder found but whose version it could not read is not checked against its minimum: it is skipped with
/// its line, and the run goes remote.
#[test]
fn a_tool_found_with_no_readable_version_is_skipped_with_its_line() {
    let case = client_case("client-tool-present");
    write(&case, MANIFEST, "[tools]\npnpm = \"10\"\n");
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[("pnpm", "echo pnpm, no version here\r\n")], &[]);

    let outcome = a_toolchain_served(&case, &box_path, ANSWERING, None);

    let skipped = "gate: builder did not probe pnpm: found, version not readable; not checking it";
    assert!(outcome.stderr.lines().any(|line| line == skipped), "{skipped}: {outcome:?}");
}

/// A builder whose SDK list cannot be read says no sdks line, which skips the SDK check; its tools are still checked.
#[test]
fn a_builder_with_no_sdks_line_still_has_its_tools_checked() {
    let case = client_case("client-tool-no-sdks");
    write(&case, "src/App/App.csproj", "<Project />\n");
    write(&case, MANIFEST, "[tools]\npnpm = \"10\"\n");
    let box_path = box_path_with_tools(&case, "exit /b 3\r\n", &[], &["pnpm"]);

    a_toolchain_refusal(&case, &box_path, &[mismatch_of(&case, "pnpm 10.0.0 or later", "none")], None);
}

/// Each tool the run cannot check is skipped with its line, and the run goes remote: a bad entry, a tool the builder's
/// probe does not answer for in its 1 s bound, an unknown name, and a name that is no tool name at all. Only the
/// probe's bound ends the hanging one, and no other line waits on a probe, so no load changes what the run says.
#[test]
fn each_tool_the_run_cannot_check_is_skipped_with_its_line_and_the_run_goes_remote() {
    let case = client_case("client-tool-skipped");
    write(
        &case,
        MANIFEST,
        "[tools]\nffmpeg = \"x\"\nnode = \"24\"\nnodejs = \"24\"\n\"No de\" = \"1\"\n",
    );
    let go = Go(case.folder().join("ssh").join("node-go"));
    let held = case.folder().join("ssh").join("node-held");
    let hang = format!("\"{FAKE_TOOL}\" wait \"{}\" >\"{}\"\r\n", go.path().display(), held.display());
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[("node", hang.as_str())], &[]);

    let outcome = a_toolchain_served(&case, &box_path, "--test-probe-limit-ms 1000 --test-dotnet-limit-ms 3600000", None);

    let skipped = [
        "gate: .gate/toolchain.toml: tools.ffmpeg is not a version: \"x\"; skipping it",
        "gate: builder did not probe node: no answer in 1 s; not checking it",
        "gate: .gate/toolchain.toml names unknown tool nodejs; skipping it",
        "gate: .gate/toolchain.toml names unknown tool \"No de\"; skipping it",
    ];
    let said: Vec<&str> = outcome
        .stderr
        .lines()
        .filter(|line| line.starts_with("gate: .gate/toolchain.toml") || line.starts_with("gate: builder did not probe "))
        .collect();
    assert_eq!(said, skipped, "{outcome:?}");
    // A stub that started holds its output file open until its job's kill ends it; one the kill came first never made it.
    if held.exists() {
        wait_until_closed(&held);
    }
}

/// A manifest that is not TOML is skipped whole with its line, and the run goes remote.
#[test]
fn an_unreadable_manifest_is_skipped_with_its_line_and_the_run_goes_remote() {
    let case = client_case("client-tool-unreadable");
    write(&case, MANIFEST, "[tools\npnpm = \"10\"\n");
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &[]);

    let outcome = a_toolchain_served(&case, &box_path, ANSWERING, None);

    let said: Vec<&str> = outcome
        .stderr
        .lines()
        .filter(|line| line.starts_with("gate: .gate/toolchain.toml"))
        .collect();
    assert!(
        said.len() == 1 && said[0].starts_with("gate: .gate/toolchain.toml: line 1, column ") && said[0].ends_with("; skipping it"),
        "{outcome:?}"
    );
}

/// The manifest is found at the repo root whatever the letter case of its folder and file names.
#[test]
fn a_manifest_named_in_another_case_is_read() {
    let case = client_case("client-tool-manifest-case");
    write(&case, ".Gate/Toolchain.TOML", "[tools]\npnpm = \"10\"\n");
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &["pnpm"]);

    a_toolchain_refusal(&case, &box_path, &[mismatch_of(&case, "pnpm 10.0.0 or later", "none")], None);
}

/// The manifest is read from the snapshot, which carries the worktree's uncommitted edits: the committed manifest
/// declares nothing, the edit a tool the builder lacks.
#[test]
fn an_uncommitted_manifest_edit_is_read() {
    let case = client_case("client-tool-uncommitted");
    write(&case, MANIFEST, "[tools]\n");
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "no tools"]);
    write(&case, MANIFEST, "[tools]\npnpm = \"10\"\n");
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &["pnpm"]);

    a_toolchain_refusal(&case, &box_path, &[mismatch_of(&case, "pnpm 10.0.0 or later", "none")], None);
}

/// Makes a declarations folder under the case's LOCALAPPDATA holding each `(name, text)` of `files`, and writes the
/// routing file [`client_case`] writes, naming that folder for the case's repo; the folder.
fn declarations_folder(case: &Case, files: &[(&str, &str)]) -> PathBuf {
    let folder = case.local_app_data().join("declarations");
    fs::create_dir_all(&folder).expect("the declarations folder can be made");
    for (name, text) in files {
        fs::write(folder.join(name), text).expect("a declaration file can be written");
    }
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": HOST,
        "slots": 2,
        "repos": { &repo: ["*tool*"] },
        "outputs": { &repo: [] },
        "declarations": { &repo: folder.to_string_lossy() },
    });
    write_remote_json(case, &routing.to_string());
    folder
}

/// A snapshot whose tree has no manifest reads the copy in the declarations folder, which feeds parity as a tree's
/// does, and the log says where it was read from.
#[test]
fn a_manifest_in_the_declarations_folder_is_read_when_the_tree_has_none() {
    let case = client_case("client-tool-declared");
    let folder = declarations_folder(&case, &[("toolchain.toml", "[tools]\npnpm = \"10\"\n")]);
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &["pnpm"]);

    let outcome = a_toolchain_refusal(&case, &box_path, &[mismatch_of(&case, "pnpm 10.0.0 or later", "none")], None);

    let read = format!("gate: .gate/toolchain.toml read from {}", folder.join("toolchain.toml").display());
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.lines().any(|line| line == read), "{read}: {log}");
    assert!(!outcome.stderr.contains("read from"), "the line is the log's alone: {outcome:?}");
}

/// The tree's own manifest wins: the declarations folder's copy, which declares a tool the builder lacks, is not read,
/// and the log alone notes the copy the tree's manifest shadows.
#[test]
fn the_tree_s_manifest_wins_over_the_declarations_folder() {
    let case = client_case("client-tool-declared-tree-wins");
    write(&case, MANIFEST, "[tools]\n");
    let folder = declarations_folder(&case, &[("toolchain.toml", "[tools]\npnpm = \"10\"\n")]);
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &["pnpm"]);

    let outcome = a_toolchain_served(&case, &box_path, ANSWERING, None);

    let log = log_texts(&case.folder().join("out.log"));
    assert!(!log.contains("read from"), "{log}");
    let note = format!("gate: .gate/toolchain.toml shadows {}", folder.join("toolchain.toml").display());
    assert!(log.lines().any(|line| line == note), "{note}: {log}");
    assert!(!outcome.stderr.contains("shadows"), "the note is the log's alone: {outcome:?}");
}

/// A snapshot whose `.gate` is a file has no manifest of its own, yet reads no declarations folder: the repo's `.gate`
/// answers for it.
#[test]
fn a_snapshot_whose_gate_is_a_file_reads_no_declarations_folder() {
    let case = client_case("client-tool-declared-gate-file");
    write(&case, ".gate", "not a folder\n");
    declarations_folder(&case, &[("toolchain.toml", "[tools]\npnpm = \"10\"\n")]);
    let marker = case.folder().join("pnpm-probed");
    let pnpm = format!(">\"{}\" echo ran\r\necho 10.0.0\r\n", marker.display());
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[("pnpm", &pnpm)], &["pnpm"]);

    a_toolchain_served(&case, &box_path, ANSWERING, None);

    let log = log_texts(&case.folder().join("out.log"));
    assert!(!log.contains("read from"), "{log}");
    assert!(
        !marker.exists(),
        "the builder was asked for pnpm, which only the declarations folder's copy declares"
    );
}

/// A copy in the declarations folder that is not TOML is skipped whole with its line, naming its full path, and the run
/// goes remote.
#[test]
fn an_unreadable_manifest_in_the_declarations_folder_is_skipped_with_its_path() {
    let case = client_case("client-tool-declared-unreadable");
    let folder = declarations_folder(&case, &[("toolchain.toml", "[tools\npnpm = \"10\"\n")]);
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &[]);

    let outcome = a_toolchain_served(&case, &box_path, ANSWERING, None);

    let start = format!("gate: {}: line 1, column ", folder.join("toolchain.toml").display());
    let said: Vec<&str> = outcome.stderr.lines().filter(|line| line.contains("toolchain.toml")).collect();
    assert!(
        said.len() == 1 && said[0].starts_with(&start) && said[0].ends_with("; skipping it"),
        "{start}: {outcome:?}"
    );
}

/// A project file naming the Godot SDK version `version`, as a Godot .NET repo's does.
pub(super) fn godot_csproj(version: &str) -> String {
    format!(
        "<Project Sdk=\"Godot.NET.Sdk/{version}\">\n  <PropertyGroup>\n    <TargetFramework>net10.0</TargetFramework>\n  \
         </PropertyGroup>\n</Project>\n"
    )
}

/// A `GATE_GODOT_ROOT` for a case, under its `ssh` folder, holding one Godot install per `(folder, line)`: a copy of the
/// fake tool named `Godot_console.exe` answering `--version` with the line beside it, as step A's box reads one. The
/// folder is left out of the case's snapshot, and lies outside the builder's own work folders, where a program would be
/// refused.
pub(super) fn godot_root(case: &Case, installs: &[(&str, &str)]) -> PathBuf {
    let root = case.folder().join("ssh").join("godot");
    for (folder, line) in installs {
        let install = root.join(folder);
        fs::create_dir_all(&install).expect("an install folder can be made");
        fs::copy(FAKE_TOOL, install.join("Godot_console.exe")).expect("the fake godot can be copied");
        fs::write(install.join("version.txt"), line).expect("the line the fake godot answers with can be written");
    }
    root
}

/// A repo whose project file pins a Godot the builder has goes remote, with no line about godot: the builder probed the
/// install the root names for its version.
#[test]
fn a_repo_pinning_a_godot_the_builder_has_routes() {
    let case = client_case("client-godot-served");
    write(&case, "src/App/Game.csproj", &godot_csproj("4.7.2"));
    let root = godot_root(&case, &[("4.7.2", "4.7.2.stable.mono.official.abc\n")]);
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &[]);

    let outcome = a_toolchain_served(&case, &box_path, ANSWERING, Some(&root));

    assert!(
        root.join("4.7.2").join("ran").exists(),
        "the builder probed the Godot the project file pins"
    );
    assert!(!outcome.stderr.contains("godot"), "{outcome:?}");
}

/// A repo whose project file pins a Godot the builder has not runs here, saying the versions it has.
#[test]
fn a_repo_pinning_a_godot_the_builder_lacks_runs_here_saying_the_mismatch() {
    let case = client_case("client-godot-lacking");
    write(&case, "src/App/Game.csproj", &godot_csproj("4.7.2"));
    let root = godot_root(&case, &[("4.3.0", "4.3.stable.mono.official.y\n")]);
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &[]);

    a_toolchain_refusal(&case, &box_path, &[mismatch_of(&case, "godot 4.7.2 exactly", "4.3.0")], Some(&root));
}

/// A repo that pins no Godot asks the builder for none: the run goes remote though the case's root names an install no
/// project file pins, and the builder's probe never runs it.
#[test]
fn a_repo_with_no_godot_pin_sends_no_godot_need() {
    let case = client_case("client-godot-unpinned");
    write(&case, "src/App/App.csproj", "<Project />\n");
    let root = godot_root(&case, &[("4.7.2", "4.7.2.stable.mono.official.abc\n")]);
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &[]);

    let outcome = a_toolchain_served(&case, &box_path, ANSWERING, Some(&root));

    assert!(!outcome.stderr.lines().any(|line| line.contains("godot")), "{outcome:?}");
    assert!(
        !root.join("4.7.2").join("ran").exists(),
        "the builder probed a godot the repo never pinned"
    );
}

/// Makes `folder` a fake Unreal Engine install whose `Engine\Build\Build.version` reports `version`,
/// `major.minor.patch`, as the box's unreal probe reads one.
pub(super) fn fake_unreal_install(folder: &Path, version: &str) {
    let parts: Vec<u64> = version
        .split('.')
        .map(|part| part.parse().expect("a fake engine's version is numbers"))
        .collect();
    let build = folder.join("Engine").join("Build");
    fs::create_dir_all(&build).expect("the fake engine's folders can be made");
    let text = json!({ "MajorVersion": parts[0], "MinorVersion": parts[1], "PatchVersion": parts[2], "Changelist": 0 });
    fs::write(build.join("Build.version"), text.to_string()).expect("the fake engine's Build.version can be written");
}

/// A `GATE_UNREAL_ROOT` for a case, under its `ssh` folder, holding one fake engine per version of `versions`, each in
/// the folder named for it. The folder is left out of the case's snapshot, and lies outside the builder's own work
/// folders, where a root would be refused.
fn unreal_root(case: &Case, versions: &[&str]) -> PathBuf {
    let root = case.folder().join("ssh").join("unreal");
    fs::create_dir_all(&root).expect("the unreal root can be made");
    for version in versions {
        fake_unreal_install(&root.join(version), version);
    }
    root
}

/// Sets the gate's `GATE_UNREAL_ROOT` to `root` — which the builder's unreal probe and its run read — or takes it out of
/// the environment when the case gives none, so a machine with one set cannot serve a case by accident.
fn set_unreal_root(gate: &mut Command, root: Option<&Path>) {
    if let Some(root) = root {
        gate.env("GATE_UNREAL_ROOT", root);
    } else {
        gate.env_remove("GATE_UNREAL_ROOT");
    }
}

/// A stale `UE_ROOT` the builder inherits from the gate's environment, which no routed command may see.
const STALE_UE_ROOT: &str = r"C:\stale\UE_5.0";

/// A routed run of a command printing its `UE_ROOT`, in a case whose builder's `GATE_UNREAL_ROOT` is `root` and whose
/// inherited `UE_ROOT` is `inherited`, none when not given: it goes remote with no parity line and runs once, on the
/// builder, and passes.
fn a_routed_unreal_run(case: &Case, root: &Path, inherited: Option<&str>) -> Outcome {
    a_routed_unreal_run_of(case, root, &[r".\tool.exe", "env", "UE_ROOT", "mark", RAN], inherited, &[])
}

/// A routed run of the command `words`, as [`a_routed_unreal_run`] runs it, with each of the variables `also` the
/// builder inherits beside `UE_ROOT`.
fn a_routed_unreal_run_of(case: &Case, root: &Path, words: &[&str], inherited: Option<&str>, also: &[(&str, &str)]) -> Outcome {
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: ["*tool*"] }, "outputs": { &repo: [] } });
    write_remote_json(case, &routing.to_string());
    let box_path = box_path_with_tools(case, &listing(&[BUILDER_SDK]), &[], &[]);
    let steps = ssh_steps(case, "ssh", &["ok", "ok"]);
    let key = netfx_present(case);
    let seams = netfx_seams(&key);
    let tuned = Tuned {
        preflight_seams: &seams,
        ..Tuned::default()
    };
    let mut gate = client_gate_tuned(case, "out.log", &steps, &tuned, words);
    gate.env("PATH", box_path);
    match inherited {
        Some(value) => gate.env("UE_ROOT", value),
        None => gate.env_remove("UE_ROOT"),
    };
    for (name, value) in also {
        gate.env(name, value);
    }
    set_unreal_root(&mut gate, Some(root));

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.lines().any(is_parity_line), "{outcome:?}");
    assert_eq!(fallbacks(&own(case, pid)), Vec::<&str>::new(), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(runs(case), (0, 1), "the command ran once, on the builder: {outcome:?}");
    outcome
}

/// A stand-in for the builder's .NET Framework SDK list that lists a 4.8 SDK in a folder of the case's, which an
/// Unreal-pinned repo's run needs, so no case reads this machine's own list.
fn netfx_present(case: &Case) -> NetfxKey {
    NetfxKey::new().with_sdk("4.8", &case.folder().join("ssh").join("netfx").join("4.8"))
}

/// The preflight seams of a tool case whose builder lists the .NET Framework SDKs of the stand-in `key`.
fn netfx_seams(key: &NetfxKey) -> String {
    format!("{ANSWERING} {}", key.seam())
}

/// Whether the routed command printed a `UE_ROOT` naming the engine `version` of `root`.
fn saw_the_engine(outcome: &Outcome, root: &Path, version: &str) -> bool {
    let engine = root.join(version).display().to_string();
    outcome.stdout.lines().any(|line| line == engine)
}

/// A repo whose manifest pins an Unreal Engine the builder has goes remote, with no line about unreal, and its command
/// sees the engine of that version.
#[test]
fn a_repo_pinning_an_unreal_the_builder_has_routes_and_sends_its_version() {
    let case = client_case("client-unreal-served");
    write(&case, MANIFEST, "[tools]\nunreal = \"5.8.0\"\n");
    let root = unreal_root(&case, &["5.7.3", "5.8.0"]);

    let outcome = a_routed_unreal_run(&case, &root, None);

    assert!(saw_the_engine(&outcome, &root, "5.8.0"), "{outcome:?}");
    assert!(!outcome.stderr.contains("unreal"), "{outcome:?}");
}

/// A repo whose manifest pins an Unreal Engine the builder has not runs here, saying the versions it has.
#[test]
fn a_repo_pinning_an_unreal_the_builder_lacks_runs_here_saying_the_mismatch() {
    let case = client_case("client-unreal-lacking");
    write(&case, MANIFEST, "[tools]\nunreal = \"5.8\"\n");
    let root = unreal_root(&case, &["5.7.3"]);
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &[]);
    let key = netfx_present(&case);

    a_toolchain_refusal_with(&case, &box_path, &[mismatch_of(&case, "unreal 5.8 exactly", "5.7.3")], |gate| {
        set_unreal_root(gate, Some(&root));
        gate.env(PREFLIGHT_SEAMS_VARIABLE, netfx_seams(&key));
    });
}

/// A repo pinning an Unreal Engine the builder has runs here when the builder lists no .NET Framework SDK, which
/// `UnrealBuildTool` needs, saying the SDK the builder lacks.
#[test]
fn a_repo_pinning_an_unreal_runs_here_when_the_builder_lacks_a_netfx_sdk() {
    let case = client_case("client-unreal-no-netfx");
    write(&case, MANIFEST, "[tools]\nunreal = \"5.8\"\n");
    let root = unreal_root(&case, &["5.8.0"]);
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &[]);
    let key = NetfxKey::new();

    a_toolchain_refusal_with(&case, &box_path, &[mismatch_of(&case, "netfx-sdk 4.6.0 or later", "none")], |gate| {
        set_unreal_root(gate, Some(&root));
        gate.env(PREFLIGHT_SEAMS_VARIABLE, netfx_seams(&key));
    });
}

/// A repo that pins no Unreal Engine asks the builder for none: the run goes remote though the builder has an engine,
/// and its command sees no `UE_ROOT`, the builder's inherited one removed.
#[test]
fn a_repo_with_no_unreal_pin_sends_no_unreal_need_or_version() {
    let case = client_case("client-unreal-unpinned");
    let root = unreal_root(&case, &["5.8.0"]);

    let outcome = a_routed_unreal_run(&case, &root, Some(STALE_UE_ROOT));

    assert!(outcome.stdout.lines().any(|line| line == "fake-tool: UE_ROOT is not set"), "{outcome:?}");
    assert!(!outcome.stderr.contains("unreal"), "{outcome:?}");
}

/// A repo whose manifest takes any Unreal Engine, `unreal = ""`, routes with the newest engine the builder has, and its
/// command sees that engine over the `UE_ROOT` the builder inherited.
#[test]
fn a_repo_with_an_any_version_unreal_entry_routes_with_the_newest_engine_in_ue_root() {
    let case = client_case("client-unreal-any");
    write(&case, MANIFEST, "[tools]\nunreal = \"\"\n");
    let root = unreal_root(&case, &["5.7.3", "5.10.0", "5.8.0"]);

    let outcome = a_routed_unreal_run(&case, &root, Some(STALE_UE_ROOT));

    assert!(saw_the_engine(&outcome, &root, "5.10.0"), "{outcome:?}");
}

/// A partial pin sends the highest engine of the builder that meets it.
#[test]
fn a_partial_unreal_pin_sends_the_highest_matching_install() {
    let case = client_case("client-unreal-partial-pin");
    write(&case, MANIFEST, "[tools]\nunreal = \"5.8\"\n");
    let root = unreal_root(&case, &["5.8.0", "5.8.2", "5.9.0"]);

    let outcome = a_routed_unreal_run(&case, &root, None);

    assert!(saw_the_engine(&outcome, &root, "5.8.2"), "{outcome:?}");
}

/// A routed run of a repo pinning an Unreal Engine reaches its command, through the box, with `UE_ROOT` naming the
/// engine chosen, over the `UE_ROOT` the builder inherited.
#[test]
fn a_routed_run_of_an_unreal_pinned_repo_reaches_its_command_with_ue_root() {
    let case = client_case("client-unreal-ue-root");
    write(&case, MANIFEST, "[tools]\nunreal = \"5.8.0\"\n");
    let root = unreal_root(&case, &["5.8.0"]);

    let outcome = a_routed_unreal_run(&case, &root, Some(STALE_UE_ROOT));

    assert!(saw_the_engine(&outcome, &root, "5.8.0"), "{outcome:?}");
    assert!(!outcome.stdout.contains(STALE_UE_ROOT), "{outcome:?}");
}

/// A routed run of a repo whose manifest pins an Unreal Engine and names a variable in its `[env]` reaches its command,
/// through the box, with that variable naming the same engine `UE_ROOT` names, over the value the builder inherited.
#[test]
fn a_routed_run_whose_manifest_names_an_engine_env_reaches_its_command_with_it() {
    let case = client_case("client-unreal-engine-env");
    write(
        &case,
        MANIFEST,
        "[tools]\nunreal = \"5.8.0\"\n[env]\nUE_MCP_TEST_ENGINE_ROOT = \"unreal\"\n",
    );
    let root = unreal_root(&case, &["5.8.0"]);
    let words = [r".\tool.exe", "env", "UE_ROOT", "env", "UE_MCP_TEST_ENGINE_ROOT", "mark", RAN];
    let stale = r"C:\stale\engine";

    let outcome = a_routed_unreal_run_of(&case, &root, &words, Some(STALE_UE_ROOT), &[("UE_MCP_TEST_ENGINE_ROOT", stale)]);

    // The command prints `UE_ROOT`, then the manifest's variable, each on its own line: both name the engine.
    let engine = root.join("5.8.0").display().to_string();
    assert!(outcome.stdout.contains(&format!("{engine}\n{engine}\n")), "{outcome:?}");
    assert!(!outcome.stdout.contains("UE_MCP_TEST_ENGINE_ROOT is not set"), "{outcome:?}");
    assert!(!outcome.stdout.contains(stale), "the inherited value is gone: {outcome:?}");
    assert!(
        !outcome.stderr.contains("toolchain.toml"),
        "the manifest reads without a problem: {outcome:?}"
    );
}

/// A Ctrl+C raised just before the manifest is read ends the run as an interrupt before the preflight: nothing reaches
/// the builder, and the command runs nowhere.
#[test]
fn a_ctrl_c_at_the_manifest_read_ends_the_run_before_the_preflight() {
    let case = client_case("client-tool-ctrl-c");
    write(&case, MANIFEST, "[tools]\npnpm = \"10\"\n");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "manifest"],
        ..Tuned::default()
    };
    let gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "mark", RAN, "out", "ran"]);

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "nothing reached the builder, so nothing was pushed");
    assert!(!outcome.stderr.contains("gate: .gate/toolchain.toml"), "{outcome:?}");
    assert_eq!(runs(&case), (0, 0), "the command ran nowhere: {outcome:?}");
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
    let log = log_texts(&case.folder().join("out.log"));
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
    run.wait_until("the run call", || calls(&steps).len() == 2);
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
    run.wait_until("the run call", || calls(&steps).len() == 2);
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
    run.wait_until("the run call", || calls(&steps).len() == 2);
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

/// Through `local-ssh`, the box's run succeeds, then the sync-back's `ls-remote` is refused by ssh on each of its three
/// attempts: the run ends TRANSFER FAILED with no fallback, and nothing after that call runs.
#[test]
fn a_git_failure_of_the_sync_back_on_every_attempt_ends_transfer_failed_with_no_fallback() {
    let case = client_case("client-back-git-failure");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "fail-other", "fail-other", "fail-other"]);

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "mark", "made-on-box.txt"],
    ));

    assert_eq!(outcome.code, Some(74), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("gate: TRANSFER FAILED: the command exited 0 on builder; git ls-remote"),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr.matches(TRANSFER_RETRY).count(), 2, "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
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

/// The start of every retry line of a transfer after the command; its reason and `n of 2` follow it.
const TRANSFER_RETRY: &str = "gate: bringing the results back from builder failed (";

/// Through `local-ssh`, the sync-back's first `ls-remote` exits 255 with nothing said, a connection dropped after the
/// banner: it is made again after the pause, and the run passes with the box's change brought back.
#[test]
fn a_passing_run_whose_ls_remote_fails_once_retries_it_and_passes() {
    let case = client_case("client-back-ls-remote-retried");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "drop", "ok", "ok"]);

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "mark", "made-on-box.txt"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(TRANSFER_RETRY).count(), 1, "{outcome:?}");
    assert!(outcome.stderr.contains("); trying again (1 of 2)"), "{outcome:?}");
    assert!(!outcome.stderr.contains(RETRY_LINE), "no pre-banner retry: {outcome:?}");
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

/// Through `local-ssh`, a command that made a file on the builder and exited 3, then every attempt of the sync-back's
/// `ls-remote` exits 255: the run says the transfer failed and keeps the command's own status and verdict.
#[test]
fn a_failing_run_whose_transfer_fails_keeps_the_command_s_status() {
    let case = client_case("client-back-failed-transfer");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "drop", "drop", "drop"]);

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "mark", "made-on-box.txt", "exit", "3"],
    ));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("gate: TRANSFER FAILED: the command exited 3 on builder; git ls-remote"),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr.matches(TRANSFER_RETRY).count(), 2, "{outcome:?}");
    assert!(outcome.stderr.contains("gate: FAILED (status 3)"), "{outcome:?}");
    assert!(!case.folder().join("made-on-box.txt").exists(), "nothing was applied: {outcome:?}");
    assert_eq!(verdict(&case), "failed");
}

/// Through `local-ssh`, the sync-back's first fetch drops with git's 128 and ssh's words of a lost connection, said
/// beside it as git's own error text: the retry line carries that text into the log, and the run still passes, not
/// failing as a log that reports an error.
#[test]
fn a_retried_fetch_whose_reason_reads_as_an_error_still_passes() {
    let case = client_case("client-fetch-retry-error-text");
    let dropped = "say-exit 255 fatal: the remote end hung up unexpectedly: error in libcrypto";
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "ok", dropped, "ok"]);

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "mark", "made-on-box.txt"],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(TRANSFER_RETRY).count(), 1, "{outcome:?}");
    assert!(outcome.stderr.contains("error in libcrypto"), "the reason is said: {outcome:?}");
    assert!(out_log(&case).contains("error in libcrypto"), "the reason is logged");
    assert!(outcome.stderr.contains("gate: synced back 1 file(s): made-on-box.txt"), "{outcome:?}");
}

/// The transfer ceiling `--test-remote-watch-call fetch` gives the sync-back's fetch, and the pause before its retry,
/// the same: a fetch that drops always has less than the pause left of the ceiling it shares with its retries, however
/// long the machine took to get there. Ten minutes, so no attempt that ends is cut by it.
const SHARED_CEILING_MS: &str = "600000";

/// Through `local-ssh`, the sync-back's fetch drops once less than the pause before a retry is left of its transfer
/// ceiling, which every attempt shares: it is not made again, and the run ends as a fetch past its ceiling does.
#[test]
fn a_fetch_that_drops_near_its_transfer_ceiling_is_not_retried_past_it() {
    let case = client_case("client-fetch-retry-past-ceiling");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "ok", "drop", "drop", "drop"]);
    let tuned = Tuned {
        kind: "local-ssh",
        extra: &["--test-remote-watch-call", "fetch", "--test-remote-timeout-ms", SHARED_CEILING_MS],
        pause: SHARED_CEILING_MS,
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "mark", "made-on-box.txt"],
    ));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(!outcome.stderr.contains(TRANSFER_RETRY), "no retry: {outcome:?}");
    assert!(
        outcome.stderr.contains("gate: TRANSFER TIMED OUT after 600 s fetching the sync-back"),
        "{outcome:?}"
    );
    assert!(!case.folder().join("made-on-box.txt").exists(), "nothing was applied: {outcome:?}");
    assert_eq!(verdict(&case), "transfer-timed-out");
    assert_eq!(
        calls(&steps),
        ["preflight", "git-receive-pack", "run", "git-upload-pack", "git-upload-pack"]
    );
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
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.contains(STALLED_LINE), "the kill line is in the log: {log}");
    assert!(!log.contains(STOPPING), "a stall's stop says only its kill line: {log}");
    assert!(!outcome.stderr.contains(STOPPING), "{outcome:?}");
    assert_eq!(verdict(&case), "stalled");
    let made = calls(&steps);
    assert!(
        made_but_maybe(&made, &["preflight", "git-receive-pack", "run", "stop"], "run"),
        "a stop, and no sync-back: {made:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_run_call_past_its_backstop_is_killed_with_the_backstop_line() {
    let case = client_case("client-backstop");
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", never(&case)), "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-backstop-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    // The run's work folder is new, so its 3600 s ceiling is raised to 5400 s, which the line names.
    let backstop = Regex::new(
        r"(?m)^gate: BACKSTOP on \S+: no end after 1 s of wall time \(5 x the 5400 s ceiling \+ 60 s\); killed the run call and stopped the command$",
    )
    .expect("the pattern compiles");
    assert!(backstop.is_match(&outcome.stderr), "{outcome:?}");
    assert!(
        !outcome.stderr.contains(STOPPING),
        "a backstop's stop says only its kill line: {outcome:?}"
    );
    assert!(!out_log(&case).contains(STOPPING), "{outcome:?}");
    assert_eq!(verdict(&case), "backstop");
    let made = calls(&steps);
    assert!(made_but_maybe(&made, &["preflight", "run", "stop"], "run"), "{made:?}");
}

/// How long a box-kill case goes on with fresh runs while the box's ceiling kills each command before it marked its
/// file: a hang guard a passing run never reaches.
const MARKED_BEFORE_THE_KILL_GUARD: Duration = Duration::from_secs(300);

/// A routed run whose command marks `made-on-box.txt` on the builder, then sleeps past its 1 s ceiling, the box's `run`
/// given `box_seams` when some: its case, steps file and outcome, from the first run whose command marked the file
/// before the box killed it. A command starved past its ceiling before it marked leaves nothing to sync back, which
/// proves nothing of the skip, so such a run is followed by a fresh one, up to the hang guard. A run that did not end
/// 124, or whose journal never reached its queue line, is returned at once, so the caller's asserts fail with its own
/// output.
fn box_killed_after_its_mark(name: &str, box_seams: Option<&str>) -> (Case, PathBuf, Outcome) {
    let guard = Instant::now() + MARKED_BEFORE_THE_KILL_GUARD;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let case = client_case(name);
        let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "ok"]);
        let tuned = Tuned {
            timeout: "1",
            ..Tuned::default()
        };
        let mut gate = client_gate_tuned(
            &case,
            "out.log",
            &steps,
            &tuned,
            &[r".\tool.exe", "mark", "made-on-box.txt", "sleep", "600000"],
        );
        if let Some(seams) = box_seams {
            gate.env(BOX_SEAMS_VARIABLE, seams);
        }
        let outcome = finish(gate);
        let queued = work_folder_if_queued(&case);
        let marked = queued.as_ref().is_some_and(|work| work.join("made-on-box.txt").exists());
        if outcome.code != Some(124) || queued.is_none() || marked {
            return (case, steps, outcome);
        }
        assert!(
            Instant::now() < guard,
            "every one of {attempts} commands was killed before made-on-box.txt was in its work folder, until the hang guard: \
             starved each time, or the box no longer leaves the killed command's file; last outcome: {outcome:?}"
        );
    }
}

/// The builder's work folder the journal's queue line names, as [`work_folder`] has it, `None` when the journal is not
/// there or has yet to name a key: a run killed before its queue line has no work folder to look in.
fn work_folder_if_queued(case: &Case) -> Option<PathBuf> {
    let journal = fs::read_to_string(case.root().join("events.jsonl")).ok()?;
    let key = serde_json::from_str::<Value>(journal.lines().next()?)
        .ok()?
        .get("key")?
        .as_str()?
        .to_owned();
    Some(case.folder().join("builder").join("work").join(key))
}

/// The real box, given a ceiling of 1 s, kills a command that would run for ten minutes: by its backstop of 5 s of wall
/// time at the latest, or at its ceiling first when its machine is free enough. The client ends the run as killed on
/// the box's kill tag, with no stop and no sync-back.
#[test]
fn a_command_the_box_kills_at_its_ceiling_ends_the_run_killed_with_no_stop_or_sync_back() {
    let (case, steps, outcome) = box_killed_after_its_mark("client-box-killed", None);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let verdict = verdict(&case);
    assert!(verdict == "timed-out" || verdict == "backstop", "{verdict}: {outcome:?}");
    let log = out_log(&case);
    let kill = Regex::new(r"(?m)^gate: (TIMED OUT|BACKSTOP) on \S+").expect("the pattern compiles");
    assert!(kill.is_match(&log), "the box's kill line is in the log: {log}");
    assert!(!log.contains("gate-remote-killed:") && !log.contains("gate-remote-adjusted:"), "{log}");
    assert_eq!(calls(&steps), ["preflight", "run"], "no stop");
    assert!(
        work_folder(&case).join("made-on-box.txt").exists(),
        "the killed command wrote its file on the builder: {outcome:?}"
    );
    assert!(
        !case.folder().join("made-on-box.txt").exists(),
        "the file the killed command wrote is not synced back: {outcome:?}"
    );
    assert!(!outcome.stderr.contains("gate: synced back"), "{outcome:?}");
}

/// The builder's work folder of the case's first run.
pub(super) fn work_folder(case: &Case) -> PathBuf {
    case.folder().join("builder").join("work").join(key_of(&events(case)))
}

/// The box seams a box-kill case gives the box's `run` through the stand-in: a sample every 20 ms of a machine all
/// free, so the box's clock runs with wall time however loaded the machine running the case is.
const FREE_BOX: &str = "--test-box-sample-ms 20 --test-box-free-percent 100";

/// The variable whose words the stand-in gives the box's `run` before its verb.
const BOX_SEAMS_VARIABLE: &str = "GATE_FAKE_SSH_BOX_SEAMS";

/// A box kill whose job the box could not end of every process is still a kill: the box says its note, then its kill
/// line and tag, so the client journals the kill's verdict and syncs nothing back of what the command wrote.
#[test]
fn a_box_kill_that_leaves_a_process_is_still_timed_out_with_no_sync_back() {
    let (case, steps, outcome) = box_killed_after_its_mark("client-box-kill-leaves", Some(&format!("{FREE_BOX} --test-box-kill-leaves 1")));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_eq!(verdict(&case), "timed-out", "{outcome:?}");
    let log = out_log(&case);
    let note = Regex::new(r"(?m)^gate: note: the box's ceiling kill left 1 process\(es\) running in job \S+\r?$").expect("the pattern compiles");
    assert!(note.is_match(&log), "the box's note is in the log: {log}");
    assert_eq!(calls(&steps), ["preflight", "run"], "no stop");
    assert!(
        work_folder(&case).join("made-on-box.txt").exists(),
        "the killed command wrote its file on the builder: {outcome:?}"
    );
    assert!(
        !case.folder().join("made-on-box.txt").exists(),
        "the file the killed command wrote is not synced back: {outcome:?}"
    );
    assert!(!outcome.stderr.contains("gate: synced back"), "{outcome:?}");
}

/// How long the builder may take to stop the quiet command of a gate killed under it and free the work folder: a hang
/// guard, since the box finds its client gone within the 200 ms heartbeat period the case gives it.
const KILLED_GATE_GUARD: Duration = Duration::from_secs(60);

/// The routed command of a case that killed its gate, held open so its pid names it alone, and ended on drop while it
/// still runs: a failing case leaves no command running, so its box, which runs the suite's `gate.exe`, ends too.
struct EndedOnDrop(gate_win32::StoppableProcess);

impl EndedOnDrop {
    /// Whether the command has ended.
    fn ended(&self) -> bool {
        matches!(self.0.wait(Duration::ZERO), Ok(true))
    }
}

impl Drop for EndedOnDrop {
    fn drop(&mut self) {
        if !self.ended() {
            // The case has already failed on its own assertion; this is its clean-up, best effort.
            let _ = self.0.terminate(1);
        }
    }
}

/// A gate killed outright, as a task runner's hard stop ends it, makes no stop call; ssh goes with the gate's job, and
/// the box, which outlives it as a builder's does, has its streams closed under it. Its command writes nothing, yet the
/// box's heartbeat finds stderr closed within a period, stops the command's job and lets go of the work folder's lock.
#[test]
fn a_killed_gate_s_quiet_routed_command_frees_the_builder_s_work_folder() {
    let case = client_case("client-killed-quiet");
    let steps = ssh_steps(&case, "ssh", &["ok", "away"]);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "pid-to", "tool-pid.log", "sleep", "120000"]);
    gate.env(BOX_SEAMS_VARIABLE, "--test-heartbeat-ms 200");
    let running = start(gate);
    running.wait_until("the run call", || calls(&steps).len() >= 2);
    let work = work_folder(&case);
    let pid_file = work.join("tool-pid.log");
    let read_pid = || fs::read_to_string(&pid_file).ok().and_then(|text| text.trim().parse::<u32>().ok());
    running.wait_until("the routed command's pid file", || read_pid().is_some());
    let pid = read_pid().expect("the pid file holds a pid");
    let tool = EndedOnDrop(
        gate_win32::StoppableProcess::open(pid)
            .expect("the routed command can be opened")
            .expect("the routed command runs"),
    );
    let lock = work.with_file_name(format!(
        "{}.lock",
        work.file_name().expect("the work folder has a name").to_string_lossy()
    ));

    drop(running.kill());

    let deadline = Instant::now() + KILLED_GATE_GUARD;
    let tool_gone = || tool.ended();
    let lock_free = || fs::OpenOptions::new().read(true).share_mode(0).open(&lock).is_ok();
    while !(tool_gone() && lock_free()) {
        assert!(
            Instant::now() < deadline,
            "{} s after the gate was killed, the command {pid} runs: {}, the work folder's lock is held: {}",
            KILLED_GATE_GUARD.as_secs(),
            !tool_gone(),
            !lock_free()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The run call's output floods the log past its cap before the box kills the command, so the box's kill line is not
/// in the log: the verdict comes from the box's kill tag all the same.
#[test]
fn a_box_kill_whose_kill_line_passed_the_log_cap_is_still_timed_out() {
    let case = client_case("client-box-kill-capped");
    let steps = ssh_steps(&case, "ssh", &["ok", "lines 200 1000", "ok", "ok"]);
    let tuned = Tuned {
        timeout: "1",
        extra: &["--test-remote-log-cap-bytes", LOG_CAP],
        ..Tuned::default()
    };
    let mut gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "sleep", "600000"]);
    gate.env(BOX_SEAMS_VARIABLE, FREE_BOX);

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let log = out_log(&case);
    assert_eq!(cuts(&log), 1, "the run call's output passed the cap: {log}");
    assert!(!log.contains("TIMED OUT"), "the box's kill line is past the cap: {log}");
    assert_eq!(verdict(&case), "timed-out", "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "no stop");
}

/// The real box runs a command that writes a file, says a kill line of its own and exits 124 with no kill tag: an
/// ordinary status, so the run is no kill and its file is synced back.
#[test]
fn a_124_of_the_command_s_own_with_no_kill_tag_is_no_kill() {
    let case = client_case("client-own-124");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "ok"]);
    let command = [
        r".\tool.exe",
        "mark",
        "made-on-box.txt",
        "err",
        "gate: TIMED OUT on builder after 1 s of load-adjusted time",
        "exit",
        "124",
    ];

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &Tuned::default(), &command));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_eq!(verdict(&case), "failed");
    assert_eq!(calls(&steps), ["preflight", "run"], "nothing stopped");
    assert!(
        case.folder().join("made-on-box.txt").exists(),
        "the file the command wrote is synced back: {outcome:?}"
    );
    assert!(outcome.stderr.contains("gate: synced back 1 file(s): made-on-box.txt"), "{outcome:?}");
}

/// The `--timeout` of a case whose push is held past it, in seconds: the command's ceiling, which the push must not
/// count against.
const SHORT_CEILING: &str = "2";

/// Starts `gate`, whose push the stand-in holds on `go` as the second call of `steps`, and lets the push go once it has
/// been held past [`SHORT_CEILING`]: the push takes at least that long, however the machine runs. A gate that ended
/// before its push reached the stand-in is not waited for. The running gate, and when it was started.
fn held_past_the_ceiling(gate: Command, steps: &Path, go: &Go) -> (Running, Instant) {
    let begun = Instant::now();
    let mut running = start(gate);
    wait_until("the push, or the gate's end", || calls(steps).len() >= 2 || !running.is_running());
    let ceiling: u64 = SHORT_CEILING.parse().expect("the ceiling is a number");
    thread::sleep(Duration::from_secs(ceiling) + Duration::from_millis(500));
    fs::write(go.path(), "").expect("the go file can be made");
    (running, begun)
}

/// The run call is given a backstop of its own far past any wait, so only the push could pass [`SHORT_CEILING`].
#[test]
fn a_push_slower_than_the_command_s_ceiling_is_not_cut_by_it() {
    let case = client_case("client-slow-push");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path())), "ok", "ok", "ok"]);
    let tuned = Tuned {
        kind: "local-ssh",
        timeout: SHORT_CEILING,
        extra: &["--test-remote-backstop-ms", "600000"],
        ..Tuned::default()
    };
    let gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]);

    let (running, _) = held_past_the_ceiling(gate, &steps, &go);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: pushed snapshot "), "the push finished: {outcome:?}");
    assert!(!outcome.stderr.contains("unreachable"), "no fallback: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
    assert_eq!(&calls(&steps)[..3], ["preflight", "git-receive-pack", "run"]);
}

/// The push is held past the ceiling, then the run call is held until the client's backstop kills it: the backstop
/// counts from the run call's start, so the run ends no sooner than the held push and the whole backstop after it.
#[test]
fn a_run_call_past_its_backstop_is_cut_only_after_the_whole_backstop_from_its_own_start() {
    let case = client_case("client-command-ceiling");
    let go = Go(case.folder().join("ssh").join("go"));
    let held = format!("hold {}", shown(go.path()));
    let steps = ssh_steps(&case, "ssh", &["ok", &held, &format!("hold {}", never(&case)), "ok"]);
    let tuned = Tuned {
        kind: "local-ssh",
        timeout: SHORT_CEILING,
        extra: &["--test-remote-backstop-ms", "2000"],
        ..Tuned::default()
    };
    let gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]);

    let (running, begun) = held_past_the_ceiling(gate, &steps, &go);
    let outcome = running.finish();
    let took = begun.elapsed();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    // The run's work folder is new, so its 2 s ceiling is raised to 4 s, which the line names.
    assert!(
        outcome
            .stderr
            .contains("no end after 2 s of wall time (5 x the 4 s ceiling + 60 s); killed the run call and stopped the command"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("unreachable"), "the push was not cut: {outcome:?}");
    assert_eq!(verdict(&case), "backstop");
    assert!(took >= Duration::from_millis(4500), "the held push, then the whole backstop: {took:?}");
    let made = calls(&steps);
    assert!(
        made_but_maybe(&made, &["preflight", "git-receive-pack", "run", "stop"], "run"),
        "{made:?}"
    );
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
    // The raised window is the run call's; the preflight gets a hang guard of its own, so only the call under the floor
    // can be cut.
    let tuned = Tuned {
        stall: "3",
        extra: &["--test-remote-watch-call", "preflight", "--test-stall-ms", "600000"],
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

/// The line a Ctrl+C that stops the command says before the stop call, with that call's bound, 30 s unless a seam
/// sets it.
const STOPPING_LINE: &str = "gate: stopping the command on builder (up to 30 s)";
/// What that line starts with, whatever its bound.
const STOPPING: &str = "gate: stopping the command on builder (up to ";

/// The case's log, `out.log`.
fn out_log(case: &Case) -> String {
    log_texts(&case.folder().join("out.log"))
}

#[test]
fn a_ctrl_c_says_once_what_the_stop_waits_for() {
    let case = client_case("client-ctrl-c-says-stop");
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("say-hold running-there {}", never(&case)), "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "run"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop"], "the command is stopped");
    assert_eq!(outcome.stderr.matches(STOPPING).count(), 1, "said once: {outcome:?}");
    let said = outcome.stderr.find(STOPPING_LINE).expect("the stop's line is said on stderr");
    let ended = outcome.stderr.find("gate: FAILED (status").expect("the run's end is said");
    assert!(said < ended, "said before the run ends: {outcome:?}");
    let log = out_log(&case);
    assert_eq!(log.matches(STOPPING).count(), 1, "logged once: {log}");
    // The interrupt comes after the run call's first line, which the log keeps before anything said after it.
    let interrupted = log.find("running-there").expect("the run call's first line is logged");
    let logged = log.find(STOPPING_LINE).expect("the stop's line is logged");
    assert!(interrupted < logged, "said after the Ctrl+C: {log}");
}

#[test]
fn a_second_ctrl_c_during_the_stop_adds_nothing() {
    let case = client_case("client-ctrl-c-twice");
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("say-hold running-there {}", never(&case)), "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "stop"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "the run still ends: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop"], "the command is stopped once");
    assert_eq!(outcome.stderr.matches(STOPPING).count(), 1, "said once: {outcome:?}");
    assert!(outcome.stderr.contains(STOPPING_LINE), "{outcome:?}");
    let log = out_log(&case);
    assert_eq!(log.matches(STOPPING).count(), 1, "logged once: {log}");
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

/// The sync-back's fetch, through `local-ssh`, held past the transfer ceiling `--test-remote-watch-call fetch` gives it
/// alone, after a command that made a file on the builder and exited `exit`: the case and how its run ended, the
/// TRANSFER TIMED OUT line said and nothing applied.
fn a_sync_back_held_past_its_transfer_ceiling(name: &str, exit: &str) -> (Case, Outcome) {
    let case = client_case(name);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", "ok", &format!("hold {}", never(&case))]);
    let tuned = Tuned {
        kind: "local-ssh",
        extra: &["--test-remote-watch-call", "fetch", "--test-remote-timeout-ms", WATCHED_MS],
        ..Tuned::default()
    };
    let command = [r".\tool.exe", "mark", "made-on-box.txt", "exit", exit];

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &command));

    let line = "gate: TRANSFER TIMED OUT after 1 s fetching the sync-back";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(!case.folder().join("made-on-box.txt").exists(), "nothing was applied: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
    (case, outcome)
}

#[test]
fn a_sync_back_fetch_past_its_transfer_ceiling_after_a_pass_is_transfer_timed_out() {
    let (case, outcome) = a_sync_back_held_past_its_transfer_ceiling("client-fetch-ceiling", "0");

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_eq!(verdict(&case), "transfer-timed-out");
}

#[test]
fn a_sync_back_fetch_past_its_transfer_ceiling_keeps_a_failed_command_s_status() {
    let (case, outcome) = a_sync_back_held_past_its_transfer_ceiling("client-fetch-ceiling-failed", "3");

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(outcome.stderr.contains("gate: FAILED (status 3)"), "{outcome:?}");
    assert_eq!(verdict(&case), "failed");
}

/// Through `local-ssh`, every attempt of the sync-back's `ls-remote` is held past the deadline
/// `--test-remote-watch-call ls-remote` gives it alone, after a command that exited 0.
#[test]
fn a_passing_run_whose_ls_remote_never_answers_ends_74_saying_the_command_passed() {
    let case = client_case("client-ls-remote-late");
    let hold = format!("hold {}", never(&case));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", &hold, &hold, &hold]);
    let tuned = Tuned {
        kind: "local-ssh",
        extra: &["--test-remote-watch-call", "ls-remote", "--test-remote-timeout-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(
        &case,
        "out.log",
        &steps,
        &tuned,
        &[r".\tool.exe", "mark", "made-on-box.txt"],
    ));

    assert_eq!(outcome.code, Some(74), "{outcome:?}");
    let line = "gate: TRANSFER FAILED: the command exited 0 on builder; git ls-remote did not answer within 300 ms";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(TRANSFER_RETRY).count(), 2, "{outcome:?}");
    let last = "gate: bringing the results back from builder failed (git ls-remote did not answer within 300 ms); trying \
                again (2 of 2)";
    assert!(outcome.stderr.contains(last), "{outcome:?}");
    assert!(!case.folder().join("made-on-box.txt").exists(), "nothing was applied: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "no fallback");
    assert_eq!(verdict(&case), "transfer-failed");
}

/// The sync-back's fetch is answered with bytes that are no protocol: git exits 128 for its own error, with no word of
/// a lost connection, so the fetch is not made again and the run fails as any git failure does.
#[test]
fn a_sync_back_fetch_that_git_fails_with_128_on_its_own_is_not_retried() {
    let outcome = a_failed_sync_back(
        "client-fetch-not-network",
        |_| vec!["ok".to_owned(), "garbage 8".to_owned()],
        &Tuned::default(),
    );

    assert!(outcome.stderr.contains("gate: the remote run failed: git "), "{outcome:?}");
    assert!(outcome.stderr.contains("(status 128)"), "{outcome:?}");
    assert!(!outcome.stderr.contains(TRANSFER_RETRY), "{outcome:?}");
    assert!(!outcome.stderr.contains("TRANSFER FAILED"), "{outcome:?}");
}

/// A sync-back fetch the watchdog stops for its silence is not made again.
#[test]
fn a_sync_back_fetch_that_stalls_is_not_retried() {
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "fetch", "--test-stall-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = a_failed_sync_back(
        "client-fetch-stall-once",
        |case| vec!["ok".to_owned(), format!("hold {}", never(case))],
        &tuned,
    );

    assert!(
        outcome
            .stderr
            .contains("gate: the remote run failed: git fetch did not answer for 300 ms"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains(TRANSFER_RETRY), "{outcome:?}");
}

/// Through `local-ssh`, a pair's main repository is fetched, then every attempt of the sibling's `ls-remote` exits
/// 255: the main repository's back commit is kept, and the TRANSFER FAILED line says where.
#[test]
fn a_pair_whose_second_transfer_fails_keeps_the_first_and_says_so() {
    let (case, _, name, sibling) = pair_case("client-pair-transfer-failed", &[], sibling_repository);
    let mut all = vec!["ok"; 6];
    all.extend(["drop"; 3]);
    let steps = ssh_steps(&case, "ssh", &all);
    let on_sibling = format!(r"..\{name}\made-on-sibling.txt");

    let outcome = finish(client_gate_through(
        &case,
        "local-ssh",
        "out.log",
        &steps,
        &[r".\tool.exe", "mark", "made-on-box.txt", "mark", &on_sibling],
    ));
    let synced = sibling.join("made-on-sibling.txt").exists();
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(74), "{outcome:?}");
    let said = outcome
        .stderr
        .lines()
        .find(|line| line.starts_with("gate: TRANSFER FAILED: the command exited 0 on builder; git ls-remote"));
    assert!(
        said.is_some_and(|line| line.contains("; the changes are kept: see git diff in each repo at refs/gate-remote/")),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr.matches(TRANSFER_RETRY).count(), 2, "{outcome:?}");
    assert!(!case.folder().join("made-on-box.txt").exists(), "nothing was applied: {outcome:?}");
    assert!(!synced, "nothing was applied to the sibling: {outcome:?}");
    assert_eq!(verdict(&case), "transfer-failed");
}

/// The first outputs call passes part of the stream through, then fails as a dropped connection does, but the box sent
/// it all and removed the archive: the retry is answered that the archive does not exist, so the pull stops there and
/// says the first call's failure.
#[test]
fn an_outputs_retry_that_finds_the_archive_gone_ends_transfer_failed_for_the_first_failure() {
    let case = outputs_case("client-outputs-fetch-gone", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "truncate 8", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(74), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(TRANSFER_RETRY).count(), 1, "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs", "outputs"]);
    let line = "gate: TRANSFER FAILED: the command exited 0 on builder; remote outputs not brought back: Connection timed out \
                during banner exchange";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new(), "no archive is left");
    assert_eq!(verdict(&case), "transfer-failed");
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

/// A command argument long enough that the run request overruns the input pipe's buffer, so a call that ends without
/// reading its input always fails the feeder's write.
fn long_argument() -> String {
    "x".repeat(16 * 1024)
}

#[test]
fn a_dropped_run_call_logs_no_line_saying_its_long_input_was_not_sent() {
    let case = client_case("client-rebooted-long");
    let steps = ssh_steps(&case, "ssh", &[&format!("boot {BOOTED}"), "drop", "ok", &format!("boot {REBOOTED}")]);
    let long = long_argument();

    let outcome = finish(lost_gate(&case, "local", &steps, &[r".\tool.exe", "out", &long]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "stop", "boot"], "the command is stopped first");
    assert_eq!(fallbacks(&events(&case)), ["rebooted"]);
    let log = log_texts(&case.folder().join("out.log"));
    let note = log.lines().find(|line| line.contains("input was not sent"));
    assert_eq!(note, None, "the call's own end says why");
}

#[test]
fn a_run_call_that_failed_after_the_banner_unread_logs_that_its_input_was_not_sent() {
    let case = client_case("client-run-unread");
    let steps = ssh_steps(&case, "ssh", &[&format!("boot {BOOTED}"), "say-exit 1 gate: the box failed"]);
    let long = long_argument();

    let outcome = finish(lost_gate(&case, "local", &steps, &[r".\tool.exe", "out", &long]));

    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.contains("gate: the run call's input was not sent"), "{outcome:?}\n{log}");
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
    // The attempt's lines, the cold note of the work folder made now among them, are said again after the command's
    // own: a tail of 40 lines keeps the command's line in sight.
    let tuned = Tuned {
        extra: &[HANG_GUARD_BOUND[0], HANG_GUARD_BOUND[1], "--tail", "40"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran here"]));

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
    // A tail of 40 lines keeps the command's line in sight below the attempt's, said again after it.
    let tuned = Tuned {
        extra: &["--test-remote-bound-ms", "200", "--tail", "40"],
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
    assert!(
        outcome
            .stderr
            .contains("gate: builder did not answer the ssh banner; trying again (1 of 1)"),
        "a bounded call's retry is its only one: {outcome:?}"
    );
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

/// The run call alone asks ssh to compress its connection, which carries the command's output; the preflight and the
/// outputs call, whose archive is compressed already, carry the options every call carries and no more.
#[test]
fn only_the_run_call_asks_ssh_for_compression() {
    let case = outputs_case("client-run-compressed", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\made.txt", "box"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"]);
    let words = call_words(&steps);
    let verbs: Vec<&str> = words.iter().filter_map(|line| line.split(' ').next()).collect();
    assert_eq!(verbs, ["preflight", "run", "outputs"], "{words:?}");
    let compressed: Vec<bool> = words.iter().map(|line| line.split(' ').any(|word| word == "Compression=yes")).collect();
    assert_eq!(compressed, [false, true, false], "{words:?}");
    assert!(
        words.iter().all(|line| line.contains("BatchMode=yes")),
        "every call carries ssh's options: {words:?}"
    );
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
    let log = log_texts(&case.folder().join("out.log"));
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
    run.wait_until("the run call", || calls(&steps).len() == 2);
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
    // The record names the builder the next run goes to, so the marker alone makes it full: a worktree with no record
    // sends full on its first run whether a marker is there or not.
    plant_outputs_from(&case, HOST);
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
    // The record names the builder the next run goes to, so the run is held to the marker alone: a worktree with no
    // record sends full on its first run.
    plant_outputs_from(&case, HOST);
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
/// file was written after a first run named its work folder, and the seam puts the run's snapshot time, which the
/// install stamps it with, an hour back, so the install keeps its hash; the pending marker has the box bring it back
/// though the run did not write it.
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

    let outcome = finish(aged_snapshot_gate(&case, "out.log", &steps, &command));

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
/// the hash of the file the outputs step put in place, stamped with the run's snapshot time, which the seam puts an
/// hour back.
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

    let outcome = finish(aged_snapshot_gate(&case, "second.log", &second, &command));

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
    // The record names the builder the next run goes to, so the marker alone makes it full: a worktree with no record
    // sends full on its first run whether a marker is there or not.
    plant_outputs_from(&case, HOST);
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
    run.wait_until("the run call", || calls(&steps).len() == 2);
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
        "argv",
    ];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(
        outcome.stdout.lines().any(|line| line.starts_with("-p:PathMap=")),
        "the box's build gets the path map, printed after argv: {outcome:?}"
    );
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

/// The start of the line of a fetch the watchdog stopped, which goes on with what it had fetched by then.
const FETCH_STOPPED_LINE: &str = "gate: remote outputs not brought back: the fetch was stopped by the watchdog";

/// Where in `stderr` the line of a fetch the watchdog stopped starts, once it says that either none or all of the
/// stand-in's `bytes`, fewer than 1024 and sent in one write, came in its whole seconds, at least 1, at the rate they
/// make together. A busy machine can start the stand-in after the watch's 300 ms, so the cut can come before its bytes.
fn fetch_stopped_at(stderr: &str, bytes: u64) -> usize {
    let pattern = format!(
        r"(?m)^{} after (0|{bytes}) B in (\d+) s \((\d+) B/s\)\r?$",
        regex::escape(FETCH_STOPPED_LINE)
    );
    let line = Regex::new(&pattern).expect("the line's pattern");
    let found = line.captures(stderr);
    assert!(found.is_some(), "the outputs line saying 0 or {bytes} B: {stderr}");
    let found = found.expect("the outputs line");
    let bytes: u64 = found[1].parse().expect("whole bytes");
    let seconds: u64 = found[2].parse().expect("whole seconds");
    let rate: u64 = found[3].parse().expect("a whole rate");
    assert!(seconds >= 1, "a fetch counts at least 1 s: {stderr}");
    assert_eq!(rate, bytes / seconds, "the rate is the bytes over the seconds: {stderr}");
    found.get(0).expect("the whole line").start()
}

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
const ONE_INSTALLED: &str =
    r"(?m)^gate: outputs: 1 file\(s\), \d+\.\d MB \(\d+\.\d MB compressed\) in (\d+\.\d) s \(pulled in (\d+\.\d) s, \d+\.\d MB/s\)\r?$";

/// The size the box's own size line in `stderr` names, in bytes.
fn box_outputs_size(stderr: &str) -> u64 {
    let line = Regex::new(r"(?m)^gate-remote-outputs-size: (\d+)\r?$").expect("the line's pattern");
    let found = line.captures(stderr);
    assert!(found.is_some(), "the box says the archive's size on a line of its own: {stderr}");
    found.expect("the box's line")[1].parse().expect("whole bytes")
}

/// The bytes a stand-in sends on a pull that does not end whole: one write, well inside the call's 1 MiB pipe, said as
/// `0.3 MB` by the pull's lines.
const PULL_BYTES: u64 = 300_000;

/// [`PULL_BYTES`] as the pull's lines say a size, in megabytes of 1048576 bytes to one decimal.
const SENT_MEGABYTES: f64 = 0.3;

/// The line of a pull that ended other than whole, its figure any.
const PULL_ENDED_ANY: &str = r"(?m)^gate: pulling outputs ended after \d+\.\d s with (\d+\.\d) MB received\r?$";

/// The megabytes the line of a pull that ended other than whole in `stderr` says it received.
fn ended_megabytes(stderr: &str) -> f64 {
    let found = Regex::new(PULL_ENDED_ANY).expect("the line's pattern").captures(stderr);
    assert!(found.is_some(), "the pull's early end: {stderr}");
    found.expect("the early end")[1].parse().expect("a figure")
}

/// The line of a pull that ended other than whole having received `received`, as the pull's lines say a size.
fn pull_ended(received: &str) -> Regex {
    let pattern = format!(
        r"(?m)^gate: pulling outputs ended after \d+\.\d s with {} received\r?$",
        regex::escape(received)
    );
    Regex::new(&pattern).expect("the line's pattern")
}

#[test]
fn a_pull_stopped_by_ctrl_c_says_how_much_it_received() {
    let case = outputs_case("client-outputs-pull-ctrl-c", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("bytes-hold {PULL_BYTES} {}", never(&case))]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "outputs"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &WRITE_BUILT));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    // The interrupt is raised at the call's first mark, which the raw copy makes only once the chunk it read is written
    // into the archive and counted, and the stand-in says nothing on standard error first: the figure is at least that
    // first chunk's, by order, however late the interrupt lands. `received > 0.0` also rests on that chunk being the raw
    // copy's first read, 64 KiB of the stand-in's one 300 000-byte write, past the 52 429 bytes below which the
    // one-decimal figure says 0.0. The call's drain can end before the stand-in's bytes are all read, so it is at most
    // what was sent.
    let ended = Regex::new(PULL_ENDED_ANY).expect("the line's pattern");
    assert_eq!(ended.find_iter(&outcome.stderr).count(), 1, "{outcome:?}");
    let received = ended_megabytes(&outcome.stderr);
    assert!(
        received > 0.0 && received <= SENT_MEGABYTES,
        "{received} MB of {PULL_BYTES} bytes sent: {outcome:?}"
    );
    let started = outcome.stderr.find("gate: pulling outputs from builder: 1 file(s)");
    assert!(
        started.is_some() && started < ended.find(&outcome.stderr).map(|found| found.start()),
        "{outcome:?}"
    );
}

#[test]
fn a_failed_pull_says_how_much_it_received() {
    let case = outputs_case("client-outputs-pull-failed", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("bytes-exit {PULL_BYTES} 3")]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &WRITE_BUILT));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let ended = pull_ended("0.3 MB");
    let at = ended.find(&outcome.stderr).map(|found| found.start());
    let failed = outcome
        .stderr
        .find("gate: remote outputs not brought back: the outputs call failed (status 3)");
    assert!(at.is_some() && at < failed, "the early end, then the existing line: {outcome:?}");
}

/// The hang hint as any phase says it, against a p90 seeded to 1 s, its run in seconds or, for a hint a starved watch
/// says 90 s or more into the run, in minutes.
const ANY_HINT: &str = r"gate: still running after \d+ (?:s|min), \d+x its usual p90 of 1 s; [^\n]+\n";

/// The seams of a run whose outputs call the stand-in holds: a stall window on that call alone that no case reaches, and
/// a progress line every 200 ms.
const PULL_HELD: [&str; 6] = [
    "--test-remote-watch-call",
    "outputs",
    "--test-stall-ms",
    "600000",
    "--test-outputs-progress-ms",
    "200",
];

/// [`PULL_HELD`] with `--test-remote-hint-from-pull`: the hang hint is due only during the pull, counted from its
/// start, so a case reads a hint said during the pull however long the run call and the sync-back take.
const PULL_HINTED: [&str; 7] = [
    PULL_HELD[0],
    PULL_HELD[1],
    PULL_HELD[2],
    PULL_HELD[3],
    PULL_HELD[4],
    PULL_HELD[5],
    "--test-remote-hint-from-pull",
];

/// Starts `command` routed under `seams` ([`PULL_HELD`] or [`PULL_HINTED`]), logging to `log`, its calls taking the
/// steps of a file named for `log`, its outputs call held by the stand-in until the file `release` names is made.
fn start_held_pull(case: &Case, log: &str, release: &Go, command: &[&str], seams: &[&str]) -> Running {
    let steps = ssh_steps(case, log, &["ok", "ok", &format!("bytes-hold 100 {}", shown(release.path()))]);
    let tuned = Tuned {
        extra: seams,
        ..Tuned::default()
    };
    start(client_gate_tuned(case, log, &steps, &tuned, command))
}

/// The command waits for `go` in the folder it runs in, then writes an output: the first five runs' snapshots send `go`,
/// so they pass at once. The first held run keeps `go` and, under `--test-remote-hint-from-pull`, can be hinted only
/// during its held pull; the second has none, so it is hinted during its run call, which it holds until the hint, and,
/// let go, says no second hint during its pull, which outlasts 3x the p90 too.
#[test]
fn a_routed_run_hinted_during_its_pull_names_the_pull_once() {
    let case = outputs_case("client-outputs-pull-hint", &BIN);
    let command = [r".\tool.exe", "wait", "go", "write", r"bin\new.txt", "hinted"];
    let history = ssh_steps(&case, "history", &["ok"; 15]);
    write(&case, "go", "");
    for _ in 0..5 {
        let outcome = finish(client_gate(&case, "history.log", &history, &command));
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
    }
    let hint = Regex::new(ANY_HINT).expect("the pattern is valid");
    // Each span in seconds or, for a hint a starved watch says 90 s or more into its phase, in minutes.
    let pulling = Regex::new(r"gate: still running after \d+ (?:s|min), \d+x its usual p90 of 1 s; pulling outputs from builder for \d+ (?:s|min)\n")
        .expect("the pattern is valid");
    let running_on = Regex::new(r"gate: still running after \d+ (?:s|min), \d+x its usual p90 of 1 s; running on builder for \d+ (?:s|min)\n")
        .expect("the pattern is valid");
    // The PC must lack the output, or the box packs nothing and no pull is made.
    let take_output = |case: &Case| {
        let built = case.folder().join("bin").join("new.txt");
        if built.exists() {
            fs::remove_file(&built).expect("the output can be removed");
        }
    };

    seed(&case, " hinted", 1.0, None);
    take_output(&case);
    let release = Go(case.folder().join("release-pull"));
    let mut running = start_held_pull(&case, "pull.log", &release, &command, &PULL_HINTED);
    running.wait_for_stderr_while_running("the pull's hint", |stderr| pulling.is_match(stderr));
    fs::write(release.path(), "").expect("the release file can be made");
    let during_pull = running.finish();
    assert_eq!(hint.find_iter(&during_pull.stderr).count(), 1, "one hint: {during_pull:?}");
    assert!(pulling.is_match(&during_pull.stderr), "it names the pull: {during_pull:?}");

    seed(&case, " hinted", 1.0, None);
    take_output(&case);
    fs::remove_file(case.folder().join("go")).expect("the go file can be removed");
    let go = Go(case.folder().join("builder").join("work").join(key_of(&events(&case))).join("go"));
    let release = Go(case.folder().join("release-run"));
    let progress = Regex::new(r"(?m)^gate: pulling outputs: ").expect("the pattern is valid");
    let mut running = start_held_pull(&case, "run.log", &release, &command, &PULL_HELD);
    running.wait_for_stderr_while_running("the run call's hint", |stderr| running_on.is_match(stderr));
    fs::write(go.path(), "").expect("the go file can be made");
    // Two progress lines: the pull has run past 3x the p90 as its run counts, the hint already said.
    running.wait_for_stderr_while_running("the pull's progress", |stderr| progress.find_iter(stderr).count() >= 2);
    fs::write(release.path(), "").expect("the release file can be made");
    let both = running.finish();

    assert_eq!(hint.find_iter(&both.stderr).count(), 1, "one hint across both phases: {both:?}");
    assert!(running_on.is_match(&both.stderr), "it names the run call: {both:?}");
}

/// The pause before the retried outputs call of [`a_pull_retried_names_its_phase_time_from_the_pull_s_start`], in
/// milliseconds and in whole seconds.
const PULL_RETRY_PAUSE: (&str, u64) = ("8000", 8);

/// The pull's first outputs call fails before the banner and the retry waits [`PULL_RETRY_PAUSE`]. Under
/// `--test-remote-hint-from-pull` the hint is due only during the pull, 9 s after its start against a p90 of 3 s; the
/// retried call is held until the hint is said, by either call's watch, so the case asks no order of the retry and the
/// hint. Due 9 s into the pull, the hint names a phase of at least the pause either way; that the phase counts from the
/// pull's start, never the call's, is the client's unit test of the hint's times.
#[test]
fn a_pull_retried_names_its_phase_time_from_the_pull_s_start() {
    let case = outputs_case("client-outputs-pull-retried", &BIN);
    let command = [r".\tool.exe", "write", r"bin\new.txt", "retried"];
    let history = ssh_steps(&case, "history", &["ok"; 15]);
    for _ in 0..5 {
        let outcome = finish(client_gate(&case, "history.log", &history, &command));
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
    }
    seed(&case, " retried", 3.0, None);
    // The PC must lack the output, or the box packs nothing and no pull is made.
    fs::remove_file(case.folder().join("bin").join("new.txt")).expect("the output can be removed");
    let release = Go(case.folder().join("release"));
    let steps = ssh_steps(
        &case,
        "ssh",
        &["ok", "ok", "fail-banner", &format!("bytes-hold 100 {}", shown(release.path()))],
    );
    let tuned = Tuned {
        extra: &PULL_HINTED,
        pause: PULL_RETRY_PAUSE.0,
        ..Tuned::default()
    };
    // In seconds or, for a hint a starved watch says 90 s or more into the pull, in minutes.
    let pulling =
        Regex::new(r"gate: still running after \d+ (?:s|min), \d+x its usual p90 of 3 s; pulling outputs from builder for (\d+) (s|min)\r?\n")
            .expect("the pattern is valid");

    let mut running = start(client_gate_tuned(&case, "out.log", &steps, &tuned, &command));
    running.wait_for_stderr_while_running("the pull's hint", |stderr| pulling.is_match(stderr));
    fs::write(release.path(), "").expect("the release file can be made");
    // The stand-in's 100 bytes are no archive, so the install fails: the case reads the hint alone.
    let outcome = running.finish();

    assert!(
        outcome
            .stderr
            .contains("gate: builder did not answer the ssh banner; trying again (1 of "),
        "the first outputs call was made again: {outcome:?}"
    );
    let found = pulling.captures(&outcome.stderr).expect("the pull's hint");
    let figure: u64 = found[1].parse().expect("a whole number");
    let phase = if &found[2] == "min" { figure * 60 } else { figure };
    assert!(
        phase >= PULL_RETRY_PAUSE.1,
        "the pull's phase is at least the pause, due 9 s into the pull: {phase} s: {outcome:?}"
    );
}

#[test]
fn a_routed_pull_says_its_start_with_the_box_s_size() {
    let case = outputs_case("client-outputs-pull-start", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &WRITE_BUILT));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let size = box_outputs_size(&outcome.stderr);
    assert!(size > 0 && size < 50_000, "a one-file archive is said as 0.0 MB: {size} bytes");
    let start = "gate: pulling outputs from builder: 1 file(s), 0.0 MB";
    let said = outcome.stderr.lines().position(|line| line == start);
    let installed = outcome.stderr.lines().position(|line| line.starts_with("gate: outputs: 1 file(s)"));
    assert!(said.is_some() && said < installed, "the start line, then the end line: {outcome:?}");
    assert_eq!(outcome.stderr.matches(start).count(), 1, "said once: {outcome:?}");
    assert!(log_texts(&case.folder().join("out.log")).contains(start), "the log has it too");
}

#[test]
fn a_long_pull_says_its_progress_every_period() {
    let case = outputs_case("client-outputs-pull-progress", &BIN);
    let release = Go(case.folder().join("release"));
    // The box's size line names a size, so a progress line with bytes in says how much of it and how long is left.
    let progress =
        Regex::new(r"(?m)^gate: pulling outputs: \d+\.\d MB of \d+\.\d MB, \d+\.\d MB/s, about \d+ (s|min) left\r?$").expect("the line's pattern");

    let mut running = start_held_pull(&case, "out.log", &release, &WRITE_BUILT, &PULL_HELD);
    running.wait_for_stderr_while_running("two progress lines", |stderr| progress.find_iter(stderr).count() >= 2);
    fs::write(release.path(), "").expect("the release file can be made");
    let outcome = running.finish();

    assert!(progress.find_iter(&outcome.stderr).count() >= 2, "{outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert!(progress.find_iter(&log).count() >= 2, "the log has them too: {log}");
}

#[test]
fn a_passing_pull_s_end_line_says_the_fetch_alone() {
    let case = outputs_case("client-outputs-pull-end", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &WRITE_BUILT));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = Regex::new(ONE_INSTALLED).expect("the line's pattern");
    let found = line.captures(&outcome.stderr);
    assert!(found.is_some(), "{outcome:?}");
    let found = found.expect("the end line");
    let whole: f64 = found[1].parse().expect("the step's seconds");
    let pulled: f64 = found[2].parse().expect("the fetch's seconds");
    assert!(pulled <= whole, "the fetch alone is within the whole step: {outcome:?}");
    assert!(
        !outcome.stderr.contains("gate: pulling outputs ended"),
        "a passing pull says no early end: {outcome:?}"
    );
}

#[test]
fn a_stopped_pull_says_how_much_it_received() {
    let case = outputs_case("client-outputs-pull-stopped", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("bytes-hold {PULL_BYTES} {}", never(&case))]);
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "outputs", "--test-stall-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &WRITE_BUILT));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    // The cut can come before the stand-in's write or between any two of the raw copy's 64 KiB reads: whatever the bytes
    // by then, the early end says the count the watchdog's line says, both read at the cut.
    let stopped = Regex::new(&format!(
        r"(?m)^{} after (\d+(?:\.\d)?) (B|KiB|MiB) in \d+ s ",
        regex::escape(FETCH_STOPPED_LINE)
    ))
    .expect("the line's pattern");
    let found = stopped.captures(&outcome.stderr);
    assert!(found.is_some(), "the watchdog's line: {outcome:?}");
    let found = found.expect("the watchdog's line");
    let unit = match &found[2] {
        "B" => 1.0,
        "KiB" => 1024.0,
        _ => 1_048_576.0,
    };
    let watchdog_bytes: f64 = found[1].parse::<f64>().expect("a figure") * unit;
    let ended = Regex::new(PULL_ENDED_ANY).expect("the line's pattern");
    let at = ended.find(&outcome.stderr).map(|found| found.start());
    let received = ended_megabytes(&outcome.stderr);
    // Each line rounds to its own tenth; one 64 KiB read more or fewer (0.0625 MB) is past what the two roundings allow.
    assert!(
        (received - watchdog_bytes / 1_048_576.0).abs() <= 0.051,
        "the early end's {received} MB and the watchdog's {watchdog_bytes} bytes are one count: {outcome:?}"
    );
    let stalled = outcome.stderr.find(STALLED_LINE);
    assert!(at.is_some() && at < stalled, "the early end, then the existing lines: {outcome:?}");
}

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

/// The installed output's last write is the time the run's snapshot was taken: not before the gate started, not after
/// it ended, and before the box's command marked [`RAN`]. The command writes the output a second after that mark, so
/// the box's own last write, which the archive carries in whole seconds, falls after it.
#[test]
fn a_routed_build_s_installed_outputs_carry_the_snapshot_time() {
    let case = outputs_case("client-outputs-snapshot-time", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "mark", RAN, "sleep", "1000", "write", r"bin\new.txt", "built"];
    let before = SystemTime::now();

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    let after = SystemTime::now();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let last_write = |path: &Path| fs::metadata(path).and_then(|metadata| metadata.modified()).expect("a last write");
    let installed = last_write(&case.folder().join("bin").join("new.txt"));
    let marked = last_write(&work_folder(&case).join(RAN));
    assert!(installed >= before, "not before the gate started: {installed:?}, {before:?}");
    assert!(installed <= after, "not after the gate ended: {installed:?}, {after:?}");
    assert!(installed < marked, "taken before the box's command ran: {installed:?}, {marked:?}");
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
/// after it, and the last run. Each run takes its steps from a file of its own: a call the watchdog kills before the
/// stand-in has logged it leaves its step untaken, which a shared file would hand to the next run's first call.
fn between_two_hits(case: &Case, prepare: impl FnOnce(&Case), between: &Between<'_>) -> (Outcome, Option<String>, Outcome) {
    let hit = ["ok", "ok", PAST_THE_CAP];
    let first = low_budget_run(case, &ssh_steps(case, "first", &hit), "first.log", &WRITE_BUILT);
    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(cap_record_text(case).as_deref(), Some(r#"{"v":1,"hits":1}"#), "{first:?}");
    prepare(case);
    let steps = ssh_steps(case, "between", between.steps);
    let middle = low_budget_run_with(case, &steps, "second.log", between.more, between.command);
    let after = cap_record_text(case);
    let last = low_budget_run(
        case,
        &ssh_steps(case, "last", &hit),
        "third.log",
        &[r".\tool.exe", "write", r"bin\new.txt", "rebuilt"],
    );
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
    fetch_stopped_at(&middle.stderr, 100);
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
fn a_failing_run_with_no_test_results_glob_makes_no_outputs_call() {
    let case = outputs_case("client-outputs-failing", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\built.txt", "box", "exit", "3"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new());
    assert!(pending_marker(&case).exists(), "{outcome:?}");
}

/// The output globs of a reports case: every file under a top `bin` folder, and every file under a `TestResults` folder.
const REPORTS: [&str; 2] = ["bin/**", "**/TestResults/**"];

/// A failing command that builds `bin\built.txt` and writes the test report `TestResults\r.trx`, then exits 3.
const FAILING_WITH_A_REPORT: [&str; 8] = [
    r".\tool.exe",
    "write",
    r"bin\built.txt",
    "box",
    "write",
    r"TestResults\r.trx",
    "<TestRun />",
    "exit",
];

/// [`FAILING_WITH_A_REPORT`] with its exit status.
fn failing_with_a_report() -> Vec<&'static str> {
    FAILING_WITH_A_REPORT.iter().copied().chain(["3"]).collect()
}

/// An [`outputs_case`] with the [`REPORTS`] globs, whose `.gitignore` leaves `TestResults/` out beside `bin/`, as a test
/// run's does.
fn reports_case(name: &str) -> Case {
    let case = outputs_case(name, &REPORTS);
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\nTestResults/\n"));
    case
}

/// Whether a line of `outcome`'s standard output or standard error starts with `start`.
fn says_a_line_starting(outcome: &Outcome, start: &str) -> bool {
    outcome.stdout.lines().chain(outcome.stderr.lines()).any(|line| line.starts_with(start))
}

#[test]
fn a_failing_run_brings_its_test_reports_back() {
    let case = reports_case("client-outputs-failing-reports");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &failing_with_a_report()));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"], "{outcome:?}");
    assert_eq!(worktree_text(&case, r"TestResults\r.trx"), "<TestRun />", "the report is installed");
    assert!(
        !case.folder().join("bin").join("built.txt").exists(),
        "nothing under bin is installed: {outcome:?}"
    );
    assert!(
        says_a_line_starting(&outcome, "gate: the failed run's test reports: 1 file(s)"),
        "{outcome:?}"
    );
    assert!(!says_a_line_starting(&outcome, "gate: outputs:"), "{outcome:?}");
    assert!(pending_marker(&case).exists(), "a failed run leaves the marker: {outcome:?}");
}

#[test]
fn a_failing_run_whose_test_reports_cannot_be_fetched_keeps_its_status() {
    let case = reports_case("client-outputs-failing-reports-unfetched");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "exit 3"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &failing_with_a_report()));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(
        says_a_line_starting(&outcome, "gate: the failed run's test reports not brought back:"),
        "{outcome:?}"
    );
    assert!(
        !case.folder().join("TestResults").join("r.trx").exists(),
        "nothing is installed: {outcome:?}"
    );
}

#[test]
fn a_failing_run_whose_box_packed_no_report_says_nothing() {
    let case = reports_case("client-outputs-failing-no-report");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\built.txt", "box", "exit", "3"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 0"), "the box packed nothing: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "{outcome:?}");
    let said = format!("{}{}", outcome.stdout, outcome.stderr);
    assert!(!said.contains("test reports"), "{outcome:?}");
    assert!(!said.contains("outputs not brought back"), "{outcome:?}");
    assert!(pending_marker(&case).exists(), "a failed run leaves the marker: {outcome:?}");
}

/// An [`outputs_case`] with the output globs `globs` and the failure output globs `failure`, whose `.gitignore` leaves
/// `dumps/` out beside `bin/`, as a run that writes crash dumps does.
fn failure_case(name: &str, globs: &[&str], failure: &[&str]) -> Case {
    let case = client_case(name);
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\ndumps/\n"));
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": HOST,
        "slots": 2,
        "repos": { &repo: ["*tool*", "*dotnet*"] },
        "outputs": { &repo: globs },
        "failureOutputs": { &repo: failure },
    });
    write_remote_json(&case, &routing.to_string());
    case
}

/// A failing command that builds `bin\built.txt` and writes the dump `dumps\a.dmp`, then exits 3.
const FAILING_WITH_A_DUMP: [&str; 9] = [
    r".\tool.exe",
    "write",
    r"bin\built.txt",
    "box",
    "write",
    r"dumps\a.dmp",
    "dump",
    "exit",
    "3",
];

#[test]
fn a_failing_run_with_a_failure_output_glob_brings_its_file_back_keeps_its_status_and_names_the_outputs() {
    let case = failure_case("client-failure-outputs", &BIN, &["dumps/*.dmp"]);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &FAILING_WITH_A_DUMP));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"], "{outcome:?}");
    assert_eq!(worktree_text(&case, r"dumps\a.dmp"), "dump", "the dump is installed");
    assert!(
        !case.folder().join("bin").join("built.txt").exists(),
        "nothing under bin is installed: {outcome:?}"
    );
    assert!(says_a_line_starting(&outcome, "gate: the failed run's outputs: 1 file(s)"), "{outcome:?}");
    assert!(!says_a_line_starting(&outcome, "gate: the failed run's test reports"), "{outcome:?}");
    assert!(pending_marker(&case).exists(), "a failed run leaves the marker: {outcome:?}");
}

#[test]
fn a_passing_run_does_not_bring_back_a_file_only_a_failure_output_glob_matches() {
    let case = failure_case("client-failure-outputs-passing", &BIN, &["dumps/*.dmp"]);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\built.txt", "box", "write", r"dumps\a.dmp", "dump"];

    let outcome = finish(client_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(worktree_text(&case, r"bin\built.txt"), "box", "the build comes back");
    assert!(
        !case.folder().join("dumps").join("a.dmp").exists(),
        "the dump stays on the builder: {outcome:?}"
    );
}

#[test]
fn a_failing_run_with_only_failure_outputs_builds_the_manifest_and_brings_its_file_back() {
    let case = failure_case("client-failure-outputs-only", &[], &["dumps/*.dmp"]);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &FAILING_WITH_A_DUMP));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"], "{outcome:?}");
    assert_eq!(worktree_text(&case, r"dumps\a.dmp"), "dump", "the dump is installed");
    assert!(
        !case.folder().join("bin").join("built.txt").exists(),
        "no output glob names bin: {outcome:?}"
    );
}

/// A failing run that brings its test reports back leaves the count: the hits on either side of it are in a row.
#[test]
fn a_failing_run_bringing_reports_between_two_fetches_past_the_cap_leaves_the_count() {
    let case = reports_case("client-outputs-cap-then-failing-reports");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", PAST_THE_CAP, "ok", "ok", "ok", "ok", "ok", PAST_THE_CAP]);

    let first = low_budget_run(&case, &steps, "first.log", &WRITE_BUILT);
    let failed = low_budget_run(&case, &steps, "second.log", &failing_with_a_report());
    let after_failed = cap_record_text(&case);
    let third = low_budget_run(&case, &steps, "third.log", &WRITE_BUILT);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(failed.code, Some(3), "{failed:?}");
    assert_eq!(
        worktree_text(&case, r"TestResults\r.trx"),
        "<TestRun />",
        "the report came back: {failed:?}"
    );
    assert_eq!(after_failed.as_deref(), Some(r#"{"v":1,"hits":1}"#), "the failing run leaves the count");
    assert_eq!(third.code, Some(1), "{third:?}");
    assert!(third.stderr.contains(&capped_again(2)), "{third:?}");
    assert_eq!(
        calls(&steps),
        [
            "preflight",
            "run",
            "outputs",
            "preflight",
            "run",
            "outputs",
            "preflight",
            "run",
            "outputs"
        ]
    );
}

/// A passing run's fetch past the cap counts one hit; the failing run after it fetches its reports past the cap too,
/// which neither counts a second hit nor clears the first.
#[test]
fn a_failing_run_s_reports_past_the_fetch_cap_keep_its_status_and_the_count() {
    let case = reports_case("client-outputs-failing-reports-capped");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", PAST_THE_CAP, "ok", "ok", PAST_THE_CAP]);

    let first = low_budget_run(&case, &steps, "first.log", &WRITE_BUILT);
    let failed = low_budget_run(&case, &steps, "second.log", &failing_with_a_report());

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(failed.code, Some(3), "the command's own status: {failed:?}");
    let line = "gate: the failed run's test reports not brought back: the archive passed 2.0 MiB";
    assert!(failed.stderr.contains(line), "{failed:?}");
    assert!(!failed.stderr.contains("passed the cap again"), "{failed:?}");
    assert_eq!(
        cap_record_text(&case).as_deref(),
        Some(r#"{"v":1,"hits":1}"#),
        "the count is unchanged: {failed:?}"
    );
    assert!(
        !case.folder().join("TestResults").join("r.trx").exists(),
        "nothing is installed: {failed:?}"
    );
}

/// The box's archive of the failed run's reports is swapped for one holding `bin/built.txt`, which the run's full globs
/// match but the narrowed ones do not: the install refuses it, as it refuses a member outside the globs, the run keeps
/// its status, and the refusal, which a passing run's would clear the marker for, leaves it.
#[test]
fn a_refused_install_of_a_failing_run_s_reports_keeps_its_status_and_the_marker() {
    let case = reports_case("client-outputs-failing-reports-refused");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &failing_with_a_report()));
    run.wait_until("the outputs call", || calls(&steps).len() == 3);
    fs::write(box_output_archive(&case), crafted_archive("bin/built.txt", "box")).expect("the archive can be replaced");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    let line = "gate: the failed run's test reports not brought back: the archive member bin/built.txt is not under the output globs";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(
        !case.folder().join("bin").join("built.txt").exists(),
        "nothing of it is installed: {outcome:?}"
    );
    assert!(pending_marker(&case).exists(), "a failed run leaves the marker: {outcome:?}");
}

/// The reports' pull sends a few bytes, then nothing: the watchdog stops it, and the run ends killed, as a passing
/// run's does, not on the command's status.
#[test]
fn a_failing_run_s_reports_pull_the_watchdog_stops_ends_124() {
    let case = reports_case("client-outputs-failing-reports-stalled");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("bytes-hold 100 {}", never(&case))]);
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "outputs", "--test-stall-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &failing_with_a_report()));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains(STALLED_LINE), "{outcome:?}");
    assert!(
        says_a_line_starting(
            &outcome,
            "gate: the failed run's test reports not brought back: the fetch was stopped by the watchdog"
        ),
        "{outcome:?}"
    );
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
    let stopped = fetch_stopped_at(&outcome.stderr, 100);
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

/// Each of the outputs call's three attempts is made again before the banner until its own retries run out: the run
/// that passed ends TRANSFER FAILED.
#[test]
fn a_fetch_dropped_before_any_byte_ends_transfer_failed_once_every_retry_ran_out() {
    let case = outputs_case("client-outputs-fetch-banner", &BIN);
    let mut all = vec!["ok", "ok"];
    all.extend(["fail-banner"; 12]);
    let steps = ssh_steps(&case, "ssh", &all);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(74), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(RETRY_LINE).count(), 9, "{outcome:?}");
    assert_eq!(outcome.stderr.matches(TRANSFER_RETRY).count(), 2, "{outcome:?}");
    let mut expected = vec!["preflight", "run"];
    expected.extend(["outputs"; 12]);
    assert_eq!(calls(&steps), expected);
    let line = "gate: TRANSFER FAILED: the command exited 0 on builder; remote outputs not brought back: Connection timed out \
                during banner exchange";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new(), "nothing is left of any attempt");
    assert!(pending_marker(&case).exists(), "{outcome:?}");
    assert_eq!(verdict(&case), "transfer-failed");
}

/// The stand-in passes 8 bytes of the stream, then exits 255 as a connection dropped mid-stream does: the box still
/// holds its archive, so the call is made again after the pause and brings the outputs back whole.
#[test]
fn a_passing_run_whose_outputs_fetch_fails_once_retries_it_and_brings_them_back() {
    let case = outputs_case("client-outputs-fetch-retried", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "bytes-exit 8 255", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains(RETRY_LINE), "no pre-banner retry: {outcome:?}");
    assert_eq!(outcome.stderr.matches(TRANSFER_RETRY).count(), 1, "{outcome:?}");
    let retry = "gate: bringing the results back from builder failed (remote outputs not brought back: the outputs call \
                 failed (status 255)); trying again (1 of 2)";
    assert!(outcome.stderr.contains(retry), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs", "outputs"]);
    let installed = fs::read_to_string(case.folder().join("bin").join("new.txt"));
    assert_eq!(installed.ok().as_deref(), Some("built"), "{outcome:?}");
    assert_eq!(fetched_archives(&case), Vec::<PathBuf>::new(), "no archive is left");
}

/// The outputs call exits 1 with an error line of its own, no failure of ssh's: it is not made again, and the run that
/// passed fails as before.
#[test]
fn a_fetch_failure_that_is_not_ssh_s_is_not_retried() {
    let case = outputs_case("client-outputs-fetch-not-ssh", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "say-exit 1 fatal: the archive cannot be read"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\new.txt", "built"]));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    assert!(!outcome.stderr.contains(TRANSFER_RETRY), "{outcome:?}");
    assert!(!outcome.stderr.contains("TRANSFER FAILED"), "{outcome:?}");
    let line = "gate: remote outputs not brought back: fatal: the archive cannot be read";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"]);
    assert_eq!(verdict(&case), "failed");
}

/// The outputs call sends a few bytes, then nothing, past the transfer ceiling `--test-remote-watch-call outputs` gives
/// it alone.
#[test]
fn an_outputs_fetch_past_its_transfer_ceiling_is_transfer_timed_out() {
    let case = outputs_case("client-outputs-fetch-ceiling", &BIN);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("bytes-hold 100 {}", never(&case))]);
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "outputs", "--test-remote-timeout-ms", WATCHED_MS],
        ..Tuned::default()
    };
    let command = [r".\tool.exe", "write", r"bin\new.txt", "built"];

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &command));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let line = "gate: TRANSFER TIMED OUT after 1 s fetching the outputs";
    let timed_out = outcome.stderr.find(line).expect("the TRANSFER TIMED OUT line");
    let stopped = fetch_stopped_at(&outcome.stderr, 100);
    assert!(timed_out < stopped, "the kill line comes first: {outcome:?}");
    assert_eq!(verdict(&case), "transfer-timed-out");
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

/// A `.tar.zst` outputs archive holding one symlink member `path` and the `.gate-outputs.json` list agreeing with an
/// empty file, as a hostile builder would send: the reader refuses a member that is not a file, for what it is.
fn crafted_link_archive(path: &str) -> Vec<u8> {
    let list = format!(r#"[{{"path":"{path}","size":0,"sha256":"{}"}}]"#, sha256_hex(""));
    let mut tar = tar::Builder::new(Vec::new());
    let members: [(&str, tar::EntryType, &[u8]); 2] = [
        (path, tar::EntryType::Symlink, &b""[..]),
        (".gate-outputs.json", tar::EntryType::Regular, list.as_bytes()),
    ];
    for (name, kind, data) in members {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().expect("a GNU header").name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_entry_type(kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_link_name("bin/target").expect("the link's target is set");
        header.set_cksum();
        tar.append(&header, data).expect("the member can be appended");
    }
    zstd::encode_all(&tar.into_inner().expect("the tar ends")[..], 3).expect("the tar compresses")
}

/// A hostile builder's archive carries a root `.lfsconfig`, which a sync-back never writes: the install refuses it, so
/// nothing of it reaches the worktree, the run becomes 1, and the refusal, decided by the archive's own members, clears
/// the pending marker.
#[test]
fn an_archive_member_that_is_an_lfsconfig_is_refused_by_the_install() {
    let case = outputs_case("client-outputs-lfsconfig", &["**"]);
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\a.txt", "built"]));
    run.wait_until("the outputs call", || calls(&steps).len() == 3);
    // The box's own archive is swapped for one carrying the `.lfsconfig`, before the call reads it.
    fs::write(box_output_archive(&case), crafted_archive(".lfsconfig", "lfs")).expect("the archive can be replaced");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let line = "gate: remote outputs not brought back: the archive member .lfsconfig is not under the output globs";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(!case.folder().join(".lfsconfig").exists(), "nothing of it is installed: {outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(
        !pending_marker(&case).exists(),
        "a refusal of the archive's own members clears the marker: {outcome:?}"
    );
}

/// A refusal the archive's own members decide still ends the run 1, but leaves the pending marker cleared, so the next
/// routed run of the worktree sends its normal manifest: a file the builder's work folder holds from an earlier run that
/// this run leaves alone comes back only under `OutputsFull`, and so it stays where it is.
#[test]
fn a_refused_install_of_a_member_outside_the_globs_clears_the_pending_marker() {
    let case = outputs_case("client-outputs-refusal-marker", &BIN);
    write(&case, r"bin\left.txt", "pc");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\a.txt", "built"]));
    run.wait_until("the outputs call", || calls(&steps).len() == 3);
    fs::write(box_output_archive(&case), crafted_archive(".lfsconfig", "lfs")).expect("the archive can be replaced");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let line = "gate: remote outputs not brought back: the archive member .lfsconfig is not under the output globs";
    assert!(outcome.stderr.contains(line), "{outcome:?}");
    assert!(
        !pending_marker(&case).exists(),
        "a refusal of the archive's own members clears the marker: {outcome:?}"
    );
    // The run refused its archive whole, so the PC keeps no `bin/a.txt`; the builder's work folder, which holds it, gains
    // a file an earlier run built and this run leaves alone.
    let key = key_of(&events(&case));
    // The record names the builder the next run goes to, so the run is held to the marker alone: a worktree with no
    // record sends full on its first run.
    plant_outputs_from(&case, HOST);
    write(&case, &format!(r"builder\work\{key}\bin\left.txt"), "box");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate-remote-outputs: 1 "),
        "the file only this run's own delta holds comes back: {outcome:?}"
    );
    assert_eq!(
        worktree_text(&case, r"bin\left.txt"),
        "pc",
        "the file only a full send would bring back stays: {outcome:?}"
    );
}

/// A builder's archive carries a member that is not a file: the install's reader refuses it for what it is, so the run
/// becomes 1, nothing of it is installed, and the refusal clears the pending marker.
#[test]
fn an_archive_with_a_link_member_is_refused_and_the_pending_marker_cleared() {
    let case = outputs_case("client-outputs-link", &BIN);
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\a.txt", "built"]));
    run.wait_until("the outputs call", || calls(&steps).len() == 3);
    fs::write(box_output_archive(&case), crafted_link_archive("bin/link")).expect("the archive can be replaced");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let refused = "cannot be opened: the outputs archive member 'bin/link' is not a file";
    assert!(outcome.stderr.contains(refused), "{outcome:?}");
    assert!(
        !case.folder().join("bin").join("link").exists(),
        "nothing of it is installed: {outcome:?}"
    );
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(
        !pending_marker(&case).exists(),
        "a refusal of the archive's own members clears the marker: {outcome:?}"
    );
}

/// A builder's archive arrives cut short: the install cannot read the stream, a fault a refetch may clear, so the run
/// becomes 1 and the pending marker stays for the next run's full send.
#[test]
fn a_truncated_archive_keeps_the_pending_marker() {
    let case = outputs_case("client-outputs-cut", &BIN);
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("hold {}", shown(go.path()))]);
    let run = start(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "write", r"bin\a.txt", "built"]));
    run.wait_until("the outputs call", || calls(&steps).len() == 3);
    let whole = crafted_archive("bin/a.txt", "box");
    fs::write(box_output_archive(&case), &whole[..whole.len() / 2]).expect("the archive can be cut short");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    assert!(outcome.stderr.contains("cannot be opened: "), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("is not a file"),
        "the stream, not a member, is what failed: {outcome:?}"
    );
    assert!(
        pending_marker(&case).exists(),
        "a fault a refetch may clear keeps the marker: {outcome:?}"
    );
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
    run.wait_until("the outputs call", || calls(&steps).len() == 3);
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
    run.wait_until("the run call", || calls(&steps).len() == 2);
    write(&case, r"bin\both.txt", "edited during the run");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_refused_install(&case, &outcome, "bin/both.txt changed during the run", "edited during the run");
}

/// The PC's `bin/both.txt` is rewritten while the run call is held with the bytes the builder writes, as a local build of
/// the same sources would: it is already in place, so the pass stands, only `bin/new.txt` is moved, and the marker is
/// cleared.
#[test]
fn a_pc_output_rewritten_with_the_same_bytes_during_the_run_counts_as_brought_back() {
    let case = outputs_case("client-outputs-install-same-bytes", &BIN);
    write(&case, r"bin\both.txt", "pc");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path())), "ok"]);
    let run = start(client_gate(&case, "out.log", &steps, &BOTH_AND_NEW));
    run.wait_until("the run call", || calls(&steps).len() == 2);
    write(&case, r"bin\both.txt", "box");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    assert!(!outcome.stderr.contains("not brought back"), "{outcome:?}");
    assert_eq!(worktree_text(&case, r"bin\both.txt"), "box", "the file holds the builder's bytes");
    assert_eq!(worktree_text(&case, r"bin\new.txt"), "built", "the other file is installed");
    let line = regex::Regex::new(ONE_INSTALLED).expect("the line's pattern");
    assert!(line.is_match(&outcome.stderr), "only the moved file is counted: {outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(!pending_marker(&case).exists(), "the marker is cleared: {outcome:?}");
}

/// The PC's coverage roots file is rewritten while the run call is held, as a local build in the same worktree would: it
/// already records the PC's roots, so it is kept as the PC wrote it, the pass stands, only `bin/new.txt` is moved, and
/// the marker is cleared.
#[test]
fn a_coverage_roots_file_a_pc_build_rewrote_during_the_run_is_kept() {
    let case = outputs_case("client-outputs-install-roots-kept", &BIN);
    write(&case, r"bin\.msCoverageSourceRootsMapping_App", "pc");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path())), "ok"]);
    let command = [
        r".\tool.exe",
        "write",
        r"bin\.msCoverageSourceRootsMapping_App",
        "the builder's roots",
        "write",
        r"bin\new.txt",
        "built",
    ];
    let run = start(client_gate(&case, "out.log", &steps, &command));
    run.wait_until("the run call", || calls(&steps).len() == 2);
    write(&case, r"bin\.msCoverageSourceRootsMapping_App", "the PC build's roots");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    assert!(!outcome.stderr.contains("not brought back"), "{outcome:?}");
    assert_eq!(
        worktree_text(&case, r"bin\.msCoverageSourceRootsMapping_App"),
        "the PC build's roots",
        "the PC's file is kept"
    );
    assert_eq!(worktree_text(&case, r"bin\new.txt"), "built", "the other file is installed");
    let line = regex::Regex::new(ONE_INSTALLED).expect("the line's pattern");
    assert!(line.is_match(&outcome.stderr), "only the moved file is counted: {outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(!pending_marker(&case).exists(), "the marker is cleared: {outcome:?}");
}

/// The PC's `bin/both.txt` and `bin/new.txt` are both edited while the run call is held: the refusal names each, one
/// line apiece in archive order, and neither is overwritten.
#[test]
fn every_pc_output_changed_during_the_run_is_named() {
    let case = outputs_case("client-outputs-install-changed-all", &BIN);
    write(&case, r"bin\both.txt", "pc");
    write(&case, r"bin\new.txt", "pc");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(go.path())), "ok"]);
    let run = start(client_gate(&case, "out.log", &steps, &BOTH_AND_NEW));
    run.wait_until("the run call", || calls(&steps).len() == 2);
    write(&case, r"bin\both.txt", "edited during the run");
    write(&case, r"bin\new.txt", "edited too");
    fs::write(go.path(), "").expect("the go file can be made");

    let outcome = run.finish();

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let refusals: Vec<&str> = outcome
        .stderr
        .lines()
        .filter(|line| line.starts_with("gate: remote outputs not brought back:"))
        .collect();
    assert_eq!(
        refusals,
        [
            "gate: remote outputs not brought back: bin/both.txt changed during the run",
            "gate: remote outputs not brought back: bin/new.txt changed during the run",
        ],
        "{outcome:?}"
    );
    assert_eq!(worktree_text(&case, r"bin\both.txt"), "edited during the run");
    assert_eq!(worktree_text(&case, r"bin\new.txt"), "edited too");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(pending_marker(&case).exists(), "the marker stays: {outcome:?}");
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
    run.wait_until("the run call", || calls(&steps).len() == 2);
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

/// A [`build_case`] whose routing file gives its repo no output globs, as the repo of the stale-output ticket has none:
/// a routed build brings nothing back, and the defaults beside its own empty list are what judge what it left.
fn no_globs_case(name: &str) -> Case {
    let case = build_case(name);
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: ["*tool*", "*dotnet*"] }, "outputs": { &repo: [] } });
    write_remote_json(&case, &routing.to_string());
    case
}

/// A [`build_case`] with `record` planted as its worktree's nothing-back record.
fn planted_case(name: &str, record: &str) -> Case {
    let case = build_case(name);
    plant_record(&case, record);
    case
}

/// Plants `record` as the case's worktree's nothing-back record.
fn plant_record(case: &Case, record: &str) {
    let key = work_folder_key(&git(case, &["rev-parse", "--show-toplevel"]));
    let path = case.root().join("remote").join(format!("{key}.nothing-back"));
    fs::create_dir_all(path.parent().expect("the record has a folder")).expect("the record's folder can be made");
    fs::write(path, record).expect("the record can be planted");
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
    run.wait_until("the run call", || calls(&steps).len() == 2);
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

/// The project a restore-here case's routed build names.
const RESTORED_PROJECT: &str = "app/App.csproj";
/// The start of the line a restore here that passed says.
const RESTORED_LINE: &str = "gate: restored app/App.csproj here in ";
/// The end of the line a restore here that passed says.
const RESTORED_END: &str = " s, so a --no-build run here can use the routed build's outputs";
/// The file whose presence makes the stand-in dotnet's restore fail ([`restore_case`]).
const RESTORE_FAILS: &str = "restore-fails";
/// The file whose presence makes the stand-in dotnet's restore wait about a minute first ([`restore_case`]).
const RESTORE_SLEEPS: &str = "restore-sleeps";
/// The file whose presence makes the stand-in dotnet's build write no output ([`restore_case`]).
const NO_OUTPUTS: &str = "no-outputs";
/// The start of the line of a restore here that could not run.
const CANNOT_RESTORE: &str = "gate: cannot run dotnet restore here: ";
/// The file the stand-in dotnet's restore writes its environment to ([`STAND_IN_DOTNET`]).
const RESTORE_ENV: &str = "restore.env";
/// A stand-in `dotnet.cmd`. On `build` or `publish` it writes `bin\new.txt` beside itself, unless [`NO_OUTPUTS`] is
/// there. On `restore` it writes its environment to [`RESTORE_ENV`], waits about a minute when [`RESTORE_SLEEPS`] is
/// there, exits 1 when [`RESTORE_FAILS`] is, and else writes the assets file of the project or folder its second word
/// names and exits 0. Any other verb exits 0; every call says its words first.
const STAND_IN_DOTNET: &str = "@echo off\r\necho %*\r\nif \"%1\"==\"build\" goto build\r\nif \"%1\"==\"publish\" goto build\r\n\
                               if not \"%1\"==\"restore\" exit /b 0\r\nset > \"%~dp0restore.env\"\r\n\
                               if exist \"%~dp0restore-sleeps\" ping -n 60 127.0.0.1 > nul\r\nif exist \"%~dp0restore-fails\" exit /b 1\r\n\
                               if exist \"%~f2\\*\" (set \"obj=%~f2\\obj\") else (set \"obj=%~dp2obj\")\r\n\
                               if not exist \"%obj%\" mkdir \"%obj%\"\r\necho {}> \"%obj%\\project.assets.json\"\r\nexit /b 0\r\n\
                               :build\r\nif exist \"%~dp0no-outputs\" exit /b 0\r\nif not exist \"%~dp0bin\" mkdir \"%~dp0bin\"\r\n\
                               echo built> \"%~dp0bin\\new.txt\"\r\nexit /b 0\r\n";

/// A [`build_case`] holding [`RESTORED_PROJECT`], with its `obj\project.assets.json` when `restored`, and the stand-in
/// dotnet [`STAND_IN_DOTNET`] as `dotnet.cmd`, which its snapshot sends, so the builder runs it too.
fn restore_case(name: &str, restored: bool) -> Case {
    let case = build_case(name);
    write(&case, RESTORED_PROJECT, "<Project />\n");
    if restored {
        write(&case, r"app\obj\project.assets.json", "{}\n");
    }
    write(&case, "dotnet.cmd", STAND_IN_DOTNET);
    case
}

/// The words of a routed `dotnet build` of `target` through the stand-in dotnet, run directly.
fn direct_build(target: &str) -> Vec<&str> {
    vec![r".\dotnet.cmd", "build", target]
}

/// Whether the run said a line of a restore here.
fn says_a_restore(outcome: &Outcome) -> bool {
    outcome
        .stderr
        .lines()
        .any(|line| line.starts_with("gate: restored ") || line.starts_with("gate: not restoring ") || line.contains("dotnet restore"))
}

/// The side file a restore here of a run logging to `out.log` writes its output to.
fn restore_log(case: &Case) -> PathBuf {
    case.folder().join("out.log.restore.log")
}

/// `path` as the gate's lines show it.
fn escaped_path(path: &Path) -> String {
    gate_router::escaped(&path.to_string_lossy())
}

#[test]
fn restore_here_after_a_routed_build_that_brought_everything_back_keeps_a_later_no_build() {
    let case = restore_case("client-restore-here", false);

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &direct_build(RESTORED_PROJECT));
    let here = finish(case.gate(&run_words("here.log", &[], &[r".\dotnet.cmd", "test", RESTORED_PROJECT, "--no-build"])));

    assert!(case.folder().join(r"bin\new.txt").is_file(), "{outcome:?}");
    let said = outcome.stderr.lines().find(|line| line.starts_with(RESTORED_LINE));
    assert!(said.is_some_and(|line| line.ends_with(RESTORED_END)), "{outcome:?}");
    assert!(restore_log(&case).is_file(), "{outcome:?}");
    assert!(case.folder().join(r"app\obj\project.assets.json").is_file(), "{outcome:?}");
    assert_eq!(here.code, Some(0), "{here:?}");
    assert!(!here.stderr.contains("gate: dropped --no-build"), "{here:?}");
}

#[test]
fn restore_here_that_fails_says_so_and_keeps_the_run_s_pass() {
    let case = restore_case("client-restore-here-fails", false);
    write(&case, RESTORE_FAILS, "");

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &direct_build(RESTORED_PROJECT));

    let failed = format!(
        "gate: dotnet restore of app/App.csproj here failed with exit 1 ({}); a --no-build run here builds first",
        escaped_path(&restore_log(&case))
    );
    assert!(outcome.stderr.lines().any(|line| line == failed), "{outcome:?}");
    assert!(restore_log(&case).is_file(), "{outcome:?}");
}

#[test]
fn no_restore_here_when_the_project_has_its_assets_file() {
    let case = restore_case("client-restore-here-present", true);

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &direct_build(RESTORED_PROJECT));

    assert!(!says_a_restore(&outcome), "{outcome:?}");
    assert!(!restore_log(&case).exists(), "{outcome:?}");
}

/// The PC holds no output and the build packs none, so it did not bring everything back.
#[test]
fn no_restore_here_after_a_routed_build_that_did_not_bring_everything_back() {
    let case = restore_case("client-restore-here-not-back", false);
    write(&case, NO_OUTPUTS, "");

    let outcome = passing_build(&case, &["ok", "ok"], &direct_build(RESTORED_PROJECT));

    assert!(outcome.stderr.contains("gate-remote-outputs: 0"), "{outcome:?}");
    assert!(!says_a_restore(&outcome), "{outcome:?}");
    assert!(!restore_log(&case).exists(), "{outcome:?}");
}

/// The build names the folder `tests/X`, which has its assets file; another project of the worktree lacks its own.
#[test]
fn no_restore_here_for_a_project_the_build_does_not_name() {
    let case = restore_case("client-restore-here-named", false);
    write(&case, r"tools\Gen\Gen.csproj", "<Project />\n");
    write(&case, r"tests\X\X.csproj", "<Project />\n");
    write(&case, r"tests\X\obj\project.assets.json", "{}\n");

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &direct_build("tests/X"));

    assert!(!says_a_restore(&outcome), "{outcome:?}");
    assert!(!restore_log(&case).exists(), "{outcome:?}");
}

#[test]
fn a_wrapped_dotnet_build_gets_no_restore_here() {
    let case = restore_case("client-restore-here-wrapped", false);
    let wrapped = [
        r".\pwsh.exe",
        "write",
        r"bin\new.txt",
        "built",
        "argv",
        "--",
        r".\dotnet.cmd",
        "build",
        RESTORED_PROJECT,
    ];

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &wrapped);

    assert!(!says_a_restore(&outcome), "{outcome:?}");
    assert!(!restore_log(&case).exists(), "{outcome:?}");
}

#[test]
fn restore_here_past_its_limit_is_stopped_and_keeps_the_run_s_pass() {
    let case = restore_case("client-restore-here-stopped", false);
    write(&case, RESTORE_SLEEPS, "");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-restore-limit-ms", "500"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &direct_build(RESTORED_PROJECT)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let stopped = format!(
        "gate: dotnet restore of app/App.csproj here ran past 500 ms and was stopped ({}); a --no-build run here builds first",
        escaped_path(&restore_log(&case))
    );
    assert!(outcome.stderr.lines().any(|line| line == stopped), "{outcome:?}");
}

#[test]
fn restore_here_whose_side_file_name_is_a_folder_cannot_run() {
    let case = restore_case("client-restore-here-side-folder", false);
    fs::create_dir_all(restore_log(&case)).expect("the folder can be made");

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &direct_build(RESTORED_PROJECT));

    let line = outcome.stderr.lines().find(|line| line.starts_with(CANNOT_RESTORE));
    let earlier = format!("cannot remove the earlier {}", escaped_path(&restore_log(&case)));
    assert!(line.is_some_and(|line| line.contains(&earlier)), "{outcome:?}");
    assert!(restore_log(&case).is_dir(), "{outcome:?}");
}

#[test]
#[expect(clippy::print_stderr, reason = "a case this machine cannot run says why it skipped")]
fn restore_here_replaces_a_link_at_its_side_file_name_and_leaves_its_target() {
    let case = restore_case("client-restore-here-side-link", false);
    let other = case.folder().join("other.txt");
    fs::write(&other, "keep").expect("the other file can be written");
    if let Err(error) = std::os::windows::fs::symlink_file(&other, restore_log(&case)) {
        eprintln!("skipped: this process cannot make a symbolic link: {error}");
        return;
    }

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &direct_build(RESTORED_PROJECT));

    assert!(outcome.stderr.lines().any(|line| line.starts_with(RESTORED_LINE)), "{outcome:?}");
    assert_eq!(fs::read_to_string(&other).expect("the other file is there"), "keep", "{outcome:?}");
    let side = fs::symlink_metadata(restore_log(&case)).expect("the side file is there");
    assert!(side.is_file() && !side.file_type().is_symlink(), "{outcome:?}");
}

/// A publish may put its outputs elsewhere, so the assets check after a restore could not find what it restored.
#[test]
fn a_routed_publish_restores_nothing_here() {
    let case = restore_case("client-restore-here-publish", false);

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &[r".\dotnet.cmd", "publish", RESTORED_PROJECT]);

    assert!(case.folder().join(r"bin\new.txt").is_file(), "{outcome:?}");
    assert!(!says_a_restore(&outcome), "{outcome:?}");
    assert!(!restore_log(&case).exists(), "{outcome:?}");
}

/// A build with an artifacts path writes its assets outside the project's `obj` folder.
#[test]
fn a_routed_build_with_an_artifacts_path_restores_nothing_here() {
    let case = restore_case("client-restore-here-artifacts", false);

    let outcome = passing_build(
        &case,
        &["ok", "ok", "ok"],
        &[r".\dotnet.cmd", "build", RESTORED_PROJECT, "--artifacts-path", "out"],
    );

    assert!(case.folder().join(r"bin\new.txt").is_file(), "{outcome:?}");
    assert!(!says_a_restore(&outcome), "{outcome:?}");
    assert!(!restore_log(&case).exists(), "{outcome:?}");
}

#[test]
fn a_routed_build_of_a_folder_says_it_is_not_restored_here() {
    let case = restore_case("client-restore-here-folder", false);
    write(&case, r"tests\X\X.csproj", "<Project />\n");

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &direct_build("tests/X"));

    let line = "gate: not restoring tests/X here: it is not a project or solution file; a --no-build run here builds first";
    assert!(outcome.stderr.lines().any(|said| said == line), "{outcome:?}");
    assert!(!restore_log(&case).exists(), "{outcome:?}");
    assert!(!case.folder().join(r"tests\X\obj\project.assets.json").exists(), "{outcome:?}");
}

/// Five routed builds make the key's history, seeded to a load-adjusted 100 s, so `auto:10` resolves to a 300 s
/// ceiling on the sixth, a warm run. Counted from the run's arrival against the floor, under 10 s would be left and
/// the restore skipped; counted from the routed call's start against the ceiling, nearly 300 s are.
#[test]
fn a_restore_after_an_auto_ceiling_raised_by_history_gets_the_time_left() {
    let case = restore_case("client-restore-here-auto", false);
    let steps = ssh_steps(&case, "ssh", &["ok"; 30]);
    for run in 0..5 {
        let outcome = finish(client_gate(&case, &format!("history-{run}.log"), &steps, &direct_build(RESTORED_PROJECT)));
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
    }
    crate::durations::seed(&case, "App.csproj", 1.0, Some(100.0));
    fs::remove_file(case.folder().join(r"app\obj\project.assets.json")).expect("the first run's restore wrote the assets file");
    let tuned = Tuned {
        timeout: "auto:10",
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &direct_build(RESTORED_PROJECT)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains(", ceiling 300 s; command "), "{outcome:?}");
    assert!(outcome.stderr.lines().any(|line| line.starts_with(RESTORED_LINE)), "{outcome:?}");
}

/// A routed build in a commit run's hooks, under the commit's marker, which this test process stands in for: the commit
/// still holds its slot while the restore runs, so the restore is handed the commit's marker.
#[test]
fn a_restore_under_a_commit_run_keeps_the_commit_s_marker() {
    let case = restore_case("client-restore-here-commit", false);
    let marker = crate::nesting::test_marker(&case, "heavy");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-parent-job-trusted"],
        ..Tuned::default()
    };
    let mut gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &direct_build(RESTORED_PROJECT));
    gate.env("GATE_SLOT_HELD", &marker).env("GATE_COMMIT_SLOT", &marker);

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.lines().any(|line| line.starts_with(RESTORED_LINE)), "{outcome:?}");
    let environment = fs::read_to_string(case.folder().join(RESTORE_ENV)).expect("the restore wrote its environment");
    let handed = format!("GATE_SLOT_HELD={marker}");
    assert!(environment.lines().any(|line| line == handed), "{outcome:?}\n{environment}");
}

#[test]
fn a_restore_here_gets_no_slot_marker() {
    let case = restore_case("client-restore-here-marker", false);

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &direct_build(RESTORED_PROJECT));

    assert!(outcome.stderr.lines().any(|line| line.starts_with(RESTORED_LINE)), "{outcome:?}");
    let environment = fs::read_to_string(case.folder().join(RESTORE_ENV)).expect("the restore wrote its environment");
    assert!(
        !environment.lines().any(|line| line.to_ascii_uppercase().starts_with("GATE_SLOT_HELD=")),
        "{environment}"
    );
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

/// A dotnet test whose `--no-build` the builder dropped, in a repo with output and failure output globs: the run still
/// carries its failure output globs, and its passing end leaves the withheld run's marker as one without them does.
#[test]
fn a_passing_no_build_run_with_failure_outputs_keeps_its_withheld_marker() {
    let case = withheld_failure_case("client-failure-outputs-withheld");
    let capped = planted_state(&case, "outputs-capped", r#"{"v":1,"hits":1}"#);
    // The run call keeps its request: it waits on the case folder, which is there already.
    let fed = format!("fed-hold {}", shown(&case.folder()));
    let steps = ssh_steps(&case, "ssh", &["ok", &fed, "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &wrapped_dotnet(&WRITE_NEW, &NO_BUILD_RELEASE_TEST)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: dropped --no-build"), "{outcome:?}");
    assert_eq!(
        last_request(&steps)["FailureOutputs"],
        json!(["dumps/*.dmp"]),
        "the run call carries its failure output globs: {outcome:?}"
    );
    assert!(!case.folder().join("bin").join("new.txt").exists(), "nothing comes back: {outcome:?}");
    assert!(pending_marker(&case).exists(), "the next run asks for every output: {outcome:?}");
    assert_eq!(
        fs::read_to_string(&capped).ok().as_deref(),
        Some(r#"{"v":1,"hits":1}"#),
        "the outputs cap record is left as it was: {outcome:?}"
    );
}

/// The hash cache holds the outputs' hashes; a withheld run's manifest lists only the files under its failure output
/// globs, so saving its cache would replace every output's.
#[test]
fn a_withheld_run_with_failure_outputs_leaves_the_hash_cache_as_it_was() {
    let case = withheld_failure_case("client-failure-outputs-withheld-cache");
    write(&case, r"dumps\old.dmp", "an earlier dump");
    let planted = "a cache an earlier run saved\n";
    let hashes = planted_state(&case, "outputs-hashes", planted);

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &wrapped_dotnet(&WRITE_NEW, &NO_BUILD_RELEASE_TEST));

    assert!(outcome.stderr.contains("gate: dropped --no-build"), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(&hashes).ok().as_deref(),
        Some(planted),
        "the cache is left as it was: {outcome:?}"
    );
}

/// A [`failure_case`] with the output globs [`BIN`] and the failure output glob `dumps/*.dmp`, holding fake-tool as
/// `pwsh.exe` too, and its `.gitignore` committed so a routed build has a `HEAD` to pin.
fn withheld_failure_case(name: &str) -> Case {
    let case = failure_case(name, &BIN, &["dumps/*.dmp"]);
    copy_tool(&case, "pwsh.exe");
    git(&case, &["add", ".gitignore"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    case
}

/// The builder builds for a dotnet test whose `--no-build` it dropped, then the command writes a crash dump and fails:
/// the withheld run still brings back the files under its failure output globs.
#[test]
fn a_failing_withheld_no_build_run_brings_its_failure_outputs_back() {
    let case = withheld_failure_case("client-failure-outputs-withheld-failing");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok"]);
    let dump_then_fail = ["write", r"dumps\a.dmp", "dump", "exit", "3"];

    let outcome = finish(client_gate(
        &case,
        "out.log",
        &steps,
        &wrapped_dotnet(&dump_then_fail, &NO_BUILD_RELEASE_TEST),
    ));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(outcome.stderr.contains("gate: dropped --no-build"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"], "{outcome:?}");
    assert_eq!(worktree_text(&case, r"dumps\a.dmp"), "dump", "the dump is installed: {outcome:?}");
    assert!(says_a_line_starting(&outcome, "gate: the failed run's outputs: 1 file(s)"), "{outcome:?}");
    assert!(pending_marker(&case).exists(), "the withheld run's marker stays: {outcome:?}");
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
    // The record names the builder the next run goes to, so the marker alone makes it full: a worktree with no record
    // sends full on its first run whether a marker is there or not.
    plant_outputs_from(&case, HOST);
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

/// A routed dotnet build of a repo with no output globs brings nothing back, so the PC's own stale `bin` folder is
/// named; the nothing-back record is written as it was before the line.
#[test]
fn a_passing_routed_build_with_no_output_globs_names_the_folders_it_left_stale() {
    let case = no_globs_case("client-stale-no-globs");
    write(&case, r"bin\app.txt", "stale");

    let outcome = passing_build(&case, &["ok", "ok"], &wrapped_build(&[], &RELEASE));

    assert!(
        outcome.stderr.lines().any(|line| line == "gate: routed build left stale: bin"),
        "{outcome:?}"
    );
    assert_eq!(
        nothing_back(&case).as_deref(),
        Some(RELEASE_RECORD),
        "the record is written as before: {outcome:?}"
    );
}

/// Everything the run built came back under its one glob, so the only judging glob left is the Godot default, which no
/// file of this PC matches.
#[test]
fn a_routed_build_that_brought_everything_back_says_nothing_stale() {
    let case = build_case("client-stale-all-back");
    write(&case, r"bin\same.txt", "same");

    let outcome = passing_build(&case, &["ok", "ok", "ok"], &wrapped_build(&WRITE_NEW, &RELEASE));

    assert_eq!(worktree_text(&case, r"bin\new.txt"), "built", "the output came back: {outcome:?}");
    assert!(!outcome.stderr.contains("gate: routed build left stale"), "{outcome:?}");
}

/// A routed build that failed says nothing of what it left stale: its failure is the news.
#[test]
fn a_failed_routed_build_prints_no_stale_line() {
    let case = no_globs_case("client-stale-failed");
    write(&case, r"bin\app.txt", "stale");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &wrapped_build(&["exit", "1"], &RELEASE)));

    assert_eq!(outcome.code, Some(1), "the build failed: {outcome:?}");
    assert!(!outcome.stderr.contains("gate: routed build left stale"), "{outcome:?}");
}

/// The command exited 0 but wrote a failure marker in its log, so the gate's verdict is FAILED: the stale line is said
/// of a run whose verdict passed, and this one's did not.
#[test]
fn a_routed_build_whose_log_reports_a_failure_prints_no_stale_line() {
    let case = no_globs_case("client-stale-marker");
    write(&case, r"bin\app.txt", "stale");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(client_gate(
        &case,
        "out.log",
        &steps,
        &wrapped_build(&["err", "error CS0001: boom"], &RELEASE),
    ));

    assert_eq!(outcome.code, Some(1), "the marker fails the run: {outcome:?}");
    assert!(
        outcome.stderr.contains("gate: command exited 0 but its log reports a failure"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("gate: routed build left stale"), "{outcome:?}");
}

/// A routed command that is no build writes no output a glob of a build's shape names, so nothing is called stale.
#[test]
fn a_routed_command_that_is_not_a_build_prints_no_stale_line() {
    let case = no_globs_case("client-stale-not-a-build");
    write(&case, r"bin\app.txt", "stale");

    let outcome = passing_build(&case, &["ok", "ok"], &[r".\tool.exe", "out", "ran there"]);

    assert!(!outcome.stderr.contains("gate: routed build left stale"), "{outcome:?}");
}

/// A build the builder turned away ran here and passed, so the run's own status would let the line print: nothing on the
/// builder is stale of it, and the fallback's own path returns before the line is even computed.
#[test]
fn a_routed_build_that_fell_back_prints_no_stale_line() {
    let case = no_globs_case("client-stale-fallback");
    write(&case, r"bin\app.txt", "stale");
    let steps = ssh_steps(&case, "ssh", &["fail-other"]);

    let (pid, outcome) = finish_with_pid(client_gate(&case, "out.log", &steps, &wrapped_build(&[], &RELEASE)));

    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    assert_eq!(fallbacks(&own(&case, pid)), ["unreachable"], "the build ran here: {outcome:?}");
    assert!(!outcome.stderr.contains("gate: routed build left stale"), "{outcome:?}");
}

/// A sibling's output is never brought back, so a routed pair build names the sibling's stale folders on the same line
/// as the main's, after them, each relative to the main worktree and judged by the same globs.
#[test]
fn a_passing_pair_build_names_a_sibling_s_stale_output_after_the_main_s() {
    let (case, leaf, name, sibling) = pair_case("client-stale-pair", &[], |case, sibling| {
        sibling_repository(case, sibling);
        fs::write(sibling.join(".gitignore"), "bin/\n").expect("the sibling's ignore file can be written");
        let folder = sibling.join("src").join("Lib").join("bin").join("Debug");
        fs::create_dir_all(&folder).expect("the sibling's output folder can be made");
        fs::write(folder.join("lib.dll"), "stale").expect("the sibling's output can be written");
    });
    copy_tool(&case, "pwsh.exe");
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\n"));
    git(&case, &["add", ".gitignore"]);
    git(&case, &["commit", "--quiet", "-m", "start"]);
    let repo = repo_name(&leaf);
    let routing = json!({
        "host": HOST,
        "slots": 2,
        "repos": { &repo: ["*tool*", "*dotnet*"] },
        "outputs": { &repo: [] },
        "siblings": { &repo: [&name] },
    });
    write_remote_json(&case, &routing.to_string());
    write(&case, r"bin\app.txt", "stale");

    let outcome = finish(client_gate(
        &case,
        "out.log",
        &ssh_steps(&case, "ssh", &["ok", "ok"]),
        &wrapped_build(&[], &RELEASE),
    ));
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = format!("gate: routed build left stale: bin, ../{name}/src/Lib/bin");
    let said: Vec<&str> = outcome.stderr.lines().filter(|said| said.starts_with("gate: routed build")).collect();
    assert_eq!(said, [line.as_str()], "{outcome:?}");
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
    let root = std::path::absolute(git_out(&case, &[], &["rev-parse", "--show-toplevel"])).expect("git names the worktree by a full path");
    let expected = format!(r"Root_{}\src\App", root.display());
    assert_eq!(worktree_text(&case, r"bin\MvcTestingAppManifest.json"), expected, "{outcome:?}");
}

/// A gate whose folder is spelled through a junction to the case folder, as it is through a `subst` drive, still names
/// the worktree by git's real spelling in the manifest it installs.
#[test]
fn an_installed_test_host_manifest_names_the_worktree_from_a_junction() {
    let case = outputs_case("client-outputs-install-rewrite-junction", &BIN);
    let first = ssh_steps(&case, "first", &["ok", "ok"]);
    let cleared = finish(client_gate(&case, "first.log", &first, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(cleared.code, Some(0), "{cleared:?}");
    let builder = std::path::absolute(case.folder().join("builder")).expect("the builder root has a full path");
    let work = format!(r"{}\work\{}", builder.display(), key_of(&events(&case)));
    let recorded = format!(r"Root_{}\src\App", work.to_uppercase());
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let command = [r".\tool.exe", "write", r"bin\MvcTestingAppManifest.json", &recorded];
    let junction = Junction::to(&case, &case.folder());
    let mut gate = client_gate(&case, "second.log", &second, &command);
    gate.current_dir(&junction.0);

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let root = std::path::absolute(git_out(&case, &[], &["rev-parse", "--show-toplevel"])).expect("git names the worktree by a full path");
    let expected = format!(r"Root_{}\src\App", root.display());
    assert_eq!(worktree_text(&case, r"bin\MvcTestingAppManifest.json"), expected, "{outcome:?}");
}

/// The coverage collector writes its mapping in UTF-16, unlike the test host's manifest: the install reads it in that
/// encoding, rewrites the builder's work folder to the worktree, and writes it back as UTF-16 with its byte order mark.
#[test]
fn an_installed_utf16_coverage_mapping_names_the_worktree() {
    let case = outputs_case("client-outputs-install-utf16", &BIN);
    first_run(&case);
    let builder = std::path::absolute(case.folder().join("builder")).expect("the builder root has a full path");
    let work = format!(r"{}\work\{}", builder.display(), key_of(&events(&case)));
    // After a `_`, the path is not one the router rewrites into the work folder: the builder sits in the worktree here.
    let recorded = utf16_le(&format!(r"Root_{}\src\App", work.to_uppercase()));
    install_bytes_from_the_box(&case, "bin/.msCoverageSourceRootsMapping_App", &recorded);
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let root = std::path::absolute(git_out(&case, &[], &["rev-parse", "--show-toplevel"])).expect("git names the worktree by a full path");
    let expected = utf16_le(&format!(r"Root_{}\src\App", root.display()));
    let installed = fs::read(case.folder().join("bin").join(".msCoverageSourceRootsMapping_App")).expect("the file is installed");
    assert_eq!(installed, expected, "UTF-16 LE, its BOM kept, the worktree named: {outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(!pending_marker(&case).exists(), "the install completed: {outcome:?}");
}

/// A fetched `.msCoverageSourceRootsMapping_*` whose bytes are not text the install can read is skipped, not installed:
/// the pass stands, the file beside it is installed, the skip is said, and the marker is cleared, since a refetch of the
/// same bytes would be skipped the same way.
#[test]
fn a_coverage_file_that_cannot_be_rewritten_is_skipped_and_the_marker_cleared() {
    let case = outputs_case("client-outputs-install-not-text", &BIN);
    first_run(&case);
    install_bytes_from_the_box(&case, "bin/.msCoverageSourceRootsMapping_App", b"\xEF\xBB\xBF\xFF\xFE");
    install_from_the_box(&case, "bin/x.dll", "built");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);

    let outcome = finish(aged_snapshot_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "the pass stands: {outcome:?}");
    assert!(
        outcome.stderr.contains("not fully brought back: bin/.msCoverageSourceRootsMapping_App"),
        "{outcome:?}"
    );
    assert!(
        outcome
            .stderr
            .contains("cannot be rewritten (it is not UTF-8 or UTF-16 with a byte order mark)"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("not brought back:"), "{outcome:?}");
    assert!(
        !case.folder().join("bin").join(".msCoverageSourceRootsMapping_App").exists(),
        "the unrewritable file is not installed: {outcome:?}"
    );
    assert_eq!(worktree_text(&case, r"bin\x.dll"), "built", "the other file is installed: {outcome:?}");
    let line = regex::Regex::new(ONE_INSTALLED).expect("the line's pattern");
    assert!(line.is_match(&outcome.stderr), "only the installed file is counted: {outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
    assert!(
        !pending_marker(&case).exists(),
        "a skip a refetch would repeat clears the marker: {outcome:?}"
    );
    // The install counts as whole, so it caches what it moved: the next run reads no file it put in place. The
    // nothing-back record is not observable here, since only a routed dotnet build writes one.
    assert!(
        hashes(&case)["files"].get("bin/x.dll").is_some(),
        "a skip a refetch would repeat still caches what it installed: {outcome:?}"
    );
}

/// A first run of `case`, which names the work-folder key, so a builder file can be put in its work folder after it.
fn first_run(case: &Case) {
    let steps = ssh_steps(case, "first", &["ok", "ok"]);
    let cleared = finish(client_gate(case, "first.log", &steps, &[r".\tool.exe", "out", "ran there"]));
    assert_eq!(cleared.code, Some(0), "{cleared:?}");
}

/// Writes `text` to the builder's work folder file `path`, its folders made. The install stamps each file it moves with
/// the run's snapshot time, not the box's last write, so a case that reads the hash the install keeps runs with
/// [`SNAPSHOT_AN_HOUR_BACK`]: the install keeps no hash of a file stamped inside the 2 s window of its moves.
fn install_from_the_box(case: &Case, path: &str, text: &str) {
    install_bytes_from_the_box(case, path, text.as_bytes());
}

/// Writes `bytes` to the builder's work folder file `path`, its folders made, as [`install_from_the_box`] does for text.
fn install_bytes_from_the_box(case: &Case, path: &str, bytes: &[u8]) {
    let work = case.folder().join("builder").join("work").join(key_of(&events(case)));
    let full = work.join(path.replace('/', "\\"));
    fs::create_dir_all(full.parent().expect("a file has a folder")).expect("the folder can be made");
    fs::write(&full, bytes).expect("the builder's file can be written");
}

/// The seam that puts each attempt's snapshot time an hour before the snapshot is taken. Every file the install moves
/// is stamped with that time, and the moves begin after the snapshot, so the stamp is at least an hour before them
/// however long the run takes, and the install keeps the file's hash.
const SNAPSHOT_AN_HOUR_BACK: [&str; 2] = ["--test-remote-snapshot-age-ms", "3600000"];

/// [`client_gate`] with [`SNAPSHOT_AN_HOUR_BACK`].
fn aged_snapshot_gate(case: &Case, log: &str, steps: &Path, command: &[&str]) -> Command {
    let tuned = Tuned {
        extra: &SNAPSHOT_AN_HOUR_BACK,
        ..Tuned::default()
    };
    client_gate_tuned(case, log, steps, &tuned, command)
}

/// `text` as UTF-16 little-endian bytes, the byte order mark first, as the .NET coverage collector writes it.
fn utf16_le(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// A run whose snapshot time the seam puts an hour back installs a file and keeps its hash at the stamp the installed
/// file's own handle reads: the next run then takes that hash instead of reading the file.
#[test]
fn an_installed_file_is_cached_at_its_installed_stamp() {
    let case = outputs_case("client-outputs-cached", &BIN);
    first_run(&case);
    install_from_the_box(&case, "bin/x.dll", "built");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);

    let outcome = finish(aged_snapshot_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

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

/// The builder's outputs hold one content twice and another the PC already holds under another path: the archive packs
/// the first once and the second not at all, and the install makes every path, the copies from the staged member and
/// from the PC's own file.
#[test]
fn a_routed_run_installs_a_deduplicated_archive_with_every_path() {
    let case = outputs_case("client-outputs-deduplicated", &BIN);
    first_run(&case);
    let held = case.folder().join("bin").join("held.txt");
    fs::create_dir_all(held.parent().expect("a file has a folder")).expect("the PC's folder can be made");
    fs::write(&held, "kept").expect("the PC's file can be written");
    install_from_the_box(&case, "bin/a.txt", "same");
    install_from_the_box(&case, "bin/b.txt", "same");
    install_from_the_box(&case, "bin/new.txt", "kept");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);

    let outcome = finish(client_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate-remote-outputs: 3 "), "every path is listed: {outcome:?}");
    for (path, text) in [("a.txt", "same"), ("b.txt", "same"), ("new.txt", "kept"), ("held.txt", "kept")] {
        let installed = fs::read_to_string(case.folder().join("bin").join(path)).expect("every path is installed");
        assert_eq!(installed, text, "{path}: {outcome:?}");
    }
    let line = regex::Regex::new(r"(?m)^gate: outputs: 3 file\(s\), ").expect("the line's pattern");
    assert!(line.is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
}

/// The run after an install takes the hash the cache holds at the installed file's stamp, not the file's: a hash
/// planted in the cache changes what the box brings back, where reading the file again would bring nothing.
#[test]
fn a_run_after_an_install_does_not_rehash_what_it_installed() {
    let case = outputs_case("client-outputs-installed-hash", &BIN);
    first_run(&case);
    install_from_the_box(&case, "bin/x.dll", "built");
    let second = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let installed = finish(aged_snapshot_gate(&case, "second.log", &second, &[r".\tool.exe", "out", "ran there"]));
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

/// An install the Ctrl+C stops keeps no hash, not even of the settled file it moved before the interrupt: the seam puts
/// the snapshot time the install stamps it with an hour back, so the interrupt is the only reason it is not kept.
#[test]
fn an_interrupted_install_caches_nothing() {
    let case = outputs_case("client-outputs-install-ctrl-c-cache", &BIN);
    first_run(&case);
    install_from_the_box(&case, "bin/a.txt", "a");
    install_from_the_box(&case, "bin/b.txt", "b");
    let steps = ssh_steps(&case, "second", &["ok", "ok", "ok"]);
    let extra = [&SNAPSHOT_AN_HOUR_BACK[..], &["--test-remote-interrupt-at", "install"]].concat();
    let tuned = Tuned {
        extra: &extra,
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

/// A routed run of `command` in a case whose builder's `GATE_GODOT_ROOT` is `root`, with no `GODOT_PATH` of its own: it
/// goes remote with no parity line and runs once, on the builder, and passes.
fn a_routed_godot_run(case: &Case, root: &Path, command: &[&str]) -> Outcome {
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: ["*tool*", "Godot_console*"] }, "outputs": { &repo: [] } });
    write_remote_json(case, &routing.to_string());
    let box_path = box_path_with_tools(case, &listing(&[BUILDER_SDK]), &[], &[]);
    let steps = ssh_steps(case, "ssh", &["ok", "ok"]);
    let mut words = command.to_vec();
    words.extend(["mark", RAN]);
    let mut gate = dotnet_gate(case, "out.log", &steps, &words);
    gate.env("PATH", box_path).env_remove("GODOT_PATH");
    set_godot_root(&mut gate, Some(root));

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.lines().any(is_parity_line), "{outcome:?}");
    assert_eq!(fallbacks(&own(case, pid)), Vec::<&str>::new(), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert_eq!(runs(case), (0, 1), "the command ran once, on the builder: {outcome:?}");
    outcome
}

/// The Godot command a routed case runs: the install's answer, then the `GODOT_PATH` the command sees.
const GODOT_COMMAND: [&str; 4] = ["Godot_console.exe", "--version", "env", "GODOT_PATH"];

/// Whether the routed run's output holds the answer of the install `version` of `root` and a `GODOT_PATH` naming it.
fn ran_the_install(outcome: &Outcome, root: &Path, version: &str) -> bool {
    let install = root.join(version).join("Godot_console.exe").display().to_string();
    let answer = format!("{version}.stable.mono.official.x");
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    lines.contains(&answer.as_str()) && lines.contains(&install.as_str())
}

/// A routed Godot run of a repo pinning a version the builder has sends it, and the builder runs that install.
#[test]
fn a_routed_godot_run_sends_the_installed_version_its_repo_pins() {
    let case = client_case("client-godot-version-sent");
    write(&case, "src/App/Game.csproj", &godot_csproj("4.7.2"));
    let root = godot_root(
        &case,
        &[("4.3.0", "4.3.0.stable.mono.official.x\n"), ("4.7.2", "4.7.2.stable.mono.official.x\n")],
    );

    let outcome = a_routed_godot_run(&case, &root, &GODOT_COMMAND);

    assert!(ran_the_install(&outcome, &root, "4.7.2"), "{outcome:?}");
}

/// A partial pin picks the highest install of the builder that meets it.
#[test]
fn a_routed_godot_run_of_a_partial_pin_picks_the_highest_matching_install() {
    let case = client_case("client-godot-partial-pin");
    write(&case, MANIFEST, "[tools]\ngodot = \"4.7\"\n");
    let root = godot_root(
        &case,
        &[
            ("4.7.1", "4.7.1.stable.mono.official.x\n"),
            ("4.7.2", "4.7.2.stable.mono.official.x\n"),
            ("4.8.0", "4.8.0.stable.mono.official.x\n"),
        ],
    );

    let outcome = a_routed_godot_run(&case, &root, &GODOT_COMMAND);

    assert!(ran_the_install(&outcome, &root, "4.7.2"), "{outcome:?}");
}

/// A case whose project files pin the differing Godot versions 4.7.2 and 4.3.0, and the root of a builder that has both.
fn several_pins_case(name: &str) -> (Case, PathBuf) {
    let case = client_case(name);
    write(&case, "src/A/Game.csproj", &godot_csproj("4.7.2"));
    write(&case, "src/B/Tool.csproj", &godot_csproj("4.3.0"));
    let root = godot_root(
        &case,
        &[("4.3.0", "4.3.0.stable.mono.official.x\n"), ("4.7.2", "4.7.2.stable.mono.official.x\n")],
    );
    (case, root)
}

/// A Godot command of a repo whose project files pin several differing Godot versions runs here, saying why before the
/// parity line, on the console and in the log, though the builder has both.
#[test]
fn a_godot_run_of_a_repo_pinning_several_godot_versions_falls_back_saying_why() {
    let (case, root) = several_pins_case("client-godot-several-pins");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: ["Godot_console*"] }, "outputs": { &repo: [] } });
    write_remote_json(&case, &routing.to_string());
    let box_path = box_path_with_tools(&case, &listing(&[BUILDER_SDK]), &[], &[]);
    let mut local_path = OsString::from(root.join("4.7.2"));
    local_path.push(";");
    local_path.push(&box_path);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let mut gate = dotnet_gate(&case, "out.log", &steps, &["Godot_console.exe", "--version", "mark", RAN]);
    gate.env("PATH", &local_path).env_remove("GODOT_PATH");
    set_godot_root(&mut gate, Some(&root));

    let (pid, outcome) = finish_with_pid(gate);

    let why = "gate: the repo pins several Godot versions (4.7.2, 4.3.0); a routed run needs one";
    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    assert_eq!(fallbacks(&own(&case, pid)), ["parity"], "{outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().filter(|line| *line == why || is_parity_line(line)).collect();
    assert_eq!(said, [why, PARITY_LINE], "{outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.lines().any(|line| line == why), "{log}");
    assert_eq!(calls(&steps), ["preflight"], "the preflight alone reached the builder");
    assert_eq!(runs(&case), (1, 0), "the command ran once, here: {outcome:?}");
}

/// A Godot command of a case runs here, saying `why` before the parity line, on the console and in the log: the
/// command it then runs here is the fake Godot console of the install folder `local`, and `root` is the builder's
/// `GATE_GODOT_ROOT`.
fn a_godot_run_refused_saying_why(case: &Case, root: &Path, local: &Path, why: &str) -> Outcome {
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: ["Godot_console*"] }, "outputs": { &repo: [] } });
    write_remote_json(case, &routing.to_string());
    let box_path = box_path_with_tools(case, &listing(&[BUILDER_SDK]), &[], &[]);
    let mut local_path = OsString::from(local);
    local_path.push(";");
    local_path.push(&box_path);
    let steps = ssh_steps(case, "ssh", &["ok", "ok"]);
    let mut gate = dotnet_gate(case, "out.log", &steps, &["Godot_console.exe", "--version", "mark", RAN]);
    gate.env("PATH", &local_path).env_remove("GODOT_PATH");
    set_godot_root(&mut gate, Some(root));

    let (pid, outcome) = finish_with_pid(gate);

    assert_eq!(outcome.code, Some(0), "the run here passed: {outcome:?}");
    assert_eq!(fallbacks(&own(case, pid)), ["parity"], "{outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().filter(|line| *line == why || is_parity_line(line)).collect();
    assert_eq!(said, [why, PARITY_LINE], "{outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.lines().any(|line| line == why), "{log}");
    assert_eq!(calls(&steps), ["preflight"], "the preflight alone reached the builder");
    assert_eq!(runs(case), (1, 0), "the command ran once, here: {outcome:?}");
    outcome
}

/// The warning a Godot command of a repo pinning no Godot version says, naming the install `version` the run uses.
pub(super) fn no_pin_warning(version: &str) -> String {
    format!(
        "gate: warning: the repo pins no Godot version, so this run uses {version}, the newest on {HOST}; pin the \
         version it needs in .gate/toolchain.toml ([tools] godot = \"x.y.z\") or as the .csproj's Godot.NET.Sdk version"
    )
}

/// A Godot command of a repo pinning no Godot version runs on the builder's newest install, warning that it chose it.
#[test]
fn a_godot_run_of_a_repo_pinning_no_godot_version_runs_on_the_newest_and_warns() {
    let case = client_case("client-godot-no-pin-newest");
    write(&case, "src/App/Game.csproj", "<Project />\n");
    let root = godot_root(
        &case,
        &[("4.3.0", "4.3.0.stable.mono.official.x\n"), ("4.7.2", "4.7.2.stable.mono.official.x\n")],
    );

    let outcome = a_routed_godot_run(&case, &root, &GODOT_COMMAND);

    let warning = no_pin_warning("4.7.2");
    assert!(ran_the_install(&outcome, &root, "4.7.2"), "{outcome:?}");
    assert_eq!(outcome.stderr.lines().filter(|line| *line == warning).count(), 1, "{outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    let warning_at = line_at(&log, &warning).expect("the warning is logged");
    let output_at = line_at(&log, "4.7.2.stable.mono.official.x").expect("the command's own output is logged");
    assert!(warning_at < output_at, "the warning comes before the command's own output: {log}");
}

/// A Godot command of a repo taking any Godot version runs on the builder's newest install, with no warning.
#[test]
fn a_godot_run_of_a_repo_taking_any_godot_version_runs_on_the_newest() {
    let case = client_case("client-godot-any-newest");
    write(&case, MANIFEST, "[tools]\ngodot = \"\"\n");
    let root = godot_root(
        &case,
        &[("4.3.0", "4.3.0.stable.mono.official.x\n"), ("4.7.2", "4.7.2.stable.mono.official.x\n")],
    );

    let outcome = a_routed_godot_run(&case, &root, &GODOT_COMMAND);

    assert!(ran_the_install(&outcome, &root, "4.7.2"), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: warning:"), "{outcome:?}");
}

/// A Godot command of a repo pinning no Godot version stays here, refused `parity` with its own line, when the builder
/// lists no Godot install.
#[test]
fn a_godot_run_of_an_unpinned_repo_falls_back_when_the_builder_lists_no_godot() {
    let case = client_case("client-godot-no-pin-no-install");
    write(&case, "src/App/Game.csproj", "<Project />\n");
    let local = godot_root(&case, &[("4.7.2", "4.7.2.stable.mono.official.x\n")]);
    let root = case.folder().join("ssh").join("godot-empty");
    fs::create_dir_all(&root).expect("the builder's empty Godot root can be made");

    a_godot_run_refused_saying_why(
        &case,
        &root,
        &local.join("4.7.2"),
        "gate: the repo pins no Godot version and builder lists no Godot install; a routed run needs one",
    );
}

/// A Godot command of a repo whose pin no listed install meets stays here, refused `parity` as the mismatch it is, with
/// no warning and no line of its own.
#[test]
fn a_godot_run_of_a_repo_whose_pin_no_install_meets_falls_back_parity() {
    let case = client_case("client-godot-pin-unmet");
    write(&case, "src/App/Game.csproj", &godot_csproj("4.7.2"));
    let root = godot_root(&case, &[("4.3.0", "4.3.0.stable.mono.official.y\n")]);

    let outcome = a_godot_run_refused_saying_why(&case, &root, &root.join("4.3.0"), &mismatch_of(&case, "godot 4.7.2 exactly", "4.3.0"));

    assert!(!outcome.stderr.contains("gate: warning:"), "{outcome:?}");
    assert!(!outcome.stderr.contains("pins no Godot version"), "{outcome:?}");
}

/// A Godot command of a repo whose Godot pins could not be read stays here, refused `parity` with its own line, though
/// the builder has an install it could have run.
#[test]
fn a_godot_run_of_a_repo_whose_godot_pins_cannot_be_read_falls_back_parity() {
    let case = client_case("client-godot-pins-unreadable");
    write(&case, MANIFEST, "[tools\npnpm = \"10\"\n");
    let root = godot_root(&case, &[("4.7.2", "4.7.2.stable.mono.official.x\n")]);

    let outcome = a_godot_run_refused_saying_why(
        &case,
        &root,
        &root.join("4.7.2"),
        "gate: the repo's Godot pins could not be read; a routed run needs one",
    );

    assert!(!outcome.stderr.contains("gate: warning:"), "{outcome:?}");
}

/// A refused Godot run of an unpinned repo says the mismatch and the parity line, and no warning about a version it will
/// not use: the warning is said only for a run that goes remote.
#[test]
fn a_refused_godot_run_of_an_unpinned_repo_says_no_pin_warning() {
    let case = client_case("client-godot-no-pin-refused");
    write(&case, "global.json", r#"{"sdk": {"version": "999.0.0"}}"#);
    write(&case, "src/App/Game.csproj", "<Project />\n");
    let root = godot_root(&case, &[("4.7.2", "4.7.2.stable.mono.official.x\n")]);

    let outcome = a_godot_run_refused_saying_why(
        &case,
        &root,
        &root.join("4.7.2"),
        &mismatch_of(&case, ".NET SDK 999.0.0 (rollForward patch)", BUILDER_SDK),
    );

    assert!(!outcome.stderr.contains("gate: warning:"), "{outcome:?}");
}

/// A Godot run of a repo taking any Godot version, on a builder that reports no Godot at all, stays here on the judge's
/// mismatch line alone: no second line says the install is missing.
#[test]
fn a_godot_run_of_a_repo_taking_any_godot_version_on_a_builder_without_godot_falls_back_with_the_mismatch_alone() {
    let case = client_case("client-godot-any-without-godot");
    write(&case, MANIFEST, "[tools]\ngodot = \"\"\n");
    let local = godot_root(&case, &[("4.7.2", "4.7.2.stable.mono.official.x\n")]);
    let root = case.folder().join("ssh").join("godot-empty");
    fs::create_dir_all(&root).expect("the builder's empty Godot root can be made");

    let outcome = a_godot_run_refused_saying_why(&case, &root, &local.join("4.7.2"), &mismatch_of(&case, "godot", "none"));

    assert!(!outcome.stderr.contains("lists no Godot install"), "{outcome:?}");
    assert!(!outcome.stderr.contains("did not list its Godot installs"), "{outcome:?}");
}

/// Any other command of a repo pinning several differing Godot versions routes as it would with no pin, sending no
/// version: the builder's command sees no `GODOT_PATH`.
#[test]
fn another_command_of_a_repo_pinning_several_godot_versions_routes_with_no_version() {
    let (case, root) = several_pins_case("client-godot-several-pins-other");

    let outcome = a_routed_godot_run(&case, &root, &[r".\tool.exe", "env", "GODOT_PATH"]);

    assert!(
        outcome.stdout.lines().any(|line| line == "fake-tool: GODOT_PATH is not set"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("several Godot versions"), "{outcome:?}");
}

/// A repo requiring any Godot, or pinning none, sends no version: the builder's command sees no `GODOT_PATH`.
#[test]
fn a_routed_run_of_any_godot_or_no_pin_sends_no_version() {
    for (name, file, text) in [
        ("client-godot-any", MANIFEST, "[tools]\ngodot = \"\"\n"),
        ("client-godot-no-pin", "src/App/App.csproj", "<Project />\n"),
    ] {
        let case = client_case(name);
        write(&case, file, text);
        let root = godot_root(&case, &[("4.7.2", "4.7.2.stable.mono.official.x\n")]);

        let outcome = a_routed_godot_run(&case, &root, &[r".\tool.exe", "env", "GODOT_PATH"]);

        assert!(
            outcome.stdout.lines().any(|line| line == "fake-tool: GODOT_PATH is not set"),
            "{name}: {outcome:?}"
        );
    }
}

/// The routed `dotnet test --no-build` of the send cases: the stand-in dotnet prints its words after `argv`, so a case
/// sees whether the builder kept `--no-build`.
const SENT_TEST: [&str; 7] = [r".\dotnet.exe", "test", "argv", "App.csproj", "-c", "Release", "--no-build"];
/// The PC's build of the send cases' one project, below the worktree root.
const SENT_DLL: &str = r"bin\Release\App.dll";
/// The log line of a send that did not land, before its reason.
const BUILD_NOT_SENT: &str = "gate: remote build not sent: ";

/// Writes the case's routing file as an [`outputs_case`]'s bringing back `**/bin/**`, its repo listed in `sendBuild`
/// when `listed`.
fn sent_routing(case: &Case, listed: bool) {
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let mut routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: ["*tool*", "*dotnet*"] }, "outputs": { &repo: BIN } });
    if listed {
        routing["sendBuild"] = json!([repo]);
    }
    write_remote_json(case, &routing.to_string());
}

/// An [`outputs_case`] bringing back `**/bin/**` whose repo `sendBuild` lists, whose project `App.csproj` at the root the
/// PC built in Release: its `bin\Release` and the two restore files of its `obj` folder, but the one named `missing`,
/// both folders ignored by git.
fn sent_build_case(name: &str, missing: Option<&str>) -> Case {
    let case = outputs_case(name, &BIN);
    sent_routing(&case, true);
    copy_tool(&case, "dotnet.exe");
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\nobj/\n"));
    write(&case, "App.csproj", "<Project />\n");
    write(&case, SENT_DLL, "the PC's build");
    for restore in ["App.csproj.nuget.g.props", "App.csproj.nuget.g.targets"] {
        if missing != Some(restore) {
            write(&case, &format!(r"obj\{restore}"), "<Project />\n");
        }
    }
    case
}

/// Whether the case's log `log` holds a line saying the PC's build was not sent, naming `why` in its reason.
fn build_not_sent(case: &Case, log: &str, why: &str) -> bool {
    let said = log_texts(&case.folder().join(log));
    said.lines()
        .any(|line| line.strip_prefix(BUILD_NOT_SENT).is_some_and(|reason| reason.contains(why)))
}

#[test]
fn a_sent_build_lands_and_the_builder_keeps_no_build() {
    let case = sent_build_case("client-sent-build-lands", None);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "inputs", "inputs", "run"],
        "the send's diff and put, then the run"
    );
    assert!(
        outcome.stdout.lines().any(|line| line == "--no-build"),
        "the builder kept it: {outcome:?}"
    );
    assert!(!outcome.stderr.contains(DROPPED_NO_BUILD_LINE), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(work_folder(&case).join(SENT_DLL)).ok().as_deref(),
        Some("the PC's build"),
        "the work folder holds the PC's build"
    );
    assert_eq!(nothing_back(&case), None, "a kept --no-build leaves no nothing-back record: {outcome:?}");
    assert!(!pending_marker(&case).exists(), "nor an outputs-pending marker: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// A withheld run whose send of the PC's build lands keeps `--no-build` and builds nothing on the builder, so it
/// writes no marker of its own; nor does its passing end clear the one an earlier run left, though it carries failure
/// output globs.
#[test]
fn a_withheld_no_build_run_with_failure_outputs_whose_send_lands_leaves_an_earlier_marker() {
    let case = sent_failure_case("client-sent-build-failure-outputs");
    let marker = planted_state(&case, "outputs-pending", "");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "the send landed: {outcome:?}");
    assert!(!outcome.stderr.contains(DROPPED_NO_BUILD_LINE), "{outcome:?}");
    assert!(marker.exists(), "the earlier run's marker stays: {outcome:?}");
}

/// A withheld run whose send of the PC's build lands builds nothing on the builder, so it writes no marker of its own.
#[test]
fn a_withheld_no_build_run_whose_send_lands_writes_no_marker() {
    let case = sent_failure_case("client-sent-build-failure-outputs-no-marker");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "the send landed: {outcome:?}");
    assert!(!pending_marker(&case).exists(), "no marker is written: {outcome:?}");
}

/// A withheld run whose send of the PC's build lands, whose command writes a crash dump and fails: it has no marker,
/// but still brings back the files under its failure output globs.
#[test]
fn a_failing_withheld_no_build_run_whose_send_lands_brings_its_failure_outputs_back() {
    let case = sent_failure_case("client-sent-build-failure-outputs-failing");
    let steps = ssh_steps(&case, "ssh", &["ok"; 5]);
    let command = [
        r".\dotnet.exe",
        "test",
        "write",
        r"dumps\a.dmp",
        "dump",
        "exit",
        "3",
        "argv",
        "App.csproj",
        "-c",
        "Release",
        "--no-build",
    ];

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "inputs", "inputs", "run", "outputs"],
        "the send landed: {outcome:?}"
    );
    assert_eq!(worktree_text(&case, r"dumps\a.dmp"), "dump", "the dump is installed: {outcome:?}");
    assert!(says_a_line_starting(&outcome, "gate: the failed run's outputs: 1 file(s)"), "{outcome:?}");
    assert!(!pending_marker(&case).exists(), "no marker is written: {outcome:?}");
}

/// A [`sent_build_case`] whose repo has the failure output glob `dumps/*.dmp` too, `dumps/` ignored by git.
fn sent_failure_case(name: &str) -> Case {
    let case = sent_build_case(name, None);
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\nobj/\ndumps/\n"));
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": HOST,
        "slots": 2,
        "repos": { &repo: ["*tool*", "*dotnet*"] },
        "outputs": { &repo: BIN },
        "failureOutputs": { &repo: ["dumps/*.dmp"] },
        "sendBuild": [repo],
    });
    write_remote_json(&case, &routing.to_string());
    case
}

/// Plants `text` as the case's worktree's `<key>.<suffix>` state file in its state root, before any run names the key;
/// its path.
fn planted_state(case: &Case, suffix: &str, text: &str) -> PathBuf {
    let key = work_folder_key(&git(case, &["rev-parse", "--show-toplevel"]));
    let path = case.root().join("remote").join(format!("{key}.{suffix}"));
    fs::create_dir_all(path.parent().expect("the state file has a folder")).expect("the state folder can be made");
    fs::write(&path, text).expect("the state file can be planted");
    path
}

#[test]
fn a_send_the_box_fails_takes_the_drop_with_its_line() {
    let case = sent_build_case("client-sent-build-fails", None);
    let steps = ssh_steps(&case, "ssh", &["ok", "say-exit 1 gate: remote inputs diff failed: not today", "ok"]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "run"], "the send's diff, then the run");
    assert!(outcome.stderr.contains(DROPPED_NO_BUILD_LINE), "{outcome:?}");
    assert!(
        !outcome.stdout.lines().any(|line| line == "--no-build"),
        "the builder dropped it: {outcome:?}"
    );
    assert!(build_not_sent(&case, "out.log", "not today"), "{outcome:?}");
    assert!(nothing_back(&case).is_some(), "the drop built on the builder: {outcome:?}");
    assert!(pending_marker(&case).exists(), "and withheld its outputs: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "never a local run");
}

/// The run call is held until the case removes the file the send installed, so the box's record of the sent build no
/// longer holds what the work folder does.
#[test]
fn a_run_the_box_refuses_for_its_sent_build_runs_again_with_the_drop() {
    let case = sent_build_case("client-sent-build-refused", None);
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", &format!("hold {}", shown(go.path())), "ok"]);
    let running = start(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));
    running.wait_until("the run call", || calls(&steps).len() == 4);
    fs::remove_file(work_folder(&case).join(SENT_DLL)).expect("the sent file can be removed");
    drop(go);

    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run", "run"], "one run again");
    assert!(outcome.stderr.contains(DROPPED_NO_BUILD_LINE), "{outcome:?}");
    assert!(
        outcome.stdout.lines().any(|line| line == "-c"),
        "the run again ran the command: {outcome:?}"
    );
    assert!(
        !outcome.stdout.lines().any(|line| line == "--no-build"),
        "with --no-build dropped: {outcome:?}"
    );
    assert!(nothing_back(&case).is_some(), "the drop built on the builder: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "never a local run");
}

#[test]
fn a_restore_file_missing_on_the_pc_sends_nothing_and_takes_the_drop() {
    let case = sent_build_case("client-sent-build-no-restore", Some("App.csproj.nuget.g.targets"));
    let steps = ssh_steps(&case, "ssh", &["ok"; 2]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "no inputs call");
    assert!(outcome.stderr.contains(DROPPED_NO_BUILD_LINE), "{outcome:?}");
    assert!(!outcome.stdout.lines().any(|line| line == "--no-build"), "{outcome:?}");
    assert!(build_not_sent(&case, "out.log", "nuget.g.targets"), "{outcome:?}");
}

/// How many of `stderr`'s lines are the dropped line.
fn dropped_lines(stderr: &str) -> usize {
    stderr.lines().filter(|line| *line == DROPPED_NO_BUILD_LINE).count()
}

/// A repo `sendBuild` does not list, with no built-here record: the route plans no send.
#[test]
fn a_repo_send_build_does_not_list_takes_the_drop_with_today_s_line() {
    let case = sent_build_case("client-sent-build-unlisted", None);
    sent_routing(&case, false);
    let steps = ssh_steps(&case, "ssh", &["ok"; 2]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "no inputs call");
    assert_eq!(dropped_lines(&outcome.stderr), 1, "{outcome:?}");
    assert!(!outcome.stderr.contains(DROPPED_UNSENT_LINE), "{outcome:?}");
    assert!(!outcome.stdout.lines().any(|line| line == "--no-build"), "{outcome:?}");
    assert!(!build_not_sent(&case, "out.log", ""), "no send was planned: {outcome:?}");
}

/// The built-here record of a [`sent_build_case`]'s worktree saying its project at the root was last built here in
/// Release, as a passing local `dotnet build App.csproj -c Release` writes it.
const BUILT_HERE_RELEASE: &str = r#"{"v":1,"projects":{".":{"configuration":"Release"}}}"#;

/// A [`sent_build_case`] whose repo `sendBuild` does not list, whose project's last build ran here in Release.
fn built_here_sent_case(name: &str) -> Case {
    let case = sent_build_case(name, None);
    sent_routing(&case, false);
    planted_state(&case, "built-here", BUILT_HERE_RELEASE);
    case
}

#[test]
fn an_unlisted_repo_s_no_build_test_built_here_sends_the_pc_s_build_and_the_builder_keeps_no_build() {
    let case = built_here_sent_case("client-sent-build-built-here");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "inputs", "inputs", "run"],
        "the send's diff and put, then the run"
    );
    assert!(
        outcome.stdout.lines().any(|line| line == "--no-build"),
        "the builder kept it: {outcome:?}"
    );
    assert_eq!(dropped_lines(&outcome.stderr), 0, "{outcome:?}");
    assert!(!outcome.stderr.contains(DROPPED_UNSENT_LINE), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(work_folder(&case).join(SENT_DLL)).ok().as_deref(),
        Some("the PC's build"),
        "the work folder holds the PC's build"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_built_here_send_the_box_fails_takes_the_drop_saying_the_build_could_not_be_sent() {
    let case = built_here_sent_case("client-sent-build-built-here-fails");
    let steps = ssh_steps(&case, "ssh", &["ok", "say-exit 1 gate: remote inputs diff failed: not today", "ok"]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "run"], "the send's diff, then the run");
    assert_eq!(
        outcome.stderr.lines().filter(|line| *line == DROPPED_UNSENT_LINE).count(),
        1,
        "{outcome:?}"
    );
    assert_eq!(dropped_lines(&outcome.stderr), 0, "not today's line: {outcome:?}");
    assert!(
        !outcome.stdout.lines().any(|line| line == "--no-build"),
        "the builder dropped it: {outcome:?}"
    );
    assert!(build_not_sent(&case, "out.log", "not today"), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new(), "never a local run");
    assert!(!built_here_record(&case).exists(), "the dropped words built on the builder: {outcome:?}");
}

/// The path of a case worktree's built-here record in its state root.
fn built_here_record(case: &Case) -> PathBuf {
    let key = work_folder_key(&git(case, &["rev-parse", "--show-toplevel"]));
    case.root().join("remote").join(format!("{key}.built-here"))
}

/// The second run routes with the built-here entry standing and plans its send, then waits for the key the first
/// holds; meanwhile the entry goes, as a routed build's removal takes it, and a nothing-back record of the run's
/// configuration stands. Read again under the key, the entry no longer matches, so the record stops the send.
#[test]
fn a_built_here_send_whose_entry_is_gone_under_the_key_is_stopped_by_a_standing_nothing_back_record() {
    let case = built_here_sent_case("client-sent-build-entry-gone");
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(dotnet_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok"; 4]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "600000", "--test-remote-wait-ms", "3600000"],
        ..Tuned::default()
    };
    let second = start(client_gate_tuned(&case, "second.log", &second_steps, &tuned, &SENT_TEST));
    second.wait_for_stderr(KEY_WAITING);
    fs::remove_file(built_here_record(&case)).expect("the built-here record can be removed");
    plant_record(&case, r#"{"v":1,"host":"builder","configuration":"Release"}"#);

    drop(go);
    let first = first.finish();
    let second = second.finish();

    assert_eq!((first.code, second.code), (Some(0), Some(0)), "{first:?} {second:?}");
    assert_eq!(calls(&second_steps), ["preflight", "run"], "nothing sent: {second:?}");
    assert!(build_not_sent(&case, "second.log", "nothing-back"), "{second:?}");
    assert_eq!(second.stderr.lines().filter(|line| *line == DROPPED_UNSENT_LINE).count(), 1, "{second:?}");
    assert!(!second.stdout.lines().any(|line| line == "--no-build"), "{second:?}");
}

/// As above, but the entry is replaced by one of another configuration while the second run waits for the key: the
/// standing nothing-back record stops the send, and the drop, which builds on the builder, removes the record.
#[test]
fn a_built_here_send_the_nothing_back_record_stops_removes_the_built_here_record() {
    let case = built_here_sent_case("client-sent-build-veto-clears");
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(dotnet_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok"; 4]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "600000", "--test-remote-wait-ms", "3600000"],
        ..Tuned::default()
    };
    let second = start(client_gate_tuned(&case, "second.log", &second_steps, &tuned, &SENT_TEST));
    second.wait_for_stderr(KEY_WAITING);
    planted_state(&case, "built-here", r#"{"v":1,"projects":{".":{"configuration":"Debug"}}}"#);
    plant_record(&case, r#"{"v":1,"host":"builder","configuration":"Release"}"#);

    drop(go);
    let first = first.finish();
    let second = second.finish();

    assert_eq!((first.code, second.code), (Some(0), Some(0)), "{first:?} {second:?}");
    assert_eq!(calls(&second_steps), ["preflight", "run"], "nothing sent: {second:?}");
    assert!(build_not_sent(&case, "second.log", "nothing-back"), "{second:?}");
    assert!(!built_here_record(&case).exists(), "the dropped words built on the builder: {second:?}");
}

/// A built-here send the box refuses at the run call runs again with the drop, which builds on the builder, so the
/// built-here record is removed.
#[test]
fn a_built_here_send_the_box_refuses_at_the_run_removes_the_built_here_record() {
    let case = built_here_sent_case("client-sent-build-refused-clears");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", &format!("hold {}", shown(go.path())), "ok"]);
    let running = start(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));
    running.wait_until("the run call", || calls(&steps).len() == 4);
    fs::remove_file(work_folder(&case).join(SENT_DLL)).expect("the sent file can be removed");
    drop(go);

    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run", "run"], "one run again");
    assert!(
        !outcome.stdout.lines().any(|line| line == "--no-build"),
        "with --no-build dropped: {outcome:?}"
    );
    assert!(!built_here_record(&case).exists(), "the drop built on the builder: {outcome:?}");
}

/// A routed build setting no configuration leaves a nothing-back record of any configuration; a passing local build of
/// the project in Debug then writes its built-here entry, but not to the record's `built` list, whose settings differ.
/// The send a routed `-c Debug` test plans from that entry is not stopped by the standing record.
#[test]
fn a_send_planned_from_a_built_here_entry_is_not_stopped_by_a_standing_nothing_back_record() {
    let case = build_case("client-sent-build-built-since");
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\nobj/\n"));
    write(&case, "dotnet.cmd", STAND_IN_DOTNET);
    write(&case, "App.csproj", "<Project />\n");
    passing_build(&case, &["ok", "ok"], &wrapped_build(&["out", "ran there"], &[]));
    let record = nothing_back(&case);
    let here = Tuned {
        extra: &["--local", "--reason", "built here"],
        ..Tuned::default()
    };
    let unused = ssh_steps(&case, "ssh-local", &[]);
    let local = finish(client_gate_tuned(
        &case,
        "local.log",
        &unused,
        &here,
        &[r".\dotnet.cmd", "build", "App.csproj", "-c", "Debug"],
    ));
    write(&case, r"bin\Debug\App.dll", "the PC's build");
    for restore in ["App.csproj.nuget.g.props", "App.csproj.nuget.g.targets"] {
        write(&case, &format!(r"obj\{restore}"), "<Project />\n");
    }
    let steps = ssh_steps(&case, "ssh-sent", &["ok"; 4]);

    let outcome = finish(dotnet_gate(
        &case,
        "out.log",
        &steps,
        &[r".\dotnet.cmd", "test", "App.csproj", "-c", "Debug", "--no-build"],
    ));

    assert!(record.is_some(), "the routed build left its record");
    assert_eq!(local.code, Some(0), "{local:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "the send landed: {outcome:?}");
    assert!(outcome.stdout.contains("--no-build"), "the builder kept it: {outcome:?}");
    assert!(!build_not_sent(&case, "out.log", "outputs are not back"), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(work_folder(&case).join(r"bin\Debug\App.dll")).ok().as_deref(),
        Some("the PC's build")
    );
}

/// A sibling pair's main repo, unlisted, whose project's last build ran here: the real client sends the PC's build to
/// the box, which lays it out in the main repo's leaf folder, and the builder keeps `--no-build`.
#[test]
fn a_pair_s_built_here_no_build_test_lands_its_send_in_the_leaf_folder() {
    let (case, leaf, name, sibling) = pair_case("client-pair-sent-build", &BIN, sibling_repository);
    let repo = repo_name(&leaf);
    let routing = json!({
        "host": HOST,
        "slots": 2,
        "repos": { &repo: ["*tool*", "*dotnet*"] },
        "outputs": { &repo: BIN },
        "siblings": { &repo: [&name] },
    });
    write_remote_json(&case, &routing.to_string());
    copy_tool(&case, "dotnet.exe");
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\nobj/\n"));
    write(&case, "App.csproj", "<Project />\n");
    write(&case, SENT_DLL, "the PC's build");
    for restore in ["App.csproj.nuget.g.props", "App.csproj.nuget.g.targets"] {
        write(&case, &format!(r"obj\{restore}"), "<Project />\n");
    }
    planted_state(&case, "built-here", BUILT_HERE_RELEASE);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));
    let landed = fs::read_to_string(work_folder(&case).join(&leaf).join(SENT_DLL)).ok();
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "inputs", "inputs", "run"],
        "the send's diff and put, then the run"
    );
    assert!(
        outcome.stdout.lines().any(|line| line == "--no-build"),
        "the builder kept it: {outcome:?}"
    );
    assert_eq!(dropped_lines(&outcome.stderr), 0, "{outcome:?}");
    assert!(!outcome.stderr.contains(DROPPED_UNSENT_LINE), "{outcome:?}");
    assert_eq!(landed.as_deref(), Some("the PC's build"), "the copy is under the leaf folder");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_built_here_no_build_run_s_send_lands_and_the_builder_keeps_no_build() {
    let case = built_here_sent_case("client-sent-build-run");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let command = [r".\dotnet.exe", "run", "argv", "--project", "App.csproj", "-c", "Release", "--no-build"];

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &command));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "inputs", "inputs", "run"],
        "the send's diff and put, then the run"
    );
    assert!(
        outcome.stdout.lines().any(|line| line == "--no-build"),
        "the builder kept it: {outcome:?}"
    );
    assert_eq!(dropped_lines(&outcome.stderr), 0, "{outcome:?}");
    assert_eq!(
        fs::read_to_string(work_folder(&case).join(SENT_DLL)).ok().as_deref(),
        Some("the PC's build"),
        "the work folder holds the PC's build"
    );
}

#[test]
fn a_standing_nothing_back_record_of_the_run_s_configuration_takes_the_drop_and_sends_nothing() {
    for (name, record) in [
        ("client-sent-build-record-same", r#"{"v":1,"host":"builder","configuration":"release"}"#),
        ("client-sent-build-record-any", r#"{"v":1,"host":"builder"}"#),
        ("client-sent-build-record-unreadable", "not a record"),
    ] {
        let case = sent_build_case(name, None);
        plant_record(&case, record);
        let steps = ssh_steps(&case, "ssh", &["ok"; 2]);

        let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

        assert_eq!(outcome.code, Some(0), "{name}: {outcome:?}");
        assert_eq!(calls(&steps), ["preflight", "run"], "{name}: nothing sent");
        assert_eq!(dropped_lines(&outcome.stderr), 1, "{name}: {outcome:?}");
        assert!(!outcome.stdout.lines().any(|line| line == "--no-build"), "{name}: {outcome:?}");
        assert!(build_not_sent(&case, "out.log", "nothing-back"), "{name}: {outcome:?}");
    }
}

#[test]
fn a_standing_nothing_back_record_of_another_configuration_still_sends() {
    let case = sent_build_case("client-sent-build-record-other", None);
    plant_record(&case, r#"{"v":1,"host":"builder","configuration":"Debug"}"#);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "{outcome:?}");
    assert!(outcome.stdout.lines().any(|line| line == "--no-build"), "{outcome:?}");
    assert_eq!(dropped_lines(&outcome.stderr), 0, "{outcome:?}");
}

/// The first run holds the worktree's work folder past the second's bound, so the second, a send's, falls back busy
/// before any send and runs here with `--no-build` kept over the PC's own build, saying no dropped line.
#[test]
fn a_busy_fallback_of_a_send_keeps_no_build_and_says_no_dropped_line() {
    let case = sent_build_case("client-sent-build-busy", None);
    write(&case, r"obj\project.assets.json", "{}\n");
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(dotnet_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "100", "--test-dotnet-limit-ms", "3600000"],
        ..Tuned::default()
    };

    let (_, second) = finish_with_pid(client_gate_tuned(&case, "second.log", &second_steps, &tuned, &SENT_TEST));
    drop(go);
    let first = first.finish();

    assert_eq!((first.code, second.code), (Some(0), Some(0)), "{first:?} {second:?}");
    assert_eq!(calls(&second_steps), Vec::<String>::new(), "the second never reached the builder");
    assert!(second.stderr.contains(BUSY_LINE), "{second:?}");
    assert_eq!(dropped_lines(&second.stderr), 0, "{second:?}");
    assert!(
        second.stdout.lines().any(|line| line == "-c") && second.stdout.lines().any(|line| line == "--no-build"),
        "the run here kept it: {second:?}"
    );
}

/// A send's busy fallback whose worktree holds a nothing-back record for its configuration drops `--no-build` and
/// says the local line once; the client never reached the builder to say its own.
#[test]
fn a_busy_fallback_of_a_send_with_a_record_drops_no_build_and_says_the_local_line() {
    let case = sent_build_case("client-sent-build-busy-record", None);
    write(&case, r"obj\project.assets.json", "{}\n");
    plant_record(&case, r#"{"v":1,"host":"builder","configuration":"release"}"#);
    let go = Go(case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&case, "first", &["ok", &format!("hold {}", shown(go.path()))]);
    let first = start(dotnet_gate(&case, "first.log", &first_steps, &[r".\tool.exe", "out", "ran there"]));
    first.wait_until("the first run's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);
    let tuned = Tuned {
        extra: &["--test-key-wait-ms", "100", "--test-dotnet-limit-ms", "3600000"],
        ..Tuned::default()
    };

    let (_, second) = finish_with_pid(client_gate_tuned(&case, "second.log", &second_steps, &tuned, &SENT_TEST));
    drop(go);
    let first = first.finish();

    assert_eq!((first.code, second.code), (Some(0), Some(0)), "{first:?} {second:?}");
    assert_eq!(calls(&second_steps), Vec::<String>::new(), "the second never reached the builder");
    assert_eq!(dropped_lines(&second.stderr), 0, "the client never said the builder's line: {second:?}");
    assert_eq!(
        second
            .stderr
            .matches("gate: dropped --no-build: the last build ran on builder and brought nothing back; building on a heavy slot")
            .count(),
        1,
        "the local line once: {second:?}"
    );
    assert!(
        !second.stdout.lines().any(|line| line == "--no-build"),
        "the run here dropped it: {second:?}"
    );
    assert!(
        nothing_back(&case).is_some(),
        "the run here builds one project, which clears no record: {second:?}"
    );
}

/// The drain poll of the drain cases, short so a wait of a few polls ends soon.
const DRAIN_POLL: [&str; 2] = ["--test-drain-poll-ms", "50"];
/// The start of the line a routed run says once as it starts to wait behind the builder drain.
const DRAIN_WAITING: &str = "gate: waiting behind the builder drain (raised ";
/// The line a routed run says once the builder admits it under a barrier that still stood.
const DRAIN_LIFTED: &str = "gate: the builder drain lifted; starting the routed run";
/// The box seams of a run, inputs or LFS call that finds every session past its lease, so a run admitted at its
/// preflight is no live session by then.
const LEASE_PASSED: &str = "--test-box-drain-lease-ms 1";
/// The steps a drain case's stand-in answers its polls with: more than any wait of the case polls.
static POLLS: [&str; 2000] = ["ok"; 2000];

/// `gate box drain` of `action` on the case's builder root, from the clock, to its end, which passes.
pub(super) fn builder_drain(case: &Case, action: &str) {
    let root = case.folder().join("builder");
    fs::create_dir_all(&root).expect("the builder root can be made");
    let request = STANDARD.encode(json!({ "Root": shown(&root), "Action": action }).to_string());
    let stop = shown(&case.folder());
    // The box under test reads no command line of the elevated gates around it, which the drain would count.
    let mut child = Command::new(GATE)
        .args([
            "box",
            "--test-box-root-walk-stop",
            &stop,
            "--test-box-drain-readable-only",
            "true",
            "drain",
            "--stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the box's drain can be started");
    let mut stdin = child.stdin.take().expect("the drain's stdin is piped");
    writeln!(stdin, "{request}").expect("the drain's request can be written");
    drop(stdin);
    let output = child.wait_with_output().expect("the box's drain ends");
    assert!(output.status.success(), "drain {action}: {output:?}");
}

/// The gate of a routed run logging to `log` through the steps file `steps`, polling the drain every [`DRAIN_POLL`].
fn drain_gate(case: &Case, log: &str, steps: &Path) -> Command {
    let tuned = Tuned {
        extra: &DRAIN_POLL,
        ..Tuned::default()
    };
    client_gate_tuned(case, log, steps, &tuned, &[r".\tool.exe", "out", "ran there"])
}

/// The names in the builder root's drain folder `name`, sorted; none when it is missing.
pub(super) fn drain_entries(case: &Case, name: &str) -> Vec<String> {
    let folder = case.folder().join("builder").join("drain").join(name);
    let mut names: Vec<String> = fs::read_dir(folder).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .map(|entry| entry.expect("a drain entry can be read").file_name().to_string_lossy().into_owned())
                .collect()
        },
    );
    names.sort();
    names
}

/// Asserts the run that ended `outcome` waited behind the drain once, was admitted after the lift and ran its command
/// on the builder, its calls through `steps` ending in a run call after the preflight of its admitted attempt, with no
/// fallback.
fn assert_waited_then_ran(case: &Case, outcome: &Outcome, steps: &Path) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    let waiting = outcome.stderr.lines().filter(|line| line.starts_with(DRAIN_WAITING)).count();
    assert_eq!(waiting, 1, "the waiting line is said once: {outcome:?}");
    assert_in_order(&outcome.stderr, &[DRAIN_WAITING, DRAIN_LIFTED]);
    let called = calls(steps);
    assert_eq!(called.last().map(String::as_str), Some("run"), "{called:?}");
    // The admitted attempt starts from its preflight, and only its sends come between that and its run call.
    let admitted = called.iter().rposition(|verb| verb == "preflight").map_or(called.len(), |at| at + 1);
    assert!(
        admitted < called.len() && called[admitted..].iter().all(|verb| ["inputs", "lfs", "run"].contains(&verb.as_str())),
        "the admitted attempt starts from its preflight: {called:?}"
    );
    assert_eq!(fallbacks(&events(case)), Vec::<&str>::new());
}

#[test]
fn a_drained_builder_refuses_the_preflight_and_the_run_waits_for_the_lift_then_runs_there() {
    let case = client_case("client-drain-lift");
    builder_drain(&case, "raise");
    let steps = ssh_steps(&case, "ssh", &POLLS);
    let running = start(drain_gate(&case, "out.log", &steps));

    running.wait_for_stderr(DRAIN_WAITING);
    builder_drain(&case, "lift");
    let outcome = running.finish();

    assert_waited_then_ran(&case, &outcome, &steps);
    let called = calls(&steps);
    assert!(
        called[..called.len() - 1].iter().all(|verb| verb == "preflight"),
        "only polls before the run: {called:?}"
    );
}

#[test]
fn a_drain_wait_polls_with_one_session_id_across_the_attempts() {
    let case = client_case("client-drain-session");
    builder_drain(&case, "raise");
    let steps = ssh_steps(&case, "ssh", &POLLS);
    let running = start(drain_gate(&case, "out.log", &steps));

    // The refused attempt's preflight and two polls: every one of them renewed the one ticket.
    running.wait_until("two drain polls", || calls(&steps).len() >= 3);
    let tickets = drain_entries(&case, "tickets");
    builder_drain(&case, "lift");
    let outcome = running.finish();

    assert_eq!(tickets.len(), 1, "one ticket for every poll: {tickets:?}");
    let ticket = &tickets[0];
    let session = ticket.strip_prefix("00000001-").expect("the first ticket in line");
    assert_eq!(session.len(), 32, "{ticket}");
    assert_waited_then_ran(&case, &outcome, &steps);
    assert_eq!(
        drain_entries(&case, "tickets"),
        Vec::<String>::new(),
        "the admitted session gave up its own ticket"
    );
    assert_eq!(drain_entries(&case, "sessions"), [session.to_owned()], "the run's session alone");
}

/// Runs a drain case through `steps`, whose call `held` is held until the barrier is raised under it and the run's
/// session has passed its lease: the drain refuses that call, and the run waits until the case lifts the barrier.
fn refused_after_admission(case: &Case, held: usize, steps: &Path) -> Outcome {
    refused_after(case, held, steps, drain_gate(case, "out.log", steps))
}

/// [`refused_after_admission`] of the routed run `gate`.
fn refused_after(case: &Case, held: usize, steps: &Path, mut gate: Command) -> Outcome {
    gate.env(BOX_SEAMS_VARIABLE, LEASE_PASSED);
    let running = start(gate);
    running.wait_until("the held call", || calls(steps).len() >= held);
    builder_drain(case, "raise");
    fs::write(case.folder().join("go"), "").expect("the go file can be made");
    running.wait_for_stderr(DRAIN_WAITING);
    builder_drain(case, "lift");
    running.finish()
}

/// The steps of a drain case whose calls before `held` pass, `held` waits for the case's go file, and every later one
/// passes.
fn held_steps(case: &Case, held: usize) -> PathBuf {
    let hold = format!("hold {}", shown(&case.folder().join("go")));
    let mut steps = vec!["ok"; held - 1];
    steps.push(hold.as_str());
    steps.extend(POLLS);
    ssh_steps(case, "ssh", &steps)
}

#[test]
fn a_run_call_refused_drain_after_its_session_died_restarts_from_the_preflight() {
    let case = client_case("client-drain-run-refused");
    let steps = held_steps(&case, 2);

    let outcome = refused_after_admission(&case, 2, &steps);

    assert_waited_then_ran(&case, &outcome, &steps);
    let called = calls(&steps);
    assert_eq!(
        called[..3],
        ["preflight", "run", "preflight"],
        "the refused run polls the preflight: {called:?}"
    );
}

#[test]
fn an_inputs_call_refused_drain_restarts_from_the_preflight() {
    let case = inputs_case("client-drain-inputs-refused");
    let steps = held_steps(&case, 2);

    let outcome = refused_after_admission(&case, 2, &steps);

    assert_waited_then_ran(&case, &outcome, &steps);
    let called = calls(&steps);
    assert_eq!(
        called[..3],
        ["preflight", "inputs", "preflight"],
        "the refused inputs poll the preflight: {called:?}"
    );
    assert!(
        called.iter().filter(|verb| *verb == "inputs").count() >= 2,
        "the admitted attempt sends its inputs: {called:?}"
    );
}

/// The poll the builder admits is the call that lays out a new work folder, which the admitted attempt's own preflight
/// then finds there: that attempt still gets the raised ceiling and says the cold note, as a first run of the worktree
/// does.
#[test]
fn a_drain_poll_that_lays_out_the_work_folder_leaves_the_admitted_attempt_its_raised_ceiling() {
    let case = client_case("client-drain-cold");
    builder_drain(&case, "raise");
    let steps = ssh_steps(&case, "ssh", &POLLS);
    let tuned = Tuned {
        timeout: "60",
        extra: &DRAIN_POLL,
        ..Tuned::default()
    };
    let running = start(client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]));

    running.wait_for_stderr(DRAIN_WAITING);
    builder_drain(&case, "lift");
    let outcome = running.finish();

    assert_waited_then_ran(&case, &outcome, &steps);
    assert_eq!(
        outcome.stderr.lines().filter(|line| *line == COLD_NOTE).count(),
        1,
        "the cold note is said once: {outcome:?}"
    );
    assert!(
        outcome.stdout.contains(", ceiling 120 s; command "),
        "the admitted attempt ran under the raised ceiling: {outcome:?}"
    );
}

/// A drain poll that cannot reach the builder while its barrier stands is still waiting: the client's unreachable line
/// goes to the log alone, and the console holds the drain's lines only.
#[test]
fn an_unreachable_drain_poll_under_a_standing_barrier_says_its_line_in_the_log_alone() {
    let case = client_case("client-drain-unreachable");
    builder_drain(&case, "raise");
    let mut answers = vec!["ok", "fail-other", "fail-other"];
    answers.extend(POLLS);
    let steps = ssh_steps(&case, "ssh", &answers);
    let running = start(drain_gate(&case, "out.log", &steps));

    // The refused attempt's preflight, the unreachable polls, and a poll the drain refuses again.
    running.wait_until("a poll after the unreachable ones", || calls(&steps).len() >= 4);
    builder_drain(&case, "lift");
    let outcome = running.finish();

    assert_waited_then_ran(&case, &outcome, &steps);
    assert!(!outcome.stderr.contains("unreachable"), "no unreachable line on the console: {outcome:?}");
    let log = log_texts(&case.folder().join("out.log"));
    assert!(
        log.lines()
            .any(|line| line.starts_with("gate: remote ") && line.contains(" unreachable: ssh: connect to host builder port 22")),
        "the log keeps the poll's line: {log}"
    );
}

/// The send of the PC's build is refused by the drain, the run's session past its lease: the run waits behind the drain
/// rather than dropping `--no-build`, and its admitted attempt sends the build again and keeps the word.
#[test]
fn a_drained_build_send_waits_instead_of_dropping_no_build() {
    let case = sent_build_case("client-drain-sent-build", None);
    let steps = held_steps(&case, 2);
    let extra: Vec<&str> = PC_SDKS_ANSWERING.iter().chain(&DRAIN_POLL).copied().collect();
    let tuned = Tuned {
        extra: &extra,
        ..Tuned::default()
    };
    let gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &SENT_TEST);

    let outcome = refused_after(&case, 2, &steps, gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains(DROPPED_NO_BUILD_LINE), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[DRAIN_WAITING, DRAIN_LIFTED]);
    assert!(
        outcome.stdout.lines().any(|line| line == "--no-build"),
        "the admitted attempt kept it: {outcome:?}"
    );
    let called = calls(&steps);
    assert_eq!(
        called[..3],
        ["preflight", "inputs", "preflight"],
        "the refused send polls the preflight: {called:?}"
    );
    assert_eq!(called.last().map(String::as_str), Some("run"), "{called:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The first attempt's preflight lays out the work folder, and the drain refuses its inputs call: the admitted attempt,
/// whose own preflight finds the folder laid out, still gets the raised ceiling and says the cold note once.
#[test]
fn a_new_work_tree_survives_an_attempt_drained_again_before_its_run_call() {
    let case = inputs_case("client-drain-cold-inputs");
    let steps = held_steps(&case, 2);
    let tuned = Tuned {
        timeout: "60",
        extra: &DRAIN_POLL,
        ..Tuned::default()
    };
    let gate = client_gate_tuned(&case, "out.log", &steps, &tuned, &[r".\tool.exe", "out", "ran there"]);

    let outcome = refused_after(&case, 2, &steps, gate);

    assert_waited_then_ran(&case, &outcome, &steps);
    assert_eq!(calls(&steps)[..3], ["preflight", "inputs", "preflight"]);
    assert_eq!(
        outcome.stderr.lines().filter(|line| *line == COLD_NOTE).count(),
        1,
        "the cold note is said once: {outcome:?}"
    );
    assert!(
        outcome.stdout.contains(", ceiling 120 s; command "),
        "the admitted attempt ran under the raised ceiling: {outcome:?}"
    );
}

/// The first poll after the lift is refused for the box's version: no admission, so no lifted line; the run's attempt
/// then meets the same refusal and falls back for it.
#[test]
fn a_poll_refused_for_its_version_says_no_lifted_line() {
    let case = client_case("client-drain-poll-version");
    builder_drain(&case, "raise");
    let mut answers = vec!["ok"];
    answers.extend(std::iter::repeat_n("version 0.0.0-other", 2000));
    let steps = ssh_steps(&case, "ssh", &answers);
    let running = start(drain_gate(&case, "out.log", &steps));

    running.wait_for_stderr(DRAIN_WAITING);
    builder_drain(&case, "lift");
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains(DRAIN_LIFTED), "no admission, no lifted line: {outcome:?}");
    let line = format!("gate: not routed: builder runs gate 0.0.0-other, this PC {}", build_version(&case));
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here") || outcome.stdout.contains("ran there"), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), ["version"]);
}

/// The key of the gone worktree's record a background sweep case seeds.
const SWEPT_KEY: &str = "app-1a2b3c4d";

/// The background sweep's stamp in the case's state root.
fn sweep_stamp(case: &Case) -> PathBuf {
    case.root().join("remote").join("prune-sweep.stamp")
}

/// The background sweep's log in the case's state root.
fn sweep_log(case: &Case) -> PathBuf {
    case.root().join("remote").join("prune-sweep.log")
}

/// Seeds the record of [`SWEPT_KEY`], whose worktree root and git common folder are both missing in the case folder, so
/// the first sweep finds it gone and says so.
fn seed_gone_record(case: &Case) {
    let record = json!({
        "v": 1,
        "root": shown(&case.folder().join("gone-wt")),
        "commonDir": shown(&case.folder().join("gone-repo").join(".git")),
        "repo": "app",
        "lastRouted": "2026-01-01T00:00:00Z",
    });
    let folder = case.root().join("remote");
    fs::create_dir_all(&folder).expect("the records' folder can be made");
    fs::write(folder.join(format!("{SWEPT_KEY}.worktree")), record.to_string()).expect("the record can be written");
}

/// Writes the case's sweep stamp holding `at` as its time.
fn seed_stamp(case: &Case, at: jiff::Timestamp) {
    let stamp = sweep_stamp(case);
    fs::create_dir_all(stamp.parent().expect("the stamp has a folder")).expect("the stamp's folder can be made");
    fs::write(&stamp, json!({ "v": 1, "at": at.to_string() }).to_string()).expect("the stamp can be written");
}

/// The time the case's sweep stamp holds, which it must hold as the stamp's format says.
fn stamp_time(case: &Case) -> jiff::Timestamp {
    let text = fs::read_to_string(sweep_stamp(case)).expect("the stamp was written");
    let stamp: Value = serde_json::from_str(&text).expect("the stamp is JSON");
    assert_eq!(stamp["v"], 1, "{text}");
    stamp["at"]
        .as_str()
        .expect("the stamp's time is text")
        .parse()
        .expect("the stamp's time is a time")
}

/// A routed run of the case through the stand-in of `steps`, with `extra` before its command, which says `ran there`
/// and exits `status`.
fn sweeping_run(case: &Case, steps: &Path, extra: &[&str], status: &str) -> Outcome {
    let tuned = Tuned { extra, ..Tuned::default() };
    finish(client_gate_tuned(
        case,
        "out.log",
        steps,
        &tuned,
        &[r".\tool.exe", "out", "ran there", "exit", status],
    ))
}

/// Waits until the background sweep has ended, its log no longer held open, and asserts the log says the seeded
/// record's worktree is gone.
fn assert_swept(case: &Case) {
    assert!(sweep_stamp(case).exists(), "a sweep starts only once its stamp is written");
    let log = sweep_log(case);
    wait_until(&format!("{} appearing", log.display()), || log.exists());
    wait_until_closed(&log);
    let text = fs::read_to_string(&log).expect("the sweep's log can be read");
    assert!(
        text.contains(&format!("gate: prune: {SWEPT_KEY}: ")) && text.contains(" is gone"),
        "{text}"
    );
}

/// Asserts the routed run ran its command there, with nothing of a sweep said on its console.
fn assert_ran_there(outcome: &Outcome, status: i32) {
    assert_eq!(outcome.code, Some(status), "{outcome:?}");
    assert!(outcome.stdout.contains("ran there"), "{outcome:?}");
    assert!(!outcome.stderr.contains("prune"), "the run says nothing of the sweep: {outcome:?}");
}

#[test]
fn a_routed_run_with_no_stamp_writes_one_and_starts_the_sweep() {
    let case = client_case("client-sweep-no-stamp");
    seed_gone_record(&case);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);
    let before = jiff::Timestamp::now();

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep"], "0");

    assert_ran_there(&outcome, 0);
    assert!(stamp_time(&case) >= before, "the stamp holds the run's time");
    assert_swept(&case);
}

#[test]
fn a_routed_run_within_six_hours_of_the_stamp_starts_no_sweep() {
    let case = client_case("client-sweep-fresh-stamp");
    seed_gone_record(&case);
    seed_stamp(&case, jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1));
    let seeded = fs::read_to_string(sweep_stamp(&case)).expect("the stamp was seeded");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep"], "0");

    assert_ran_there(&outcome, 0);
    assert_eq!(
        fs::read_to_string(sweep_stamp(&case)).expect("the stamp is there"),
        seeded,
        "the stamp is left"
    );
    assert!(!sweep_log(&case).exists(), "no sweep started");
}

#[test]
fn a_routed_run_past_six_hours_of_the_stamp_starts_the_sweep() {
    let case = client_case("client-sweep-old-stamp");
    seed_gone_record(&case);
    let old = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(7);
    seed_stamp(&case, old);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep"], "0");

    assert_ran_there(&outcome, 0);
    assert!(stamp_time(&case) > old, "the stamp is written again");
    assert_swept(&case);
}

#[test]
fn a_future_stamp_starts_the_sweep() {
    let case = client_case("client-sweep-future-stamp");
    seed_gone_record(&case);
    let future = jiff::Timestamp::now() + jiff::SignedDuration::from_hours(1);
    seed_stamp(&case, future);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep"], "0");

    assert_ran_there(&outcome, 0);
    assert!(stamp_time(&case) < future, "the stamp is written again with the run's time");
    assert_swept(&case);
}

#[test]
fn a_malformed_stamp_starts_the_sweep() {
    let case = client_case("client-sweep-malformed-stamp");
    seed_gone_record(&case);
    let stamp = sweep_stamp(&case);
    fs::create_dir_all(stamp.parent().expect("the stamp has a folder")).expect("the stamp's folder can be made");
    fs::write(&stamp, "nonsense").expect("the stamp can be written");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep"], "0");

    assert_ran_there(&outcome, 0);
    stamp_time(&case);
    assert_swept(&case);
}

#[test]
fn a_routed_run_whose_breakaway_is_refused_starts_no_sweep_and_writes_no_stamp() {
    let case = client_case("client-sweep-refused");
    refuse_breakaway();
    seed_gone_record(&case);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep"], "0");

    assert_ran_there(&outcome, 0);
    assert_eq!(calls(&steps), ["preflight", "run"], "the builder was called");
    assert!(!sweep_stamp(&case).exists(), "no stamp is written");
    assert!(!sweep_log(&case).exists(), "no sweep started");
}

#[test]
fn a_routed_run_without_the_seam_starts_no_sweep() {
    let case = client_case("client-sweep-no-seam");
    seed_gone_record(&case);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = sweeping_run(&case, &steps, &[], "0");

    assert_ran_there(&outcome, 0);
    assert!(!sweep_stamp(&case).exists(), "no stamp is written");
    assert!(!sweep_log(&case).exists(), "no sweep started");
}

#[test]
fn a_local_run_starts_no_sweep() {
    let case = client_case("client-sweep-local");
    seed_gone_record(&case);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep", "--local", "--reason", "kept here"], "0");

    assert_ran_there(&outcome, 0);
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
    assert!(!sweep_stamp(&case).exists(), "no stamp is written");
    assert!(!sweep_log(&case).exists(), "no sweep started");
}

#[test]
fn a_routed_run_that_falls_back_before_its_builder_call_starts_no_sweep() {
    let case = client_case("client-sweep-fallback");
    seed_gone_record(&case);
    let steps = ssh_steps(&case, "ssh", &["fail-other"]);

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep"], "0");

    assert_ran_there(&outcome, 0);
    assert_eq!(calls(&steps), ["preflight"], "only the preflight was called");
    assert_eq!(fallbacks(&events(&case)), ["unreachable"]);
    assert!(!sweep_stamp(&case).exists(), "no stamp is written");
    assert!(!sweep_log(&case).exists(), "no sweep started");
}

#[test]
fn a_sweep_that_cannot_start_removes_its_stamp_and_leaves_the_run_s_status() {
    let case = client_case("client-sweep-unstartable");
    seed_gone_record(&case);
    fs::create_dir_all(sweep_log(&case)).expect("a folder can stand at the sweep log's path");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep"], "3");

    assert_ran_there(&outcome, 3);
    assert!(outcome.stderr.contains("gate: FAILED (status 3)"), "{outcome:?}");
    assert!(!sweep_stamp(&case).exists(), "the stamp of a sweep that never started is removed");
    assert!(sweep_log(&case).is_dir(), "the folder is left as it was");
}

#[test]
fn a_routed_run_s_status_and_output_are_unchanged_by_the_sweep() {
    let case = client_case("client-sweep-status");
    seed_gone_record(&case);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = sweeping_run(&case, &steps, &["--test-prune-sweep"], "3");

    assert_ran_there(&outcome, 3);
    assert!(outcome.stderr.contains("gate: FAILED (status 3)"), "{outcome:?}");
    assert_swept(&case);
}

/// The line a routed run's log gets once its outputs pull moved its archive over the data channel.
const CHANNEL_PULL: &str = r"(?m)^gate: data channel: pull (\d+) in \d+\.\d s\r?$";

/// A loopback port free a moment ago: the system's pick for a listener closed at once, which the box's channel then
/// listens on alone.
fn free_channel_port() -> u16 {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("a loopback listener binds");
    listener.local_addr().expect("the listener's address is read").port()
}

/// A [`client_case`] whose repo routes its `*tool*` commands to the Windows `builders` entry [`FIRST_BUILDER`] alone,
/// with the output globs [`BIN`] and, when given, the `data` address `data`; `bin/` is left out of git.
fn channel_case(name: &str, data: Option<&str>) -> Case {
    let case = client_case(name);
    write(&case, ".gitignore", &format!("{CLIENT_IGNORED}bin/\n"));
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let mut entry = json!({ "host": FIRST_BUILDER, "os": "windows", "repos": { &repo: ["*tool*"] }, "outputs": { &repo: BIN } });
    if let Some(data) = data {
        entry["data"] = json!(data);
    }
    let routing = json!({ "host": HOST, "repos": { &repo: ["*never*"] }, "builders": [entry] });
    write_remote_json(&case, &routing.to_string());
    case
}

/// A routed run of `case` logging to `log` whose command writes `content` to `bin\new.txt`, through the stand-in's
/// steps file `name`: the preflight and the run call, then the outputs call, whose request the stand-in keeps; every
/// box call is given a data channel on the loopback port `port` alone. The steps file and the run's outcome.
fn channel_run(case: &Case, (name, log): (&str, &str), port: u16, content: &str) -> (PathBuf, Outcome) {
    let fed = format!("fed-hold {}", shown(&case.folder()));
    let steps = ssh_steps(case, name, &["ok", "ok", &fed]);
    let mut gate = client_gate(case, log, &steps, &[r".\tool.exe", "write", r"bin\new.txt", content]);
    gate.env(BOX_SEAMS_VARIABLE, format!("--test-box-channel-ports {port}-{port}"));
    (steps, finish(gate))
}

/// The two proofs, as hex, of the data channel the outputs call kept by the steps file `steps` asked for.
fn channel_proofs(steps: &Path) -> (String, String) {
    let sent = last_request(steps);
    let proof = |key: &str| {
        sent["Channel"][key]
            .as_str()
            .expect("the outputs call asks for a channel with both proofs")
            .to_owned()
    };
    (proof("BoxProof"), proof("PcProof"))
}

/// With `data` on a Windows builder, the outputs pull goes over the data channel: the request asks for it from this
/// PC's loopback address, the log says the pull's bytes, which are the box's own sent count, and the built file is
/// installed with the bytes the command wrote, as the ssh pull installs them.
#[test]
fn an_outputs_pull_with_data_goes_over_the_data_channel_and_installs_what_was_built() {
    let case = channel_case("client-channel-pull", Some("127.0.0.1"));

    let (steps, outcome) = channel_run(&case, ("ssh", "out.log"), free_channel_port(), "built over the channel");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"]);
    let sent = last_request(&steps);
    assert_eq!(sent["Channel"]["Address"], json!("127.0.0.1"), "{sent}");
    assert_eq!(sent["Channel"]["Peer"], json!("127.0.0.1"), "{sent}");
    let log = out_log(&case);
    let pulled = Regex::new(CHANNEL_PULL).expect("the pattern compiles").captures(&log);
    let bytes = pulled.map(|found| found[1].to_owned());
    assert!(bytes.is_some(), "the pull line is in the log: {log}");
    let sent_line = format!("gate: data channel sent {} bytes sha256 ", bytes.unwrap_or_default());
    assert!(log.contains(&sent_line), "the box sent what the PC counted: {log}");
    assert!(!log.contains("gate: data channel not used"), "{log}");
    let installed = fs::read_to_string(case.folder().join("bin").join("new.txt")).expect("the built file is installed");
    assert_eq!(installed, "built over the channel");
    assert_eq!(outputs_leftovers(&case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
}

/// With no `data`, the outputs pull is today's: its request asks for no channel and the log says nothing of one.
#[test]
fn an_outputs_pull_without_data_asks_for_no_channel_and_logs_none() {
    let case = channel_case("client-channel-none", None);

    let (steps, outcome) = channel_run(&case, ("ssh", "out.log"), free_channel_port(), "built over ssh");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"]);
    let sent = last_request(&steps);
    assert!(sent.get("Channel").is_none(), "{sent}");
    assert!(!out_log(&case).contains("gate: data channel"), "{}", out_log(&case));
    let installed = fs::read_to_string(case.folder().join("bin").join("new.txt")).expect("the built file is installed");
    assert_eq!(installed, "built over ssh");
}

/// Each run's outputs call draws its own two proofs: two runs ask with four different ones.
#[test]
fn two_runs_outputs_calls_carry_different_proofs() {
    let case = channel_case("client-channel-proofs", Some("127.0.0.1"));

    let (first_steps, first) = channel_run(&case, ("ssh", "out.log"), free_channel_port(), "first");
    let first_proofs = channel_proofs(&first_steps);
    let (second_steps, second) = channel_run(&case, ("ssh2", "out2.log"), free_channel_port(), "second");
    let second_proofs = channel_proofs(&second_steps);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(second.code, Some(0), "{second:?}");
    let all = [&first_proofs.0, &first_proofs.1, &second_proofs.0, &second_proofs.1];
    for (at, proof) in all.iter().enumerate() {
        assert!(all[at + 1..].iter().all(|other| other != proof), "every proof differs: {all:?}");
    }
}

/// What of the outputs request kept by the steps file `steps` carries no proof's hex but only the proofs: the request's
/// base64 word, and for each proof the 32 characters of the word that encode only its hex digits.
fn proof_words(steps: &Path) -> Vec<String> {
    let input = fs::read_to_string(beside(steps, ".input")).expect("the stand-in kept the call's input");
    let word = input.lines().next().expect("the input has a request line").to_owned();
    let json = STANDARD.decode(&word).expect("the request is base64");
    let (box_proof, pc_proof) = channel_proofs(steps);
    let mut words = vec![word.clone()];
    for proof in [box_proof, pc_proof] {
        let at = json
            .windows(proof.len())
            .position(|window| window == proof.as_bytes())
            .expect("the request carries the proof's hex");
        // The first whole group of three bytes inside the proof's 32 digits: its four characters, and the next 28,
        // encode 24 of those digits alone.
        let start = at.div_ceil(3) * 4;
        words.push(word[start..start + 32].to_owned());
    }
    words
}

/// No proof the request carried reaches the run's log, its journal or its console, as hex or inside the request's
/// base64 word.
#[test]
fn a_channel_pull_s_proofs_appear_in_no_log_journal_or_console_output() {
    let case = channel_case("client-channel-secret", Some("127.0.0.1"));

    let (steps, outcome) = channel_run(&case, ("ssh", "out.log"), free_channel_port(), "kept secret");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        Regex::new(CHANNEL_PULL).expect("the pattern compiles").is_match(&out_log(&case)),
        "the pull went over the channel"
    );
    let (box_proof, pc_proof) = channel_proofs(&steps);
    let words = proof_words(&steps);
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    let journal = fs::read_to_string(case.root().join("events.jsonl")).expect("the journal was written");
    for (what, text) in [
        ("log", &log),
        ("journal", &journal),
        ("stdout", &outcome.stdout),
        ("stderr", &outcome.stderr),
    ] {
        for proof in [&box_proof, &pc_proof] {
            assert!(!text.to_ascii_lowercase().contains(proof.as_str()), "the {what} holds no proof");
        }
        for word in &words {
            assert!(!text.contains(word.as_str()), "the {what} holds no part of the request word");
        }
    }
}

/// Two channel pulls in a row on a one-port range both go over the channel: the PC closes first, so the box's port is
/// free again for the next.
#[test]
fn two_channel_pulls_in_a_row_on_one_port_both_go_over_the_channel() {
    let case = channel_case("client-channel-one-port", Some("127.0.0.1"));
    let port = free_channel_port();
    let pulled = Regex::new(CHANNEL_PULL).expect("the pattern compiles");

    let (_, first) = channel_run(&case, ("ssh", "out.log"), port, "first");
    let (_, second) = channel_run(&case, ("ssh2", "out2.log"), port, "second");

    assert_eq!(first.code, Some(0), "{first:?}");
    assert!(
        pulled.is_match(&out_log(&case)),
        "the first pull went over the channel: {}",
        out_log(&case)
    );
    assert_eq!(second.code, Some(0), "{second:?}");
    let second_log = log_texts(&case.folder().join("out2.log"));
    assert!(pulled.is_match(&second_log), "the second pull went over the channel: {second_log}");
    let installed = fs::read_to_string(case.folder().join("bin").join("new.txt")).expect("the built file is installed");
    assert_eq!(installed, "second");
}

/// A routed run of `case` logging to `log` whose command writes `content` to `bin\new.txt`, through the stand-in's
/// steps file `name`: the preflight, the run call, the outputs call, whose request the stand-in keeps, and a second
/// outputs call, the pull over ssh a `channel` refusal leads to. Every box call is given a data channel on the loopback
/// port `port` alone and the box seams `box_seams` after it, and the run the options `extra`. The steps file and the
/// run's outcome.
fn channel_run_tuned(case: &Case, (name, log): (&str, &str), (port, box_seams): (u16, &str), extra: &[&str], content: &str) -> (PathBuf, Outcome) {
    let fed = format!("fed-hold {}", shown(&case.folder()));
    let steps = ssh_steps(case, name, &["ok", "ok", &fed, "ok"]);
    let tuned = Tuned { extra, ..Tuned::default() };
    let mut gate = client_gate_tuned(case, log, &steps, &tuned, &[r".\tool.exe", "write", r"bin\new.txt", content]);
    gate.env(
        BOX_SEAMS_VARIABLE,
        format!("--test-box-channel-ports {port}-{port} {box_seams}").trim_end(),
    );
    (steps, finish(gate))
}

/// The box's line saying why its data channel did not complete, as the run's log holds it.
const NOT_COMPLETED: &str = "gate: data channel not completed: ";

/// A channel pull ends on the PC's close, never on a box-side window: with the box's accept window and idle limit an
/// hour, past the suite's hang guards, the run still pulls over the channel, so the box's wait for the PC's close after
/// the ack ended at that close and the box exited after it; and the log holds no line of a channel wait that ran out.
#[test]
fn a_channel_pull_ends_on_the_pc_s_close_without_sitting_out_a_box_side_window() {
    let case = channel_case("client-channel-close", Some("127.0.0.1"));
    let windows = "--test-box-channel-accept-ms 3600000 --test-box-channel-idle-ms 3600000";

    let (steps, outcome) = channel_run_tuned(&case, ("ssh", "out.log"), (free_channel_port(), windows), &[], "closed first");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run", "outputs"]);
    let log = out_log(&case);
    assert!(
        Regex::new(CHANNEL_PULL).expect("the pattern compiles").is_match(&log),
        "the pull went over the channel: {log}"
    );
    assert!(!log.contains(NOT_COMPLETED), "no channel wait ran out: {log}");
    assert!(!log.contains("gate: data channel not used"), "{log}");
    let installed = fs::read_to_string(case.folder().join("bin").join("new.txt")).expect("the built file is installed");
    assert_eq!(installed, "closed first");
}

/// A channel pull of `case` whose PC side takes the fault `fault` (`--test-remote-channel-fault`), its stand-in's steps
/// file `ssh`, logging to `out.log`, the command writing `content`: the steps file and the run's outcome.
fn faulted_run(case: &Case, fault: &str, content: &str) -> (PathBuf, Outcome) {
    let extra = ["--test-remote-channel-fault", fault];
    channel_run_tuned(case, ("ssh", "out.log"), (free_channel_port(), ""), &extra, content)
}

/// Asserts that the run of `case` that ended as `outcome`, through the steps file `steps`, pulled its outputs over ssh
/// after the box refused its data channel: the outputs call made twice, the not-used line saying why as `why` starts,
/// the box's line saying why its channel did not complete as `box_why` starts, no pull line, and the built file
/// installed whole with `content`, with no archive or stage left.
fn assert_pulled_over_ssh(case: &Case, (steps, outcome): (&Path, &Outcome), (why, box_why): (&str, &str), content: &str) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(steps), ["preflight", "run", "outputs", "outputs"]);
    let log = out_log(case);
    let not_used = format!("gate: data channel not used: {why}");
    assert!(
        log.lines().any(|line| line.starts_with(&not_used) && line.ends_with("; sent over ssh")),
        "{log}"
    );
    assert!(log.contains(&format!("{NOT_COMPLETED}{box_why}")), "the box refused the channel: {log}");
    assert!(
        !Regex::new(CHANNEL_PULL).expect("the pattern compiles").is_match(&log),
        "nothing came over the channel: {log}"
    );
    let installed = fs::read_to_string(case.folder().join("bin").join("new.txt")).expect("the built file is installed");
    assert_eq!(installed, content, "the ssh pull installed the archive whole");
    assert_eq!(outputs_leftovers(case), Vec::<PathBuf>::new(), "the archive and the stage are removed");
}

/// A PC that takes the box's proof for a mismatch sends nothing on the channel and drops it: the box, given no proof of
/// the PC's, refuses the channel, and the run pulls over ssh.
#[test]
fn a_box_proof_the_pc_refuses_gets_no_byte_and_the_run_pulls_over_ssh() {
    let case = channel_case("client-channel-box-proof", Some("127.0.0.1"));

    let (steps, outcome) = faulted_run(&case, "box-proof", "proof refused");

    assert_pulled_over_ssh(
        &case,
        (&steps, &outcome),
        ("the box's proof does not match", "the PC closed the channel before its proof"),
        "proof refused",
    );
}

/// A PC that stops after half the archive and drops the channel without an ack installs nothing from it: the box
/// refuses the channel, and the ssh pull installs the archive whole.
#[test]
fn a_truncated_channel_pull_installs_nothing_and_the_ssh_pull_installs_the_archive_whole() {
    let case = channel_case("client-channel-truncate", Some("127.0.0.1"));

    let (steps, outcome) = faulted_run(&case, "truncate", "truncated");

    assert_pulled_over_ssh(&case, (&steps, &outcome), ("the test seam stopped the pull after ", ""), "truncated");
}

/// A PC that read an altered archive acks what it read, which the box's check refuses: nothing installs from the
/// channel, and the ssh pull installs the archive whole.
#[test]
fn an_altered_channel_pull_is_refused_and_the_ssh_pull_installs_the_archive_whole() {
    let case = channel_case("client-channel-alter", Some("127.0.0.1"));

    let (steps, outcome) = faulted_run(&case, "alter", "altered");

    let why = "the PC's acknowledgement does not match what was sent";
    assert_pulled_over_ssh(&case, (&steps, &outcome), (why, why), "altered");
}

/// A PC that read the whole archive and drops the channel before its ack installs nothing from it: the box keeps the
/// archive and refuses the channel, and the ssh pull installs it whole.
#[test]
fn a_channel_dropped_before_the_ack_leaves_the_archive_to_the_ssh_pull() {
    let case = channel_case("client-channel-no-ack", Some("127.0.0.1"));

    let (steps, outcome) = faulted_run(&case, "no-ack", "not acked");

    let why = "the test seam dropped the channel before the PC's acknowledgement";
    assert_pulled_over_ssh(&case, (&steps, &outcome), (why, ""), "not acked");
}
