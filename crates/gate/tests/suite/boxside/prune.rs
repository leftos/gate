//! `gate box prune --root <folder>`, the sweep a builder's scheduled task runs: a work folder no run has used for over a
//! day goes with its records, its lock left in place; one a run holds, one a run fetched into lately, one that is a link
//! and a name that is no key all stay. Each case builds its builder root by hand under the case's folder, with no git.

use std::fs::{self, File};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE};

use super::{bare_box_command, hold_lock};
use crate::support::{Case, Outcome, finish, junction, shown};

/// How long ago an idle work folder and its records were last written: two days, well past the sweep's one.
const AGED: Duration = Duration::from_hours(48);
/// The records beside a work folder `k`, as `k.<record>`.
const RECORDS: [&str; 8] = ["lock", "warm", "layout", "inputs", "tools", "packages", "index", "command"];
/// The summary line of a sweep that removed nothing and saw no work folder.
const NOTHING: &str = "gate: remote prune removed 0 of 0 work folders\n";

/// `gate box prune --root <root>`, run to its end.
fn prune(case: &Case, root: &Path) -> Outcome {
    finish(bare_box_command(case, &["box", "prune", "--root", &shown(root)]))
}

/// Sets the last write of the file or folder at `path` to `age` ago.
fn age(path: &Path, age: Duration) {
    File::options()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .and_then(|opened| opened.set_modified(SystemTime::now() - age))
        .expect("the last write can be set");
}

/// A builder root under the case's folder holding the work folder `k`, its `.git\FETCH_HEAD`, and every record beside
/// it, all just written.
fn root_with_k(case: &Case) -> PathBuf {
    let root = case.folder().join("remote");
    let folder = root.join(r"work\k");
    fs::create_dir_all(folder.join(".git")).expect("the work folder can be made");
    fs::write(folder.join(r".git\FETCH_HEAD"), "").expect("the fetch stamp can be written");
    for record in RECORDS {
        fs::write(root.join(format!(r"work\k.{record}")), "").expect("a record can be written");
    }
    root
}

/// Ages the work folder `k` under `root`, its `.git\FETCH_HEAD` and every record by [`AGED`], the folder last, since a
/// change inside it moves its own last write.
fn age_k(root: &Path) {
    let work = root.join("work");
    age(&work.join(r"k\.git\FETCH_HEAD"), AGED);
    for record in RECORDS {
        age(&work.join(format!("k.{record}")), AGED);
    }
    age(&work.join("k"), AGED);
}

/// Asserts the work folder `k` under `root` and every record but its lock are gone, its lock and an empty `pruned`
/// folder left.
fn assert_k_gone(root: &Path, outcome: &Outcome) {
    let work = root.join("work");
    assert!(!work.join("k").exists(), "the work folder is gone: {outcome:?}");
    for record in &RECORDS[1..] {
        assert!(!work.join(format!("k.{record}")).exists(), "k.{record} is gone: {outcome:?}");
    }
    assert!(work.join("k.lock").is_file(), "the lock stays: {outcome:?}");
    let left = fs::read_dir(root.join("pruned")).expect("the pruned folder is there").count();
    assert_eq!(left, 0, "nothing is left in the pruned folder: {outcome:?}");
}

/// Asserts the work folder `k` under `root` and every record are all still there.
fn assert_k_kept(root: &Path, outcome: &Outcome) {
    let work = root.join("work");
    assert!(work.join(r"k\.git\FETCH_HEAD").is_file(), "the work folder stays: {outcome:?}");
    for record in RECORDS {
        assert!(work.join(format!("k.{record}")).is_file(), "k.{record} stays: {outcome:?}");
    }
}

#[test]
fn an_idle_work_folder_and_its_records_are_removed() {
    let case = Case::new("prune_idle");
    let root = root_with_k(&case);
    age_k(&root);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "gate: remote pruned k: idle 48 h\ngate: remote prune removed 1 of 1 work folders\n",
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_k_gone(&root, &outcome);
}

#[test]
fn a_work_folder_whose_fetch_head_is_fresh_stays() {
    let case = Case::new("prune_fresh_fetch");
    let root = root_with_k(&case);
    age_k(&root);
    age(&root.join(r"work\k\.git\FETCH_HEAD"), Duration::ZERO);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: remote prune removed 0 of 1 work folders\n", "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_k_kept(&root, &outcome);
}

#[test]
fn a_pair_work_folder_whose_tree_fetched_recently_stays() {
    let case = Case::new("prune_pair_fresh_fetch");
    let root = root_with_k(&case);
    let leaf_git = root.join(r"work\k\leaf\.git");
    fs::create_dir_all(&leaf_git).expect("the pair's tree can be made");
    fs::write(leaf_git.join("FETCH_HEAD"), "").expect("the tree's fetch stamp can be written");
    age_k(&root);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: remote prune removed 0 of 1 work folders\n", "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_k_kept(&root, &outcome);
}

#[test]
fn a_work_folder_whose_lock_a_run_holds_stays() {
    let case = Case::new("prune_held");
    let root = root_with_k(&case);
    age_k(&root);
    let held = hold_lock(&root.join("work"), "k.lock");

    let outcome = prune(&case, &root);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: remote prune removed 0 of 1 work folders\n", "{outcome:?}");
    assert_eq!(outcome.stderr, "", "a held lock is no failure: {outcome:?}");
    assert_k_kept(&root, &outcome);
}

#[test]
fn a_junction_inside_an_idle_folder_goes_without_its_target() {
    let case = Case::new("prune_inner_junction");
    let root = root_with_k(&case);
    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    fs::write(outside.join("file"), "kept").expect("the outside file can be written");
    junction(&root.join(r"work\k\linked"), &outside);
    age_k(&root);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "gate: remote pruned k: idle 48 h\ngate: remote prune removed 1 of 1 work folders\n",
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_k_gone(&root, &outcome);
    assert_eq!(
        fs::read_to_string(outside.join("file")).expect("the junction's target is left"),
        "kept",
        "{outcome:?}"
    );
}

#[test]
fn a_work_entry_that_is_a_junction_is_left_and_named() {
    let case = Case::new("prune_work_junction");
    let root = case.folder().join("remote");
    fs::create_dir_all(root.join("work")).expect("the work folder can be made");
    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    fs::write(outside.join("file"), "kept").expect("the outside file can be written");
    let link = root.join(r"work\k");
    junction(&link, &outside);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, NOTHING, "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate: remote could not prune k: its work folder is not a plain folder\n",
        "{outcome:?}"
    );
    let kind = fs::symlink_metadata(&link).expect("the junction stays").file_type();
    assert!(kind.is_symlink(), "the junction is left as it is: {outcome:?}");
    assert_eq!(
        fs::read_to_string(outside.join("file")).expect("the junction's target is left"),
        "kept",
        "{outcome:?}"
    );
}

#[test]
fn a_leftover_pruned_folder_goes_at_the_next_sweep() {
    let case = Case::new("prune_leftover");
    let root = case.folder().join("remote");
    let leftover = root.join(r"pruned\old-1");
    fs::create_dir_all(&leftover).expect("a leftover can be made");
    fs::write(leftover.join("file"), "").expect("a leftover's file can be written");

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, NOTHING, "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(!leftover.exists(), "the leftover is removed: {outcome:?}");
}

#[test]
fn the_records_of_a_key_without_a_folder_are_removed() {
    let case = Case::new("prune_records_alone");
    let root = case.folder().join("remote");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    fs::write(work.join("k.layout"), "pair x").expect("a layout record can be written");
    fs::write(work.join("k.tools"), "").expect("a tools record can be written");

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, NOTHING, "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(!work.join("k.layout").exists(), "the layout record is gone: {outcome:?}");
    assert!(!work.join("k.tools").exists(), "the tools record is gone: {outcome:?}");
}

#[test]
fn a_missing_work_folder_prunes_nothing() {
    let case = Case::new("prune_no_work");
    let root = case.folder().join("remote");
    fs::create_dir_all(&root).expect("a root without a work folder can be made");
    let nowhere = case.folder().join("nowhere");

    for (what, at) in [("a root without a work folder", &root), ("a root that is not there", &nowhere)] {
        let outcome = prune(&case, at);

        assert_eq!(outcome.code, Some(0), "{what}: {outcome:?}");
        assert_eq!(outcome.stdout, NOTHING, "{what}: {outcome:?}");
        assert_eq!(outcome.stderr, "", "{what}: {outcome:?}");
    }
}

#[test]
fn a_name_that_is_not_a_key_is_left_alone() {
    let case = Case::new("prune_not_a_key");
    let root = case.folder().join("remote");
    let folder = root.join(r"work\not a key");
    fs::create_dir_all(&folder).expect("a folder that is no key can be made");
    age(&folder, AGED);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, NOTHING, "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(folder.is_dir(), "the folder stays: {outcome:?}");
    assert!(!root.join(r"work\not a key.lock").exists(), "no lock is made for it: {outcome:?}");
}

/// A folder `outside` beside the case's builder root holding `file`, reading `kept`, for a junction to lead to.
fn outside_folder(case: &Case) -> PathBuf {
    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    fs::write(outside.join("file"), "kept").expect("the outside file can be written");
    outside
}

/// Asserts the file `file` in the folder `outside` still reads `kept`.
fn assert_outside_kept(outside: &Path, outcome: &Outcome) {
    assert_eq!(
        fs::read_to_string(outside.join("file")).expect("the junction's target is left"),
        "kept",
        "{outcome:?}"
    );
}

#[test]
fn a_pruned_folder_that_is_a_junction_is_refused() {
    let case = Case::new("prune_pruned_junction");
    let root = case.folder().join("remote");
    fs::create_dir_all(root.join("work")).expect("the work folder can be made");
    let outside = outside_folder(&case);
    junction(&root.join("pruned"), &outside);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    let pruned = shown(&root.join("pruned"));
    assert_eq!(
        outcome.stderr,
        format!("gate: remote could not prune: {pruned} is not a plain folder\n"),
        "{outcome:?}"
    );
    assert_outside_kept(&outside, &outcome);
}

#[test]
fn a_work_folder_that_is_a_junction_is_refused_and_its_target_left() {
    let case = Case::new("prune_work_is_junction");
    let elsewhere = root_with_k(&case);
    age_k(&elsewhere);
    let root = case.folder().join("linked");
    fs::create_dir_all(&root).expect("the builder root can be made");
    junction(&root.join("work"), &elsewhere.join("work"));

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    let work = shown(&root.join("work"));
    assert_eq!(
        outcome.stderr,
        format!("gate: remote could not prune: {work} is not a plain folder\n"),
        "{outcome:?}"
    );
    assert_k_kept(&elsewhere, &outcome);
}

#[test]
fn an_idle_folder_whose_git_is_a_junction_to_a_fresh_fetch_goes() {
    let case = Case::new("prune_git_junction");
    let root = root_with_k(&case);
    let git = root.join(r"work\k\.git");
    fs::remove_file(git.join("FETCH_HEAD")).expect("the fetch stamp can be removed");
    fs::remove_dir(&git).expect("the .git folder can be removed");
    let outside = outside_folder(&case);
    fs::write(outside.join("FETCH_HEAD"), "").expect("the outside fetch stamp can be written");
    junction(&git, &outside);
    age_k(&root);
    age(&outside.join("FETCH_HEAD"), Duration::ZERO);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "gate: remote pruned k: idle 48 h\ngate: remote prune removed 1 of 1 work folders\n",
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_k_gone(&root, &outcome);
    assert_outside_kept(&outside, &outcome);
    assert!(outside.join("FETCH_HEAD").is_file(), "the outside fetch stamp is left: {outcome:?}");
}

#[test]
fn a_key_whose_lock_is_a_link_stays_and_is_named() {
    let case = Case::new("prune_lock_junction");
    let root = root_with_k(&case);
    let lock = root.join(r"work\k.lock");
    fs::remove_file(&lock).expect("the lock can be removed");
    let outside = outside_folder(&case);
    junction(&lock, &outside);
    age_k(&root);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: remote prune removed 0 of 1 work folders\n", "{outcome:?}");
    assert!(
        outcome.stderr.starts_with("gate: remote could not open the work folder lock ") && outcome.stderr.lines().count() == 1,
        "{outcome:?}"
    );
    let work = root.join("work");
    assert!(work.join(r"k\.git\FETCH_HEAD").is_file(), "the work folder stays: {outcome:?}");
    for record in &RECORDS[1..] {
        assert!(work.join(format!("k.{record}")).is_file(), "k.{record} stays: {outcome:?}");
    }
    assert!(
        fs::symlink_metadata(&lock).expect("the link stays").file_type().is_symlink(),
        "{outcome:?}"
    );
    assert_outside_kept(&outside, &outcome);
}

#[test]
fn a_fresh_record_keeps_an_idle_folder() {
    let case = Case::new("prune_fresh_record");
    let root = root_with_k(&case);
    age_k(&root);
    age(&root.join(r"work\k.warm"), Duration::ZERO);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: remote prune removed 0 of 1 work folders\n", "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_k_kept(&root, &outcome);
}

#[test]
fn a_failed_rename_leaves_the_folder_marked_changing() {
    let case = Case::new("prune_rename_fails");
    let root = root_with_k(&case);
    let held_path = root.join(r"work\k\held");
    fs::write(&held_path, "").expect("a file in the work folder can be written");
    age_k(&root);
    // A file held open with no sharing keeps its folder from being renamed.
    let held = File::options().read(true).share_mode(0).open(&held_path).expect("the file can be held");

    let outcome = prune(&case, &root);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: remote prune removed 0 of 1 work folders\n", "{outcome:?}");
    assert!(
        outcome.stderr.starts_with("gate: remote could not prune k: ") && outcome.stderr.lines().count() == 1,
        "{outcome:?}"
    );
    let work = root.join("work");
    assert!(work.join(r"k\.git\FETCH_HEAD").is_file(), "the work folder stays: {outcome:?}");
    assert_eq!(
        fs::read_to_string(work.join("k.layout")).expect("the layout record stays"),
        "changing",
        "the next run clears the folder: {outcome:?}"
    );
    assert!(work.join("k.index").is_file(), "the index record stays: {outcome:?}");
}

/// The work folder `k` under `root` held open sharing reading and writing but not deletion, as a process whose working
/// folder it is holds it; held until the caller drops it.
fn hold_folder_k(root: &Path) -> File {
    File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(root.join(r"work\k"))
        .expect("the work folder can be held")
}

#[test]
fn a_folder_not_idle_held_without_delete_sharing_stays_without_a_line() {
    let case = Case::new("prune_held_folder");
    let root = root_with_k(&case);
    let held = hold_folder_k(&root);

    let outcome = prune(&case, &root);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: remote prune removed 0 of 1 work folders\n", "{outcome:?}");
    assert_eq!(outcome.stderr, "", "a folder in use is no failure: {outcome:?}");
    assert_k_kept(&root, &outcome);
}

#[test]
fn an_idle_folder_held_without_delete_sharing_stays_with_its_line() {
    let case = Case::new("prune_held_idle_folder");
    let root = root_with_k(&case);
    age_k(&root);
    let held = hold_folder_k(&root);

    let outcome = prune(&case, &root);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: remote prune removed 0 of 1 work folders\n", "{outcome:?}");
    assert!(
        outcome.stderr.starts_with("gate: remote could not prune k: ") && outcome.stderr.lines().count() == 1,
        "a process lingering in an idle folder is named: {outcome:?}"
    );
    assert_k_kept(&root, &outcome);
}

#[test]
fn a_record_that_is_a_junction_goes_without_its_target() {
    let case = Case::new("prune_record_junction");
    let root = case.folder().join("remote");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    let outside = outside_folder(&case);
    junction(&work.join("k.warm"), &outside);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, NOTHING, "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(
        fs::symlink_metadata(work.join("k.warm")).is_err(),
        "the record's name is gone: {outcome:?}"
    );
    assert_outside_kept(&outside, &outcome);
}

#[test]
fn a_junction_left_in_pruned_goes_without_its_target() {
    let case = Case::new("prune_leftover_junction");
    let root = case.folder().join("remote");
    fs::create_dir_all(root.join("pruned")).expect("the pruned folder can be made");
    let outside = outside_folder(&case);
    let left = root.join(r"pruned\k-1-1");
    junction(&left, &outside);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, NOTHING, "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(fs::symlink_metadata(&left).is_err(), "the junction is removed: {outcome:?}");
    assert_outside_kept(&outside, &outcome);
}

#[test]
fn an_idle_folder_without_a_lock_goes_in_the_first_sweep() {
    let case = Case::new("prune_no_lock");
    let root = root_with_k(&case);
    age_k(&root);
    fs::remove_file(root.join(r"work\k.lock")).expect("the lock can be removed");
    age(&root.join(r"work\k"), AGED);

    let outcome = prune(&case, &root);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "gate: remote pruned k: idle 48 h\ngate: remote prune removed 1 of 1 work folders\n",
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_k_gone(&root, &outcome);
}

#[test]
fn a_relative_root_is_refused() {
    let case = Case::new("prune_relative_root");

    let outcome = finish(bare_box_command(&case, &["box", "prune", "--root", "work"]));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate: box prune --root is not an absolute path on a drive: \"work\"\n",
        "{outcome:?}"
    );
}
