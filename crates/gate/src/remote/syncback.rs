//! The client's sync-back: after a run call that ended with the command's own status, the changes the command left on
//! the builder are fetched from `refs/gate-back/<key>` and applied to the worktree when it still matches the snapshot.
//! The builder is trusted less than this PC. A fetched commit that is not the one the builder advertised, or that was
//! not made from this snapshot, is ignored. One that touches a `.git*` path or makes a link or a submodule, or one the
//! worktree changed under during the run, is kept at `refs/gate-remote/<key>-back` and fails a run that had passed;
//! one that a step before its apply could not take further, as it failed or was interrupted, is kept there too, and
//! the line that ends the run says where to see its changes.
//! The two git calls that reach the builder take the pre-banner retry and keep their lines out of the log; any other
//! git failure is the run's, status 1 and `gate: the remote run failed: ...`, with no fallback. Every name and git line
//! said here has its control characters escaped.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

use super::client::{Attempt, Called, Echo, End, Watch};
use super::git::{self, Git, GitError};
use super::payload;
use super::retry::GIT_SSH_FAILED;
use super::snapshot::{self, BACK, REFS, Snapshot};
use crate::boxside::payload::escaped;

/// The patch an applied sync-back is written to in the git folder, named for the process and a nonce, and removed
/// whether or not it applied.
const PATCH: &str = "gate-remote-";
const PATCH_SUFFIX: &str = ".patch";
/// The modes of a symbolic link and of a submodule, which no entry of a back commit may take.
const LINK_MODES: [&str; 2] = ["120000", "160000"];
/// What the name of a path's component starts with, its letter case aside, when git reads the file itself.
const GIT_NAME: &str = ".git";

/// Fetches and applies the builder's changes for `snapshot`, or ends the run as `status` when there is nothing to do,
/// the run's own when the changes were ignored, and 1 when a passing run's changes could not be applied.
pub(super) fn sync_back(attempt: &mut Attempt<'_, '_>, snapshot: &Snapshot, status: i64) -> Result<End, End> {
    attempt.go_on()?;
    let target = attempt.transport.push_target(attempt.call.host, attempt.call.repo);
    let pack = attempt.transport.upload_pack();
    let reference = format!("refs/gate-back/{}", attempt.call.key);
    let remote = Remote {
        url: &target.url,
        pack: pack.as_deref(),
        ssh: target.ssh_command.as_deref(),
        reference: &reference,
    };
    let git = Git::find(Path::new(attempt.call.root)).map_err(End::Failed)?;
    let Some(advertised) = has_back(attempt, &git, &remote)? else {
        return Ok(End::Status(status));
    };
    fetch(attempt, &git, &remote)?;
    let back = line(attempt, &git, &["rev-parse", "FETCH_HEAD"])?;
    if back != advertised || !made_from(attempt, &git, &back, &snapshot.commit)? {
        attempt.say(ignored_line(&back, &snapshot.commit));
        return Ok(End::Status(status));
    }
    receive(attempt, &git, snapshot, &back, status)
}

/// The back commit the builder advertises for this run, from `ls-remote` of its ref; `None` when it holds none.
fn has_back(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>) -> Result<Option<String>, End> {
    let ls_remote = Network {
        verb: "ls-remote",
        watch: Watch::Deadline(attempt.deadline_for("ls-remote", attempt.limit)),
    };
    let called = to_the_builder(attempt, git, remote, &ls_remote, &remote.args(&["ls-remote"]))?;
    Ok(advertised(&called.lines, remote.reference))
}

/// Fetches the back ref the builder holds into `FETCH_HEAD`, every object it sends checked, and no tag or submodule.
/// It says its progress, which a live fetch keeps sending on standard error, so only a fetch that went quiet stalls.
fn fetch(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>) -> Result<(), End> {
    let words = [
        "-c",
        "fetch.fsckObjects=true",
        "fetch",
        "--progress",
        "--no-recurse-submodules",
        "--no-tags",
    ];
    let fetch = Network {
        verb: "fetch",
        watch: Watch::Fetch(attempt.limits_for("fetch")),
    };
    to_the_builder(attempt, git, remote, &fetch, &remote.args(&words)).map(drop)
}

/// One git call of the sync-back that reaches the builder: its verb, and how it is watched. `ls-remote` gets the git
/// limit; `fetch`, which may take long, ends once nothing came from it for the run's stall window, or at the run's
/// ceiling.
struct Network {
    verb: &'static str,
    watch: Watch,
}

/// git with `args`, the call `network` that reaches the builder: retried once when it failed before the banner, its
/// lines kept out of the log, and killed as its watch says or at a line longer than the cap. A failure is the run's,
/// or its interrupt when the console interrupt was raised during the call.
fn to_the_builder(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>, network: &Network, args: &[String]) -> Result<Called, End> {
    let (called, _) = attempt.with_retry(GIT_SSH_FAILED, |attempt| {
        attempt.git(git, args, remote.ssh, Echo::Unlogged, network.watch)
    })?;
    if let Some(cut) = called.cut {
        attempt.noted(&called);
        return Err(End::Failed(escaped(&cut.reason(&format!("git {}", network.verb)))));
    }
    if called.status != 0 {
        attempt.go_on()?;
        return Err(git_failure(attempt.call.root, args, &called));
    }
    Ok(called)
}

/// Whether `back` was made from `snapshot`, as its `rev-list --parents` line says.
fn made_from(attempt: &Attempt<'_, '_>, git: &Git, back: &str, snapshot: &str) -> Result<bool, End> {
    let parents = line(attempt, git, &["rev-list", "--parents", "-n", "1", back])?;
    Ok(from_snapshot(&parents, snapshot))
}

/// Applies the back commit `back`, or keeps it when it touches what a sync-back never writes or the worktree no longer
/// matches the snapshot. A step that fails or is interrupted before the patch applied keeps it as well, so its changes
/// are not lost; a patch that applied but cannot be removed then fails the run with nothing kept.
fn receive(attempt: &mut Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &str, status: i64) -> Result<End, End> {
    let commit = snapshot.commit.as_str();
    let refused = refusal(attempt, git, snapshot, back).map_err(|failed| kept_after(attempt, git, commit, back, failed))?;
    if let Some(said) = refused {
        keep(attempt, git, back).map_err(End::Failed)?;
        attempt.say(said);
        return Ok(End::Status(if status == 0 { 1 } else { status }));
    }
    let (names, patch) = apply(attempt, git, snapshot, back).map_err(|failed| kept_after(attempt, git, commit, back, failed))?;
    remove(&patch)?;
    attempt.say(synced_line(&names));
    Ok(End::Status(status))
}

/// Why `back` is kept rather than applied: the line said for a back commit that touches what a sync-back never writes,
/// or for one the worktree changed under during the run; `None` when it may be applied.
fn refusal(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &str) -> Result<Option<String>, End> {
    let listing = git_call(attempt, git, &["diff-tree", "-r", "-z", "--raw", &snapshot.commit, back])?;
    Ok(match forbidden(&listing) {
        Some(path) => Some(touched_line(&path, &snapshot.commit, back)),
        None if changed(attempt, git, snapshot)? => Some(not_applied_line(&snapshot.commit, back)),
        None => None,
    })
}

/// How the run ends once `back`, which a step before its apply could not take past `failed`, is kept; what
/// [`after_keep`] decides, and its line said.
fn kept_after(attempt: &mut Attempt<'_, '_>, git: &Git, snapshot: &str, back: &str, failed: End) -> End {
    let kept = keep(attempt, git, back);
    let (end, said) = after_keep(failed, kept, &see_diff(snapshot, back));
    if let Some(line) = said {
        attempt.say(line);
    }
    end
}

/// How a run that a step before the apply ended at `failed` ends once the back commit's keep came out as `kept`, and
/// the line it says then: a failure's reason says the changes are kept and where to `see` them, or that they could not
/// be kept and why; an interrupt whose changes are kept stays an interrupt and says so, and one whose changes could
/// not be kept is a failure, since they are lost. No reason is dropped: any other end stands, and a keep that failed
/// beside it is said.
fn after_keep(failed: End, kept: Result<(), String>, see: &str) -> (End, Option<String>) {
    match (failed, kept) {
        (End::Failed(why), Ok(())) => (End::Failed(format!("{why}; the changes are kept: {see}")), None),
        (End::Failed(why), Err(not_kept)) => (End::Failed(format!("{why}; the changes could not be kept either: {not_kept}")), None),
        (End::Interrupted, Ok(())) => (
            End::Interrupted,
            Some(format!("gate: the run was interrupted; the changes are kept: {see}")),
        ),
        (End::Interrupted, Err(not_kept)) => (
            End::Failed(format!("the run was interrupted, and the changes could not be kept: {not_kept}")),
            None,
        ),
        (other, Ok(())) => (other, None),
        (other, Err(not_kept)) => (other, Some(format!("gate: the changes could not be kept: {not_kept}"))),
    }
}

/// `see git diff <snapshot> <back>`, the two commits' short shas: where every line that keeps a back commit points.
fn see_diff(snapshot: &str, back: &str) -> String {
    format!("see git diff {} {}", short(snapshot), short(back))
}

/// Whether the worktree no longer matches the snapshot: the tree `git add -A` would write for it now is not the
/// snapshot's own.
fn changed(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot) -> Result<bool, End> {
    let now = snapshot::worktree(Path::new(attempt.call.root), attempt.limit, attempt.call.interrupt).map_err(escaped_end)?;
    let then = line(attempt, git, &["rev-parse", &format!("{}^{{tree}}", snapshot.commit)])?;
    Ok(now != then)
}

/// Keeps `back` at the worktree's `refs/gate-remote/<key>-back`, for the changes a run could not apply; else why not,
/// escaped.
fn keep(attempt: &Attempt<'_, '_>, git: &Git, back: &str) -> Result<(), String> {
    let name = format!("{REFS}{}{BACK}", attempt.call.key);
    let args = ["update-ref", name.as_str(), back];
    let call = git::Call::locked(Path::new(attempt.call.root), &args, attempt.limit);
    git.output(&call).map(drop).map_err(|error| escaped(&String::from(error)))
}

/// Writes the back commit's changes as a patch into the git folder and applies it to the worktree; the names of the
/// files it changed, and the patch, which the caller removes. A patch that did not apply is removed here, and the
/// apply's failure is the one the run ends with.
fn apply(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &str) -> Result<(Vec<String>, PathBuf), End> {
    let names = names(attempt, git, snapshot, back)?;
    let patch = patch_file(attempt, git)?;
    let shown = patch.display().to_string();
    let output = format!("--output={shown}");
    let diff = ["diff-tree", "-p", "--binary", "--full-index", "-r", &output, &snapshot.commit, back];
    let made = git_call(attempt, git, &diff);
    // NTFS protection refuses a path that names `.git` by its 8.3 short name or an alternate stream, whatever the
    // repository's own setting.
    let apply = ["-c", "core.protectNTFS=true", "apply", "--whitespace=nowarn", "--", &shown];
    match made.and_then(|_| locked_call(attempt, git, &apply)) {
        Ok(_) => Ok((names, patch)),
        Err(failed) => {
            // The apply's failure is the run's; a patch that cannot be removed after it is the lesser problem.
            drop(remove(&patch));
            Err(failed)
        }
    }
}

/// The names of the files the back commit `back` changes, against the snapshot, as git's plumbing lists them.
fn names(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &str) -> Result<Vec<String>, End> {
    let listed = git_call(attempt, git, &["diff-tree", "-r", "-z", "--name-only", &snapshot.commit, back])?;
    Ok(listed.split('\0').filter(|name| !name.is_empty()).map(str::to_owned).collect())
}

/// A new, empty file in the git folder for this run's patch, named for the process and a nonce so no other process
/// can name it beforehand; one already there fails the run.
fn patch_file(attempt: &Attempt<'_, '_>, git: &Git) -> Result<PathBuf, End> {
    let name = format!("{PATCH}{}-{}{PATCH_SUFFIX}", std::process::id(), payload::job_nonce());
    let printed = line(attempt, git, &["rev-parse", "--git-path", &name])?;
    let path = Path::new(attempt.call.root).join(printed);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| End::Failed(format!("cannot make the patch file {}: {error}", path.display())))?;
    Ok(path)
}

/// Removes the patch; a patch that cannot be removed fails the run, as PowerShell's `Remove-Item` would.
fn remove(patch: &Path) -> Result<(), End> {
    match std::fs::remove_file(patch) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(End::Failed(format!("cannot remove {}: {error}", patch.display()))),
        _ => Ok(()),
    }
}

/// Runs git with `args` in the worktree and returns what it wrote on standard output; a failure is the run's.
fn git_call(attempt: &Attempt<'_, '_>, git: &Git, args: &[&str]) -> Result<String, End> {
    let call = git::Call {
        interrupt: attempt.call.interrupt,
        ..git::Call::plain(Path::new(attempt.call.root), args, attempt.limit)
    };
    git.output(&call).map_err(escaped_end)
}

/// Runs git with `args` in the worktree as [`git_call`] does, but as a [`git::Call::locked`] call, which no Ctrl+C
/// ends while it holds a lock in the git folder.
fn locked_call(attempt: &Attempt<'_, '_>, git: &Git, args: &[&str]) -> Result<String, End> {
    let call = git::Call::locked(Path::new(attempt.call.root), args, attempt.limit);
    git.output(&call).map_err(escaped_end)
}

/// A git failure as the attempt's end, its text escaped: an interrupt is the run's interrupt, any other a failure.
fn escaped_end(error: GitError) -> End {
    match error {
        GitError::Interrupted => End::Interrupted,
        GitError::Failed(why) => End::Failed(escaped(&why)),
    }
}

/// The first line of what git with `args` wrote on standard output: what every call that asks git for one value reads.
fn line(attempt: &Attempt<'_, '_>, git: &Git, args: &[&str]) -> Result<String, End> {
    git_call(attempt, git, args)?
        .lines()
        .next()
        .map(|line| line.trim_end().to_owned())
        .ok_or_else(|| End::Failed(format!("git {} printed nothing", args.join(" "))))
}

/// The git call of the sync-back that failed, as the attempt's own failure: no fallback, the run's status 1.
fn git_failure(root: &str, args: &[String], called: &Called) -> End {
    End::Failed(escaped(&format!(
        "git {} failed in {root} (status {}): {}",
        args.join(" "),
        called.status,
        git::failure_text(&called.lines)
    )))
}

/// The hash `ls-remote` printed among its `lines` for exactly `reference`: a ref that only ends in it is another's.
fn advertised(lines: &[String], reference: &str) -> Option<String> {
    lines.iter().find_map(|line| {
        let (hash, name) = line.split_once('\t')?;
        (name == reference).then(|| hash.to_owned())
    })
}

/// The first path the back commit's `diff-tree -r -z --raw` listing names that a sync-back never writes: one with a
/// component whose name starts with `.git`, its letter case aside, or one the back commit makes a symbolic link or a
/// submodule. `None` when every path may be written.
fn forbidden(listing: &str) -> Option<String> {
    let mut fields = listing.split('\0');
    while let Some(field) = fields.next() {
        let Some(meta) = field.strip_prefix(':') else {
            continue;
        };
        let words: Vec<&str> = meta.split(' ').collect();
        let link = words.get(1).is_some_and(|mode| LINK_MODES.contains(mode));
        // A rename or a copy names its source and its destination; a plain change names one path.
        let paths = if words.get(4).is_some_and(|change| change.starts_with(['R', 'C'])) {
            2
        } else {
            1
        };
        if let Some(path) = fields.by_ref().take(paths).find(|path| link || is_git_s(path)) {
            return Some(path.to_owned());
        }
    }
    None
}

/// Whether `path` has a component whose name starts with `.git`, its letter case aside.
fn is_git_s(path: &str) -> bool {
    path.split('/')
        .any(|part| part.get(..GIT_NAME.len()).is_some_and(|start| start.eq_ignore_ascii_case(GIT_NAME)))
}

/// Whether the back commit whose `rev-list --parents -n 1` line is `parents` was made from `snapshot`: one parent, and
/// it is the snapshot itself.
pub(super) fn from_snapshot(parents: &str, snapshot: &str) -> bool {
    let line = parents.lines().next().unwrap_or_default();
    let mut words = line.split(' ').skip(1);
    matches!((words.next(), words.next()), (Some(parent), None) if parent == snapshot)
}

/// The line said for a back commit not made from this snapshot, or not the one the builder advertised, which is left
/// where it is.
pub(super) fn ignored_line(back: &str, snapshot: &str) -> String {
    format!(
        "gate: ignored stale remote changes {}: not made from snapshot {}",
        short(back),
        short(snapshot)
    )
}

/// The line said for a back commit the worktree changed under, named so the changes can be read with `git diff`.
pub(super) fn not_applied_line(snapshot: &str, back: &str) -> String {
    format!(
        "gate: remote changes not applied: the worktree changed during the run; {}",
        see_diff(snapshot, back)
    )
}

/// The line said for a back commit that touches `path`, which a sync-back never writes.
fn touched_line(path: &str, snapshot: &str, back: &str) -> String {
    format!(
        "gate: remote changes not applied: they touch {}, which a sync-back never writes; {}",
        escaped(path),
        see_diff(snapshot, back)
    )
}

/// The line said for the changes applied to the worktree, saying `0 file(s)` and nothing after the colon when the back
/// commit changed none, as PowerShell joins no names.
pub(super) fn synced_line(names: &[String]) -> String {
    let shown: Vec<String> = names.iter().map(|name| escaped(name)).collect();
    format!("gate: synced back {} file(s): {}", names.len(), shown.join(", "))
}

/// The first eight hex digits of a commit sha, or the whole sha when it is shorter.
fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// Where the sync-back's two calls that reach the builder go: the push's URL and ssh, the fetch's own pack command,
/// and the ref the back commit lives at.
struct Remote<'a> {
    url: &'a str,
    pack: Option<&'a str>,
    ssh: Option<&'a str>,
    reference: &'a str,
}

impl Remote<'_> {
    /// The words of a call: its `words`, the pack command when there is one, then `--`, the URL and the ref.
    fn args(&self, words: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        args.extend(self.pack.map(|pack| format!("--upload-pack={pack}")));
        args.extend(["--".to_owned(), self.url.to_owned(), self.reference.to_owned()]);
        args
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose interrupt, transport or git cannot be had must fail loudly")]
mod tests {
    use std::path::Path;

    use gate_state::StateRoot;
    use gate_win32::Interrupt;

    use super::{
        Remote, advertised, after_keep, fetch, forbidden, from_snapshot, has_back, ignored_line, not_applied_line, see_diff, synced_line,
        touched_line,
    };
    use crate::remote::RemoteCall;
    use crate::remote::client::{Attempt, End};
    use crate::remote::git::Git;
    use crate::remote::transport::Local;

    const BACK: &str = "8e8777a0e71c530018dc15bdebd7288f4f432aa4";
    const SNAPSHOT: &str = "b8a4812193314ab4b2404479b93f2338078c69eb";
    /// Where the lines of a kept back commit point, for [`SNAPSHOT`] and [`BACK`].
    const SEE: &str = "see git diff b8a48121 8e8777a0";

    #[test]
    fn a_failure_whose_changes_are_kept_says_where_to_see_them() {
        assert_eq!(see_diff(SNAPSHOT, BACK), SEE);
        let (end, said) = after_keep(End::Failed("git apply failed".to_owned()), Ok(()), SEE);
        assert!(
            matches!(&end, End::Failed(why) if why == "git apply failed; the changes are kept: see git diff b8a48121 8e8777a0"),
            "{end:?}"
        );
        assert_eq!(said, None, "the failure's own line says it");
    }

    #[test]
    fn a_failure_whose_changes_could_not_be_kept_says_both_reasons() {
        let (end, said) = after_keep(End::Failed("git apply failed".to_owned()), Err("git update-ref failed".to_owned()), SEE);
        assert!(
            matches!(&end, End::Failed(why) if why == "git apply failed; the changes could not be kept either: git update-ref failed"),
            "{end:?}"
        );
        assert_eq!(said, None);
    }

    #[test]
    fn an_interrupt_whose_changes_are_kept_stays_an_interrupt_and_says_where_they_are() {
        let (end, said) = after_keep(End::Interrupted, Ok(()), SEE);
        assert!(matches!(end, End::Interrupted), "{end:?}");
        assert_eq!(
            said.as_deref(),
            Some("gate: the run was interrupted; the changes are kept: see git diff b8a48121 8e8777a0")
        );
    }

    #[test]
    fn an_interrupt_whose_changes_could_not_be_kept_fails_with_both() {
        let (end, said) = after_keep(End::Interrupted, Err("git update-ref failed".to_owned()), SEE);
        assert!(
            matches!(&end, End::Failed(why) if why == "the run was interrupted, and the changes could not be kept: git update-ref failed"),
            "{end:?}"
        );
        assert_eq!(said, None);
    }

    #[test]
    fn any_other_end_stands_and_a_failed_keep_beside_it_is_said() {
        let (end, said) = after_keep(End::Status(3), Err("git update-ref failed".to_owned()), SEE);
        assert!(matches!(end, End::Status(3)), "{end:?}");
        assert_eq!(said.as_deref(), Some("gate: the changes could not be kept: git update-ref failed"));
        let (end, said) = after_keep(End::Status(3), Ok(()), SEE);
        assert!(matches!(end, End::Status(3)), "{end:?}");
        assert_eq!(said, None);
    }

    /// git's call to the builder fails while the console interrupt is raised, as a Ctrl+C ends the git it reaches: the
    /// run is an interrupt, not the client's failure.
    #[test]
    fn a_ctrl_c_during_a_sync_back_call_to_the_builder_is_an_interrupt() {
        let folder = std::env::temp_dir();
        let root = folder.to_string_lossy();
        let log = folder.join(format!("gate-sync-back-interrupt-{}.log", std::process::id()));
        let state = StateRoot::new(r"C:\gate-sync-back-tests", None);
        let interrupt = Interrupt::install().expect("the console handler is installed");
        interrupt.raise();
        let call = RemoteCall {
            host: "builder",
            key: "app-1a2b3c4d",
            root: &root,
            repo: "app",
            subfolder: "",
            words: &[],
            outputs: &[],
            log: &log,
            timeout: 60,
            stall: 60,
            tail: 0,
            no_markers: false,
            interrupt: Some(interrupt),
            state: &state,
        };
        let transport = Local::new(r"local:C:\gate-sync-back-tests", None).expect("a local transport");
        let mut attempt = Attempt::for_tests(&call, &transport);
        let git = Git::find(Path::new(&*root)).expect("git is found");
        let remote = Remote {
            url: r"C:\gate-sync-back-tests\no-such.git",
            pack: None,
            ssh: None,
            reference: "refs/gate-back/app-1a2b3c4d",
        };

        let listed = has_back(&mut attempt, &git, &remote);
        let fetched = fetch(&mut attempt, &git, &remote);

        assert!(matches!(listed, Err(End::Interrupted)), "the ls-remote: {listed:?}");
        assert!(matches!(fetched, Err(End::Interrupted)), "the fetch: {fetched:?}");
        std::fs::remove_file(&log).expect("the log the calls opened can be removed");
    }

    #[test]
    fn a_back_commit_of_this_snapshot_alone_is_one_parent_and_it() {
        assert!(from_snapshot(&format!("{BACK} {SNAPSHOT}"), SNAPSHOT), "one parent, the snapshot");
        assert!(
            !from_snapshot(&format!("{BACK} 6a1c2f3d4e5f60718293a4b5c6d7e8f90a1b2c3d"), SNAPSHOT),
            "another parent"
        );
        assert!(!from_snapshot(BACK, SNAPSHOT), "no parent at all");
        assert!(!from_snapshot("", SNAPSHOT), "no line");
        assert!(
            !from_snapshot(&format!("{BACK} {SNAPSHOT} 6a1c2f3d4e5f60718293a4b5c6d7e8f90a1b2c3d"), SNAPSHOT),
            "two parents"
        );
    }

    #[test]
    fn only_the_ref_named_whole_is_the_advertised_back_commit() {
        let lines = |texts: &[&str]| texts.iter().map(|text| (*text).to_owned()).collect::<Vec<_>>();
        let reference = "refs/gate-back/app-1a2b3c4d";
        assert_eq!(advertised(&lines(&[&format!("{BACK}\t{reference}")]), reference), Some(BACK.to_owned()));
        assert_eq!(
            advertised(
                &lines(&[&format!("{SNAPSHOT}\trefs/gate-back/x/{reference}"), &format!("{BACK}\t{reference}")]),
                reference
            ),
            Some(BACK.to_owned()),
            "a ref that ends in the name is another's"
        );
        assert_eq!(
            advertised(&lines(&[&format!("{SNAPSHOT}\trefs/gate-back/x/{reference}")]), reference),
            None
        );
        assert_eq!(advertised(&lines(&["warning: something", ""]), reference), None, "no ref line");
    }

    /// One entry of `diff-tree -r -z --raw`: its modes, its change and its paths, each field ended by NUL.
    fn entry(old: &str, new: &str, change: &str, paths: &[&str]) -> String {
        let mut text = format!(":{old} {new} {SNAPSHOT} {BACK} {change}\0");
        for path in paths {
            text.push_str(path);
            text.push('\0');
        }
        text
    }

    #[test]
    fn a_back_commit_may_not_touch_git_s_own_files_or_make_a_link_or_a_submodule() {
        let plain = entry("100644", "100644", "M", &["src/a.txt"]);
        let added = entry("000000", "100755", "A", &["run.sh"]);
        assert_eq!(forbidden(&format!("{plain}{added}")), None, "files git does not read itself");
        assert_eq!(forbidden(""), None, "no change");
        for (listing, path) in [
            (entry("100644", "100644", "M", &[".gitignore"]), ".gitignore"),
            (entry("000000", "100644", "A", &["sub/.GitAttributes"]), "sub/.GitAttributes"),
            (entry("000000", "100644", "A", &[".gitmodules"]), ".gitmodules"),
            (
                entry("000000", "100644", "A", &["deep/.git/hooks/pre-commit"]),
                "deep/.git/hooks/pre-commit",
            ),
            (entry("000000", "120000", "A", &["link"]), "link"),
            (entry("100644", "120000", "T", &["was-a-file"]), "was-a-file"),
            (entry("000000", "160000", "A", &["vendor/lib"]), "vendor/lib"),
            (entry("100644", "100644", "R100", &["a.txt", ".gitignore"]), ".gitignore"),
        ] {
            assert_eq!(forbidden(&format!("{plain}{listing}")), Some(path.to_owned()), "{listing:?}");
        }
        let deleted_link = entry("120000", "000000", "D", &["old-link"]);
        assert_eq!(forbidden(&deleted_link), None, "a link the back commit removes");
        let first = format!(
            "{}{}",
            entry("000000", "120000", "A", &["one"]),
            entry("000000", "100644", "A", &[".gitignore"])
        );
        assert_eq!(forbidden(&first), Some("one".to_owned()), "the first path it names");
    }

    #[test]
    fn the_sync_back_lines_cut_the_commits_to_eight_hex_digits() {
        assert_eq!(
            ignored_line(BACK, SNAPSHOT),
            "gate: ignored stale remote changes 8e8777a0: not made from snapshot b8a48121"
        );
        assert_eq!(
            not_applied_line(SNAPSHOT, BACK),
            "gate: remote changes not applied: the worktree changed during the run; see git diff b8a48121 8e8777a0"
        );
        assert_eq!(
            ignored_line("abc", "de"),
            "gate: ignored stale remote changes abc: not made from snapshot de"
        );
        assert_eq!(
            touched_line(".gitignore", SNAPSHOT, BACK),
            "gate: remote changes not applied: they touch .gitignore, which a sync-back never writes; see git diff b8a48121 8e8777a0"
        );
        assert_eq!(
            touched_line("a\n.gitignore", SNAPSHOT, BACK),
            r"gate: remote changes not applied: they touch a\n.gitignore, which a sync-back never writes; see git diff b8a48121 8e8777a0",
            "a name from the builder cannot break the line"
        );
    }

    #[test]
    fn the_synced_back_line_names_the_files_and_says_zero_with_nothing_after_the_colon() {
        let names = ["made-on-box.txt".to_owned()];
        assert_eq!(synced_line(&names), "gate: synced back 1 file(s): made-on-box.txt");
        assert_eq!(synced_line(&[]), "gate: synced back 0 file(s): ", "as PowerShell joins no names");
        let two = ["a.txt".to_owned(), "b.txt".to_owned()];
        assert_eq!(synced_line(&two), "gate: synced back 2 file(s): a.txt, b.txt");
        let forged = ["a\r\ngate: forged".to_owned()];
        assert_eq!(
            synced_line(&forged),
            r"gate: synced back 1 file(s): a\r\ngate: forged",
            "a name from the builder cannot break the line"
        );
    }

    #[test]
    fn the_network_calls_carry_the_pack_then_dash_dash_the_url_and_the_ref() {
        let remote = Remote {
            url: "builder:D:/gate-remote/app.git",
            pack: Some("pwsh -NoProfile -NonInteractive -Command git-upload-pack"),
            ssh: Some("ssh -o BatchMode=yes"),
            reference: "refs/gate-back/app-1a2b3c4d",
        };
        assert_eq!(
            remote.args(&["ls-remote"]),
            [
                "ls-remote",
                "--upload-pack=pwsh -NoProfile -NonInteractive -Command git-upload-pack",
                "--",
                "builder:D:/gate-remote/app.git",
                "refs/gate-back/app-1a2b3c4d",
            ]
        );
        let local = Remote { pack: None, ..remote };
        assert_eq!(
            local.args(&["-c", "fetch.fsckObjects=true", "fetch", "-q"]),
            [
                "-c",
                "fetch.fsckObjects=true",
                "fetch",
                "-q",
                "--",
                "builder:D:/gate-remote/app.git",
                "refs/gate-back/app-1a2b3c4d"
            ]
        );
    }
}
