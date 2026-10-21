//! The box's records and folders, each looked at as itself: a link is never followed, and a record is read only when
//! it is a plain file. A record is replaced whole, through a synced temp file renamed over it, and the work lock is an
//! `flock` on a plain file.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::lines::{RootFacts, folder_failure, folder_flaw, job_record_text};

/// The most bytes of a record beside the work folder, or of a tree's alternates file, read.
pub const RECORD_LIMIT: u64 = 64 * 1024;

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

/// How many temp files this process has made for [`replace_plain`], so two replaces at once never pick one name.
static REPLACES: AtomicU64 = AtomicU64::new(0);

/// Why [`replace_plain`] failed.
#[derive(Debug)]
pub enum ReplaceFailure {
    /// The path holds what it held before: the new text was not put in place.
    NotWritten(io::Error),
    /// The new text is in place, but its folder could not be synced after the rename.
    Unsynced(io::Error),
}

impl fmt::Display for ReplaceFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplaceFailure::NotWritten(error) => write!(formatter, "{error}"),
            ReplaceFailure::Unsynced(error) => {
                write!(formatter, "the new text is in place but its folder could not be synced: {error}")
            }
        }
    }
}

/// Writes `bytes` as the whole of `path` at once, so a reader finds the old text or the new one and never a part: into
/// a new temp file beside it, `<name>.gate-new-<pid>-<n>` ([`temp_name`]), made with `O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0600, and
/// synced, then renamed over `path`; the folder, opened before the temp file is made, is synced after. A leftover temp
/// file of that name, which only an ended process with this one's pid can have left, is removed and the making tried
/// once more. Only a missing path or a plain file is replaced, looked at before the temp file is made and again before
/// the rename, its other hard links keeping the old text; a link, a folder or anything else at `path` is left as it
/// is, and the temp file removed. A volume that refuses `sync_all` (`ENOTSUP`, `EINVAL`: macOS's `F_FULLFSYNC` on SMB
/// or NFS) is synced with `fsync`.
///
/// # Errors
///
/// [`ReplaceFailure::NotWritten`]: `path` being anything but a plain file, the folder's open, the temp file's making,
/// write or sync, or the rename; a temp file that cannot be removed is named in the error.
/// [`ReplaceFailure::Unsynced`]: the folder's sync after the rename.
pub fn replace_plain(path: &Path, bytes: &[u8]) -> Result<(), ReplaceFailure> {
    replace_numbered(path, bytes, REPLACES.fetch_add(1, Ordering::Relaxed))
}

/// [`replace_plain`] through the temp file numbered `number`.
fn replace_numbered(path: &Path, bytes: &[u8], number: u64) -> Result<(), ReplaceFailure> {
    let folder = put_in_place(path, bytes, number).map_err(ReplaceFailure::NotWritten)?;
    synced(&folder).map_err(ReplaceFailure::Unsynced)
}

/// Puts `bytes` at `path` through the temp file numbered `number` ([`replace_plain`]); the folder, open, for its sync.
fn put_in_place(path: &Path, bytes: &[u8], number: u64) -> io::Result<File> {
    let Some(name) = path.file_name() else {
        return Err(io::Error::other(format!("{} names no file", path.display())));
    };
    let folder_path = match path.parent() {
        Some(folder) if !folder.as_os_str().is_empty() => folder,
        _ => Path::new("."),
    };
    replaceable(path)?;
    let folder = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(folder_path)?;
    let temp = folder_path.join(temp_name(name, number));
    let mut file = match new_temp(&temp) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(&temp).map_err(|left| {
                io::Error::new(
                    left.kind(),
                    format!("the leftover temp file {} could not be removed: {left}", temp.display()),
                )
            })?;
            new_temp(&temp)?
        }
        made => made?,
    };
    let replaced = file
        .write_all(bytes)
        .and_then(|()| synced(&file))
        .and_then(|()| replaceable(path))
        .and_then(|()| fs::rename(&temp, path));
    drop(file);
    if let Err(error) = replaced {
        return match fs::remove_file(&temp) {
            Ok(()) => Err(error),
            Err(left) => Err(io::Error::new(
                error.kind(),
                format!("{error}; the temp file {} could not be removed: {left}", temp.display()),
            )),
        };
    }
    Ok(folder)
}

/// The longest name a file may have on the box's volumes, in bytes (`NAME_MAX`).
const NAME_MAX: usize = 255;

/// The name of [`replace_plain`]'s temp file numbered `number` for the file `name`: `<name>.gate-new-<pid>-<n>`, the
/// kept part of `name` shortened, ending on a UTF-8 character's boundary, when the whole would pass [`NAME_MAX`].
fn temp_name(name: &OsStr, number: u64) -> OsString {
    let suffix = format!(".gate-new-{}-{number}", std::process::id());
    let bytes = name.as_bytes();
    let mut kept = bytes.len().min(NAME_MAX.saturating_sub(suffix.len()));
    // A continuation byte (`10xxxxxx`) at the cut is inside a character, which is left out whole.
    while kept > 0 && kept < bytes.len() && bytes[kept] & 0xC0 == 0x80 {
        kept -= 1;
    }
    let mut temp = OsString::from_vec(bytes[..kept].to_vec());
    temp.push(suffix);
    temp
}

/// Makes the temp file `temp` of [`replace_plain`]: new, mode 0600, a link at it never followed.
fn new_temp(temp: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(temp)
}

/// Syncs `file` to the drive with `sync_all` (macOS's `F_FULLFSYNC`), or with `fsync` on a volume that refuses that
/// (`ENOTSUP`, `EINVAL`).
fn synced(file: &File) -> io::Result<()> {
    match file.sync_all() {
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) => {
            // SAFETY: the descriptor is `file`'s, open for the whole call; `fsync` reads nothing else.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        synced => synced,
    }
}

/// Whether `path` may be replaced: missing, or a plain file, looked at as itself.
fn replaceable(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(io::Error::other("it is not a plain file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Writes the job record `path`, naming the job `job` and its process group `group` ([`job_record_text`]), whole
/// and synced ([`replace_plain`]).
///
/// # Errors
///
/// Why the record could not be replaced, or its folder synced after.
pub fn write_job_record(path: &Path, job: &str, group: i32) -> Result<(), ReplaceFailure> {
    replace_plain(path, job_record_text(job, group).as_bytes())
}

/// A work lock this box holds: an exclusive `flock` on its file, released when this is dropped and the file closed.
#[derive(Debug)]
pub struct HeldLock {
    file: File,
    path: PathBuf,
}

impl HeldLock {
    /// Whether the lock's name still names the file this box locked, looked at as itself: false once it is removed or
    /// another file is put at it, when another box could lock that one.
    ///
    /// # Errors
    ///
    /// Why the locked file or its name could not be looked at, other than the name missing.
    pub fn still_held(&self) -> io::Result<bool> {
        let locked = self.file.metadata()?;
        match fs::symlink_metadata(&self.path) {
            Ok(named) => Ok(named.dev() == locked.dev() && named.ino() == locked.ino()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// What taking a work lock found.
#[derive(Debug)]
pub enum Locked {
    /// The lock is this box's until the value is dropped.
    Held(HeldLock),
    /// Another holds it.
    Busy,
}

/// How many more times [`take_lock`] tries after locking a file that was no longer at the lock's name.
const LOCK_RETRIES: u32 = 3;

/// Takes the work lock `path` without waiting: opened (made, mode 0600, when missing) with `O_NOFOLLOW|O_CLOEXEC`, so
/// a link at it is never followed and no command the box starts inherits it, and without waiting on a FIFO; then
/// `flock(LOCK_EX|LOCK_NB)`. [`Locked::Busy`] when another open of it holds the lock. A locked file no longer at the
/// name, removed or replaced between the open and the lock ([`HeldLock::still_held`]), is let go and the lock taken
/// again, up to three more times.
///
/// # Errors
///
/// The open, `path` being anything but a plain file of one link (a link at it is `ELOOP`; a hard-linked one names the
/// file to remove), a failed `flock` other than a busy lock, a look at the name failing, or the locked file no longer
/// at the name on every try.
pub fn take_lock(path: &Path) -> io::Result<Locked> {
    take_lock_between(path, &mut || {})
}

/// [`take_lock`], calling `between` after each open, before its lock.
fn take_lock_between(path: &Path, between: &mut dyn FnMut()) -> io::Result<Locked> {
    for _ in 0..=LOCK_RETRIES {
        let file = lock_file(path)?;
        between();
        // SAFETY: the descriptor is `file`'s, open for the whole call; `flock` reads nothing else.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Ok(Locked::Busy)
            } else {
                Err(error)
            };
        }
        let held = HeldLock { file, path: path.to_owned() };
        if held.still_held()? {
            return Ok(Locked::Held(held));
        }
    }
    Err(io::Error::other(format!(
        "{} was replaced by another file each of the {} times it was locked",
        path.display(),
        LOCK_RETRIES + 1
    )))
}

/// Opens the work lock's file `path` for [`take_lock`]: a plain file of one link, made when missing.
fn lock_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!("{} is not a plain file", path.display())));
    }
    if metadata.nlink() != 1 {
        let shown = path.display();
        return Err(io::Error::other(format!(
            "{shown} has {} hard links, so another name shares its lock: remove {shown}",
            metadata.nlink()
        )));
    }
    Ok(file)
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
    use std::ffi::{CString, OsStr, OsString};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{HeldLock, Locked, replace_numbered, replace_plain, take_lock, take_lock_between, temp_name, write_job_record};
    use crate::lines::{JobRecord, job_record};

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

        /// The names in the case folder, sorted, so a test sees any temp file left behind.
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.base)
                .expect("the case folder is read")
                .map(|entry| entry.expect("an entry is read").file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Makes a FIFO at `path`.
    fn fifo_at(path: &Path) {
        let name = CString::new(path.as_os_str().as_bytes()).expect("the path holds no NUL");
        // SAFETY: `name` is a NUL-terminated path that lives across the call; `mkfifo` only reads it.
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
    }

    /// The lock `taken` holds; the case fails when it is not held.
    #[expect(clippy::panic, reason = "a case whose lock is not held must fail loudly")]
    fn held_lock(taken: std::io::Result<Locked>) -> HeldLock {
        match taken {
            Ok(Locked::Held(held)) => held,
            other => panic!("the lock is not held: {other:?}"),
        }
    }

    #[test]
    fn a_plain_file_is_replaced_whole_and_no_temp_file_is_left() {
        let case = Case::new("plain");
        let file = case.base.join("alternates");
        fs::write(&file, "a longer text than the new one\n").expect("the file is written");
        replace_plain(&file, b"new\n").expect("a plain file is replaced");
        assert_eq!(fs::read(&file).expect("the file is read"), b"new\n");
        assert_eq!(case.names(), ["alternates"]);
    }

    #[test]
    fn a_record_whose_name_is_250_bytes_is_replaced_through_a_temp_name_within_the_name_limit() {
        let case = Case::new("long-name");
        let name = "a".repeat(250);
        let file = case.base.join(&name);
        replace_plain(&file, b"new\n").expect("a 250-byte name is replaced");
        assert_eq!(fs::read(&file).expect("the file is read"), b"new\n");
        assert_eq!(case.names(), [name], "no temp file is left");
    }

    #[test]
    fn a_shortened_temp_name_keeps_whole_characters_and_fits_the_name_limit() {
        // 250 bytes: two-byte characters between single-byte ones, so some cuts fall inside a character.
        let name = format!("a{}a", "é".repeat(124));
        for number in [0, 7, 1234, u64::MAX] {
            let temp = temp_name(OsStr::new(&name), number);
            assert!(temp.len() <= 255, "{number}: {} bytes", temp.len());
            let text = temp.to_str().expect("the temp name is UTF-8");
            let suffix = format!(".gate-new-{}-{number}", std::process::id());
            let kept = text.strip_suffix(&suffix).expect("the suffix is whole");
            assert!(name.starts_with(kept), "{number}: {kept}");
            assert!(255 - suffix.len() - kept.len() <= 1, "{number}: at most one byte short of the limit");
        }
        let short = temp_name(OsStr::new("alternates"), 3);
        assert_eq!(short, OsString::from(format!("alternates.gate-new-{}-3", std::process::id())));
    }

    #[test]
    fn a_missing_file_is_made() {
        let case = Case::new("missing");
        let file = case.base.join("record");
        replace_plain(&file, b"made").expect("a missing file is made");
        assert_eq!(fs::read(&file).expect("the file is read"), b"made");
        assert_eq!(case.names(), ["record"]);
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_reader() {
        let case = Case::new("fifo");
        let fifo = case.base.join("alternates");
        fifo_at(&fifo);

        let (sender, receiver) = mpsc::channel();
        let target = fifo.clone();
        thread::spawn(move || sender.send(replace_plain(&target, b"x\n").map_err(|error| error.to_string())));
        let written = receiver.recv_timeout(HANG_GUARD).expect("the write does not wait on the FIFO");
        assert!(written.is_err(), "a FIFO is no plain file");
        assert!(fs::symlink_metadata(&fifo).expect("the FIFO is kept").file_type().is_fifo());
        assert_eq!(case.names(), ["alternates"], "the temp file is removed");
    }

    #[test]
    fn a_hard_linked_file_is_replaced_and_its_other_link_kept() {
        let case = Case::new("hard-link");
        let (other, file) = (case.base.join("other"), case.base.join("alternates"));
        fs::write(&other, "kept\n").expect("the other file is written");
        fs::hard_link(&other, &file).expect("the hard link is made");

        replace_plain(&file, b"new\n").expect("the name is replaced");
        assert_eq!(fs::read(&file).expect("the file is read"), b"new\n");
        assert_eq!(fs::read(&other).expect("the other file is read"), b"kept\n");
    }

    #[test]
    fn a_folder_at_the_name_is_refused_and_kept() {
        let case = Case::new("folder");
        let folder = case.base.join("alternates");
        fs::create_dir(&folder).expect("the folder is made");
        fs::write(folder.join("inside"), "kept\n").expect("a file in the folder is written");

        let error = replace_plain(&folder, b"new\n").expect_err("a folder is no plain file");
        assert_eq!(error.to_string(), "it is not a plain file");
        assert_eq!(fs::read(folder.join("inside")).expect("the folder's file is read"), b"kept\n");
        assert_eq!(case.names(), ["alternates"], "the temp file is removed");
    }

    #[test]
    fn a_link_at_the_name_is_refused_and_its_target_untouched() {
        let case = Case::new("link");
        let (target, link) = (case.base.join("target"), case.base.join("alternates"));
        fs::write(&target, "kept\n").expect("the target is written");
        symlink(&target, &link).expect("the link is made");

        assert!(replace_plain(&link, b"new\n").is_err(), "a link is no plain file");
        assert!(fs::symlink_metadata(&link).expect("the link is kept").file_type().is_symlink());
        assert_eq!(fs::read(&target).expect("the target is read"), b"kept\n");
        assert_eq!(case.names(), ["alternates", "target"], "the temp file is removed");
    }

    #[test]
    fn a_leftover_temp_file_of_this_process_is_replaced_and_the_write_made() {
        let case = Case::new("leftover");
        let file = case.base.join("alternates");
        let leftover = case.base.join(format!("alternates.gate-new-{}-0", std::process::id()));
        fs::write(&leftover, "left by a box killed mid-write\n").expect("the leftover is written");

        replace_numbered(&file, b"new\n", 0).expect("a leftover of this process's own temp name is replaced");
        assert_eq!(fs::read(&file).expect("the file is read"), b"new\n");
        assert_eq!(case.names(), ["alternates"]);
    }

    #[test]
    fn a_replaced_file_is_the_builder_users_alone() {
        let case = Case::new("mode");
        let file = case.base.join("record");
        fs::write(&file, "old\n").expect("the file is written");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("the mode is set");

        replace_plain(&file, b"new\n").expect("the file is replaced");
        let mode = fs::metadata(&file).expect("the file is looked at").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    #[test]
    fn a_job_record_is_written_as_the_stop_reads_it() {
        let case = Case::new("job-record");
        let record = case.base.join("k.pgid");
        write_job_record(&record, "gate-remote-k-0123456789ab", 4242).expect("the record is written");
        write_job_record(&record, "gate-remote-k-ffffffffffff", 99).expect("the record is replaced");
        let bytes = fs::read(&record).expect("the record is read");
        assert_eq!(job_record(&bytes, "gate-remote-k-ffffffffffff"), JobRecord::Group(99));
        assert_eq!(case.names(), ["k.pgid"]);
    }

    #[test]
    fn a_held_lock_is_busy_to_another_open_until_it_is_dropped() {
        let case = Case::new("lock");
        let path = case.base.join("k.lock");
        let held = take_lock(&path).expect("the lock opens");
        assert!(matches!(held, Locked::Held(_)), "{held:?}");
        let mode = fs::metadata(&path).expect("the lock file is made").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");

        assert!(matches!(take_lock(&path).expect("the lock opens"), Locked::Busy));
        drop(held);
        assert!(matches!(take_lock(&path).expect("the lock opens"), Locked::Held(_)), "free once dropped");
    }

    #[test]
    fn a_lock_whose_file_is_removed_and_made_again_is_no_longer_held() {
        let case = Case::new("lock-lost");
        let path = case.base.join("k.lock");
        let held = held_lock(take_lock(&path));
        assert!(held.still_held().expect("the lock is looked at"), "held while its file is at the name");

        fs::remove_file(&path).expect("the lock file is removed");
        assert!(!held.still_held().expect("the lock is looked at"), "lost once the name is gone");
        fs::write(&path, "").expect("another file is put at the name");
        assert!(!held.still_held().expect("the lock is looked at"), "lost once another file has the name");
    }

    #[test]
    fn a_lock_whose_name_is_swapped_between_open_and_lock_is_retried() {
        let case = Case::new("lock-swap");
        let (path, aside) = (case.base.join("k.lock"), case.base.join("aside"));
        let mut swap = || {
            fs::rename(&path, &aside).expect("the opened file is moved aside");
            fs::write(&path, "").expect("another file is put at the name");
        };

        let mut swaps = 0;
        let taken = take_lock_between(&path, &mut || {
            if swaps == 0 {
                swaps += 1;
                swap();
            }
        });
        let held = held_lock(taken);
        assert!(
            held.still_held().expect("the lock is looked at"),
            "the file at the name is the one locked"
        );
        assert!(
            matches!(take_lock(&path).expect("the lock opens"), Locked::Busy),
            "the name's file is locked"
        );
        drop(held);

        let error = take_lock_between(&path, &mut swap).expect_err("a name swapped every time is given up on");
        assert_eq!(
            error.to_string(),
            format!("{} was replaced by another file each of the 4 times it was locked", path.display())
        );
    }

    #[test]
    fn a_link_a_folder_or_a_hard_linked_file_at_the_lock_is_an_error_not_busy() {
        let case = Case::new("lock-flaws");
        let target = case.base.join("target");
        fs::write(&target, "").expect("the target is written");
        let link = case.base.join("link.lock");
        symlink(&target, &link).expect("the link is made");
        let folder = case.base.join("folder.lock");
        fs::create_dir(&folder).expect("the folder is made");
        let hard = case.base.join("hard.lock");
        fs::hard_link(&target, &hard).expect("the hard link is made");

        for path in [&link, &folder, &hard] {
            assert!(take_lock(path).is_err(), "{}", path.display());
        }
    }

    #[test]
    fn a_fifo_at_the_lock_is_refused_without_waiting() {
        let case = Case::new("lock-fifo");
        let fifo = case.base.join("k.lock");
        fifo_at(&fifo);

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(take_lock(&fifo).map(drop).map_err(|error| error.to_string())));
        let taken = receiver.recv_timeout(HANG_GUARD).expect("the open does not wait on the FIFO");
        assert!(taken.is_err(), "a FIFO is no plain file");
    }
}
