//! What `gate cutover` writes: the cutover flag that hands the machine from the PowerShell gate to `gate.exe`, and the
//! copy of the PowerShell gate's slot counts into `gate.exe`'s default root.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;

use crate::counts::{WriteError, kept_counts, read_counts_file, remove_partial, temp_path, write_replacing};
use crate::model::{Generation, SlotCounts};
use crate::v1;

/// What [`copy_slot_counts_to_v1`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CountsCopy {
    /// `gate.exe`'s counts file was already there, readable or not, and is kept as it was.
    Kept,
    /// The PowerShell gate has no counts file, so nothing was written.
    NoPowerShellCounts,
    /// gate-state rejects the PowerShell gate's counts file, so nothing was written and gates use the default counts.
    Ignored {
        /// The PowerShell gate's counts file.
        path: PathBuf,
        /// Why it is rejected, such as `it is not valid JSON: <parser message>`.
        reason: String,
    },
    /// The counts the PowerShell gate's file sets were written to `gate.exe`'s counts file. A value gate-state rejects
    /// for one kind, such as `"heavy":0`, is dropped silently and that kind keeps its default. When no value is left the
    /// counts are empty and nothing is written, as a machine at its defaults holds no counts file.
    Copied(SlotCounts),
}

/// Writes the cutover flag, `<local app data>\gate\cutover.flag`, holding its write time as ISO 8601 UTC, such as
/// `2026-10-11T14:15:02.123456789Z`. The launcher reads only whether the flag exists, never what it holds. A missing
/// `gate` folder is made, and a flag already there is replaced: the text is written beside it first, then moved over it.
///
/// # Errors
///
/// [`WriteError`] naming the flag when its folder or the file beside it cannot be written, or the move fails every try.
pub fn write_cutover_flag(local_app_data: &Path) -> Result<PathBuf, WriteError> {
    let path = Generation::cutover_flag(local_app_data);
    match write_replacing(&path, &Timestamp::now().to_string()) {
        Ok(()) => Ok(path),
        Err(source) => Err(WriteError { path, source }),
    }
}

/// Copies the PowerShell gate's slot counts, `<local app data>\gate\slot-counts.json`, into `gate.exe`'s counts file
/// in its default root, `<local app data>\gate\v1\slot-counts.json`, whatever `GATE_STATE_DIR` says, so both
/// generations count the same slots. Only the values gate-state's reader keeps are copied, written as `gate.exe`'s
/// text (`{"v":1,"heavy":3}`); the PowerShell gate's file is only read.
///
/// `gate.exe`'s file is never replaced: when it is there, readable or not, it is kept, so a Gates-tab edit made since an
/// earlier cutover survives a rerun. The text is written beside it first and linked into place only while no file is
/// there, so a write that lands between the look and the link wins too. Missing `gate` and `v1` folders are made.
///
/// # Errors
///
/// [`WriteError`] naming `gate.exe`'s counts file when whether it exists cannot be told, or its folder, the file beside
/// it or the link cannot be written.
pub fn copy_slot_counts_to_v1(local_app_data: &Path) -> Result<CountsCopy, WriteError> {
    let target = v1::counts_path(&Generation::V1.state_dir(local_app_data));
    let fail = |source: io::Error| WriteError {
        path: target.clone(),
        source,
    };
    if target.try_exists().map_err(fail)? {
        return Ok(CountsCopy::Kept);
    }
    let source = Generation::PowerShell.state_dir(local_app_data).join("slot-counts.json");
    let file = read_counts_file(&source);
    let counts = match &file {
        Ok(None) => return Ok(CountsCopy::NoPowerShellCounts),
        Err(reason) => {
            return Ok(CountsCopy::Ignored {
                path: source,
                reason: reason.clone(),
            });
        }
        Ok(Some(_)) => kept_counts(&file, &source),
    };
    if counts.is_empty() {
        return Ok(CountsCopy::Copied(counts));
    }
    let text = v1::counts_text(counts).map_err(|error| fail(io::Error::other(error)))?;
    if write_new(&target, &text).map_err(fail)? {
        Ok(CountsCopy::Copied(counts))
    } else {
        Ok(CountsCopy::Kept)
    }
}

/// Writes `text` at `path` only while no file is there: beside it first, as `<path>.<pid>.<n>.tmp`, then linked into
/// place. True when it was written, false when a file was there first, which is kept. A missing parent folder is made.
fn write_new(path: &Path, text: &str) -> io::Result<bool> {
    if let Some(folder) = path.parent()
        && !folder.as_os_str().is_empty()
    {
        fs::create_dir_all(folder)?;
    }
    let temp = temp_path(path);
    if let Err(error) = fs::write(&temp, text) {
        return Err(remove_partial(&temp, error));
    }
    match link_no_replace(&temp, path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // The file there first is kept either way; a temp file this cannot remove stays beside it, since no later
            // write uses its name.
            let _ = fs::remove_file(&temp);
            Ok(false)
        }
        Err(error) => Err(remove_partial(&temp, error)),
    }
}

/// Puts the file `temp` at `target` only while no file is at `target`: `target` is made a hard link to `temp`, which is
/// then removed. Unlike a rename, the link never replaces a file another writer put there first. It is public, and
/// hidden from the docs, only so the no-replace case can be tested directly.
///
/// # Errors
///
/// [`io::ErrorKind::AlreadyExists`] when a file is at `target`, or the io error the link met otherwise; either way both
/// files are left as they were.
pub fn link_no_replace(temp: &Path, target: &Path) -> io::Result<()> {
    fs::hard_link(temp, target)?;
    // The file is in place; a temp file this cannot remove stays beside it, since no later write uses its name.
    let _ = fs::remove_file(temp);
    Ok(())
}
