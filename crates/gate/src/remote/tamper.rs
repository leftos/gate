//! `--test-remote-tamper <point>:<step>`: a change the client makes to its own folders at one point of a routed run, so
//! a case can change a folder after the route checked it and before, or while, the client holds it: before the client
//! holds the run's folders (`before-hold`), or once the main repository's snapshot is taken (`after-first-snapshot`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::RemoteCall;
use super::client::End;

/// The point before the client holds the run's folders.
pub(super) const BEFORE_HOLD: &str = "before-hold";
/// The point once the main repository's snapshot is taken, before the siblings' are.
pub(super) const AFTER_FIRST_SNAPSHOT: &str = "after-first-snapshot";

/// Makes the change `tamper`, the seam's `<point>:<step>`, names when its point is `point`; nothing for another point
/// or when no change is named.
///
/// # Errors
///
/// A change that cannot be made, as the run's failure naming it.
pub(super) fn at(tamper: Option<&str>, point: &str, call: &RemoteCall<'_>) -> Result<(), End> {
    let Some(step) = tamper
        .and_then(|tamper| tamper.strip_prefix(point))
        .and_then(|rest| rest.strip_prefix(':'))
    else {
        return Ok(());
    };
    made(step, call).map_err(|why| End::Failed(format!("the test seam's change {point}:{step} cannot be made: {why}")))
}

/// Makes the change `step` to the folders of `call`.
fn made(step: &str, call: &RemoteCall<'_>) -> Result<(), String> {
    match step {
        "replace-root" => replaced(Path::new(call.root)),
        "case" => cased(first_sibling(call)?),
        "replace" => replaced(first_sibling(call)?),
        "worktree" => worktree_written(first_sibling(call)?),
        other => Err(format!("{other} is no change the seam makes")),
    }
}

/// The root of the first sibling `call` carries.
fn first_sibling<'c>(call: &'c RemoteCall<'_>) -> Result<&'c Path, String> {
    call.siblings
        .first()
        .map(|sibling| Path::new(sibling.root()))
        .ok_or_else(|| "the run carries no sibling".to_owned())
}

/// The folder beside `folder` named as it is with `suffix` after its name.
fn beside(folder: &Path, suffix: &str) -> Result<PathBuf, String> {
    let name = folder.file_name().ok_or_else(|| format!("{} has no folder name", folder.display()))?;
    Ok(folder.with_file_name(format!("{}{suffix}", name.to_string_lossy())))
}

/// Moves `folder` to `{folder}-replaced` and makes a new folder holding an empty `.git` folder in its place: the same
/// path, another folder id.
fn replaced(folder: &Path) -> Result<(), String> {
    let away = beside(folder, "-replaced")?;
    fs::rename(folder, &away).map_err(|error| format!("cannot move {} away: {error}", folder.display()))?;
    fs::create_dir_all(folder.join(".git")).map_err(|error| format!("cannot make {} anew: {error}", folder.display()))
}

/// Renames `folder` to its name in upper case, so its path on disk differs from the declared one in letter case alone.
fn cased(folder: &Path) -> Result<(), String> {
    let name = folder.file_name().ok_or_else(|| format!("{} has no folder name", folder.display()))?;
    let upper = name.to_string_lossy().to_uppercase();
    if upper == name.to_string_lossy() {
        return Err(format!("the name of {} has no lower-case letter", folder.display()));
    }
    fs::rename(folder, folder.with_file_name(upper)).map_err(|error| format!("cannot rename {}: {error}", folder.display()))
}

/// Appends `core.worktree` to the git config of the repository at `folder`, naming `{folder}-canary`.
fn worktree_written(folder: &Path) -> Result<(), String> {
    let canary = beside(folder, "-canary")?;
    let config = folder.join(".git").join("config");
    let line = format!("[core]\n\tworktree = {}\n", canary.to_string_lossy().replace('\\', "/"));
    fs::OpenOptions::new()
        .append(true)
        .open(&config)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .map_err(|error| format!("cannot write {}: {error}", config.display()))
}
