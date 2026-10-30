//! The snapshot a routed run sends the builder: the worktree as `git add -A` would stage it, untracked files in and
//! ignored ones out, committed with no parent through a copy of the index, so the real index and every worktree file
//! stay as they were; the bridge commit, the same tree on the previous snapshot that builder received; and the local
//! ref, one per builder, naming the snapshot each builder last received. Every commit is made by git under the
//! `gate-remote` identity, through its environment, and is never signed.

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
/// Where the local refs of the snapshots live, one a key for `host` and one a key under each other builder's id
/// ([`snapshot_ref`]), beside the `-back` refs of changes kept from the builder.
pub(super) const REFS: &str = "refs/gate-remote/";
pub(super) const BACK: &str = "-back";
/// Where the PC's `origin`'s remote-tracking refs live, as `for-each-ref` names them.
const ORIGIN: &str = "refs/remotes/origin/";

/// One worktree's snapshot commit and, when there was a previous snapshot, its bridge commit, both full shas; the
/// repository's object store, as the snapshot's `git rev-parse --git-path objects` named it, which the sync-back
/// fetches beside; and the commits of the PC's `HEAD` and of its default branch, which a shallow clone has neither of.
pub(crate) struct Snapshot {
    pub commit: String,
    pub bridge: Option<String>,
    pub objects: PathBuf,
    pub head: Option<String>,
    pub base: Option<Base>,
}

/// The PC's default branch as its `origin` names it: the branch's name, the part after `refs/remotes/origin/`, and its
/// commit.
pub(crate) struct Base {
    /// The branch's name.
    pub name: String,
    /// The commit at that branch, full.
    pub commit: String,
}

/// The snapshot of each of `roots`, in order, under the work-folder `key`, each git call under `bounds` or ended by the
/// console `interrupt`, each bridge built on the snapshot the builder `builder` ([`gate_router::Builder::id`], `None`
/// for `host`) last received; else why the call could not be made. git is found once, for every call of every root.
pub(crate) fn take(
    roots: &[PathBuf],
    key: &str,
    builder: Option<&str>,
    bounds: GitBounds,
    interrupt: Option<Interrupt>,
) -> Result<Vec<Snapshot>, GitError> {
    let Some(first) = roots.first() else {
        return Ok(Vec::new());
    };
    let git = Git::find(first).map_err(GitError::Failed)?;
    roots
        .iter()
        .map(|root| take_one(&Calls::new(&git, root, &[], bounds, interrupt), key, builder))
        .collect()
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

/// The snapshot of the pair's repository `repo`, as [`take`] takes one, under the work-folder `key`, for the builder
/// `builder`.
pub(crate) fn take_bounded(
    repo: Bounded<'_>,
    key: &str,
    builder: Option<&str>,
    bounds: GitBounds,
    interrupt: Option<Interrupt>,
) -> Result<Snapshot, GitError> {
    let git = Git::find(repo.root).map_err(GitError::Failed)?;
    let variables = repo.variables();
    take_one(&Calls::new(&git, repo.root, &variables, bounds, interrupt), key, builder)
}

/// Where git finds the pair's repository `repo`, searching from its root up to its ceiling and given no place, as one
/// `git rev-parse --show-toplevel --path-format=absolute --absolute-git-dir --git-common-dir` prints it.
pub(crate) fn place_of(repo: Bounded<'_>, bounds: GitBounds, interrupt: Option<Interrupt>) -> Result<Place, GitError> {
    let git = Git::find(repo.root).map_err(GitError::Failed)?;
    let ceiling = [(CEILING, repo.ceiling.as_os_str())];
    let calls = Calls::new(&git, repo.root, &ceiling, bounds, interrupt);
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

/// The local ref of the snapshot of one work-folder key that one builder last received, made only by [`snapshot_ref`],
/// so [`save`] can be given no other name.
#[derive(Debug)]
pub(crate) struct SnapshotRef(String);

impl SnapshotRef {
    /// The ref's full name, as git is given it.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The local ref of the snapshot of the work-folder `key` that the builder `builder` last received, by
/// [`gate_router::Builder::id`]: `refs/gate-remote/<key>` for `host`, `None`, and `refs/gate-remote/<id>/<key>` for the
/// builder of the id `id`. A key holds a `-` and an id never does, so no two builders' refs meet.
pub(crate) fn snapshot_ref(key: &str, builder: Option<&str>) -> SnapshotRef {
    SnapshotRef(format!("{}{key}", builder_refs(builder)))
}

/// The folder of the local refs of the snapshots the builder `builder` last received, [`REFS`] for `host`, `None`.
fn builder_refs(builder: Option<&str>) -> String {
    builder.map_or_else(|| REFS.to_owned(), |id| format!("{REFS}{id}/"))
}

/// Points `reference`, the builder's [`snapshot_ref`], at `commit` in the repository of `root`, once the builder has
/// the snapshot, so the next bridge commit to that builder builds on it, git given `bound`, a pair's repository's
/// [`Bounded::variables`], none for a run of one. git holds the ref's lock while it runs, so it is a
/// [`git::Call::locked`] call, which no Ctrl+C ends: it ends on its own or at `bounds`' limit.
pub(crate) fn save(root: &Path, bound: &[(&str, &OsStr)], reference: &SnapshotRef, commit: &str, bounds: GitBounds) -> Result<(), GitError> {
    let git = Git::find(root).map_err(GitError::Failed)?;
    let args = ["update-ref", reference.as_str(), commit];
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
    worktree_tree(&Calls::new(&git, root, &[], bounds, interrupt)).map(|(tree, _)| tree)
}

/// The snapshot of the worktree `calls` asks about under the work-folder `key`, its bridge built on the snapshot the
/// builder `builder` last received.
fn take_one(calls: &Calls<'_>, key: &str, builder: Option<&str>) -> Result<Snapshot, GitError> {
    let listing = calls.output(
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname) %(objectname) %(symref)",
            &builder_refs(builder),
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
            "refs/remotes/origin/master",
        ],
        &[],
        &[],
    )?;
    let previous = previous(&listing, key, builder);
    let (tree, paths) = worktree_tree(calls)?;
    // A shallow clone sends no history: a receiver refuses a push from one.
    let (head, base) = if paths.shallow {
        (None, None)
    } else {
        (paths.head, default_branch(&listing))
    };
    let commit = calls.committed(&["commit-tree", "--no-gpg-sign", &tree, "-m", SNAPSHOT_MESSAGE])?;
    let bridge = match &previous {
        Some(parent) => Some(calls.committed(&["commit-tree", "--no-gpg-sign", &tree, "-p", parent, "-m", BRIDGE_MESSAGE])?),
        None => None,
    };
    Ok(Snapshot {
        commit,
        bridge,
        objects: paths.objects,
        head,
        base,
    })
}

/// The default branch and its commit from `for-each-ref`'s `<ref> <sha> <symref>` lines: the branch the
/// `refs/remotes/origin/HEAD` symref names when it is under `refs/remotes/origin/`, else `main`, else `master`, else
/// none. A branch whose name the box would refuse ([`gate_wire::is_branch_name`]) gives none, and no other stands in for
/// it, so no base is sent for that repository.
fn default_branch(listing: &str) -> Option<Base> {
    let refs: Vec<(&str, &str, &str)> = listing
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(' ')?;
            let (commit, symref) = rest.split_once(' ').unwrap_or((rest, ""));
            Some((name, commit, symref))
        })
        .collect();
    let origin_head = format!("{ORIGIN}HEAD");
    if let Some((_, commit, symref)) = refs.iter().find(|(name, _, _)| *name == origin_head.as_str())
        && let Some(name) = symref.strip_prefix(ORIGIN)
    {
        return gate_wire::is_branch_name(name).then(|| Base {
            name: name.to_owned(),
            commit: (*commit).to_owned(),
        });
    }
    ["main", "master"].iter().find_map(|branch| {
        let wanted = format!("{ORIGIN}{branch}");
        let (_, commit, _) = refs.iter().find(|(name, _, _)| *name == wanted.as_str())?;
        Some(Base {
            name: (*branch).to_owned(),
            commit: (*commit).to_owned(),
        })
    })
}

/// The snapshot a bridge to the builder `builder` builds on, from `for-each-ref`'s `<ref> <sha> <symref>` lines, newest
/// first, among that builder's own refs only ([`snapshot_ref`]): the key's own, else the newest any worktree of the
/// repository sent that builder, never a kept `-back` commit; `None` when there is none.
fn previous(listing: &str, key: &str, builder: Option<&str>) -> Option<String> {
    let folder = builder_refs(builder);
    let refs: Vec<(&str, &str)> = listing
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(' ')?;
            let (sha, _) = rest.split_once(' ').unwrap_or((rest, ""));
            Some((name, sha))
        })
        .filter(|(name, _)| name.strip_prefix(folder.as_str()).is_some_and(|rest| !rest.contains('/')))
        .collect();
    let own = snapshot_ref(key, builder);
    refs.iter()
        .find(|(name, _)| *name == own.as_str())
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

impl<'a> Calls<'a> {
    /// The calls of `git` about the worktree at `root`, given `bound`, under `bounds` or ended by `interrupt`.
    fn new(git: &'a Git, root: &'a Path, bound: &'a [(&'a str, &'a OsStr)], bounds: GitBounds, interrupt: Option<Interrupt>) -> Self {
        Calls {
            git,
            root,
            bound,
            bounds,
            interrupt,
        }
    }

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
/// and the worktree's [`GitPaths`].
fn worktree_tree(calls: &Calls<'_>) -> Result<(String, GitPaths), GitError> {
    let paths = paths(calls, &copy_name(std::process::id()))?;
    let tree = write_tree(calls, &paths.index, &paths.copy);
    let removed = remove_leftovers(&paths.copy);
    let tree = tree?;
    removed.map(|()| (tree, paths))
}

/// The index, the index copy and the object store of the worktree's repository, and what the call that named them says
/// of its history: whether it is a shallow clone, and the commit of its born `HEAD`, `None` for an unborn one.
struct GitPaths {
    index: PathBuf,
    copy: PathBuf,
    objects: PathBuf,
    shallow: bool,
    head: Option<String>,
}

/// The worktree's [`GitPaths`], as one `git rev-parse` call names them: three `--git-path` answers, then
/// `--is-shallow-repository`'s `true` or `false`, then the commit of a born `HEAD`, which git prints for no other.
fn paths(calls: &Calls<'_>, name: &str) -> Result<GitPaths, GitError> {
    let printed = calls.output(
        &[
            "rev-parse",
            "--git-path",
            "index",
            "--git-path",
            name,
            "--git-path",
            "objects",
            "--is-shallow-repository",
            "--revs-only",
            "HEAD^{commit}",
        ],
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
    let shallow = match lines.next() {
        Some("true") => true,
        Some("false") => false,
        _ => {
            return Err(GitError::Failed(format!(
                "git rev-parse --is-shallow-repository did not print true or false: {}",
                printed.trim()
            )));
        }
    };
    let head = lines.next().map(str::to_owned).filter(|head| is_commit(head));
    Ok(GitPaths {
        index: git_path(calls.root, index),
        copy: git_path(calls.root, copy),
        objects: git_path(calls.root, objects),
        shallow,
        head,
    })
}

/// Whether `text` is a whole commit id: 40 or 64 lower-case hex digits.
fn is_commit(text: &str) -> bool {
    gate_wire::is_lower_hex(text, 40) || gate_wire::is_lower_hex(text, 64)
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

    use super::{
        Bounded, DATES, IDENTITY, asked, copy_name, default_branch, git_path, lock_of, place_of, previous, repository, save, snapshot_ref, take,
        worktree,
    };
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
        assert_eq!(
            previous(listing, OWN, None),
            Some("ccc".to_owned()),
            "the key's own ref wins over a newer one"
        );
        assert_eq!(
            previous(listing, "third-0", None),
            Some("bbb".to_owned()),
            "a -back ref is left out, and the newest of the rest wins"
        );
        assert_eq!(
            previous("refs/gate-remote/a x\nrefs/gate-remote/b y\n", "c", None),
            Some("x".to_owned()),
            "the newest comes first"
        );
        assert_eq!(previous("refs/gate-remote/app-1a2b3c4d-back aaa\n", OWN, None), None, "only a -back ref");
        assert_eq!(previous("", OWN, None), None, "no snapshot yet");
    }

    /// A listing that names the symref too leaves an empty third field on every line of a builder's refs: the sha is
    /// the second field alone.
    #[test]
    fn the_previous_snapshot_is_the_sha_before_the_empty_symref() {
        let listing = "refs/gate-remote/app-1a2b3c4d ccc \n";
        assert_eq!(
            previous(listing, OWN, None),
            Some("ccc".to_owned()),
            "the second field, never the rest of the line"
        );
    }

    /// The id of a `builders` entry, as [`gate_router::Builder::id`] spells one.
    const ID: &str = "0a1b2c3d";

    #[test]
    fn host_bridges_only_on_its_own_flat_refs() {
        let listing = "refs/gate-remote/0a1b2c3d/app-1a2b3c4d ddd\nrefs/gate-remote/0a1b2c3d/other-3 eee\n\
                       refs/gate-remote/other-2 bbb\nrefs/gate-remote/app-1a2b3c4d ccc\n";
        assert_eq!(
            previous(listing, OWN, None),
            Some("ccc".to_owned()),
            "host's own ref, not a builder's newer one"
        );
        assert_eq!(
            previous(listing, "third-0", None),
            Some("bbb".to_owned()),
            "the newest of host's refs, a builder's newer refs left out"
        );
        assert_eq!(
            previous("refs/gate-remote/0a1b2c3d/app-1a2b3c4d ddd\n", OWN, None),
            None,
            "only another builder's ref"
        );
    }

    #[test]
    fn a_builder_bridges_on_its_own_ref_of_the_key() {
        let listing = "refs/gate-remote/9f8e7d6c/app-1a2b3c4d eee\nrefs/gate-remote/app-1a2b3c4d ccc\n\
                       refs/gate-remote/0a1b2c3d/other-2 bbb\nrefs/gate-remote/0a1b2c3d/app-1a2b3c4d ddd\n";
        assert_eq!(
            previous(listing, OWN, Some(ID)),
            Some("ddd".to_owned()),
            "the builder's own ref of the key, over its newer ref, host's and another builder's"
        );
    }

    #[test]
    fn a_builder_without_the_key_s_ref_takes_its_newest_that_is_no_back_ref() {
        let listing = "refs/gate-remote/9f8e7d6c/other-4 fff\nrefs/gate-remote/other-5 ggg\n\
                       refs/gate-remote/0a1b2c3d/other-1-back aaa\nrefs/gate-remote/0a1b2c3d/other-2 bbb\n\
                       refs/gate-remote/0a1b2c3d/other-3 ccc\n";
        assert_eq!(
            previous(listing, OWN, Some(ID)),
            Some("bbb".to_owned()),
            "the newest of the builder's own refs, never a -back one, host's or another builder's"
        );
    }

    #[test]
    fn a_builder_with_no_refs_of_its_own_has_no_previous_snapshot() {
        let listing = "refs/gate-remote/9f8e7d6c/app-1a2b3c4d eee\nrefs/gate-remote/app-1a2b3c4d ccc\nrefs/gate-remote/other-2 bbb\n";
        assert_eq!(
            previous(listing, OWN, Some(ID)),
            None,
            "host's and another builder's refs are not its own"
        );
    }

    /// The `for-each-ref` listing the default branch is chosen from: `%(refname) %(objectname) %(symref)`, the symref
    /// empty for every ref that is not `refs/remotes/origin/HEAD`.
    #[test]
    fn origin_head_s_target_is_the_default_branch() {
        let listing = "refs/remotes/origin/HEAD aaa refs/remotes/origin/trunk\n\
                       refs/remotes/origin/main bbb \nrefs/remotes/origin/trunk aaa \n";
        assert_eq!(
            default_branch(listing).map(|base| base.name),
            Some("trunk".to_owned()),
            "origin/HEAD's target is the default branch, over origin/main"
        );
        assert_eq!(
            default_branch(listing).map(|base| base.commit),
            Some("aaa".to_owned()),
            "the branch's commit"
        );
    }

    #[test]
    fn origin_main_stands_in_when_origin_head_is_unset() {
        let listing = "refs/remotes/origin/main bbb \nrefs/remotes/origin/master ccc \n";
        assert_eq!(
            default_branch(listing).map(|base| base.name),
            Some("main".to_owned()),
            "origin/main when origin/HEAD is not there"
        );
        assert_eq!(
            default_branch(listing).map(|base| base.commit),
            Some("bbb".to_owned()),
            "origin/main's commit"
        );
    }

    #[test]
    fn origin_master_stands_in_when_neither_is_there() {
        let listing = "refs/remotes/origin/master ccc \n";
        assert_eq!(
            default_branch(listing).map(|base| base.name),
            Some("master".to_owned()),
            "origin/master alone"
        );
        assert_eq!(
            default_branch(listing).map(|base| base.commit),
            Some("ccc".to_owned()),
            "origin/master's commit"
        );
    }

    #[test]
    fn a_default_branch_the_box_would_refuse_is_not_sent() {
        let alone = "refs/remotes/origin/HEAD aaa refs/remotes/origin/dev@x\nrefs/remotes/origin/dev@x aaa \n";
        assert_eq!(default_branch(alone).map(|base| base.name), None, "a name the box refuses is no base");
        let beside_main = format!("{alone}refs/remotes/origin/main bbb \n");
        assert_eq!(
            default_branch(&beside_main).map(|base| base.name),
            None,
            "origin/HEAD's branch is the default, so origin/main does not stand in for a refused one"
        );
    }

    #[test]
    fn no_origin_ref_gives_no_default_branch() {
        assert_eq!(default_branch("").map(|base| base.name), None, "no origin ref at all");
    }

    #[test]
    fn an_origin_head_pointing_outside_origin_is_ignored() {
        let listing = "refs/remotes/origin/HEAD aaa refs/heads/x\nrefs/remotes/origin/main bbb \n";
        assert_eq!(
            default_branch(listing).map(|base| base.name),
            Some("main".to_owned()),
            "a symref outside refs/remotes/origin/ is no default branch, and origin/main stands in"
        );
        assert_eq!(
            default_branch(listing).map(|base| base.commit),
            Some("bbb".to_owned()),
            "origin/main's commit"
        );
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
        let first = take(roots, OWN, None, GIT_BOUNDS, None).expect("the first snapshot is made");
        assert_eq!(first.len(), 1, "one snapshot for one root");
        assert_eq!(first[0].bridge, None, "no snapshot before the first");
        save(&root, &[], &snapshot_ref(OWN, None), &first[0].commit, GIT_BOUNDS).expect("the ref is saved");
        fs::write(root.join("a.txt"), "b\n").expect("a file can be changed");

        let second = take(roots, OWN, None, GIT_BOUNDS, None).expect("the second snapshot is made");

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
        let made = take(roots, OWN, None, GIT_BOUNDS, None).expect("the snapshot is made");

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
            let made = take(slice::from_ref(&folder.to_path_buf()), OWN, None, GIT_BOUNDS, None).expect("the snapshot is made");
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
