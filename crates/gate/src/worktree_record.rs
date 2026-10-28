//! The worktree record of a work-folder key ([`v1::worktree_record_path`]): read as itself through any link at its
//! name, and written by a routed run and by `gate warm` for the live worktree whose root the key was made from.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

use gate_state::{KernelObject, StateRoot, key_hash, v1};
use gate_win32::{AcquireError, NamedMutex, OwnedMutexGuard};
use jiff::Timestamp;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT};

use crate::console::Console;
use crate::route;

/// The most of a worktree record read: a record is a few hundred bytes.
const RECORD_CAP: usize = 4096;

/// The mutex of the work-folder `key` under the state root `root`, the one a routed run holds, taken without waiting:
/// `None` when another gate holds it; else the line, after `gate: `, saying it cannot be opened or taken.
pub(crate) fn try_hold_key(root: &StateRoot, key: &str) -> Result<Option<OwnedMutexGuard>, String> {
    let name = root.object_name(KernelObject::Routed { hash: key_hash(key) });
    let cannot = |error: String| format!("cannot take the mutex {name}: {error}");
    let mutex = NamedMutex::open(&name).map_err(|error| cannot(error.to_string()))?;
    match mutex.acquire_owned(Duration::ZERO) {
        Ok(held) => Ok(Some(held)),
        Err(AcquireError::Timeout) => Ok(None),
        Err(AcquireError::Failed(error)) => Err(cannot(error.to_string())),
    }
}

/// The worktree record at `path` ([`v1::worktree_record_path`]), opened as itself, at most [`RECORD_CAP`] bytes.
/// `None` when there is none, or it cannot be read: a link, a junction, a folder, a longer file or one not of its
/// format.
pub(crate) fn read_worktree_record(path: &Path) -> Option<v1::WorktreeRecord> {
    let bytes = read_regular_within(path, RECORD_CAP).ok()?;
    v1::worktree_record(&bytes).ok()
}

/// Writes the worktree record of the work-folder `key` under the state root `state` for the live worktree at `root` of
/// the repo `repo`, when [`v1::needs_write`] says so: one that cannot be read ([`read_worktree_record`]) is written
/// again, and one that says the worktree is gone (`goneSince`) is written without it. A worktree whose git common
/// folder cannot be read ([`route::common_folder_of`]) or is no Unicode path writes none and says nothing. A write is
/// tried once; one that fails is said through `console` and changes nothing else.
pub(crate) fn leave(state: &Path, root: &str, repo: &str, key: &str, console: &mut Console) {
    let Some(common) = route::common_folder_of(Path::new(root)) else {
        return;
    };
    let Some(common) = common.to_str() else {
        return;
    };
    let fresh = v1::WorktreeRecord {
        root: root.to_owned(),
        common_dir: common.to_owned(),
        repo: repo.to_owned(),
        last_routed: Timestamp::now(),
        gone_since: None,
    };
    let path = v1::worktree_record_path(state, key);
    let existing = read_worktree_record(&path);
    if !v1::needs_write(existing.as_ref(), &fresh) {
        return;
    }
    if let Err(error) = v1::write_worktree_record(&path, &fresh) {
        console.err(&format!("gate: cannot write {}: {error}", path.display()));
    }
}

/// The bytes of the regular file at `path`, opened as itself, with `cap` bytes the most it reads: a link, a junction, a
/// folder or a longer file cannot be read.
///
/// # Errors
///
/// A file that is missing (`NotFound`), cannot be opened or read, or is not a regular file; one longer than `cap` bytes
/// as [`io::ErrorKind::FileTooLarge`].
pub(crate) fn read_regular_within(path: &Path, cap: usize) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Err(io::Error::other("it is a link, a junction or not a file"));
    }
    let mut bytes = Vec::new();
    file.take(u64::try_from(cap + 1).unwrap_or(u64::MAX)).read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err(io::Error::new(io::ErrorKind::FileTooLarge, format!("it is larger than {cap} bytes")));
    }
    Ok(bytes)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders must fail loudly")]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use gate_state::v1;

    use super::read_worktree_record;
    use crate::test_support::junction;

    /// A folder of this test process's own named `name`, empty.
    fn scratch(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-worktree-record-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    /// A worktree record of the worktree `X:/dev/app`.
    fn worktree_record() -> v1::WorktreeRecord {
        v1::WorktreeRecord {
            root: "X:/dev/app".to_owned(),
            common_dir: r"X:\dev\app\.git".to_owned(),
            repo: "app".to_owned(),
            last_routed: "2026-10-21T12:00:00Z".parse().expect("a time"),
            gone_since: None,
        }
    }

    #[test]
    fn a_worktree_record_reads_and_a_missing_or_longer_one_reads_as_none() {
        let folder = scratch("read");
        let path = folder.join("app-1a2b3c4d.worktree");
        assert_eq!(read_worktree_record(&path), None, "no record");

        v1::write_worktree_record(&path, &worktree_record()).expect("the record can be written");
        assert_eq!(read_worktree_record(&path), Some(worktree_record()));

        let mut longer = fs::read(&path).expect("the record can be read");
        longer.resize(4097, b' ');
        fs::write(&path, &longer).expect("the longer record can be written");
        assert_eq!(read_worktree_record(&path), None, "a record longer than 4096 bytes, though JSON");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_junction_at_the_worktree_record_name_reads_as_none() {
        let folder = scratch("junction");
        let target = folder.join("elsewhere");
        fs::create_dir_all(&target).expect("the target can be made");
        let path = folder.join("app-1a2b3c4d.worktree");
        junction(&path, &target);

        assert_eq!(read_worktree_record(&path), None, "a junction is no record");
        fs::remove_dir(&path).expect("the junction can be removed");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }
}
