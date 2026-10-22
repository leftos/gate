//! A folder held by its descriptor, and the calls made beneath it: each name is reached on its parent folder's
//! descriptor, a link at it never followed and a FIFO never waited on, so a link swapped in on the way never leads a
//! call outside the folder.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// The flags every folder is opened with: a folder only, a link at the name never followed, a FIFO never waited on,
/// and the descriptor closed in every program the box starts.
const FOLDER_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

/// A folder, open by its descriptor until this is dropped.
#[derive(Debug)]
pub struct Folder {
    fd: OwnedFd,
}

impl Folder {
    /// Opens the folder at the absolute path `path`, a link at its last component never followed.
    ///
    /// # Errors
    ///
    /// `path` relative or holding a NUL (`InvalidInput`); else the open: `path` missing, a link, or not a folder.
    pub fn open(path: &Path) -> io::Result<Folder> {
        if !path.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the path is not absolute"));
        }
        let path = c_string(path.as_os_str())?;
        // SAFETY: `path` is a NUL-terminated string that lives across the call; `open` only reads it.
        let fd = unsafe { libc::open(path.as_ptr(), FOLDER_FLAGS) };
        folder_of(fd)
    }

    /// Opens the folder `name` in this folder, a link at it never followed.
    ///
    /// # Errors
    ///
    /// The open: `name` missing, a link, or not a folder.
    pub fn open_child(&self, name: &OsStr) -> io::Result<Folder> {
        let name = c_name(name)?;
        // SAFETY: the descriptor is this folder's, open for the whole call; `name` is a NUL-terminated string that
        // lives across it, which `openat` only reads.
        let fd = unsafe { libc::openat(self.fd.as_raw_fd(), name.as_ptr(), FOLDER_FLAGS) };
        folder_of(fd)
    }

    /// The names in this folder, `.` and `..` left out, in the order the folder lists them: read from the start
    /// through a new open of the folder beneath its own descriptor, closed in every program the box starts.
    ///
    /// # Errors
    ///
    /// The listing's open or a read of it.
    pub fn entries(&self) -> io::Result<Vec<OsString>> {
        // SAFETY: the descriptor is this folder's, open for the whole call; the name is a NUL-terminated literal.
        let opened = owned(unsafe { libc::openat(self.fd.as_raw_fd(), c".".as_ptr(), FOLDER_FLAGS) })?;
        // SAFETY: `opened` is an open descriptor this call owns; `fdopendir` takes it over only when it succeeds.
        let stream = unsafe { libc::fdopendir(opened.as_raw_fd()) };
        if stream.is_null() {
            return Err(io::Error::last_os_error());
        }
        // The listing owns the descriptor now and closes it with itself.
        let _ = opened.into_raw_fd();
        Listing(stream).names()
    }

    /// What this folder is, looked at on its descriptor.
    ///
    /// # Errors
    ///
    /// The look.
    pub fn stat(&self) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the descriptor is this folder's, open for the whole call, and `stat` is room for the one `stat` the
        // call writes.
        if unsafe { libc::fstat(self.fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fstat` returned 0, so it filled `stat`.
        Ok(unsafe { stat.assume_init() })
    }

    /// What `name` in this folder is, looked at as itself: a link is never followed.
    ///
    /// # Errors
    ///
    /// The look: `name` missing, or this folder unreadable.
    pub fn stat_child(&self, name: &OsStr) -> io::Result<libc::stat> {
        let name = c_name(name)?;
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the descriptor is this folder's, open for the whole call; `name` is a NUL-terminated string that
        // lives across it, and `stat` is room for the one `stat` the call writes.
        let looked = unsafe { libc::fstatat(self.fd.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        if looked != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fstatat` returned 0, so it filled `stat`.
        Ok(unsafe { stat.assume_init() })
    }

    /// Removes `name` in this folder as itself: an empty folder when `folder`, else a file or a link, never what a
    /// link leads to.
    ///
    /// # Errors
    ///
    /// The removal: `name` missing, a folder that is not empty, or not what `folder` says.
    pub fn remove_child(&self, name: &OsStr, folder: bool) -> io::Result<()> {
        let name = c_name(name)?;
        let flags = if folder { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: the descriptor is this folder's, open for the whole call; `name` is a NUL-terminated string that
        // lives across it, which `unlinkat` only reads.
        if unsafe { libc::unlinkat(self.fd.as_raw_fd(), name.as_ptr(), flags) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Removes `name` in this folder with everything in it: a folder's children removed beneath that folder's own
    /// descriptor, then the folder; anything else, a link included, removed as itself and never followed. A name found
    /// missing at any step, removed by another meanwhile, counts as removed. A folder on another device than
    /// its parent folder (`st_dev`) is refused with [`ANOTHER_VOLUME`] and nothing in it touched; a bind mount from
    /// the same file system, which keeps the device, is not caught.
    ///
    /// # Errors
    ///
    /// The first look, open, listing or removal that failed, or a folder on another device; what was removed before it stays
    /// removed.
    pub fn remove_child_tree(&self, name: &OsStr) -> io::Result<()> {
        gone_is_removed(self.tree_removed(name))
    }

    /// [`Folder::remove_child_tree`], a name found missing failing it.
    fn tree_removed(&self, name: &OsStr) -> io::Result<()> {
        let folder = self.stat_child(name)?.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if folder {
            let child = self.open_child(name)?;
            if child.stat()?.st_dev != self.stat()?.st_dev {
                return Err(io::Error::other(ANOTHER_VOLUME));
            }
            for entry in child.entries()? {
                child.remove_child_tree(&entry)?;
            }
        }
        self.remove_child(name, folder)
    }
}

/// Why [`Folder::remove_child_tree`] refuses a tree holding a folder on another device than its parent folder, true
/// of the child it names at whatever depth that folder is.
pub const ANOTHER_VOLUME: &str = "it holds another volume";

/// `removal`, a name found missing (`ENOENT`) counted as removed.
fn gone_is_removed(removal: io::Result<()>) -> io::Result<()> {
    match removal {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// An open listing of a folder, closed, with the descriptor it owns, when this is dropped.
struct Listing(*mut libc::DIR);

impl Listing {
    /// The names left in the listing, `.` and `..` left out.
    fn names(&self) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        loop {
            // `readdir` says the listing's end and a failed read alike, by a null entry; only a failure sets errno.
            errno_cleared();
            // SAFETY: the listing is open for the whole call.
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    None | Some(0) => Ok(names),
                    Some(_) => Err(error),
                };
            }
            // SAFETY: `entry` is the listing's own, valid until its next read, and its name is NUL-terminated.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name.to_vec()));
            }
        }
    }
}

impl Drop for Listing {
    fn drop(&mut self) {
        // SAFETY: the listing is open and closed only here, once.
        // A close that fails leaves nothing to undo: the listing was only read.
        let _ = unsafe { libc::closedir(self.0) };
    }
}

/// This thread's errno.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
use libc::__errno_location as errno_location;
/// This thread's errno.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
use libc::__error as errno_location;

/// Sets this thread's errno to 0.
fn errno_cleared() {
    // SAFETY: the location is this thread's errno, valid for the thread's life.
    unsafe { *errno_location() = 0 };
}

/// The child's name `name` as the NUL-terminated string a call beneath a folder takes: one component, so empty, `.`,
/// `..` and a name holding `/` or a NUL are refused (`InvalidInput`).
fn c_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not one name in a folder", Path::new(name).display()),
        ));
    }
    c_string(name)
}

/// `text` as the NUL-terminated string a call takes; one holding a NUL is refused (`InvalidInput`).
fn c_string(text: &OsStr) -> io::Result<CString> {
    CString::new(text.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name holds a NUL"))
}

/// The descriptor `fd` a call returned, owned; else the call's error.
fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a descriptor the call just opened, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The folder the open that returned `fd` opened; else the open's error.
fn folder_of(fd: libc::c_int) -> io::Result<Folder> {
    owned(fd).map(|fd| Folder { fd })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose temp folder cannot be made must fail loudly")]
mod tests {
    use std::ffi::{CString, OsStr, OsString};
    use std::fs;
    use std::io::ErrorKind;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::Folder;

    /// How long a removal may take before the case calls it hung: a hang guard, never reached by a removal that works.
    const HANG_GUARD: Duration = Duration::from_secs(300);

    /// A temp folder of the case `name`, private to this process, removed when the case ends.
    struct Case {
        base: PathBuf,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-beneath-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            fs::create_dir_all(&base).expect("the case folder is made");
            Case { base }
        }

        /// The names in the case folder, sorted.
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.base)
                .expect("the case folder is read")
                .map(|entry| entry.expect("an entry is read").file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        /// The case folder, opened.
        fn folder(&self) -> Folder {
            Folder::open(&self.base).expect("the case folder opens")
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

    #[test]
    fn a_link_at_the_opened_name_refuses_the_open() {
        let case = Case::new("open-link");
        let (target, link) = (case.base.join("target"), case.base.join("link"));
        fs::create_dir(&target).expect("the target folder is made");
        symlink(&target, &link).expect("the link is made");

        assert!(Folder::open(&link).is_err(), "a link at the path is not followed");
        assert!(
            case.folder().open_child(OsStr::new("link")).is_err(),
            "a link at the child's name is not followed"
        );
        assert!(Folder::open(&target).is_ok(), "the folder itself opens");
    }

    #[test]
    fn a_tree_of_nested_folders_and_files_is_removed_whole() {
        let case = Case::new("tree");
        let deep = case.base.join("tree").join("a").join("b");
        fs::create_dir_all(&deep).expect("the nested folders are made");
        fs::create_dir(case.base.join("tree").join("empty")).expect("an empty folder is made");
        for file in [case.base.join("tree").join("top.txt"), deep.join("deep.txt")] {
            fs::write(file, "x\n").expect("a file is written");
        }
        fs::write(case.base.join("kept.txt"), "kept\n").expect("a file beside the tree is written");

        case.folder().remove_child_tree(OsStr::new("tree")).expect("the tree is removed");
        assert_eq!(case.names(), ["kept.txt"], "the tree goes and what is beside it stays");
    }

    #[test]
    fn a_link_inside_the_tree_is_removed_as_itself_and_the_folder_it_leads_to_kept() {
        let case = Case::new("tree-link");
        let outside = case.base.join("outside");
        fs::create_dir(&outside).expect("the outside folder is made");
        fs::write(outside.join("file.txt"), "kept\n").expect("the outside file is written");
        let tree = case.base.join("tree");
        fs::create_dir_all(tree.join("inner")).expect("the tree is made");
        symlink(&outside, tree.join("out")).expect("the link at the tree's top is made");
        symlink(&outside, tree.join("inner").join("out")).expect("the link deeper in the tree is made");

        case.folder().remove_child_tree(OsStr::new("tree")).expect("the tree is removed");
        assert_eq!(case.names(), ["outside"]);
        assert_eq!(fs::read(outside.join("file.txt")).expect("the outside file is read"), b"kept\n");
    }

    #[test]
    fn a_fifo_in_the_tree_is_removed_without_waiting() {
        let case = Case::new("tree-fifo");
        let tree = case.base.join("tree");
        fs::create_dir(&tree).expect("the tree is made");
        fifo_at(&tree.join("fifo"));

        let (sender, receiver) = mpsc::channel();
        let base = case.base.clone();
        thread::spawn(move || {
            let removed = Folder::open(&base).and_then(|folder| folder.remove_child_tree(OsStr::new("tree")));
            sender.send(removed.map_err(|error| error.to_string()))
        });
        let removed = receiver.recv_timeout(HANG_GUARD).expect("the removal does not wait on the FIFO");
        assert_eq!(removed, Ok(()));
        assert!(case.names().is_empty(), "{:?}", case.names());
    }

    #[test]
    fn a_listed_child_removed_by_another_before_its_look_counts_as_removed() {
        let case = Case::new("gone");
        fs::create_dir_all(case.base.join("tree").join("inner")).expect("a folder is made");
        fs::write(case.base.join("file.txt"), "x\n").expect("a file is written");
        let folder = case.folder();
        let listed = folder.entries().expect("the folder is listed");

        fs::remove_dir_all(case.base.join("tree")).expect("another removes the folder");
        fs::remove_file(case.base.join("file.txt")).expect("another removes the file");
        for name in &listed {
            folder.remove_child_tree(name).expect("a child already gone counts as removed");
        }
        assert!(case.names().is_empty(), "{:?}", case.names());
    }

    #[test]
    fn a_child_s_name_of_other_than_one_component_is_refused() {
        let case = Case::new("names");
        fs::create_dir(case.base.join("a")).expect("a folder is made");
        let folder = case.folder();

        for name in ["", ".", "..", "a/b"] {
            let name = OsStr::new(name);
            let kinds = [
                folder.open_child(name).map(drop),
                folder.stat_child(name).map(drop),
                folder.remove_child(name, true),
                folder.remove_child_tree(name),
            ]
            .map(|called| called.map_err(|error| error.kind()));
            assert_eq!(kinds, [Err(ErrorKind::InvalidInput); 4], "{name:?}");
        }
        assert_eq!(case.names(), ["a"], "nothing is removed");
    }

    #[test]
    fn a_relative_path_is_refused() {
        let opened = Folder::open(Path::new("relative")).map(drop).map_err(|error| error.kind());
        assert_eq!(opened, Err(ErrorKind::InvalidInput));
    }

    #[test]
    fn a_listing_leaves_out_the_folder_and_its_parent() {
        let case = Case::new("list");
        fs::create_dir(case.base.join("folder")).expect("a folder is made");
        fs::write(case.base.join("file.txt"), "x\n").expect("a file is written");
        let folder = case.folder();

        for _ in 0..2 {
            let mut names = folder.entries().expect("the folder is listed");
            names.sort();
            assert_eq!(
                names,
                [OsString::from("file.txt"), OsString::from("folder")],
                "each listing starts at the top"
            );
        }
    }
}
