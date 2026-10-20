//! `gate box inflight`: how many of the builder's work folders a run holds. A run holds its work folder by holding `<root>\work\<key>.lock`
//! open with no sharing at all (`gate box run`, and the PowerShell wrapper's `Invoke-Run`), so a probe that opens each
//! lock for reading, sharing everything, is refused exactly while a run holds it. Each handle is closed at once, so a
//! run that starts just after the probe can still take its lock; one whose own open lands while the probe holds it is
//! refused busy (status 75), and its PC runs the command here instead.

use std::fs::{self, DirEntry, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use gate_router::escaped;
use gate_wire::Payload;
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::run::{LOCK_VIOLATION, SHARING_VIOLATION};
use crate::console::Console;

/// Says how many of the work folders under the payload's `Root` a run holds, then one line naming each; the exit
/// status: 0 however many that is, [`FAILED_STATUS`] when the work folder cannot be listed at all, or [`USAGE_STATUS`]
/// for a payload without a `Root`. A `Root`, or a `work` folder, that is not there is no run in flight and no error.
pub(crate) fn inflight(payload: &Payload, console: &mut Console) -> u32 {
    let root = match payload.path("Root") {
        Ok(root) => root,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    let work = format!(r"{}\work", root.trim_end_matches(['\\', '/']));
    let mut lines = match locks(&work) {
        Ok(locks) => locks.into_iter().filter_map(|path| in_flight(&path)).collect::<Vec<_>>(),
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            console.err(&format!("gate: remote cannot list {}: {}", escaped(&work), escaped(&error.to_string())));
            return u32::from(FAILED_STATUS);
        }
    };
    lines.sort();
    console.out(&counted(lines.len()));
    for line in &lines {
        console.out(line);
    }
    0
}

/// The line saying how many runs are in flight.
fn counted(runs: usize) -> String {
    format!("gate: remote in flight {runs}")
}

/// The paths under the work folder `work`, in the order the folder lists them; else why it cannot be listed, an entry
/// that cannot be read no different from the folder itself.
fn locks(work: &str) -> Result<Vec<PathBuf>, io::Error> {
    let entries = fs::read_dir(work)?.collect::<Result<Vec<DirEntry>, _>>()?;
    Ok(entries.iter().map(DirEntry::path).collect())
}

/// The line naming `path` as one run in flight, `None` when no run holds it or it is not a run's lock: one open for
/// reading that shares everything, refused with a sharing violation while a run holds the lock with no sharing at all.
/// A lock that cannot be read at all counts as held, since the drain must not pass on one it could not look at, and its
/// line says why.
fn in_flight(path: &Path) -> Option<String> {
    if !is_lock(path) {
        return None;
    }
    let key = path.file_stem()?.to_string_lossy();
    let line = |rest: &str| format!("gate: remote in flight {}{rest}", escaped(&key));
    match probe(path) {
        Probe::Free => None,
        Probe::Held => Some(line("")),
        Probe::Unreadable(error) => Some(line(&format!(" (cannot read: {})", escaped(&error)))),
    }
}

/// Whether `path` is named as a run's lock: `<key>.lock`, as `gate box run` and the PowerShell wrapper name it.
fn is_lock(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("lock"))
}

/// How one probe of a lock ended.
enum Probe {
    /// No run holds it: the probe's handle, closed at once.
    Free,
    /// A run holds it, so the open was refused with a sharing violation.
    Held,
    /// It could not be opened at all, so it counts as held; why.
    Unreadable(String),
}

/// How `path` stands, from one open for reading that shares everything. The handle is closed before this returns, so a
/// run that starts just after can take the lock.
fn probe(path: &Path) -> Probe {
    match OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
    {
        Ok(lock) => {
            drop(lock);
            Probe::Free
        }
        Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => Probe::Held,
        Err(error) => Probe::Unreadable(error.to_string()),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make or take its lock must fail loudly")]
mod tests {
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::PathBuf;

    use super::{Probe, probe};

    /// A scratch folder for one test, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-inflight-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("an earlier scratch folder can be removed");
        }
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
    }

    #[test]
    fn a_probe_of_a_free_lock_leaves_it_takeable_by_the_next_open() {
        let lock = scratch("free").join("k.lock");
        fs::write(&lock, "").expect("a free lock can be written");

        assert!(matches!(probe(&lock), Probe::Free), "a lock no one holds is free");
        let taken = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&lock)
            .expect("the probe closed its handle, so the next open can take the free lock");
        drop(taken);
    }
}
