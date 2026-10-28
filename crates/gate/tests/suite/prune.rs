//! `gate prune --builders` on the PC: each case's worktree records judged against real git worktrees made in the case's
//! folder, and the keys of the gone ones sent through the stand-in ssh to the real `gate box prune` on two builder roots
//! in the case's folder, `builder-1` the routing file's `host` and `builder-2` its one `hosts` entry.

use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use gate_state::{KernelObject, StateRoot, key_hash, v1};
use gate_win32::NamedMutex;
use serde_json::{Value, json};
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::support::{
    Case, FAKE_SSH, GATE, GATE_VARIABLE, GIT_REDIRECTS, Outcome, ROOT_WALK_STOP_VARIABLE, STEPS_VARIABLE, calls, finish, git, git_home_for, shown,
    ssh_steps, write_remote_json,
};

/// The routing file of a prune case: two builders.
const ROUTING: &str = r#"{"host": "builder-1", "hosts": ["builder-2"], "repos": {"app": ["*"]}}"#;
/// The key of a case's worktree record.
const KEY: &str = "app-1a2b3c4d";
/// A time a worktree was found gone long before any case runs.
const LONG_GONE: &str = "2026-01-01T00:00:00Z";
/// The steps the stand-in answers a case's calls with: more than any case makes.
static ANSWERS: [&str; 8] = ["ok"; 8];
/// The usage refusal of a box too old for the keyed prune.
const OLD_BOX: &str = "say-exit 2 usage: gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs|drain --stdin";

/// A case whose routing file lists `builder-1` and `builder-2`, with a folder for the stand-in's files.
fn prune_case(name: &str) -> Case {
    let case = Case::new(name);
    write_remote_json(&case, ROUTING);
    fs::create_dir_all(case.folder().join("ssh")).expect("the stand-in's folder can be made");
    case
}

/// The root of the builder `alias` in the case's folder.
fn builder_root(case: &Case, alias: &str) -> PathBuf {
    case.folder().join(alias)
}

/// The work folder of [`KEY`] on the builder `alias`, holding a file, made.
fn work_folder(case: &Case, alias: &str) -> PathBuf {
    let folder = builder_root(case, alias).join("work").join(KEY);
    fs::create_dir_all(&folder).expect("the work folder can be made");
    fs::write(folder.join("file"), "x").expect("a file can be written in the work folder");
    folder
}

/// `gate prune --builders` with `extra` after its seams, its state root the case's, through the stand-in of the steps
/// file `steps` to each builder's own root, to its end.
fn pruned(case: &Case, steps: &Path, extra: &[&str]) -> Outcome {
    pruned_under(case, steps, &builder_root(case, "builder-1"), extra)
}

/// [`pruned`] with `builder-1` laid out under `first` in place of its own root.
fn pruned_under(case: &Case, steps: &Path, first: &Path, extra: &[&str]) -> Outcome {
    let transport = format!("local:builder-1={}|builder-2={}", shown(first), shown(&builder_root(case, "builder-2")));
    let mut args = vec![
        "prune",
        "--builders",
        "--test-remote-transport",
        &transport,
        "--test-remote-ssh",
        FAKE_SSH,
    ];
    args.extend_from_slice(extra);
    let mut command = case.gate(&args);
    command
        .env("GATE_STATE_DIR", case.root())
        .env(STEPS_VARIABLE, steps)
        .env(GATE_VARIABLE, GATE)
        .env(ROOT_WALK_STOP_VARIABLE, case.folder());
    finish(command)
}

/// A real linked worktree `wt` of the repo `repo` with one commit, both in the case's folder: the worktree's root and
/// the repo's common folder, as the record writes them.
fn worktree(case: &Case) -> (String, String) {
    git(case, &["init", "--quiet", "repo"]);
    git(case, &["-C", "repo", "commit", "--allow-empty", "--quiet", "-m", "first"]);
    git(case, &["-C", "repo", "worktree", "add", "--quiet", "../wt"]);
    (shown(&case.folder().join("wt")), shown(&case.folder().join("repo").join(".git")))
}

/// [`worktree`], its folder then deleted, so git lists it prunable.
fn gone_worktree(case: &Case) -> (String, String) {
    let (root, common) = worktree(case);
    fs::remove_dir_all(&root).expect("the worktree's folder can be deleted");
    (root, common)
}

/// Writes the worktree record of [`KEY`] in the case's state root, as the v1 writer shapes it: the worktree `root`, its
/// common folder `common`, and `gone_since` when given; its path.
fn write_record(case: &Case, root: &str, common: &str, gone_since: Option<&str>) -> PathBuf {
    let mut record = json!({ "v": 1, "root": root, "commonDir": common, "repo": "app", "lastRouted": "2026-10-27T00:00:00Z" });
    if let Some(since) = gone_since {
        record["goneSince"] = json!(since);
    }
    let folder = case.root().join("remote");
    fs::create_dir_all(&folder).expect("the records' folder can be made");
    let path = folder.join(format!("{KEY}.worktree"));
    fs::write(&path, record.to_string()).expect("the record can be written");
    path
}

/// The record at `path`, read as JSON.
fn record_of(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("the record can be read")).expect("the record is JSON")
}

/// The lock of [`KEY`]'s work folder on the builder `alias`, held with no sharing at all, as a run holds it, until the
/// returned file is dropped.
fn hold_lock(case: &Case, alias: &str) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(builder_root(case, alias).join("work").join(format!("{KEY}.lock")))
        .expect("the work folder's lock can be held")
}

/// A gone worktree's record, gone long enough for its builders to be called, and [`KEY`]'s work folder on each builder.
fn long_gone(case: &Case) -> (PathBuf, PathBuf) {
    let (root, common) = gone_worktree(case);
    write_record(case, &root, &common, Some(LONG_GONE));
    (work_folder(case, "builder-1"), work_folder(case, "builder-2"))
}

/// The files the PC keeps of [`KEY`], each written: `host`'s own, `builder-2`'s own, and the ones they share, the
/// record aside.
struct PcFiles {
    host: Vec<PathBuf>,
    other: Vec<PathBuf>,
    shared: Vec<PathBuf>,
}

/// The id of `builder-2`, the folder of its own files and refs.
fn other_id() -> String {
    gate_router::builder_id("builder-2")
}

/// Writes every file the PC keeps of [`KEY`] in the case's state root.
fn pc_files(case: &Case) -> PcFiles {
    let state = case.root();
    let own = |id: Option<&str>| {
        vec![
            v1::outputs_hashes_path(&state, id, KEY),
            v1::inputs_hashes_path(&state, id, KEY),
            v1::inputs_carried_path(&state, id, KEY),
            v1::sent_build_hashes_path(&state, id, KEY),
            v1::outputs_capped_path(&state, id, KEY),
        ]
    };
    let id = other_id();
    let files = PcFiles {
        host: own(None),
        other: own(Some(&id)),
        shared: vec![
            v1::outputs_pending_path(&state, KEY),
            v1::nothing_back_path(&state, KEY),
            v1::warm_record_path(&state, KEY),
            v1::warm_log_path(&state, KEY),
            v1::warm_lock_path(&state, KEY),
            v1::remote_dir(&state).join(format!("{KEY}.outputs-from")),
            v1::remote_dir(&state).join(format!("{KEY}.built-here")),
        ],
    };
    for path in files.host.iter().chain(&files.other).chain(&files.shared) {
        fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the file's folder can be made");
        fs::write(path, "x").expect("the file can be written");
    }
    files
}

/// Whether every one of `paths` is there.
fn all_there(paths: &[PathBuf]) -> bool {
    paths.iter().all(|path| path.exists())
}

/// Whether none of `paths` is there.
fn none_there(paths: &[PathBuf]) -> bool {
    paths.iter().all(|path| !path.exists())
}

/// The refs a routed run leaves for [`KEY`] in a repo, and one of another key that stays.
fn key_refs() -> Vec<String> {
    vec![
        format!("refs/gate-remote/{KEY}"),
        format!("refs/gate-remote/{KEY}-back"),
        format!("refs/gate-remote/{}/{KEY}", other_id()),
        "refs/gate-remote/other-1a2b3c4d".to_owned(),
    ]
}

/// Points each of [`key_refs`] at the head of the repo in the case's folder `repo`, but the sync-back ref, which
/// points at a commit of the head's tree committed in 2020, so the removal finds it old.
fn leave_refs(case: &Case, repo: &str) {
    let old = old_commit(case, repo);
    for name in key_refs() {
        let target = if name.ends_with("-back") { old.as_str() } else { "HEAD" };
        git(case, &["-C", repo, "update-ref", &name, target]);
    }
}

/// A commit of the head's tree of the repo in the case's folder `repo`, its committer date set in 2020 through
/// `GIT_COMMITTER_DATE`: its id.
fn old_commit(case: &Case, repo: &str) -> String {
    let mut command = Command::new("git");
    for name in GIT_REDIRECTS {
        command.env_remove(name);
    }
    git_home_for(&mut command);
    let output = command
        .args(["-c", "user.name=gate suite", "-c", "user.email=suite@example.invalid", "-C", repo])
        .args(["commit-tree", "HEAD^{tree}", "-m", "an old sync-back"])
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .current_dir(case.folder())
        .stdin(Stdio::null())
        .output()
        .expect("git starts");
    assert!(output.status.success(), "git commit-tree: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The names of the refs under `refs/gate-remote/` of the repo in the case's folder `repo`.
fn refs_in(case: &Case, repo: &str) -> Vec<String> {
    git(case, &["-C", repo, "for-each-ref", "--format=%(refname)", "refs/gate-remote/"])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A record of a worktree long gone, each builder's work folder of [`KEY`], every file the PC keeps of it, and the
/// key's refs in the worktree's repo: the record's path, its root, and the files.
fn seeded(case: &Case) -> (PathBuf, String, PcFiles) {
    let (root, common) = gone_worktree(case);
    let record = write_record(case, &root, &common, Some(LONG_GONE));
    work_folder(case, "builder-1");
    work_folder(case, "builder-2");
    leave_refs(case, "repo");
    (record, root, pc_files(case))
}

/// The file at `path` held open sharing `share`, until the returned file is dropped.
fn held_file(path: &Path, share: u32) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(share)
        .open(path)
        .expect("the file can be held")
}

#[test]
fn a_key_every_builder_pruned_has_its_pc_files_refs_and_record_removed() {
    let case = prune_case("prune_pc_removed");
    let (record, root, files) = seeded(&case);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "gate: remote builder-1 pruned {KEY} (worktree gone)\ngate: remote builder-2 pruned {KEY} (worktree gone)\n\
             gate: prune: removed {KEY} ({root})\n"
        ),
        "{outcome:?}"
    );
    assert!(none_there(&files.host) && none_there(&files.other), "each builder's own files are gone");
    assert!(none_there(&files.shared), "the shared files are gone");
    assert!(!record.exists(), "the record is gone");
    assert_eq!(
        refs_in(&case, "repo"),
        ["refs/gate-remote/other-1a2b3c4d"],
        "only another key's ref stays"
    );
}

#[test]
fn a_key_one_builder_held_keeps_its_shared_files_and_record_and_loses_the_pruned_builder_s_own() {
    let case = prune_case("prune_pc_one_held");
    let (record, _, files) = seeded(&case);
    let held = hold_lock(&case, "builder-1");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("gate: remote builder-1 kept {KEY}: in use\ngate: remote builder-2 pruned {KEY} (worktree gone)\n"),
        "{outcome:?}"
    );
    assert!(all_there(&files.host), "the holding builder keeps its own files");
    assert!(none_there(&files.other), "the pruned builder's own files are gone");
    assert!(all_there(&files.shared), "the shared files stay");
    assert!(record.exists(), "the record stays");
    assert_eq!(refs_in(&case, "repo"), key_refs(), "every ref stays");
}

#[test]
fn a_key_whose_mutex_is_held_is_kept_in_use() {
    let case = prune_case("prune_pc_mutex_held");
    let (record, _, files) = seeded(&case);
    let name = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::Routed { hash: key_hash(KEY) });
    let held = NamedMutex::open(&name)
        .expect("the mutex opens")
        .acquire_owned(Duration::from_secs(5))
        .expect("the mutex is free");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.ends_with(&format!("gate: prune: kept {KEY}: in use\n")), "{outcome:?}");
    assert!(all_there(&files.host) && all_there(&files.other), "each builder's own files stay");
    assert!(all_there(&files.shared), "the shared files stay");
    assert!(record.exists(), "the record stays");
    assert_eq!(refs_in(&case, "repo"), key_refs(), "every ref stays");
}

#[test]
fn a_key_a_warm_up_holds_is_kept() {
    let case = prune_case("prune_pc_warm_held");
    let (record, _, files) = seeded(&case);
    let held = held_file(&v1::warm_lock_path(&case.root(), KEY), FILE_SHARE_READ | FILE_SHARE_WRITE);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.ends_with(&format!("gate: prune: kept {KEY}: a warm-up holds it\n")),
        "{outcome:?}"
    );
    assert!(all_there(&files.shared), "the shared files stay");
    assert!(record.exists(), "the record stays");
    assert_eq!(refs_in(&case, "repo"), key_refs(), "every ref stays");
}

#[test]
fn a_sync_back_ref_younger_than_seven_days_is_kept_with_the_record() {
    let case = prune_case("prune_pc_young_back");
    let (record, _, files) = seeded(&case);
    let back = format!("refs/gate-remote/{KEY}-back");
    git(&case, &["-C", "repo", "update-ref", &back, "HEAD"]);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .ends_with(&format!("gate: prune: kept {KEY}'s sync-back ref {back}: it is less than 7 days old\n")),
        "{outcome:?}"
    );
    assert!(none_there(&files.shared), "the shared files are gone");
    assert!(record.exists(), "the record stays for a later sweep");
    assert_eq!(
        refs_in(&case, "repo"),
        [back.as_str(), "refs/gate-remote/other-1a2b3c4d"],
        "the young sync-back ref stays, the key's others go"
    );
}

#[test]
fn a_file_that_cannot_be_removed_keeps_the_record_and_fails() {
    let case = prune_case("prune_pc_cannot_remove");
    let (record, _, _) = seeded(&case);
    let pending = v1::outputs_pending_path(&case.root(), KEY);
    let held = held_file(&pending, FILE_SHARE_READ | FILE_SHARE_WRITE);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    drop(held);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let said = format!("gate: prune: could not remove {}: ", shown(&pending));
    assert!(outcome.stderr.contains(&said), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: prune: removed"), "{outcome:?}");
    assert!(record.exists(), "the record stays for the next sweep");
    assert!(pending.exists(), "the held file stays");
}

#[test]
fn a_sibling_repo_s_refs_for_the_key_are_removed() {
    let case = prune_case("prune_pc_sibling");
    write_remote_json(
        &case,
        r#"{"host": "builder-1", "hosts": ["builder-2"], "repos": {"app": ["*"]}, "siblings": {"app": ["lib", "absent"]}}"#,
    );
    git(&case, &["init", "--quiet", "repo"]);
    git(&case, &["-C", "repo", "commit", "--allow-empty", "--quiet", "-m", "first"]);
    git(&case, &["-C", "repo", "worktree", "add", "--quiet", "../repo.wt/wt"]);
    let root = case.folder().join(r"repo.wt\wt");
    fs::remove_dir_all(&root).expect("the worktree's folder can be deleted");
    let record = write_record(&case, &shown(&root), &shown(&case.folder().join(r"repo\.git")), Some(LONG_GONE));
    for repo in [r"repo.wt\lib", "lib"] {
        git(&case, &["init", "--quiet", repo]);
        git(&case, &["-C", repo, "commit", "--allow-empty", "--quiet", "-m", "first"]);
        leave_refs(&case, repo);
    }
    leave_refs(&case, "repo");
    work_folder(&case, "builder-1");
    work_folder(&case, "builder-2");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        refs_in(&case, r"repo.wt\lib"),
        ["refs/gate-remote/other-1a2b3c4d"],
        "the sibling beside the worktree keeps only another key's ref"
    );
    assert_eq!(refs_in(&case, "repo"), ["refs/gate-remote/other-1a2b3c4d"], "so does the worktree's repo");
    assert_eq!(
        refs_in(&case, "lib"),
        key_refs(),
        "a folder beside the main checkout is no sibling: every ref stays"
    );
    assert!(!record.exists(), "the record is gone: {outcome:?}");
}

#[test]
fn no_worktree_records_says_so_and_passes() {
    let case = prune_case("prune_no_records");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "gate: prune: no worktree records\n", "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
}

#[test]
fn a_quiet_sweep_with_no_worktree_records_says_nothing_and_passes() {
    let case = prune_case("prune_quiet_no_records");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &["--quiet"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
}

#[test]
fn a_live_worktree_s_record_is_left_alone_and_no_builder_is_called() {
    let case = prune_case("prune_live");
    let (root, common) = worktree(&case);
    let path = write_record(&case, &root, &common, None);
    let before = fs::read(&path).expect("the record can be read");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
    assert_eq!(fs::read(&path).expect("the record can be read"), before, "the record is not written");
}

#[test]
fn a_gone_worktree_is_marked_on_its_first_sweep_and_no_builder_is_called() {
    let case = prune_case("prune_first_sweep");
    let (root, common) = gone_worktree(&case);
    let path = write_record(&case, &root, &common, None);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("gate: prune: {KEY}: {root} is gone; its builders are pruned on a sweep an hour from now\n"),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
    let record = record_of(&path);
    assert!(record["goneSince"].is_string(), "the record is marked gone: {record}");
    assert_eq!(record["root"], json!(root), "{record}");
}

#[test]
fn a_worktree_found_again_has_its_gone_mark_cleared() {
    let case = prune_case("prune_found_again");
    let (root, common) = worktree(&case);
    let path = write_record(&case, &root, &common, Some(LONG_GONE));
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
    let record = record_of(&path);
    assert_eq!(record.get("goneSince"), None, "the mark is cleared: {record}");
    assert_eq!(record["root"], json!(root), "{record}");
    assert_eq!(record["commonDir"], json!(common), "{record}");
}

#[test]
fn a_gone_worktree_past_the_age_is_pruned_on_every_builder_and_said_with_each_alias() {
    let case = prune_case("prune_past_age");
    let (root, common) = gone_worktree(&case);
    write_record(&case, &root, &common, None);
    let (one, two) = (work_folder(&case, "builder-1"), work_folder(&case, "builder-2"));
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let first = pruned(&case, &steps, &[]);
    let second = pruned(&case, &steps, &["--test-prune-gone-age-ms", "0"]);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(
        second.stderr,
        format!(
            "gate: remote builder-1 pruned {KEY} (worktree gone)\ngate: remote builder-2 pruned {KEY} (worktree gone)\n\
             gate: prune: removed {KEY} ({root})\n"
        ),
        "{second:?}"
    );
    assert_eq!(calls(&steps), ["prune", "prune"], "the second sweep alone calls each builder once");
    assert!(!one.exists() && !two.exists(), "both builders' work folders are gone: {second:?}");
}

#[test]
fn a_record_whose_common_folder_is_gone_reads_as_gone() {
    let case = prune_case("prune_common_gone");
    let root = shown(&case.folder().join("gone-wt"));
    let path = write_record(&case, &root, &shown(&case.folder().join(r"gone-repo\.git")), None);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("gate: prune: {KEY}: {root} is gone; its builders are pruned on a sweep an hour from now\n"),
        "{outcome:?}"
    );
    assert!(record_of(&path)["goneSince"].is_string(), "the record is marked gone");
}

#[test]
fn a_worktree_on_a_missing_drive_is_kept_as_unknown() {
    let case = prune_case("prune_missing_drive");
    let letter = ('D'..='Z')
        .rev()
        .find(|letter| !Path::new(&format!(r"{letter}:\")).exists())
        .expect("a drive letter is unused");
    let root = format!(r"{letter}:\dev\app");
    let path = write_record(&case, &root, &format!(r"{letter}:\dev\app\.git"), None);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("gate: prune: kept {KEY}: cannot tell whether {root} is gone: the drive {letter}:\\ is not there\n"),
        "{outcome:?}"
    );
    assert_eq!(record_of(&path).get("goneSince"), None, "the record is not marked");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
}

#[test]
fn an_unreachable_builder_fails_the_sweep_and_the_other_is_still_pruned() {
    let case = prune_case("prune_unreachable");
    let (one, two) = long_gone(&case);
    let steps = ssh_steps(&case, "ssh", &["on builder-1", "fail-host", "on builder-2", "ok"]);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "gate: remote builder-1 could not be reached: ssh: connect to host builder-1 port 22: Connection refused\n\
             gate: remote builder-2 pruned {KEY} (worktree gone)\n"
        ),
        "{outcome:?}"
    );
    assert!(one.exists(), "the unreached builder keeps its work folder");
    assert!(!two.exists(), "the other's is gone");
}

#[test]
fn a_box_that_refuses_the_verb_keeps_the_key_without_failing() {
    let case = prune_case("prune_old_box");
    let (one, two) = long_gone(&case);
    let steps = ssh_steps(&case, "ssh", &[OLD_BOX, OLD_BOX]);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "gate: remote builder-1 kept {KEY}: its box cannot prune by key\n\
             gate: remote builder-2 kept {KEY}: its box cannot prune by key\n"
        ),
        "{outcome:?}"
    );
    assert!(one.exists() && two.exists(), "both work folders stay");
}

#[test]
fn a_key_the_box_says_nothing_of_is_kept_and_fails() {
    let case = prune_case("prune_said_nothing");
    long_gone(&case);
    let steps = ssh_steps(&case, "ssh", &["exit 0", "exit 0"]);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "gate: remote builder-1 kept {KEY}: its box said nothing of it\n\
             gate: remote builder-2 kept {KEY}: its box said nothing of it\n"
        ),
        "{outcome:?}"
    );
}

#[test]
fn a_held_work_lock_keeps_the_key_without_failing() {
    let case = prune_case("prune_held_lock");
    let (one, two) = long_gone(&case);
    let held = hold_lock(&case, "builder-1");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("gate: remote builder-1 kept {KEY}: in use\ngate: remote builder-2 pruned {KEY} (worktree gone)\n"),
        "{outcome:?}"
    );
    assert!(one.exists(), "the held key's work folder stays");
    assert!(!two.exists(), "the other builder's is gone");
}

#[test]
fn a_record_whose_stem_the_box_would_read_as_a_flag_is_skipped() {
    let case = prune_case("prune_flag_stem");
    let (root, common) = gone_worktree(&case);
    let written = write_record(&case, &root, &common, Some(LONG_GONE));
    let path = written.with_file_name(format!("-{KEY}.worktree"));
    fs::rename(&written, &path).expect("the record can be renamed");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("gate: prune: skipped {}: it cannot be read as a worktree record\n", shown(&path)),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
}

#[test]
fn a_git_error_is_unknown_and_no_builder_is_called() {
    let case = prune_case("prune_git_error");
    let root = shown(&case.folder().join("gone-wt"));
    let common = case.folder().join("not-git");
    fs::create_dir_all(&common).expect("the folder can be made");
    write_record(&case, &root, &shown(&common), Some(LONG_GONE));
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let unknown = format!("gate: prune: kept {KEY}: cannot tell whether {root} is gone: git worktree list failed: ");
    assert!(outcome.stderr.starts_with(&unknown), "{outcome:?}");
    assert_eq!(outcome.stderr.lines().count(), 1, "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
}

#[test]
fn a_gone_root_git_still_lists_is_unknown_and_no_builder_is_called() {
    let case = prune_case("prune_still_listed");
    let (root, common) = worktree(&case);
    git(&case, &["-C", "repo", "worktree", "lock", "../wt"]);
    fs::remove_dir_all(&root).expect("the worktree's folder can be deleted");
    write_record(&case, &root, &common, Some(LONG_GONE));
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned(&case, &steps, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("gate: prune: kept {KEY}: cannot tell whether {root} is gone: git still lists it as a worktree\n"),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder is called");
}

#[test]
fn a_builder_root_with_a_space_is_refused_and_fails_the_sweep() {
    let case = prune_case("prune_root_space");
    let (_, two) = long_gone(&case);
    let spaced = case.folder().join("builder 1");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = pruned_under(&case, &steps, &spaced, &[]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "gate: remote builder-1 cannot prune: its root {} has a space\ngate: remote builder-2 pruned {KEY} (worktree gone)\n",
            shown(&spaced)
        ),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), ["prune"], "the builder with the space is not called");
    assert!(!two.exists(), "the other builder's work folder is gone");
}
