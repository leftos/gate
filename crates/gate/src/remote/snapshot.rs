//! The snapshot a routed run sends the builder: the worktree as `git add -A` would stage it, untracked files in and
//! ignored ones out, committed with no parent through a copy of the index, so the real index and every worktree file
//! stay as they were; the bridge commit, the same tree on the previous snapshot; and the local ref naming the snapshot
//! the builder last received. Every commit is made by git under the `gate-remote` identity, through its environment,
//! and is never signed.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gate_win32::Interrupt;

use crate::remote::git::{self, Git, GitBounds, GitError};

/// The author and committer of every commit the snapshot makes, and of the builder's back commit.
pub(crate) const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "gate-remote"),
    ("GIT_AUTHOR_EMAIL", "gate-remote@localhost"),
    ("GIT_COMMITTER_NAME", "gate-remote"),
    ("GIT_COMMITTER_EMAIL", "gate-remote@localhost"),
];
/// The variables a commit must not see: a hook or a rebase that exported them would freeze the dates the refs are
/// sorted by, and so the snapshot a bridge builds on.
const DATES: [&str; 2] = ["GIT_AUTHOR_DATE", "GIT_COMMITTER_DATE"];
const SNAPSHOT_MESSAGE: &str = "gate-remote snapshot";
const BRIDGE_MESSAGE: &str = "gate-remote bridge";
/// Where the local refs of the snapshots live, one a key, beside the `-back` refs of changes kept from the builder.
pub(super) const REFS: &str = "refs/gate-remote/";
pub(super) const BACK: &str = "-back";

/// One worktree's snapshot commit and, when there was a previous snapshot, its bridge commit, both full shas; and the
/// repository's object store, as the snapshot's `git rev-parse --git-path objects` named it, which the sync-back
/// fetches beside.
pub(crate) struct Snapshot {
    pub commit: String,
    pub bridge: Option<String>,
    pub objects: PathBuf,
}

/// The snapshot of each of `roots`, in order, under the work-folder `key`, each git call under `bounds` or ended by the
/// console `interrupt`; else why the call could not be made. git is found once, for every call of every root.
pub(crate) fn take(roots: &[PathBuf], key: &str, bounds: GitBounds, interrupt: Option<Interrupt>) -> Result<Vec<Snapshot>, GitError> {
    let Some(first) = roots.first() else {
        return Ok(Vec::new());
    };
    let git = Git::find(first).map_err(GitError::Failed)?;
    roots.iter().map(|root| take_one(&git, root, &[], key, bounds, interrupt)).collect()
}

/// A repository of a sibling pair as its git calls ask about it: its worktree's folder; the folder git's search for a
/// repository stops at, `GIT_CEILING_DIRECTORIES`, the pair's parent folder, so git never reaches a repository above;
/// and, once checked, where git found it, which every later call is given outright, so a `core.worktree`, a
/// `commondir` or a `gitdir:` written after the check cannot point it elsewhere; `None` before the check.
#[derive(Clone, Copy)]
pub(crate) struct Bounded<'a> {
    pub root: &'a Path,
    pub ceiling: &'a Path,
    pub place: Option<&'a Place>,
}

impl<'a> Bounded<'a> {
    /// The variables every git call about the repository is given: the ceiling, and with a checked place its git
    /// folder, its worktree, the root, and its common folder.
    pub(crate) fn variables(&self) -> Vec<(&'a str, &'a OsStr)> {
        let mut variables = vec![(CEILING, self.ceiling.as_os_str())];
        if let Some(place) = self.place {
            variables.extend([
                ("GIT_DIR", OsStr::new(place.git_dir.as_str())),
                ("GIT_WORK_TREE", self.root.as_os_str()),
                ("GIT_COMMON_DIR", OsStr::new(place.common.as_str())),
            ]);
        }
        variables
    }
}

/// Where git finds a pair's repository: the top folder of its worktree, its git folder and its common git folder, each
/// absolute, as git printed them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Place {
    pub top: String,
    pub git_dir: String,
    pub common: String,
}

/// The variable naming the folder git's search for a repository stops at, which every git call about a pair's
/// repository is given.
pub(crate) const CEILING: &str = "GIT_CEILING_DIRECTORIES";

/// The snapshot of the pair's repository `repo`, as [`take`] takes one, under the work-folder `key`.
pub(crate) fn take_bounded(repo: Bounded<'_>, key: &str, bounds: GitBounds, interrupt: Option<Interrupt>) -> Result<Snapshot, GitError> {
    let git = Git::find(repo.root).map_err(GitError::Failed)?;
    take_one(&git, repo.root, &repo.variables(), key, bounds, interrupt)
}

/// Where git finds the pair's repository `repo`, searching from its root up to its ceiling and given no place, as one
/// `git rev-parse --show-toplevel --path-format=absolute --absolute-git-dir --git-common-dir` prints it.
pub(crate) fn place_of(repo: Bounded<'_>, bounds: GitBounds, interrupt: Option<Interrupt>) -> Result<Place, GitError> {
    let git = Git::find(repo.root).map_err(GitError::Failed)?;
    let ceiling = [(CEILING, repo.ceiling.as_os_str())];
    let calls = Calls {
        git: &git,
        root: repo.root,
        bound: &ceiling,
        bounds,
        interrupt,
    };
    let args = [
        "rev-parse",
        "--show-toplevel",
        "--path-format=absolute",
        "--absolute-git-dir",
        "--git-common-dir",
    ];
    let printed = calls.output(&args, &[], &[])?;
    let mut lines = printed.lines().map(str::trim_end);
    match (lines.next(), lines.next(), lines.next(), lines.next()) {
        (Some(top), Some(git_dir), Some(common), None) => Ok(Place {
            top: top.to_owned(),
            git_dir: git_dir.to_owned(),
            common: common.to_owned(),
        }),
        _ => Err(GitError::Failed(format!(
            "git {} did not print three paths: {}",
            args.join(" "),
            printed.trim()
        ))),
    }
}

/// Points `refs/gate-remote/<key>` at `commit` in the repository of `root`, once the builder has the snapshot, so the
/// next bridge commit builds on it, git given `bound`, a pair's repository's [`Bounded::variables`], none for a run
/// of one. git holds the ref's lock while it runs, so it is a [`git::Call::locked`] call, which no Ctrl+C ends: it ends
/// on its own or at `bounds`' limit.
pub(crate) fn save(root: &Path, bound: &[(&str, &OsStr)], key: &str, commit: &str, bounds: GitBounds) -> Result<(), GitError> {
    let git = Git::find(root).map_err(GitError::Failed)?;
    let args = ["update-ref", &format!("{REFS}{key}"), commit];
    git.output(&git::Call {
        set: bound,
        ..git::Call::locked(root, &args, bounds)
    })
    .map(drop)
}

/// The tree of the worktree at `root` as `git add -A` and `git write-tree` write it through a copy of its index: what
/// the sync-back compares against the snapshot's tree, and no commit. git is ended at `bounds`' limit or by the console
/// `interrupt`.
pub(super) fn worktree(root: &Path, bounds: GitBounds, interrupt: Option<Interrupt>) -> Result<String, GitError> {
    let git = Git::find(root).map_err(GitError::Failed)?;
    worktree_tree(&Calls {
        git: &git,
        root,
        bound: &[],
        bounds,
        interrupt,
    })
    .map(|(tree, _)| tree)
}

fn take_one(
    git: &Git,
    root: &Path,
    bound: &[(&str, &OsStr)],
    key: &str,
    bounds: GitBounds,
    interrupt: Option<Interrupt>,
) -> Result<Snapshot, GitError> {
    let calls = Calls {
        git,
        root,
        bound,
        bounds,
        interrupt,
    };
    let listing = calls.output(
        &["for-each-ref", "--sort=-committerdate", "--format=%(refname) %(objectname)", REFS],
        &[],
        &[],
    )?;
    let previous = previous(&listing, key);
    let (tree, objects) = worktree_tree(&calls)?;
    let commit = calls.committed(&["commit-tree", "--no-gpg-sign", &tree, "-m", SNAPSHOT_MESSAGE])?;
    let bridge = match &previous {
        Some(parent) => Some(calls.committed(&["commit-tree", "--no-gpg-sign", &tree, "-p", parent, "-m", BRIDGE_MESSAGE])?),
        None => None,
    };
    Ok(Snapshot { commit, bridge, objects })
}

/// The snapshot a bridge builds on, from `for-each-ref`'s `<ref> <sha>` lines, newest first: the key's own, else the
/// newest any worktree of the repository made, never a kept `-back` commit; `None` when there is none.
fn previous(listing: &str, key: &str) -> Option<String> {
    let refs: Vec<(&str, &str)> = listing.lines().filter_map(|line| line.split_once(' ')).collect();
    let own = format!("{REFS}{key}");
    refs.iter()
        .find(|(name, _)| *name == own)
        .or_else(|| refs.iter().find(|(name, _)| !name.ends_with(BACK)))
        .map(|(_, sha)| (*sha).to_owned())
}

/// git asked about one worktree, with the variables of a pair's repository ([`Bounded::variables`]), the bounds and the
/// interrupt every call of one snapshot shares.
struct Calls<'a> {
    git: &'a Git,
    root: &'a Path,
    bound: &'a [(&'a str, &'a OsStr)],
    bounds: GitBounds,
    interrupt: Option<Interrupt>,
}

impl Calls<'_> {
    /// What git wrote on stdout for `args`, with `set` and the bound variables given and `unset` removed.
    fn output(&self, args: &[&str], set: &[(&str, &OsStr)], unset: &[&str]) -> Result<String, GitError> {
        let mut given = set.to_vec();
        given.extend(self.bound.iter().copied());
        self.git.output(&self.call(args, &given, unset))
    }

    /// The first line of what git wrote on stdout for `args`, with `set` given and `unset` removed.
    fn first_line(&self, args: &[&str], set: &[(&str, &OsStr)], unset: &[&str]) -> Result<String, GitError> {
        first_line(args, &self.output(args, set, unset)?)
    }

    /// The first line of what git wrote on stdout for `args` under the snapshot's identity, with the date variables
    /// removed: every commit is made this way.
    fn committed(&self, args: &[&str]) -> Result<String, GitError> {
        let identity: Vec<(&str, &OsStr)> = IDENTITY.iter().map(|(name, value)| (*name, OsStr::new(value))).collect();
        self.first_line(args, &identity, &DATES)
    }

    /// One of this worktree's calls.
    fn call<'c>(&'c self, args: &'c [&'c str], set: &'c [(&'c str, &'c OsStr)], unset: &'c [&'c str]) -> git::Call<'c> {
        git::Call {
            set,
            unset,
            interrupt: self.interrupt,
            ..git::Call::bounded(self.root, args, self.bounds)
        }
    }
}

/// The first line of what git wrote for `args`; git that wrote nothing is a failure, since every caller asks for a
/// value.
fn first_line(args: &[&str], text: &str) -> Result<String, GitError> {
    text.lines()
        .next()
        .map(|line| line.trim_end().to_owned())
        .ok_or_else(|| GitError::Failed(format!("git {} printed nothing", args.join(" "))))
}

/// The tree of the worktree at `root`, written through a copy of its index, which is removed however the write ends,
/// and the repository's object store.
fn worktree_tree(calls: &Calls<'_>) -> Result<(String, PathBuf), GitError> {
    let paths = paths(calls, &copy_name(std::process::id()))?;
    let tree = write_tree(calls, &paths.index, &paths.copy);
    let removed = remove_leftovers(&paths.copy);
    let tree = tree?;
    removed.map(|()| (tree, paths.objects))
}

/// The index, the index copy and the object store of the worktree's repository.
struct GitPaths {
    index: PathBuf,
    copy: PathBuf,
    objects: PathBuf,
}

/// The worktree's [`GitPaths`], as one `git rev-parse --git-path` call names them.
fn paths(calls: &Calls<'_>, name: &str) -> Result<GitPaths, GitError> {
    let printed = calls.output(
        &["rev-parse", "--git-path", "index", "--git-path", name, "--git-path", "objects"],
        &[],
        &[],
    )?;
    let mut lines = printed.lines().map(str::trim_end);
    let (Some(index), Some(copy)) = (lines.next(), lines.next()) else {
        return Err(GitError::Failed(format!(
            "git rev-parse --git-path did not print both paths: {}",
            printed.trim()
        )));
    };
    let objects = lines
        .next()
        .filter(|objects| !objects.is_empty())
        .ok_or_else(|| GitError::Failed("git rev-parse --git-path objects printed nothing".to_owned()))?;
    Ok(GitPaths {
        index: git_path(calls.root, index),
        copy: git_path(calls.root, copy),
        objects: git_path(calls.root, objects),
    })
}

/// `git add -A` and `git write-tree` on `copy`, a fresh copy of `index`; with no index, git starts from an empty one.
fn write_tree(calls: &Calls<'_>, index: &Path, copy: &Path) -> Result<String, GitError> {
    remove_leftovers(copy)?;
    match fs::copy(index, copy) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound && !index.exists() => {}
        Err(error) => {
            return Err(GitError::Failed(format!(
                "cannot copy the index {} to {}: {error}",
                index.display(),
                copy.display()
            )));
        }
    }
    let env: [(&str, &OsStr); 1] = [("GIT_INDEX_FILE", copy.as_os_str())];
    calls.output(&["add", "-A"], &env, &[])?;
    calls.first_line(&["write-tree"], &env, &[])
}

/// Removes the index copy `copy` and the lock git makes beside it: a git killed at its limit would leave a lock that
/// fails the next gate of the same process id, which holds a copy of the same name.
fn remove_leftovers(copy: &Path) -> Result<(), GitError> {
    for path in [copy.to_path_buf(), lock_of(copy)] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(GitError::Failed(format!("cannot remove {}: {error}", path.display()))),
        }
    }
    Ok(())
}

/// The lock git makes beside the index file it is given.
fn lock_of(copy: &Path) -> PathBuf {
    let mut name = copy.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// The index copy's name in the git folder, for the process `pid`.
fn copy_name(pid: u32) -> String {
    format!("gate-remote-{pid}.index")
}

/// A path `git rev-parse --git-path` printed, read against the worktree `root` it was asked in.
fn git_path(root: &Path, printed: &str) -> PathBuf {
    root.join(printed)
}

/// What git with `args` wrote in `root`, git found once, as the snapshot's own calls find it.
#[cfg(test)]
fn asked(root: &Path, args: &[&str]) -> Result<String, GitError> {
    let git = Git::find(root).map_err(GitError::Failed)?;
    git.output(&git::Call::bounded(root, args, git::GIT_BOUNDS))
}

/// A new git repository in the unit tests' temp folder for the test `name`, holding `a.txt`.
#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its repository must fail loudly")]
pub(super) fn repository(name: &str) -> PathBuf {
    let root = crate::test_support::temp_dir().join(format!("gate-snapshot-{name}-{}", std::process::id()));
    if root.exists() {
        fs::remove_dir_all(&root).expect("a repository left by an earlier run can be removed");
    }
    fs::create_dir_all(&root).expect("the repository's folder can be made");
    asked(&root, &["init", "--quiet"]).expect("git init succeeds");
    fs::write(root.join("a.txt"), "a\n").expect("a file can be written");
    root
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its repository must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::slice;

    use super::{Bounded, DATES, IDENTITY, asked, copy_name, git_path, lock_of, place_of, previous, repository, save, take, worktree};
    use crate::remote::git::{GIT_BOUNDS, GitError};

    const OWN: &str = "app-1a2b3c4d";

    /// A folder with no `.git` of its own inside another repository is no repository to a search bounded at its parent:
    /// git fails, and never names the repository above.
    #[test]
    fn a_folder_with_no_git_inside_a_repository_is_not_found_above_its_ceiling() {
        let outer = repository("ceiling");
        let parent = outer.join("dev");
        let folder = parent.join("lib");
        fs::create_dir_all(&folder).expect("the folder can be made");
        let bounded = Bounded {
            root: &folder,
            ceiling: &parent,
            place: None,
        };

        let found = place_of(bounded, GIT_BOUNDS, None);

        let outer_shown = outer.to_string_lossy().replace('\\', "/");
        fs::remove_dir_all(&outer).expect("the repository can be removed");
        let failed = match &found {
            Err(GitError::Failed(why)) => Some(why.replace('\\', "/")),
            _ => None,
        };
        assert!(failed.is_some(), "git found a repository above the ceiling: {found:?}");
        assert!(
            failed.is_some_and(|why| !why.contains(&format!("{outer_shown}/.git"))),
            "the repository above is not named: {found:?}"
        );
    }

    #[test]
    fn the_previous_snapshot_is_the_key_s_own_else_the_newest_that_is_no_back_ref() {
        let listing = "refs/gate-remote/other-1-back aaa\nrefs/gate-remote/other-2 bbb\nrefs/gate-remote/app-1a2b3c4d ccc\n";
        assert_eq!(previous(listing, OWN), Some("ccc".to_owned()), "the key's own ref wins over a newer one");
        assert_eq!(
            previous(listing, "third-0"),
            Some("bbb".to_owned()),
            "a -back ref is left out, and the newest of the rest wins"
        );
        assert_eq!(
            previous("refs/gate-remote/a x\nrefs/gate-remote/b y\n", "c"),
            Some("x".to_owned()),
            "the newest comes first"
        );
        assert_eq!(previous("refs/gate-remote/app-1a2b3c4d-back aaa\n", OWN), None, "only a -back ref");
        assert_eq!(previous("", OWN), None, "no snapshot yet");
    }

    #[test]
    fn the_index_copy_is_named_for_the_process_inside_the_git_folder() {
        assert_eq!(copy_name(4242), "gate-remote-4242.index");
        let root = Path::new(r"C:\work\app");
        assert_eq!(
            git_path(root, ".git/gate-remote-1.index"),
            root.join(".git/gate-remote-1.index"),
            "a path git prints relative to the root"
        );
        assert_eq!(
            git_path(root, "C:/work/app/.git/worktrees/wt/gate-remote-1.index"),
            PathBuf::from("C:/work/app/.git/worktrees/wt/gate-remote-1.index"),
            "an absolute path as printed"
        );
        assert_eq!(
            lock_of(&root.join(".git").join("gate-remote-1.index")),
            PathBuf::from(r"C:\work\app\.git\gate-remote-1.index.lock"),
            "git's lock for the index file it is given"
        );
    }

    #[test]
    fn the_snapshot_commits_as_gate_remote_and_gives_the_dates_up() {
        assert_eq!(
            IDENTITY,
            [
                ("GIT_AUTHOR_NAME", "gate-remote"),
                ("GIT_AUTHOR_EMAIL", "gate-remote@localhost"),
                ("GIT_COMMITTER_NAME", "gate-remote"),
                ("GIT_COMMITTER_EMAIL", "gate-remote@localhost"),
            ]
        );
        assert_eq!(
            DATES,
            ["GIT_AUTHOR_DATE", "GIT_COMMITTER_DATE"],
            "a hook's or a rebase's dates are removed from a commit"
        );
    }

    #[test]
    fn a_saved_snapshot_is_the_parent_of_the_next_bridge_commit() {
        let root = repository("saved");
        let roots = slice::from_ref(&root);
        let first = take(roots, OWN, GIT_BOUNDS, None).expect("the first snapshot is made");
        assert_eq!(first.len(), 1, "one snapshot for one root");
        assert_eq!(first[0].bridge, None, "no snapshot before the first");
        save(&root, &[], OWN, &first[0].commit, GIT_BOUNDS).expect("the ref is saved");
        fs::write(root.join("a.txt"), "b\n").expect("a file can be changed");

        let second = take(roots, OWN, GIT_BOUNDS, None).expect("the second snapshot is made");

        let bridge = second[0].bridge.as_deref().expect("a bridge commit");
        let ask = |args: &[&str]| asked(&root, args).expect("git answers");
        assert_eq!(
            ask(&["rev-list", "--parents", "-n", "1", bridge]).trim_end(),
            format!("{bridge} {}", first[0].commit)
        );
        let tree = |commit: &str| ask(&["rev-parse", &format!("{commit}^{{tree}}")]);
        assert_eq!(tree(bridge), tree(&second[0].commit), "the bridge carries the snapshot's tree");
        fs::remove_dir_all(&root).expect("the repository can be removed");
    }

    #[test]
    fn the_worktree_tree_is_the_snapshot_s_own_and_makes_no_commit() {
        let root = repository("worktree-tree");
        fs::write(root.join("b.txt"), "b\n").expect("a file can be written");
        let roots = slice::from_ref(&root);
        let made = take(roots, OWN, GIT_BOUNDS, None).expect("the snapshot is made");

        let tree = worktree(&root, GIT_BOUNDS, None).expect("the worktree tree is written");

        let snapshot = asked(&root, &["rev-parse", &format!("{}^{{tree}}", made[0].commit)]).expect("git answers");
        assert_eq!(
            tree,
            snapshot.trim_end(),
            "the tree the sync-back compares against, with no commit of its own"
        );
        fs::write(root.join("b.txt"), "c\n").expect("a file can be changed");
        assert_ne!(worktree(&root, GIT_BOUNDS, None).expect("the tree is written again"), tree, "read afresh");
        fs::remove_dir_all(&root).expect("the repository can be removed");
    }

    /// The snapshot names its repository's object store: the `.git\objects` of a plain repository, and the common
    /// folder's `objects` for a worktree `git worktree add` made, which the sync-back fetches beside.
    #[test]
    fn the_snapshot_names_the_repository_s_object_store() {
        let root = repository("objects");
        let store = fs::canonicalize(root.join(".git").join("objects")).expect("the store is there");
        let objects_of = |folder: &Path| {
            let made = take(slice::from_ref(&folder.to_path_buf()), OWN, GIT_BOUNDS, None).expect("the snapshot is made");
            fs::canonicalize(&made[0].objects).expect("the named store is there")
        };
        assert_eq!(objects_of(&root), store, "a plain repository's own");

        let ask = |args: &[&str]| asked(&root, args).expect("git answers");
        ask(&["add", "-A"]);
        ask(&["-c", "user.name=t", "-c", "user.email=t@localhost", "commit", "-q", "-m", "start"]);
        let linked = root.with_file_name(format!("{}-linked", root.file_name().expect("a name").to_string_lossy()));
        if linked.exists() {
            fs::remove_dir_all(&linked).expect("an earlier run's worktree can be removed");
        }
        ask(&["worktree", "add", "-q", "--detach", &linked.to_string_lossy()]);

        assert_eq!(objects_of(&linked), store, "a linked worktree's is the common folder's");
        fs::remove_dir_all(&linked).expect("the worktree can be removed");
        fs::remove_dir_all(&root).expect("the repository can be removed");
    }

    #[test]
    fn a_git_failure_says_the_call_its_folder_its_status_and_git_s_lines() {
        let root = repository("failed");
        assert_eq!(
            asked(&root, &["rev-parse", "--verify", "no-such-ref"]),
            Err(GitError::Failed(format!(
                "git rev-parse --verify no-such-ref failed in {} (status 128): fatal: Needed a single revision",
                root.display()
            )))
        );
        fs::remove_dir_all(&root).expect("the repository can be removed");
    }
}
