//! A run's back commit, as the Windows box makes it: the tree the command left committed on the snapshot's `HEAD` with
//! the payload's identity and pushed to the bare repository's `refs/gate-back/<key>`, which the PC fetches; nothing for
//! a tree the command left unchanged. Every git step is the run's own ([`run_git`]), with the attribute and LFS storage
//! pins its checkout ran with, its group recorded in `<work>.pgid`.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use crate::command::removed_if_held;
use crate::console::{LineSink, Output};
use crate::files::{self, HeldLock};
use crate::git::{Failed, GitClass, Tree};
use crate::run::{Request, run_git};

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

/// Makes the back commit of `request`'s work folder and pushes it to `refs/gate-back/<key>` in its bare repository,
/// under the work lock `lock`, git found along `login_path` (the box's own `PATH` for `None`), its lines said through
/// `output`. The commit, `None` when the tree is unchanged and nothing was pushed.
///
/// The work folder's index, which the checkout just refreshed, is copied to `<work>.index` ([`index_copied`]), so
/// `add -A` skips every unchanged file; `add -A` and `write-tree` run on that copy, `write-tree` only while the copy is
/// still there and the lock held, so a vanished copy never commits an empty tree. The copy is removed after while the
/// lock is held ([`removed_if_held`]), a removal that fails said and failing nothing. A tree equal to `HEAD^{tree}` is
/// no commit; else `commit-tree -p HEAD` with the payload's identity, then `push -q`, which runs to its end though the
/// client goes, as it writes the shared bare repository; the lock is looked at before each.
///
/// # Errors
///
/// Why the commit was not made or pushed.
pub fn back_commit<S: LineSink + 'static>(
    request: &Request<'_>,
    lock: &HeldLock,
    login_path: Option<&str>,
    output: &Arc<Output<S>>,
) -> Result<Option<String>, BackFailed> {
    let git = run_git(request, lock, login_path, output);
    let failed = |failed: Failed| back_failed(output, failed);
    let tree = Tree::at(request.work);
    index_copied(request, lock, output)?;
    let index = [("GIT_INDEX_FILE", request.index)];
    let written = git
        .step_with("add", &tree.args(&["add", "-A"]), GitClass::Long, &index)
        .and_then(|_| copy_still_there(request, lock))
        .and_then(|()| git.step_with("write-tree", &tree.args(&["write-tree"]), GitClass::Short, &index))
        .and_then(|said| first_line("write-tree", &said));
    removed_if_held(request.index, lock, output);
    let written = written.map_err(failed)?;
    let head = git
        .step_as("rev-parse", &tree.args(&["rev-parse", "HEAD^{tree}"]), GitClass::Short)
        .and_then(|said| first_line("rev-parse", &said))
        .map_err(failed)?;
    if written == head {
        return Ok(None);
    }
    let identity: Vec<(&str, &str)> = request.identity.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
    let commit_args = ["commit-tree", written.as_str(), "-p", "HEAD", "-m", MESSAGE];
    let commit = held(request, lock, "commit-tree")
        .and_then(|()| git.step_with("commit-tree", &tree.args(&commit_args), GitClass::Short, &identity))
        .and_then(|said| first_line("commit-tree", &said))
        .map_err(failed)?;
    let target = format!("{commit}:refs/gate-back/{}", request.key);
    held(request, lock, "push")
        .and_then(|()| git.bare_step("push", &tree.args(&["push", "-q", request.bare, &target]), GitClass::Long))
        .map_err(failed)?;
    Ok(Some(commit))
}

/// The work folder's `.git/index` copied to `request`'s `<work>.index` while the work lock `lock` is held: `.git` a
/// plain folder, not a link; an earlier copy, a plain file, removed, and anything else at the name refused;
/// `.git/index` opened without following a link and without waiting on a FIFO (`O_NOFOLLOW | O_NONBLOCK`), and the
/// opened file checked a plain file before it is read, so nothing swapped in at its name can hold the run; the copy
/// made new (`O_CREAT | O_EXCL | O_NOFOLLOW`, mode 0600) and, when the copying fails partway, removed again. Git's
/// `add -A` then replaces the copy through `<work>.index.lock`, which leaves it with git's own mode. Else why not.
fn index_copied<S: LineSink>(request: &Request<'_>, lock: &HeldLock, output: &Output<S>) -> Result<(), BackFailed> {
    let dot_git = Path::new(request.work).join(".git");
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
    io::copy(&mut source, &mut made).map(drop).map_err(|error| {
        drop(made);
        removed_if_held(request.index, lock, output);
        not_copied(error.to_string())
    })
}

/// Whether the copied index of `request` is still a plain file at its name and the work lock `lock` still held, just
/// before `write-tree`: git reads a missing index as an empty one. Else the step's failure.
fn copy_still_there(request: &Request<'_>, lock: &HeldLock) -> Result<(), Failed> {
    held(request, lock, "write-tree")?;
    if fs::symlink_metadata(request.index).is_ok_and(|metadata| metadata.is_file()) {
        Ok(())
    } else {
        Err(Failed::line(format!(
            "gate: remote write-tree failed: the copied index {} is gone",
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
