//! The box's records and folders, each looked at as itself: a link is never followed, and a record is read only when
//! it is a plain file.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use crate::lines::{RootFacts, folder_failure, folder_flaw};

/// Opens `path` without following a link at it and without waiting on a FIFO, and reads at most `limit` bytes of it;
/// else why not, a link (`ELOOP`) and anything but a plain file included.
///
/// # Errors
///
/// The open, the read, or `path` not being a plain file.
pub fn read_plain(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!("{} is not a plain file", path.display())));
    }
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Writes `bytes` as the whole of `path`, made when missing, without following a link at it and without waiting on a
/// FIFO; only a plain file with no other hard link is emptied and written, so nothing outside `path` changes.
///
/// # Errors
///
/// The open, the write, or `path` being anything but a plain file of one link; a link at `path` is `ELOOP`.
pub fn write_plain(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!("{} is not a plain file", path.display())));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::other(format!("{} has {} hard links", path.display(), metadata.nlink())));
    }
    file.set_len(0)?;
    file.write_all(bytes)
}

/// What `path` is, looked at as itself and never through a link, and the box's effective user id.
///
/// # Errors
///
/// Why `path` cannot be looked at, a missing path included.
pub fn facts(path: &Path) -> io::Result<RootFacts> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(RootFacts {
        owner: metadata.uid(),
        euid: effective_uid(),
        mode: metadata.mode(),
        is_symlink: metadata.file_type().is_symlink(),
        is_dir: metadata.is_dir(),
    })
}

/// Why the folder `path` is not one the box uses ([`folder_flaw`]), as the line saying it: a link, no folder, another
/// user's, or one that cannot be looked at. `None` when it is the box user's own folder or is missing.
#[must_use]
pub fn unusable_folder(path: &Path) -> Option<String> {
    let why = match facts(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => error.to_string(),
        Ok(facts) => folder_flaw(facts, &user_name(facts.euid))?,
    };
    Some(folder_failure(&path.display().to_string(), &why))
}

/// The box's effective user id.
#[must_use]
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes nothing, touches no memory and always succeeds.
    unsafe { libc::geteuid() }
}

/// The name the box's lines give its user, whose effective id is `euid`: `USER`, else `uid <euid>`.
#[must_use]
pub fn user_name(euid: u32) -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| format!("uid {euid}"))
}

/// Whether `path` is a folder, as itself: no link, nothing missing.
#[must_use]
pub fn plain_folder(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Whether anything may be at `path`, a link included, looked at as itself: only a path found missing is not.
#[must_use]
pub fn exists(path: &Path) -> bool {
    !matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose temp folder cannot be made must fail loudly")]
mod tests {
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::write_plain;

    /// How long a write may take before the case calls it hung: a hang guard, never reached by a write that works.
    const HANG_GUARD: Duration = Duration::from_secs(300);

    /// A temp folder of the case `name`, private to this process, removed when the case ends.
    struct Case {
        base: PathBuf,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-files-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            fs::create_dir_all(&base).expect("the case folder is made");
            Case { base }
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_plain_file_is_written_whole() {
        let case = Case::new("plain");
        let file = case.base.join("alternates");
        fs::write(&file, "a longer text than the new one\n").expect("the file is written");
        write_plain(&file, b"new\n").expect("a plain file is written");
        assert_eq!(fs::read(&file).expect("the file is read"), b"new\n");
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_reader() {
        let case = Case::new("fifo");
        let fifo = case.base.join("alternates");
        let name = CString::new(fifo.as_os_str().as_bytes()).expect("the path holds no NUL");
        // SAFETY: `name` is a NUL-terminated path that lives across the call; `mkfifo` only reads it.
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(write_plain(&fifo, b"x\n").map_err(|error| error.to_string())));
        let written = receiver.recv_timeout(HANG_GUARD).expect("the write does not wait on the FIFO");
        assert!(written.is_err(), "a FIFO is no plain file");
    }

    #[test]
    fn a_hard_linked_file_is_refused_and_its_other_link_kept() {
        let case = Case::new("hard-link");
        let (other, file) = (case.base.join("other"), case.base.join("alternates"));
        fs::write(&other, "kept\n").expect("the other file is written");
        fs::hard_link(&other, &file).expect("the hard link is made");

        assert!(write_plain(&file, b"new\n").is_err(), "a file with another link is refused");
        assert_eq!(fs::read(&other).expect("the other file is read"), b"kept\n");
    }
}
