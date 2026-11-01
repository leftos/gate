//! The forwarder hooks folder of a commit run whose pre-commit stage passed on a builder: one `sh` script for each hook
//! the real hooks folder holds, which the child commit runs in place of the real ones through `GIT_CONFIG_COUNT`. Each
//! runs its real hook, but the pre-commit script skips the real one while `HEAD` and the staged tree are what the
//! builder checked, as the child's environment alone says. Once the child ends, the commit it made is checked against
//! the checked tree ([`commit_checked`]).

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gate_router::escaped;

use crate::environment::{STAGE_HEAD_VARIABLE, STAGE_TREE_VARIABLE};
use crate::remote::git::{self, Git, GitBounds, GitError};
use crate::slots::{self, Liveness};

/// What a commit stage's builder checked: the `HEAD` the stage sent, and the checked tree, the tree of the index once
/// the stage's fixes were applied to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Checked {
    pub head: String,
    pub tree: String,
}

/// The folder under the state root that holds the commit runs' forwarder folders.
const HOOKS_FOLDER: &str = "hooks";
/// What a forwarder folder's name ends in while it is written, before it is renamed into place.
const WRITING: &str = ".tmp";
/// What the name of a hook git ships as an example ends in, which git never runs.
const SAMPLE: &str = ".sample";
/// How long a forwarder folder whose gate is gone is kept: a process that outlived the child with its environment
/// would run no hook at all once the folder is gone.
const KEPT_FOR: Duration = Duration::from_hours(7 * 24);

/// The start of the forwarder pre-commit script, before the line naming the real hook.
const PRE_COMMIT_HEAD: &str = concat!(
    "#!/bin/sh\n",
    "# Written by gate for one commit run: skips the real pre-commit only while HEAD and the staged tree are what the builder checked.\n",
);
/// The rest of the forwarder pre-commit script: the checked tree is compared on a copy of the index, beside it.
const PRE_COMMIT_BODY: &str = r#"copy="$GIT_INDEX_FILE.gate-checked.$$"
trap 'rm -f "$copy"' EXIT
trap 'rm -f "$copy"; exit 130' INT
trap 'rm -f "$copy"; exit 143' TERM
checked() {
  [ -n "$GATE_STAGE_HEAD" ] && [ -n "$GATE_STAGE_TREE" ] && [ -n "$GIT_INDEX_FILE" ] || return 1
  head=$(git rev-parse --verify -q HEAD) || return 1
  [ "$head" = "$GATE_STAGE_HEAD" ] || return 1
  cp "$GIT_INDEX_FILE" "$copy" || return 1
  tree=$(GIT_INDEX_FILE="$copy" git write-tree) || return 1
  [ "$tree" = "$GATE_STAGE_TREE" ]
}
if checked; then exit 0; fi
rm -f "$copy"
trap - EXIT INT TERM
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 GATE_STAGE_HEAD GATE_STAGE_TREE
exec "$real" "$@"
"#;
/// The start of every other hook's forwarder script, before the line running the real hook.
const FORWARDER_HEAD: &str = concat!(
    "#!/bin/sh\n",
    "# Written by gate for one commit run: runs the real hook.\n",
    "unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 GATE_STAGE_HEAD GATE_STAGE_TREE\n",
);

/// The forwarder folder of one commit run, written before its child starts. The folder outlives the run: a process the
/// child started may outlive it with the child's environment, and would run no hook at all once the folder is gone, so
/// only [`sweep`] deletes it. [`Forwarders::end`], and [`Drop`] after a panic or an end that skipped it, remove only
/// what a write left beside it.
pub(crate) struct Forwarders {
    /// The folder.
    folder: PathBuf,
}

/// What a commit run hands its child so the hooks its stage ran on a builder are skipped while nothing changed.
pub(crate) struct Skip {
    /// The guard of the forwarder folder, which lives until the child's commit is checked; the folder itself is left for
    /// [`sweep`].
    pub forwarders: Forwarders,
    /// The variables set over the child's environment: `core.hooksPath` naming the forwarder folder through
    /// `GIT_CONFIG_COUNT`, and the checked `HEAD` and tree.
    pub variables: Vec<(&'static str, String)>,
}

/// The line a commit run says when its child runs the hooks the builder ran again, for the reason `why`.
pub(crate) fn no_skip_line(why: &str) -> String {
    format!("gate: cannot skip the hooks the builder ran ({why}); the local hooks run again")
}

/// The line a commit run says when the commit its child made cannot be checked against the checked tree, for the
/// reason `why`.
pub(crate) fn cannot_check_line(why: &str) -> String {
    format!("gate: cannot check the commit against the routed hooks' tree ({why})")
}

/// The forwarder folder of the gate `owner`, its pid and start in Unix epoch milliseconds, under the state root
/// `state_root`, written for the real hooks folder `real`, and the variables that point the child at it with what was
/// `checked`. No skip is made when the gate's own environment sets `GIT_CONFIG_COUNT` (`config_count_set`), which the
/// child's would add to.
///
/// # Errors
///
/// Why no skip is made: `GIT_CONFIG_COUNT` is set, the real folder could not be read (`real`'s error), or the forwarder
/// folder could not be written whole.
pub(crate) fn skip(
    state_root: &Path,
    owner: (u32, u64),
    config_count_set: bool,
    real: Result<PathBuf, String>,
    checked: &Checked,
) -> Result<Skip, String> {
    if config_count_set {
        return Err("GIT_CONFIG_COUNT is already set".to_owned());
    }
    let real = real?;
    let folder = forwarder_folder(state_root, owner);
    let value = unicode(&folder)?.replace('\\', "/");
    let forwarders = Forwarders::write(&folder, &real)?;
    let variables = vec![
        ("GIT_CONFIG_COUNT", "1".to_owned()),
        ("GIT_CONFIG_KEY_0", "core.hooksPath".to_owned()),
        ("GIT_CONFIG_VALUE_0", value),
        (STAGE_HEAD_VARIABLE, checked.head.clone()),
        (STAGE_TREE_VARIABLE, checked.tree.clone()),
    ];
    Ok(Skip { forwarders, variables })
}

/// The forwarder folder of the gate `(pid, start_ms)` under the state root `state_root`.
fn forwarder_folder(state_root: &Path, (pid, start_ms): (u32, u64)) -> PathBuf {
    state_root.join(HOOKS_FOLDER).join(format!("{pid}-{start_ms}"))
}

/// `path` as text, any verbatim prefix dropped; else why it cannot be, a path git and `sh` could not be given whole.
fn unicode(path: &Path) -> Result<String, String> {
    let plain = git::without_verbatim(path);
    plain
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("the folder {} is no Unicode path", plain.display()))
}

impl Forwarders {
    /// Writes `folder` with one script for each regular file of the real hooks folder `real` whose name does not end
    /// in [`SAMPLE`]: written whole beside it first, then renamed into place, so no half-written folder is ever there.
    ///
    /// # Errors
    ///
    /// Why the folder was not written: the real folder cannot be read, its path or a hook's name cannot be written into
    /// a script, or a write failed.
    fn write(folder: &Path, real: &Path) -> Result<Forwarders, String> {
        let real_text = unicode(real)?;
        if real_text.contains(UNQUOTABLE) {
            return Err(format!("the hooks folder {} holds a quote, a newline or a NUL", escaped(&real_text)));
        }
        let scripts = scripts(real, &real_text.replace('\\', "/"))?;
        let writing = writing_of(folder);
        let written = write_all(&writing, folder, &scripts);
        if let Err(error) = written {
            // The write's failure is the one said; a leftover that cannot be removed after it is swept later.
            let _ = removed(&writing);
            return Err(format!("cannot write the forwarder hooks folder {}: {error}", folder.display()));
        }
        Ok(Forwarders { folder: folder.to_owned() })
    }

    /// Removes what a write left beside the folder, the folder itself kept for [`sweep`].
    pub(crate) fn end(&self) {
        // Nothing is left to say a failure through at the run's end; the sweep removes a leftover later.
        let _ = removed(&writing_of(&self.folder));
    }
}

impl Drop for Forwarders {
    /// [`Forwarders::end`], after a panic or an end that skipped it.
    fn drop(&mut self) {
        self.end();
    }
}

/// The folder the forwarder folder `folder` is written as before it is renamed into place.
fn writing_of(folder: &Path) -> PathBuf {
    let mut writing = folder.as_os_str().to_owned();
    writing.push(WRITING);
    PathBuf::from(writing)
}

/// What a path or a hook's name written into a forwarder script between single quotes may not hold.
const UNQUOTABLE: [char; 3] = ['\'', '\n', '\0'];

/// Each forwarder script for the real hooks folder `real`, whose path the scripts name as `real_text`: its name and
/// text.
fn scripts(real: &Path, real_text: &str) -> Result<Vec<(String, String)>, String> {
    let unreadable = |error: io::Error| format!("cannot read the hooks folder {}: {error}", real.display());
    let mut scripts = Vec::new();
    for entry in fs::read_dir(real).map_err(unreadable)? {
        if let Some(script) = script(real, real_text, &entry.map_err(unreadable)?)? {
            scripts.push(script);
        }
    }
    Ok(scripts)
}

/// The forwarder script for the entry `entry` of the real hooks folder `real`, named `real_text` in the script: `None`
/// for a sample, which git never runs. Git runs every other entry as the hook its name names, and fails a commit on one
/// it cannot start, so only a regular file starting `#!` is forwarded.
///
/// # Errors
///
/// Why no folder of forwarders can stand in for the real one: the entry cannot be read, its name cannot be written into
/// a script, or it is anything but a regular file starting `#!` (a folder, a file with no `#!` start, a Windows
/// executable), which no `sh` script can stand in for.
fn script(real: &Path, real_text: &str, entry: &fs::DirEntry) -> Result<Option<(String, String)>, String> {
    let unreadable = |error: io::Error| format!("cannot read the hooks folder {}: {error}", real.display());
    let Ok(name) = entry.file_name().into_string() else {
        return Err(format!("the hooks folder {} holds a name that is no Unicode", escaped(real_text)));
    };
    if name.ends_with(SAMPLE) {
        return Ok(None);
    }
    let path = entry.path();
    let forwardable = fs::metadata(&path).map_err(unreadable)?.is_file() && start_of(&path).map_err(unreadable)? == b"#!";
    if !forwardable {
        return Err(format!(
            "the hooks folder {} holds a hook git would run but gate cannot forward: {}",
            escaped(real_text),
            escaped(&name)
        ));
    }
    if name.contains(UNQUOTABLE) {
        return Err(format!("the hook {} holds a quote, a newline or a NUL", escaped(&name)));
    }
    let text = if name == "pre-commit" {
        [PRE_COMMIT_HEAD, "real='", real_text, "/pre-commit'\n", PRE_COMMIT_BODY].concat()
    } else {
        [FORWARDER_HEAD, "exec '", real_text, "/", &name, "' \"$@\"\n"].concat()
    };
    Ok(Some((name, text)))
}

/// The first two bytes of the file `path`, fewer for a shorter file.
fn start_of(path: &Path) -> io::Result<Vec<u8>> {
    let mut start = Vec::with_capacity(2);
    fs::File::open(path)?.take(2).read_to_end(&mut start)?;
    Ok(start)
}

/// Writes `scripts` into the new folder `writing`, then renames it `folder`, either first removed when an earlier try
/// left it.
fn write_all(writing: &Path, folder: &Path, scripts: &[(String, String)]) -> io::Result<()> {
    removed(writing)?;
    removed(folder)?;
    fs::create_dir_all(writing)?;
    for (name, text) in scripts {
        fs::write(writing.join(name), text)?;
    }
    fs::rename(writing, folder)
}

/// Removes the folder `path` and everything in it; a folder that is not there is removed already.
fn removed(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Deletes the forwarder folders under the state root `state_root` whose gate is gone and that were last changed more
/// than [`KEPT_FOR`] before `now`, a folder still being written among them; a gate that cannot be checked counts as
/// live, and an entry of any other name or kind is left alone. The lines saying what could not be read or deleted.
pub(crate) fn sweep(state_root: &Path, now: SystemTime) -> Vec<String> {
    let hooks = state_root.join(HOOKS_FOLDER);
    let entries = match fs::read_dir(&hooks) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![format!("gate: cannot read the forwarder hooks folders in {}: {error}", hooks.display())],
    };
    entries
        .filter_map(|entry| match entry {
            Ok(entry) => swept(&entry.path(), now).err(),
            Err(error) => Some(format!("gate: cannot read the forwarder hooks folders in {}: {error}", hooks.display())),
        })
        .collect()
}

/// Deletes the forwarder folder `path` when [`sweep`] would.
fn swept(path: &Path, now: SystemTime) -> Result<(), String> {
    let Some((pid, start_ms)) = path.file_name().and_then(|name| name.to_str()).and_then(owner_of) else {
        return Ok(());
    };
    if !matches!(slots::gate_liveness(pid, start_ms), Liveness::Gone) {
        return Ok(());
    }
    let failed = |error: io::Error| format!("gate: cannot sweep the forwarder hooks folder {}: {error}", path.display());
    let found = fs::symlink_metadata(path).map_err(failed)?;
    let modified = found.modified().map_err(failed)?;
    if found.is_dir() && now.duration_since(modified).is_ok_and(|age| age > KEPT_FOR) {
        removed(path).map_err(failed)?;
    }
    Ok(())
}

/// The gate a forwarder folder's name `name` names: `<pid>-<start ms>`, with [`WRITING`] after it while it is written.
fn owner_of(name: &str) -> Option<(u32, u64)> {
    let name = name.strip_suffix(WRITING).unwrap_or(name);
    let (pid, start) = name.split_once('-')?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(pid) || !digits(start) {
        return None;
    }
    Some((pid.parse().ok()?, start.parse().ok()?))
}

/// Whether the commit the child left in the worktree at `root` is the one the stage `checked`, each git call of `git`
/// under `bounds`: a `HEAD` still the checked one made no commit; any other must have the checked `HEAD` as its parent
/// and the checked tree as its tree.
///
/// # Errors
///
/// The line saying the commit is not the one checked, or that it could not be checked.
pub(crate) fn commit_checked(root: &Path, git: &Git, bounds: GitBounds, checked: &Checked) -> Result<(), String> {
    let cannot = |error: GitError| cannot_check_line(&String::from(error));
    let head = git
        .output(&git::Call::bounded(root, &["rev-parse", "--verify", "HEAD"], bounds))
        .map_err(cannot)?;
    let head = head.trim_end();
    if head == checked.head {
        return Ok(());
    }
    let (parent_of, tree_of) = (format!("{head}^"), format!("{head}^{{tree}}"));
    let printed = git
        .output(&git::Call::bounded(root, &["rev-parse", &parent_of, &tree_of], bounds))
        .map_err(cannot)?;
    let mut lines = printed.lines().map(str::trim_end);
    let (Some(parent), Some(tree)) = (lines.next(), lines.next()) else {
        let why = format!("git rev-parse printed no parent and tree: {}", printed.trim());
        return Err(cannot(GitError::Failed(why)));
    };
    if parent == checked.head && tree == checked.tree {
        return Ok(());
    }
    let short = head.get(..12).unwrap_or(head);
    Err(format!(
        "gate: commit {short} is not what the routed hooks checked: expected parent {} and tree {}, got parent {parent} and tree {tree}",
        checked.head, checked.tree
    ))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its repository must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    use super::{Checked, Skip, commit_checked, no_skip_line, skip, sweep};
    use crate::commit::stage::real_hooks_folder;
    use crate::remote::git::{self, Git};
    use crate::test_support::temp_dir;

    /// A real pre-commit hook that marks its run.
    const MARKING: &str = "#!/bin/sh\necho pre-commit >> .git/ran.txt\n";
    /// A real pre-commit hook that marks its run and fails.
    const FAILING: &str = "#!/bin/sh\necho pre-commit >> .git/ran.txt\nexit 1\n";
    /// A real commit-msg hook that marks its run with its argument.
    const COMMIT_MSG: &str = "#!/bin/sh\necho \"commit-msg $1\" >> .git/ran.txt\n";
    /// The committer of every test commit.
    const COMMITTER: [&str; 6] = [
        "-c",
        "user.name=gate-test",
        "-c",
        "user.email=gate-test@localhost",
        "-c",
        "commit.gpgsign=false",
    ];

    /// What git with `args` wrote in `folder`, trimmed.
    fn git_in(folder: &Path, args: &[&str]) -> String {
        let git = Git::find(folder).expect("git is found");
        git.output(&git::Call::bounded(folder, args, git::GIT_BOUNDS))
            .expect("a test's git call succeeds")
            .trim_end()
            .to_owned()
    }

    /// A new, empty folder for the test `name`.
    fn scratch(name: &str) -> PathBuf {
        let folder = temp_dir().join(format!("gate-forward-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    /// Commits what is staged in `root` with no hook run, `extra` words given to the commit.
    fn commit_plain(root: &Path, extra: &[&str]) {
        git_in(
            root,
            &[&COMMITTER[..], &["commit", "--quiet", "--no-verify", "-m", "plain"], extra].concat(),
        );
    }

    /// A repository in the folder of the test `name`, `a.txt` committed, whose real hooks are `pre_commit` and
    /// [`COMMIT_MSG`], beside a sample; and a state root beside it.
    fn repository(name: &str, pre_commit: &str) -> (PathBuf, PathBuf) {
        let folder = scratch(name);
        let root = folder.join("repo");
        fs::create_dir_all(&root).expect("the repository's folder can be made");
        git_in(&root, &["init", "--quiet"]);
        fs::write(root.join("a.txt"), "a\n").expect("a file can be written");
        git_in(&root, &["add", "-A"]);
        commit_plain(&root, &[]);
        let hooks = root.join(".git").join("hooks");
        fs::create_dir_all(&hooks).expect("the hooks folder can be made");
        fs::write(hooks.join("pre-commit"), pre_commit).expect("the hook can be written");
        fs::write(hooks.join("commit-msg"), COMMIT_MSG).expect("the hook can be written");
        fs::write(hooks.join("pre-push.sample"), "#!/bin/sh\nexit 1\n").expect("the sample can be written");
        (root, folder.join("state"))
    }

    /// Writes `text` to `path` in `root` and stages it.
    fn stage(root: &Path, path: &str, text: &str) {
        fs::write(root.join(path), text).expect("a file can be written");
        git_in(root, &["add", "--", path]);
    }

    /// `HEAD` and the tree of the index of `root`, as a stage checks them.
    fn checked_now(root: &Path) -> Checked {
        Checked {
            head: git_in(root, &["rev-parse", "HEAD"]),
            tree: git_in(root, &["write-tree"]),
        }
    }

    /// The skip a commit run makes in `root` for `checked`, its forwarder folder under `state`.
    fn skipping(root: &Path, state: &Path, checked: &Checked) -> Skip {
        let git = Git::find(root).expect("git is found");
        let real = real_hooks_folder(root, &git, git::GIT_BOUNDS).map_err(String::from);
        skip(state, (std::process::id(), 1), false, real, checked).expect("the skip is made")
    }

    /// Whether a plain `git commit` in `root`, given `variables` over the test's environment, made its commit; no copy
    /// of the index is left beside it either way.
    fn committed(root: &Path, variables: &[(&str, String)]) -> bool {
        let mut command = Command::new("git");
        command
            .current_dir(root)
            .args(COMMITTER)
            .args(["commit", "--quiet", "-m", "staged"])
            .envs(variables.iter().map(|(name, value)| (*name, value.as_str())))
            .stdin(std::process::Stdio::null());
        let status = gate_win32::spawn(&mut command)
            .expect("git commit starts")
            .wait()
            .expect("git commit runs");
        let left: Vec<String> = fs::read_dir(root.join(".git"))
            .expect("the git folder lists")
            .map(|entry| entry.expect("an entry lists").file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".gate-checked."))
            .collect();
        assert_eq!(left, Vec::<String>::new(), "no copy of the index is left");
        status.success()
    }

    /// The marks the real hooks of `root` left, which are then cleared.
    fn ran(root: &Path) -> Vec<String> {
        let path = root.join(".git").join("ran.txt");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::remove_file(&path);
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn a_commit_of_the_checked_head_and_tree_skips_only_the_real_pre_commit() {
        let (root, state) = repository("skips", MARKING);
        stage(&root, "b.txt", "b\n");
        let checked = checked_now(&root);
        let skip = skipping(&root, &state, &checked);

        assert!(committed(&root, &skip.variables), "the commit is made");

        let marks = ran(&root);
        assert_eq!(marks.len(), 1, "only commit-msg ran: {marks:?}");
        assert!(
            marks[0].starts_with("commit-msg ") && marks[0].ends_with("COMMIT_EDITMSG"),
            "commit-msg ran with its argument: {marks:?}"
        );
        assert_eq!(git_in(&root, &["rev-parse", "HEAD^"]), checked.head, "the commit is on the checked HEAD");
    }

    #[test]
    fn a_file_staged_after_the_check_runs_the_real_pre_commit() {
        let (root, state) = repository("staged-after", MARKING);
        stage(&root, "b.txt", "b\n");
        let checked = checked_now(&root);
        let skip = skipping(&root, &state, &checked);
        stage(&root, "c.txt", "c\n");

        assert!(committed(&root, &skip.variables));

        assert_eq!(ran(&root)[0], "pre-commit");
    }

    #[test]
    fn a_head_moved_after_the_check_runs_the_real_pre_commit() {
        let (root, state) = repository("head-moved", MARKING);
        stage(&root, "b.txt", "b\n");
        let checked = checked_now(&root);
        let skip = skipping(&root, &state, &checked);
        commit_plain(&root, &[]);
        stage(&root, "c.txt", "c\n");

        assert!(committed(&root, &skip.variables));

        assert_eq!(ran(&root)[0], "pre-commit");
    }

    #[test]
    fn an_unset_checked_tree_or_an_empty_checked_head_runs_the_real_pre_commit() {
        let (root, state) = repository("unset", MARKING);
        stage(&root, "b.txt", "b\n");
        let skip = skipping(&root, &state, &checked_now(&root));
        let no_tree: Vec<(&str, String)> = skip.variables.iter().filter(|(name, _)| *name != "GATE_STAGE_TREE").cloned().collect();

        assert!(committed(&root, &no_tree));
        assert_eq!(ran(&root)[0], "pre-commit", "with no checked tree");

        stage(&root, "c.txt", "c\n");
        let skip = skipping(&root, &state, &checked_now(&root));
        let empty_head: Vec<(&str, String)> = skip
            .variables
            .iter()
            .map(|(name, value)| (*name, if *name == "GATE_STAGE_HEAD" { String::new() } else { value.clone() }))
            .collect();

        assert!(committed(&root, &empty_head));
        assert_eq!(ran(&root)[0], "pre-commit", "with an empty checked HEAD");
    }

    #[test]
    fn a_failing_real_pre_commit_under_a_mismatch_fails_the_commit() {
        let (root, state) = repository("failing", FAILING);
        stage(&root, "b.txt", "b\n");
        let checked = checked_now(&root);
        let skip = skipping(&root, &state, &checked);
        stage(&root, "c.txt", "c\n");

        assert!(!committed(&root, &skip.variables), "the real pre-commit failed the commit");

        assert_eq!(ran(&root), ["pre-commit"]);
        assert_eq!(git_in(&root, &["rev-parse", "HEAD"]), checked.head, "no commit was made");
    }

    /// The names in `folder`, sorted.
    fn names_in(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .expect("the folder lists")
            .map(|entry| entry.expect("an entry lists").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_writer_forwards_every_shebang_hook_but_the_samples_and_leaves_no_temporary_folder() {
        let (root, state) = repository("writer", MARKING);
        let real = root.join(".git").join("hooks");
        fs::write(real.join("commit-msg.sample"), "no shebang\n").expect("a sample can be written");
        let skip = skipping(&root, &state, &checked_now(&root));
        let folder = state.join("hooks").join(format!("{}-1", std::process::id()));

        assert_eq!(
            names_in(&state.join("hooks")),
            [format!("{}-1", std::process::id())],
            "no temporary folder"
        );
        assert_eq!(names_in(&folder), ["commit-msg", "pre-commit"]);
        let shown = git::without_verbatim(&real).to_string_lossy().replace('\\', "/");
        let forwarder = fs::read_to_string(folder.join("commit-msg")).expect("the forwarder reads");
        let expected = format!(
            "#!/bin/sh\n# Written by gate for one commit run: runs the real hook.\n\
             unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 GATE_STAGE_HEAD GATE_STAGE_TREE\n\
             exec '{shown}/commit-msg' \"$@\"\n"
        );
        assert_eq!(forwarder, expected);
        let pre_commit = fs::read_to_string(folder.join("pre-commit")).expect("the forwarder reads");
        assert!(pre_commit.contains(&format!("\nreal='{shown}/pre-commit'\n")), "{pre_commit}");
        assert!(!pre_commit.contains('\r'), "LF line endings");
        let value = skip
            .variables
            .iter()
            .find(|(name, _)| *name == "GIT_CONFIG_VALUE_0")
            .map(|(_, value)| value.clone());
        assert_eq!(value, Some(git::without_verbatim(&folder).to_string_lossy().replace('\\', "/")));

        skip.forwarders.end();
        assert!(folder.exists(), "end keeps the folder for the sweep");
    }

    #[test]
    fn the_writer_writes_no_pre_commit_when_the_real_folder_has_none_and_drop_keeps_the_folder_but_not_a_leftover() {
        let (root, state) = repository("no-pre-commit", MARKING);
        fs::remove_file(root.join(".git").join("hooks").join("pre-commit")).expect("the hook can be removed");
        let folder = state.join("hooks").join(format!("{}-1", std::process::id()));
        let leftover = state.join("hooks").join(format!("{}-1.tmp", std::process::id()));

        let skip = skipping(&root, &state, &checked_now(&root));
        assert_eq!(names_in(&folder), ["commit-msg"]);
        fs::create_dir_all(&leftover).expect("a leftover can be made");
        drop(skip);

        assert!(folder.exists(), "drop keeps the folder for the sweep");
        assert!(!leftover.exists(), "drop removes a write's leftover");
    }

    #[test]
    fn the_writer_refuses_every_hook_that_is_no_shebang_file() {
        let (root, state) = repository("starts", MARKING);
        let real = root.join(".git").join("hooks");
        let shown = git::without_verbatim(&real).to_string_lossy().replace('\\', "/");
        let commit_msg = real.join("commit-msg");
        let cases: [(&str, &str, &[u8]); 4] = [
            ("a folder", "commit-msg", b""),
            ("no shebang", "commit-msg", b"echo no shebang\n"),
            ("a BOM before the shebang", "commit-msg", b"\xEF\xBB\xBF#!/bin/sh\nexit 0\n"),
            ("a Windows executable", "commit-msg.exe", b"MZ\x90\0"),
        ];

        for ((what, name, bytes), start) in cases.into_iter().zip(10_u64..) {
            let _ = fs::remove_file(&commit_msg);
            let _ = fs::remove_dir(&commit_msg);
            let _ = fs::remove_file(real.join("commit-msg.exe"));
            if what == "a folder" {
                fs::create_dir(&commit_msg).expect("a folder can be made among the hooks");
            } else {
                fs::write(real.join(name), bytes).expect("the hook can be written");
            }
            let owner = (std::process::id(), start);

            let refused = skip(&state, owner, false, Ok(real.clone()), &checked_now(&root)).err();

            let why = format!("the hooks folder {shown} holds a hook git would run but gate cannot forward: {name}");
            assert_eq!(refused.as_deref(), Some(why.as_str()), "{what}");
            assert!(
                !state.join("hooks").join(format!("{}-{}", owner.0, owner.1)).exists(),
                "no folder for {what}"
            );
        }
    }

    #[test]
    fn the_writer_refuses_a_hook_whose_name_holds_a_quote_naming_it_as_it_is() {
        let (root, state) = repository("quoted-name", MARKING);
        let real = root.join(".git").join("hooks");
        fs::write(real.join("it's-hook"), MARKING).expect("the hook can be written");

        let refused = skip(&state, (std::process::id(), 1), false, Ok(real), &checked_now(&root)).err();

        assert_eq!(refused.as_deref(), Some("the hook it's-hook holds a quote, a newline or a NUL"));
    }

    #[test]
    fn the_writer_refuses_a_real_folder_whose_path_holds_a_quote() {
        let folder = scratch("quote");
        let real = folder.join("it's-hooks");
        fs::create_dir_all(&real).expect("the folder can be made");
        fs::write(real.join("pre-commit"), MARKING).expect("the hook can be written");
        let state = folder.join("state");
        let checked = Checked {
            head: "h".to_owned(),
            tree: "t".to_owned(),
        };

        let refused = skip(&state, (std::process::id(), 1), false, Ok(real), &checked).err();

        assert!(refused.is_some_and(|why| why.contains("holds a quote")), "the path is refused");
        assert!(!state.join("hooks").join(format!("{}-1", std::process::id())).exists(), "no folder");
        assert!(
            !state.join("hooks").join(format!("{}-1.tmp", std::process::id())).exists(),
            "no temporary folder"
        );
    }

    #[test]
    fn no_skip_is_made_when_git_config_count_is_set_or_the_real_folder_cannot_be_read() {
        let state = scratch("no-skip").join("state");
        let checked = Checked {
            head: "h".to_owned(),
            tree: "t".to_owned(),
        };

        let set = skip(&state, (1, 2), true, Ok(state.clone()), &checked).err();
        let unread = skip(&state, (1, 2), false, Err("cannot read".to_owned()), &checked).err();

        assert_eq!(set.as_deref(), Some("GIT_CONFIG_COUNT is already set"));
        assert_eq!(unread.as_deref(), Some("cannot read"));
        assert!(!state.join("hooks").exists(), "nothing is written");
        assert_eq!(
            no_skip_line("GIT_CONFIG_COUNT is already set"),
            "gate: cannot skip the hooks the builder ran (GIT_CONFIG_COUNT is already set); the local hooks run again"
        );
    }

    #[test]
    fn the_sweep_deletes_only_a_gone_gate_s_folders_a_week_old() {
        let state = scratch("sweep").join("state");
        let hooks = state.join("hooks");
        let pid = std::process::id();
        let start = gate_win32::process_start_of(pid).expect("the test's start reads").expect("the test runs");
        let own = format!("{pid}-{}", crate::slots::epoch_ms(start));
        let names = [
            format!("{pid}-1"),
            format!("{pid}-2.tmp"),
            own.clone(),
            "other".to_owned(),
            "12-ab".to_owned(),
        ];
        for name in &names {
            fs::create_dir_all(hooks.join(name)).expect("a folder can be made");
        }

        assert_eq!(sweep(&state, SystemTime::now()), Vec::<String>::new());
        assert_eq!(names_in(&hooks).len(), 5, "a gone gate's folder younger than a week is kept");

        let later = SystemTime::now() + Duration::from_hours(8 * 24);
        assert_eq!(sweep(&state, later), Vec::<String>::new());
        let mut kept = vec!["12-ab".to_owned(), own, "other".to_owned()];
        kept.sort();
        assert_eq!(names_in(&hooks), kept, "a live gate's folder and other names are kept");
    }

    #[test]
    fn the_check_after_the_child_passes_no_commit_and_the_checked_one_only() {
        let (root, _) = repository("check", MARKING);
        let git = Git::find(&root).expect("git is found");
        let check = |checked: &Checked| commit_checked(&root, &git, git::GIT_BOUNDS, checked);
        stage(&root, "b.txt", "b\n");
        let checked = checked_now(&root);

        assert_eq!(check(&checked), Ok(()), "no commit was made");
        commit_plain(&root, &[]);
        assert_eq!(check(&checked), Ok(()), "the commit is on the checked HEAD with the checked tree");

        let mid = git_in(&root, &["rev-parse", "HEAD"]);
        commit_plain(&root, &["--allow-empty"]);
        let head = git_in(&root, &["rev-parse", "HEAD"]);
        let parent_line = format!(
            "gate: commit {} is not what the routed hooks checked: expected parent {} and tree {}, got parent {mid} and tree {}",
            &head[..12],
            checked.head,
            checked.tree,
            checked.tree
        );
        assert_eq!(check(&checked), Err(parent_line), "another parent");

        let on_mid = Checked {
            head: mid.clone(),
            tree: checked.tree.clone(),
        };
        let other = Checked {
            head: head.clone(),
            tree: checked.tree.clone(),
        };
        stage(&root, "c.txt", "c\n");
        commit_plain(&root, &[]);
        let last = git_in(&root, &["rev-parse", "HEAD"]);
        let tree = git_in(&root, &["rev-parse", "HEAD^{tree}"]);
        let tree_line = format!(
            "gate: commit {} is not what the routed hooks checked: expected parent {head} and tree {}, got parent {head} and tree {tree}",
            &last[..12],
            checked.tree
        );
        assert_eq!(check(&other), Err(tree_line), "another tree");
        assert!(check(&on_mid).is_err(), "a commit two past the checked HEAD");
    }
}
