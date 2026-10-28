//! The PC's part of `gate prune --builders`: what the PC keeps of a gone worktree's key, removed once the builders said
//! what they did with it, under the key's mutex and only while the record still says what the sweep judged. Each
//! builder's own files go once that builder pruned the key; the files every builder shares, the key's refs in the
//! record's repo and its siblings, and last the worktree record, go only once every builder did and no warm-up holds the
//! key. A sync-back ref younger than [`BACK_KEPT`] is kept, and the record with it.

use std::fs;
use std::io::ErrorKind;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use gate_router::{Config, escaped, sibling_root};
use gate_state::v1;
use jiff::Timestamp;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

use crate::console::Console;
use crate::prune_cmd::{GitAt, GitFault, Outcome, Outcomes, Scanned, git_run};
use crate::remote::git::Git;
use crate::resolve::ResolvedRoot;
use crate::worktree_record;

/// The folder of the refs a routed run leaves in a repo for the builders' snapshots and the kept sync-back commits.
const REFS: &str = "refs/gate-remote/";
/// What the name of a kept sync-back ref ends with, after its key.
const BACK: &str = "-back";
/// How long a kept sync-back commit, which the user may still want, is kept from its committer date: 7 days.
const BACK_KEPT: i64 = 7 * 24 * 60 * 60;
/// The error a file opened by another without delete sharing gives a removal.
const SHARING_VIOLATION: i32 = 32;

/// Where the removal runs: the state root, the routing file that names each repo's siblings, and the git it runs.
pub(super) struct Removal<'a> {
    pub(super) state: &'a ResolvedRoot,
    pub(super) config: &'a Config,
    pub(super) git: &'a Git,
}

/// How far the removal of a key got, the worse of two being the greater.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Removed {
    /// Everything it was to remove is gone.
    Whole,
    /// Something is kept for a later sweep, and that is no failure.
    Kept,
    /// Something could not be removed: a failure, said.
    Failed,
}

/// Removes what the PC keeps of the key of each of `records` as `outcomes` says each builder did with it, saying each
/// key removed whole, each kept and each failure; whether anything failed.
pub(super) fn removed(removal: &Removal<'_>, records: &[&Scanned], outcomes: &Outcomes, console: &mut Console) -> bool {
    let mut failed = false;
    for scanned in records {
        if let Some(builders) = outcomes.get(&scanned.key) {
            failed |= key_removed(removal, scanned, builders, console) == Removed::Failed;
        }
    }
    failed
}

/// Removes what the PC keeps of the key of `scanned`, which `builders` answered for, when any of them pruned it: under
/// the key's mutex taken without waiting, and only while the record on disk is still the one the sweep judged, which
/// it leaves for the next sweep, silently, when it changed or cannot be read. A builder's own files that cannot be
/// removed keep the shared ones and the record too, so the next sweep tries the key again.
fn key_removed(removal: &Removal<'_>, scanned: &Scanned, builders: &[(Option<String>, Outcome)], console: &mut Console) -> Removed {
    let (state, key) = (&removal.state.path, &scanned.key);
    if !builders.iter().any(|(_, outcome)| *outcome == Outcome::Removed) {
        return Removed::Kept;
    }
    let _key = match worktree_record::try_hold_key(&removal.state.root, key) {
        Ok(Some(held)) => held,
        Ok(None) => {
            console.err(&format!("gate: prune: kept {key}: in use"));
            return Removed::Kept;
        }
        Err(line) => {
            console.err(&format!("gate: prune: kept {key}: {line}"));
            return Removed::Failed;
        }
    };
    if worktree_record::read_worktree_record(&v1::worktree_record_path(state, key)).as_ref() != Some(&scanned.record) {
        return Removed::Kept;
    }
    if !builders_removed(state, key, builders, console) {
        return Removed::Failed;
    }
    if !every_removed(builders) {
        return Removed::Kept;
    }
    shared_removed(removal, scanned, console)
}

/// Removes, under the state root `state`, the own files of `key` of each of `builders` that pruned it, saying each
/// that cannot be removed; whether every one is gone.
fn builders_removed(state: &Path, key: &str, builders: &[(Option<String>, Outcome)], console: &mut Console) -> bool {
    let mut removed = true;
    for (id, _) in builders.iter().filter(|(_, outcome)| *outcome == Outcome::Removed) {
        for path in own_files(state, id.as_deref(), key) {
            removed &= file_removed(&path, console);
        }
    }
    removed
}

/// Whether every one of `builders` pruned the key: none held it or failed, and at least one was called.
fn every_removed(builders: &[(Option<String>, Outcome)]) -> bool {
    !builders.is_empty() && builders.iter().all(|(_, outcome)| *outcome == Outcome::Removed)
}

/// The files of `key` the builder whose id is `id` has of its own under the state root `state`.
fn own_files(state: &Path, id: Option<&str>, key: &str) -> [PathBuf; 5] {
    [
        v1::outputs_hashes_path(state, id, key),
        v1::inputs_hashes_path(state, id, key),
        v1::inputs_carried_path(state, id, key),
        v1::sent_build_hashes_path(state, id, key),
        v1::outputs_capped_path(state, id, key),
    ]
}

/// The files of `key` every builder shares under the state root `state`, the warm-up lock and the worktree record aside.
fn shared_files(state: &Path, key: &str) -> [PathBuf; 5] {
    [
        v1::outputs_pending_path(state, key),
        v1::nothing_back_path(state, key),
        v1::warm_record_path(state, key),
        v1::warm_log_path(state, key),
        v1::outputs_from_path(state, key),
    ]
}

/// Removes the file at `path`, one that is not there being removed; else says why it cannot be, and is false.
fn file_removed(path: &Path, console: &mut Console) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(error) => {
            console.err(&format!("gate: prune: could not remove {}: {error}", path.display()));
            false
        }
    }
}

/// Removes the warm-up lock of the key of `scanned` first, then the shared files and the refs, and last the record,
/// saying the key removed once it is gone.
fn shared_removed(removal: &Removal<'_>, scanned: &Scanned, console: &mut Console) -> Removed {
    let (state, key) = (&removal.state.path, &scanned.key);
    let lock = warm_lock_removed(state, key, console);
    if lock != Removed::Whole {
        return lock;
    }
    let mut removed = Removed::Whole;
    for path in shared_files(state, key) {
        if !file_removed(&path, console) {
            removed = Removed::Failed;
        }
    }
    removed = removed.max(refs_removed(removal, scanned, console));
    if removed != Removed::Whole {
        return removed;
    }
    if !file_removed(&v1::worktree_record_path(state, key), console) {
        return Removed::Failed;
    }
    console.err(&format!("gate: prune: removed {key} ({})", escaped(&scanned.record.root)));
    Removed::Whole
}

/// Removes the warm-up lock of `key` under the state root `state`: kept, said, when a warm-up holds it open, since `gate
/// warm` takes it without the key's mutex; failed, said, when it cannot be removed otherwise.
fn warm_lock_removed(state: &Path, key: &str, console: &mut Console) -> Removed {
    let lock = v1::warm_lock_path(state, key);
    match fs::remove_file(&lock) {
        Ok(()) => Removed::Whole,
        Err(error) if error.kind() == ErrorKind::NotFound => Removed::Whole,
        Err(error) if error.raw_os_error() == Some(SHARING_VIOLATION) => {
            console.err(&format!("gate: prune: kept {key}: a warm-up holds it"));
            Removed::Kept
        }
        Err(error) => {
            console.err(&format!("gate: prune: could not remove {}: {error}", lock.display()));
            Removed::Failed
        }
    }
}

/// Deletes the refs of the key of `scanned` in the record's repo, when its common folder is there, and in each sibling
/// repo the routing file allows that repo, laid beside the worktree root as routing lays it ([`sibling_root`]) and
/// skipped when it is no repo folder ([`repo_folder`]).
fn refs_removed(removal: &Removal<'_>, scanned: &Scanned, console: &mut Console) -> Removed {
    let (record, key) = (&scanned.record, &scanned.key);
    let common = Path::new(&record.common_dir);
    let mut removed = Removed::Whole;
    if !fs::symlink_metadata(common).is_err_and(|error| error.kind() == ErrorKind::NotFound) {
        let git_dir = format!("--git-dir={}", record.common_dir);
        let at = GitAt { folder: common, set: &[] };
        let shown = common.parent().unwrap_or(common);
        removed = removed.max(repo_refs_removed(removal.git, &at, &[&git_dir], shown, key, console));
    }
    let names = match removal.config.allowed_siblings(&record.repo) {
        Ok(names) => names,
        Err(problem) => {
            console.err(&format!("gate: prune: kept {key}'s refs in its siblings: {}", escaped(problem)));
            return Removed::Failed;
        }
    };
    for name in names {
        let folder = PathBuf::from(sibling_root(&record.root, name));
        let Some(parent) = folder.parent().filter(|_| repo_folder(&folder)) else {
            continue;
        };
        let set = [("GIT_CEILING_DIRECTORIES", parent.as_os_str())];
        let at = GitAt { folder: &folder, set: &set };
        removed = removed.max(repo_refs_removed(removal.git, &at, &[], &folder, key, console));
    }
    removed
}

/// Whether `folder` is a sibling repo the refs are deleted in: a folder, not a link or junction, holding a `.git`.
fn repo_folder(folder: &Path) -> bool {
    let plain =
        fs::symlink_metadata(folder).is_ok_and(|metadata| metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0);
    plain && fs::symlink_metadata(folder.join(".git")).is_ok()
}

/// Deletes the refs of `key` ([`key_ref`]) in the repo git finds at `at` given the words `head` first, the repo folder
/// `folder`: one listing, then one `update-ref` of every ref it names but a sync-back ref younger than [`BACK_KEPT`],
/// which is kept, said. Failed, said, when git fails.
fn repo_refs_removed(git: &Git, at: &GitAt<'_>, head: &[&str], folder: &Path, key: &str, console: &mut Console) -> Removed {
    let kept = |fault: GitFault, console: &mut Console| {
        let (GitFault::Said(why) | GitFault::Ended(why)) = fault;
        console.err(&format!("gate: prune: kept {key}'s refs in {}: {why}", folder.display()));
        Removed::Failed
    };
    let listing = [head, &["for-each-ref", "--format=%(refname) %(committerdate:unix)", REFS][..]].concat();
    let refs = match git_run(git, "git for-each-ref", at, &listing, None) {
        Ok(refs) => refs,
        Err(fault) => return kept(fault, console),
    };
    let now = Timestamp::now().as_second();
    let mut removed = Removed::Whole;
    let mut deletes = String::new();
    for (name, date) in refs.lines().filter_map(|line| line.split_once(' ')) {
        if !key_ref(name, key) {
            continue;
        }
        if name.ends_with(BACK) && young(date, now) {
            console.err(&format!("gate: prune: kept {key}'s sync-back ref {name}: it is less than 7 days old"));
            removed = Removed::Kept;
            continue;
        }
        deletes.push_str("delete ");
        deletes.push_str(name);
        deletes.push('\n');
    }
    if deletes.is_empty() {
        return removed;
    }
    let update = [head, &["update-ref", "--no-deref", "--stdin"][..]].concat();
    match git_run(git, "git update-ref", at, &update, Some(deletes.as_bytes())) {
        Ok(_) => removed,
        Err(fault) => kept(fault, console),
    }
}

/// Whether a commit whose committer date is `date`, in seconds since 1970 as `for-each-ref` writes it, is younger than
/// [`BACK_KEPT`] at `now`; one dated after `now` is. A date that cannot be read, as of a ref to no commit, is not.
fn young(date: &str, now: i64) -> bool {
    date.trim().parse::<i64>().is_ok_and(|at| now.saturating_sub(at) < BACK_KEPT)
}

/// Whether the ref `name` is one a routed run left for `key`: [`REFS`] then the key, the key and [`BACK`], or a
/// builder's id, `/` and the key, the key compared as written.
fn key_ref(name: &str, key: &str) -> bool {
    let Some(rest) = name.strip_prefix(REFS) else {
        return false;
    };
    if rest == key || rest.strip_suffix(BACK) == Some(key) {
        return true;
    }
    rest.split_once('/').is_some_and(|(id, tail)| !id.is_empty() && tail == key)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot write or read its files must fail loudly")]
mod tests {
    use std::fs;
    use std::path::Path;

    use gate_router::Config;
    use gate_state::{StateRoot, v1};

    use super::{BACK_KEPT, Removal, builders_removed, every_removed, key_ref, own_files, removed, shared_files, young};
    use crate::console::Console;
    use crate::prune_cmd::{Outcome, Outcomes, Scanned};
    use crate::remote::git::Git;
    use crate::resolve::ResolvedRoot;
    use crate::test_support::temp_dir;

    const KEY: &str = "app-1a2b3c4d";
    const ID: &str = "9f8e7d6c";

    /// The folder `name` in the unit tests' folder, emptied.
    fn fresh(name: &str) -> std::path::PathBuf {
        let folder = temp_dir().join(name);
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("the last run's folder can be removed");
        }
        folder
    }

    /// Writes "x" at each of `paths`, its folder made.
    fn written(paths: &[std::path::PathBuf]) {
        for path in paths {
            fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the folder can be made");
            fs::write(path, "x").expect("the file can be written");
        }
    }

    /// A gone worktree's record, found gone at `since`.
    fn record(since: Option<&str>) -> v1::WorktreeRecord {
        v1::WorktreeRecord {
            root: r"X:\nowhere\wt".to_owned(),
            common_dir: r"X:\nowhere\repo\.git".to_owned(),
            repo: "app".to_owned(),
            last_routed: "2026-10-01T00:00:00Z".parse().expect("a time"),
            gone_since: since.map(|since| since.parse().expect("a time")),
        }
    }

    fn all_there(paths: &[std::path::PathBuf]) -> bool {
        paths.iter().all(|path| path.exists())
    }

    #[test]
    fn a_pruned_builder_loses_its_own_files_and_a_held_one_keeps_them() {
        let state = fresh("prune-own-files");
        let (host, other) = (own_files(&state, None, KEY), own_files(&state, Some(ID), KEY));
        written(&host);
        written(&other);
        let mut console = Console::new();

        let removed = builders_removed(
            &state,
            KEY,
            &[(None, Outcome::Removed), (Some(ID.to_owned()), Outcome::Held)],
            &mut console,
        );

        assert!(removed, "every file of the pruned builder is gone");
        assert!(
            host.iter().all(|path| !path.exists()),
            "the pruned builder's own files are gone: {host:?}"
        );
        assert!(all_there(&other), "the held builder's own files stay: {other:?}");
        assert!(console.take_notes().is_empty(), "a removal that works says nothing");
    }

    #[test]
    fn a_record_rewritten_since_the_sweep_judged_it_keeps_everything() {
        let state = fresh("prune-rewritten");
        let path = v1::worktree_record_path(&state, KEY);
        written(std::slice::from_ref(&path));
        v1::write_worktree_record(&path, &record(None)).expect("the record can be written");
        let (own, shared) = (own_files(&state, None, KEY), shared_files(&state, KEY));
        written(&own);
        written(&shared);
        let resolved = ResolvedRoot {
            path: state.clone(),
            root: StateRoot::new(&state.to_string_lossy(), None),
        };
        let config = Config::parse(r#"{"host": "builder-1", "repos": {"app": ["*"]}}"#).expect("the routing file is usable");
        let git = Git::find(Path::new(temp_dir())).expect("git is found");
        let removal = Removal {
            state: &resolved,
            config: &config,
            git: &git,
        };
        let judged = Scanned {
            key: KEY.to_owned(),
            path: path.clone(),
            record: record(Some("2026-10-02T00:00:00Z")),
        };
        let outcomes = Outcomes::from([(KEY.to_owned(), vec![(None, Outcome::Removed)])]);
        let mut console = Console::new();

        let failed = removed(&removal, &[&judged], &outcomes, &mut console);

        assert!(!failed, "a record rewritten since is no failure");
        assert!(all_there(&own) && all_there(&shared) && path.exists(), "nothing is removed");
        assert!(console.take_notes().is_empty(), "nothing is said");
    }

    #[test]
    fn the_shared_files_wait_until_every_builder_pruned_the_key() {
        let id = Some(ID.to_owned());
        assert!(every_removed(&[(None, Outcome::Removed), (id.clone(), Outcome::Removed)]));
        assert!(
            !every_removed(&[(None, Outcome::Removed), (id.clone(), Outcome::Held)]),
            "a held key keeps them"
        );
        assert!(
            !every_removed(&[(None, Outcome::Failed), (id, Outcome::Removed)]),
            "a failed builder keeps them"
        );
        assert!(!every_removed(&[]), "a key no builder was called for keeps them");
    }

    #[test]
    fn only_the_key_s_own_refs_match() {
        for name in ["app-1a2b3c4d", "app-1a2b3c4d-back", "win-2/app-1a2b3c4d"] {
            assert!(key_ref(&format!("refs/gate-remote/{name}"), KEY), "{name} is the key's");
        }
        for name in ["app-1a2b3c4d-x", "app-1a2b3c4dd", "win-2/app-1a2b3c4d/x", "App-1a2b3c4d", "/app-1a2b3c4d"] {
            assert!(!key_ref(&format!("refs/gate-remote/{name}"), KEY), "{name} is not the key's");
        }
        assert!(!key_ref("refs/heads/app-1a2b3c4d", KEY), "a ref outside the folder is not the key's");
    }

    #[test]
    fn a_sync_back_commit_is_young_for_seven_days() {
        let now = 1_800_000_000;
        assert!(young(&(now - BACK_KEPT + 1).to_string(), now));
        assert!(!young(&(now - BACK_KEPT).to_string(), now), "seven days old is old");
        assert!(young(&(now + 60).to_string(), now), "a date after now is young");
        assert!(!young("", now), "a ref to no commit has no date");
    }
}
