//! The remote client's snapshot, through `--test-remote-snapshot`: a routed run that stops once it has made the
//! snapshot and bridge commits of its worktree, a real git repository in the case folder, and says them.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use gate_router::repo_name;
use serde_json::json;

use crate::support::{
    Case, GIT_REDIRECTS, Outcome, copy_tool, events, finish, git, git_init, heavy_words, shown, wait_for_journal_line, write_remote_json,
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
