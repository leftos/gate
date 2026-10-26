//! A run's back commits, as the Windows box makes them: each repo's tree the command left committed on its snapshot's
//! `HEAD` with the payload's identity and pushed to its bare repository's `refs/gate-back/<key>`, which the PC fetches,
//! all or none; nothing for a tree the command left unchanged. Every git step is the run's own ([`run_git`]), with the attribute and LFS storage
//! pins its checkout ran with, its group recorded in `<work>.pgid`.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use crate::command::removed_if_held;
use crate::console::{GAT, LineSink, Output, Stream};
use crate::files::{self, HeldLock};
use crate::git::{Failed, GitClass, RunGit};
use crate::run::{Repo, Request, back_ref_cleared, run_git};

/// The back commit's message, as the Windows box writes it.
const MESSAGE: &str = "gate-remote back";

/// Why a back commit was not made or pushed.
#[derive(Debug)]
pub enum BackFailed {
    /// A step failed, for this: said as `gate: remote sync-back failed: <why>`.
    Step(String),
    /// The client is gone: the failure ending the run, with [`crate::git::CLOSED_STATUS`].
    Gone(Failed),
}

/// Makes the back commit of each repo of `request` and pushes it to `refs/gate-back/<key>` in its bare repository, all
/// or none, as the Windows box's `back_commit` does: every repo's commit made first, the main one first
/// ([`repo_back_commit`]), then each pushed, and a failed push deleting every back ref this run pushed and, on a pair
/// run, the failing repo's own ([`undone`]), so the PC finds a back commit of every changed repo or of none; a work
/// lock found lost before or after a push deletes nothing, as another run may hold the back refs then. All under
/// the work lock `lock`, git found along `login_path` (the box's own `PATH` for `None`), its lines said through
/// `output`. Each repo's commit in the order of [`Request::repos`], `None` for a repo whose tree is unchanged.
///
/// # Errors
///
/// Why a commit was not made or pushed.
pub fn back_commit<S: LineSink + 'static>(
    request: &Request<'_>,
    lock: &HeldLock,
    login_path: Option<&str>,
    output: &Arc<Output<S>>,
) -> Result<Vec<Option<String>>, BackFailed> {
    let git = run_git(request, lock, login_path, output);
    let mut commits = Vec::with_capacity(1 + request.siblings.len());
    for repo in request.repos() {
        commits.push(repo_back_commit(request, repo, lock, &git, output)?);
    }
    let mut pushed = Vec::with_capacity(commits.len());
    for (repo, commit) in request.repos().zip(&commits) {
        let Some(commit) = commit else { continue };
        let target = format!("{commit}:refs/gate-back/{}", request.key);
        let step = repo.step("push");
        // A lock lost before the push deletes nothing: another run may hold the work folder and its back refs now.
        held(request, lock, &step).map_err(|failed| back_failed(output, failed))?;
        if let Err(failed) = git.bare_step(
            &step,
            &repo.git_tree().args(&[
                "push",
                "-q",
                "--no-recurse-submodules",
                "--receive-pack=git-receive-pack",
                &repo.bare,
                &target,
            ]),
            GitClass::Long,
        ) {
            // On a pair run, the failing repo's own ref too: a push that failed after its ref was updated leaves it
            // behind. A run of one repo takes no step it did not take before. Nothing is undone once the lock is lost.
            if !request.siblings.is_empty() {
                pushed.push(repo);
            }
            if matches!(lock.still_held(), Ok(true)) {
                undone(request, &pushed, &git, output);
            }
            return Err(back_failed(output, failed));
        }
        pushed.push(repo);
    }
    Ok(commits)
}

/// The `/`-separated paths the main repo's back commit `commit` changes against `HEAD`, as the Windows box's
/// `back_paths` reads them: `git diff-tree --name-only -r -z` in the main repo's tree, a step of the run's own git
/// ([`run_git`]) under the work lock `lock`, found along `login_path`, its group recorded so `box stop` ends it.
///
/// # Errors
///
/// Why they could not be read: the step's failure, or a path that is not UTF-8.
pub fn back_paths<S: LineSink + 'static>(
    request: &Request<'_>,
    lock: &HeldLock,
    login_path: Option<&str>,
    output: &Arc<Output<S>>,
    commit: &str,
) -> Result<Vec<String>, BackFailed> {
    let git = run_git(request, lock, login_path, output);
    let repo = &request.main;
    let tree = repo.git_tree();
    let args = tree.args(&["diff-tree", "--name-only", "-r", "-z", "HEAD", commit]);
    let said = git
        .step_whole(&repo.step("diff-tree"), &args, GitClass::Short)
        .map_err(|failed| back_failed(output, failed))?;
    said.split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).map_err(|_| BackFailed::Step("diff-tree printed a path that is not UTF-8".to_owned())))
        .collect()
}

/// After a failed push, the back ref of each repo of `pushed` deleted ([`back_ref_cleared`]), every one tried: a
/// deletion that fails is said, unless the client is gone, and fails nothing more, the push's own failure ending the
/// run.
fn undone<S: LineSink + 'static>(request: &Request<'_>, pushed: &[&Repo<'_>], git: &RunGit<'_, S>, output: &Output<S>) {
    for repo in pushed {
        let Err(failed) = back_ref_cleared(git, request.key, repo) else {
            continue;
        };
        if !output.client_gone() {
            let lines: Vec<(Stream, _, &str)> = failed.lines.iter().map(|line| (Stream::Err, GAT, line.as_str())).collect();
            // A note: the run ends on the push's failure, and a client gone finds out at its line.
            let _ = output.say_together(&lines);
        }
    }
}

/// The tree the command left in `repo` committed on its snapshot's `HEAD` with the payload's identity, unpushed;
/// nothing when the tree is unchanged. Each step is named for the repo on a pair run ([`Repo::step`]).
///
/// The tree's index, which the checkout just refreshed, is copied to `<work>.index` ([`index_copied`]), so `add -A`
/// skips every unchanged file; `add -A` and `write-tree` run on that copy, `write-tree` only while the copy is still
/// there and the lock held, so a vanished copy never commits an empty tree. The copy is removed after while the lock is
/// held ([`removed_if_held`]), a removal that fails said and failing nothing; the run's one copy is used by each repo
/// in turn. A tree equal to `HEAD^{tree}` is no commit; else `commit-tree -p HEAD`, the lock looked at before it. Else
/// why the commit was not made.
fn repo_back_commit<S: LineSink + 'static>(
    request: &Request<'_>,
    repo: &Repo<'_>,
    lock: &HeldLock,
    git: &RunGit<'_, S>,
    output: &Arc<Output<S>>,
) -> Result<Option<String>, BackFailed> {
    let failed = |failed: Failed| back_failed(output, failed);
    let tree = repo.git_tree();
    index_copied(request, &repo.tree, lock, output)?;
    let index = [("GIT_INDEX_FILE", request.index)];
    let (add, write_tree) = (repo.step("add"), repo.step("write-tree"));
    let written = git
        .step_with(&add, &tree.args(&["add", "-A"]), GitClass::Long, &index)
        .and_then(|_| copy_still_there(request, lock, &write_tree))
        .and_then(|()| git.step_with(&write_tree, &tree.args(&["write-tree"]), GitClass::Short, &index))
        .and_then(|said| first_line(&write_tree, &said));
    removed_if_held(request.index, lock, output);
    let written = written.map_err(failed)?;
    let rev_parse = repo.step("rev-parse");
    let head = git
        .step_as(&rev_parse, &tree.args(&["rev-parse", "HEAD^{tree}"]), GitClass::Short)
        .and_then(|said| first_line(&rev_parse, &said))
        .map_err(failed)?;
    if written == head {
        return Ok(None);
    }
    let identity: Vec<(&str, &str)> = request.identity.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
    let commit_args = ["commit-tree", written.as_str(), "-p", "HEAD", "-m", MESSAGE];
    let commit_tree = repo.step("commit-tree");
    let commit = held(request, lock, &commit_tree)
        .and_then(|()| git.step_with(&commit_tree, &tree.args(&commit_args), GitClass::Short, &identity))
        .and_then(|said| first_line(&commit_tree, &said))
        .map_err(failed)?;
    Ok(Some(commit))
}

/// The `.git/index` of the repo tree `tree` copied to `request`'s `<work>.index` while the work lock `lock` is held: `.git` a
/// plain folder, not a link; an earlier copy, a plain file, removed, and anything else at the name refused;
/// `.git/index` opened without following a link and without waiting on a FIFO (`O_NOFOLLOW | O_NONBLOCK`), and the
/// opened file checked a plain file before it is read, so nothing swapped in at its name can hold the run; the copy
/// made new (`O_CREAT | O_EXCL | O_NOFOLLOW`, mode 0600) and, when the copying fails partway, removed again. The copy
/// keeps the index's modification time, so git's racy-git check still compares the content of entries written in the
/// index's own second. Git's `add -A` then replaces the copy through `<work>.index.lock`, which leaves it with git's own
/// mode. Else why not.
fn index_copied<S: LineSink>(request: &Request<'_>, tree: &str, lock: &HeldLock, output: &Output<S>) -> Result<(), BackFailed> {
    let dot_git = Path::new(tree).join(".git");
    let index = dot_git.join("index");
    let copy = Path::new(request.index);
    let not_copied = |why: String| BackFailed::Step(format!("cannot copy {} to {}: {why}", index.display(), request.index));
    if !matches!(lock.still_held(), Ok(true)) {
        return Err(not_copied(format!("the work folder lock {} is no longer held", request.lock)));
    }
    if !files::plain_folder(&dot_git) {
        return Err(not_copied(format!("{} is a link or not a folder", dot_git.display())));
    }
    match fs::symlink_metadata(copy) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(metadata) if metadata.is_file() => removed_if_held(request.index, lock, output),
        Ok(_) => return Err(not_copied(format!("{} is a link or not a file", request.index))),
        Err(error) => return Err(not_copied(error.to_string())),
    }
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&index)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ENOENT | libc::ELOOP) => not_copied(format!("{} is missing or a link", index.display())),
            _ => not_copied(error.to_string()),
        })?;
    let metadata = source.metadata().map_err(|error| not_copied(error.to_string()))?;
    if !metadata.is_file() {
        return Err(not_copied(format!("{} is not a plain file", index.display())));
    }
    let mut made: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(copy)
        .map_err(|error| not_copied(error.to_string()))?;
    io::copy(&mut source, &mut made)
        .map(drop)
        .and_then(|()| metadata.modified())
        .and_then(|modified| made.set_times(fs::FileTimes::new().set_modified(modified)))
        .map_err(|error| {
            drop(made);
            removed_if_held(request.index, lock, output);
            not_copied(error.to_string())
        })
}

/// Whether the copied index of `request` is still a plain file at its name and the work lock `lock` still held, just
/// before the `write-tree` step `step`: git reads a missing index as an empty one. Else the step's failure.
fn copy_still_there(request: &Request<'_>, lock: &HeldLock, step: &str) -> Result<(), Failed> {
    held(request, lock, step)?;
    if fs::symlink_metadata(request.index).is_ok_and(|metadata| metadata.is_file()) {
        Ok(())
    } else {
        Err(Failed::line(format!(
            "gate: remote {step} failed: the copied index {} is gone",
            request.index
        )))
    }
}

/// Whether the work lock `lock` of `request` is still held before the step `step`; else the step's failure, as a git
/// step whose lock is lost fails.
fn held(request: &Request<'_>, lock: &HeldLock, step: &str) -> Result<(), Failed> {
    if matches!(lock.still_held(), Ok(true)) {
        Ok(())
    } else {
        Err(Failed::line(format!(
            "gate: remote {step} failed: the work folder lock {} is no longer held",
            request.lock
        )))
    }
}

/// The first line of the step `step`'s standard output `said`; a step that printed none fails.
fn first_line(step: &str, said: &[u8]) -> Result<String, Failed> {
    String::from_utf8_lossy(said)
        .lines()
        .next()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Failed::line(format!("gate: remote {step} printed nothing")))
}

/// The back commit's failure of a step that failed as `failed`: the client gone, when it is; else the step's lines
/// without their `gate: remote ` start, as the sync-back line names them.
fn back_failed<S: LineSink>(output: &Output<S>, failed: Failed) -> BackFailed {
    if output.client_gone() {
        return BackFailed::Gone(failed);
    }
    let why: Vec<&str> = failed
        .lines
        .iter()
        .map(|line| line.strip_prefix("gate: remote ").unwrap_or(line))
        .collect();
    BackFailed::Step(why.join("; "))
}
