//! File calls the standard library does not make: a rename that never replaces the file its target names, an open of
//! a name inside an open folder, an open that reads an open file through its reparse point's filter, and whether two
//! open files are one.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile};
use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_GENERIC_READ, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx, MoveFileExW,
    ReOpenFile,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// Opens `name`, one file or folder name, inside the open folder `folder` for reading, as itself: a link or a junction
/// so named is opened as the link, not followed. The name is looked up in the folder the handle holds, whatever its
/// path names by now, and matched in any letter case unless that folder is case-sensitive, as a path open matches it.
/// The new handle shares reading, writing and deletion.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::NotFound`] when nothing in the folder has that name, of kind
/// [`io::ErrorKind::InvalidInput`], before any system call, for a name that is not one name ([`one_name`]) or is too
/// long to pass, else the system's error.
pub fn open_beneath(folder: &File, name: &str) -> io::Result<File> {
    one_name(name)?;
    let mut wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    let length = u16::try_from(size_of_val(wide.as_slice())).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name is too long"))?;
    let object_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).map_err(io::Error::other)?,
        RootDirectory: folder.as_raw_handle(),
        ObjectName: &raw const object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let mut handle = ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: the name's buffer, the attributes and the status block outlive the call, the attributes' root is the
    // open handle `folder` owns, and `handle` is writable; synchronous I/O and `FILE_GENERIC_READ`'s `SYNCHRONIZE`
    // make a handle std reads from as from any other.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            FILE_GENERIC_READ,
            &raw const attributes,
            &raw mut status_block,
            ptr::null(),
            0,
            share,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: the call reads nothing but the status it is given.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code.cast_signed()));
    }
    // SAFETY: a success is a fresh handle nothing else owns; the returned file owns and closes it from now on.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Refuses, as [`io::ErrorKind::InvalidInput`], a `name` that is not one file or folder name of the folder it is
/// opened in: an empty one, `.`, `..`, or one holding `\`, `/`, `:` or NUL.
fn one_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['\\', '/', ':', '\0']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not one file or folder name"),
        ));
    }
    Ok(())
}

/// Opens the open file `file` again for reading, through the filter of any reparse point it is: the same file, opened
/// without `FILE_FLAG_OPEN_REPARSE_POINT`, so a file a filter serves (a deduplicated, compressed or projected one)
/// reads as its content rather than as the reparse point. The new handle shares reading, writing and deletion.
///
/// # Errors
///
/// The system's error when the file cannot be opened again, such as a sharing violation when `file` itself was opened
/// sharing less than that.
pub fn reopen_through_filter(file: &File) -> io::Result<File> {
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    // SAFETY: the handle `file` owns is open for the whole call; with no flags the new handle follows reparse points.
    let handle = unsafe { ReOpenFile(file.as_raw_handle(), GENERIC_READ, share, 0) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a success is a fresh handle nothing else owns; the returned file owns and closes it from now on.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Whether the open files `first` and `second` are one file: the same volume serial number and the same 128-bit file
/// id.
///
/// # Errors
///
/// The system's error when either handle's file id cannot be read, as on a file system that keeps none.
pub fn same_file(first: &File, second: &File) -> io::Result<bool> {
    Ok(file_id(first)? == file_id(second)?)
}

/// The volume serial number and the 128-bit file id of the open file `file`.
fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
    let mut information = FILE_ID_INFO::default();
    let size = u32::try_from(size_of::<FILE_ID_INFO>()).map_err(io::Error::other)?;
    // SAFETY: the handle `file` owns is open for the whole call, and `information` is a writable `FILE_ID_INFO` of
    // `size` bytes, the buffer the `FileIdInfo` class fills.
    let read = unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), FileIdInfo, (&raw mut information).cast(), size) };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((information.VolumeSerialNumber, information.FileId.Identifier))
}

/// Renames the file `source` to `target` on the same volume, never replacing a file `target` already names. Both are
/// made absolute and given the verbatim prefix first, so a path past `MAX_PATH` is moved as well.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::AlreadyExists`] when `target` names a file already, of kind
/// [`io::ErrorKind::InvalidInput`] for a path holding a NUL, else the system's error, such as a sharing violation for a
/// file another process holds open without delete sharing.
pub fn move_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    let source = wide_path(source)?;
    let target = wide_path(target)?;
    // SAFETY: both are NUL-terminated UTF-16 buffers that outlive the call; with no flags the call neither copies across
    // volumes nor replaces the target.
    let moved = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The backslash that separates a path's components.
const SEP: u16 = b'\\' as u16;
/// The colon after a drive letter.
const COLON: u16 = b':' as u16;
/// What a verbatim path starts with, `\\?\`, which the system takes as it is, past `MAX_PATH`.
const VERBATIM: [u16; 4] = [SEP, SEP, b'?' as u16, SEP];
/// What an NT object path starts with, `\??\`, which is left as it is.
const NT_OBJECT: [u16; 4] = [SEP, b'?' as u16, b'?' as u16, SEP];
/// What a verbatim UNC path starts with, `\\?\UNC\`.
const VERBATIM_UNC: [u16; 8] = [SEP, SEP, b'?' as u16, SEP, b'U' as u16, b'N' as u16, b'C' as u16, SEP];
/// What a device path starts with, `\\.\`.
const DEVICE: [u16; 4] = [SEP, SEP, b'.' as u16, SEP];

/// `path` made absolute and verbatim, as a NUL-terminated UTF-16 buffer the system takes past `MAX_PATH`; else the
/// error for a path that holds a NUL, which would cut it short, or one that cannot be made absolute.
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the path {} holds a NUL", path.display()),
        ));
    }
    let absolute: Vec<u16> = std::path::absolute(path)?.as_os_str().encode_wide().collect();
    let mut wide = verbatim(&absolute);
    wide.push(0);
    Ok(wide)
}

/// The absolute, normalized path `absolute` with the verbatim prefix, as the standard library gives one: `C:\…` becomes
/// `\\?\C:\…`, a device path `\\.\…` becomes `\\?\…` and a UNC path `\\server\…` becomes `\\?\UNC\server\…`; a path
/// already verbatim, or an NT object path, is left as it is, and so is any other.
fn verbatim(absolute: &[u16]) -> Vec<u16> {
    let (prefix, rest): (&[u16], &[u16]) = if let [_, COLON, SEP, ..] = absolute {
        (&VERBATIM, absolute)
    } else if absolute.starts_with(&VERBATIM) || absolute.starts_with(&NT_OBJECT) {
        (&[], absolute)
    } else if let Some(rest) = absolute.strip_prefix(&DEVICE) {
        (&VERBATIM, rest)
    } else if let Some(rest) = absolute.strip_prefix(&[SEP, SEP]) {
        (&VERBATIM_UNC, rest)
    } else {
        (&[], absolute)
    };
    [prefix, rest].concat()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose files cannot be made must fail loudly")]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS};

    use super::{move_no_replace, open_beneath, same_file, verbatim};

    /// The folder `path` held open, as a caller of [`open_beneath`] holds one.
    fn held(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("the folder can be opened")
    }

    /// Whether the open `file` is a link opened as itself: a name-surrogate reparse point.
    fn is_link_itself(file: &File) -> bool {
        let metadata = file.metadata().expect("the handle's metadata can be read");
        metadata.file_type().is_symlink() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[test]
    fn a_name_is_opened_in_the_held_folder_in_any_letter_case() {
        let folder = folder("case");
        std::fs::write(folder.join("siblings.toml"), "a").expect("the file can be written");

        let mut opened = open_beneath(&held(&folder), "Siblings.TOML").expect("the name is found in another case");
        let mut text = String::new();
        opened.read_to_string(&mut text).expect("the file can be read");

        assert_eq!(text, "a");
        drop(opened);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_missing_name_is_not_found() {
        let folder = folder("missing");

        let opened = open_beneath(&held(&folder), "siblings.toml");

        assert_eq!(opened.map(drop).map_err(|error| error.kind()), Err(io::ErrorKind::NotFound));
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "a test's one mklink takes no spawn lock")]
    fn a_junction_at_the_name_is_opened_as_itself() {
        let folder = folder("junction");
        std::fs::create_dir(folder.join("target")).expect("the junction's target can be made");
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(folder.join("link"))
            .arg(folder.join("target"))
            .output()
            .expect("cmd.exe runs");
        assert!(made.status.success(), "a junction needs no privilege: {made:?}");

        let opened = open_beneath(&held(&folder), "link").expect("the junction can be opened");

        assert!(is_link_itself(&opened), "the junction is opened as itself, not followed");
        drop(opened);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    #[expect(clippy::print_stderr, reason = "a skipped case says why it skipped")]
    fn a_file_symlink_at_the_name_is_opened_as_itself() {
        let folder = folder("symlink");
        std::fs::write(folder.join("target.txt"), "a").expect("the file can be written");
        if let Err(error) = std::os::windows::fs::symlink_file(folder.join("target.txt"), folder.join("link.txt")) {
            eprintln!("skipped: a file symlink cannot be made here (no privilege, or developer mode off): {error}");
            std::fs::remove_dir_all(&folder).expect("the folder can be removed");
            return;
        }

        let opened = open_beneath(&held(&folder), "link.txt").expect("the symlink can be opened");

        assert!(is_link_itself(&opened), "the symlink is opened as itself, not followed");
        drop(opened);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_name_that_is_not_one_name_is_refused() {
        let folder = folder("names");
        let held = held(&folder);
        for name in ["", ".", "..", r"a\b", "a/b", "a:b", "a\0b"] {
            let opened = open_beneath(&held, name);
            assert_eq!(
                opened.map(drop).map_err(|error| error.kind()),
                Err(io::ErrorKind::InvalidInput),
                "{name:?}"
            );
        }
        drop(held);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn same_file_tells_one_file_from_another() {
        let folder = folder("same");
        let (a, b) = (folder.join("a.txt"), folder.join("b.txt"));
        std::fs::write(&a, "a").expect("the file can be written");
        std::fs::write(&b, "b").expect("the file can be written");
        let open = |path: &Path| File::open(path).expect("the file can be opened");

        assert!(same_file(&open(&a), &open(&a)).expect("the ids can be read"), "two handles of one file");
        assert!(!same_file(&open(&a), &open(&b)).expect("the ids can be read"), "two files");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    /// [`verbatim`] of the absolute path `absolute`, read back as text.
    fn verbatim_of(absolute: &str) -> String {
        let wide: Vec<u16> = absolute.encode_utf16().collect();
        String::from_utf16(&verbatim(&wide)).expect("the path is UTF-16")
    }

    #[test]
    fn each_kind_of_absolute_path_gets_the_verbatim_prefix_once() {
        assert_eq!(verbatim_of(r"C:\a\b.txt"), r"\\?\C:\a\b.txt", "a drive path");
        assert_eq!(verbatim_of(r"\\server\share\a"), r"\\?\UNC\server\share\a", "a UNC path");
        assert_eq!(verbatim_of(r"\\.\C:\a"), r"\\?\C:\a", "a device path");
        assert_eq!(verbatim_of(r"\\?\C:\a"), r"\\?\C:\a", "a verbatim path");
        assert_eq!(verbatim_of(r"\\?\UNC\server\share"), r"\\?\UNC\server\share", "a verbatim UNC path");
        assert_eq!(verbatim_of(r"\??\C:\a"), r"\??\C:\a", "an NT object path");
        assert_eq!(verbatim_of(r"\a"), r"\a", "anything else");
    }

    /// A new, empty folder of the test `name`'s own.
    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-win32-file-{name}-{}", std::process::id()));
        if folder.exists() {
            std::fs::remove_dir_all(&folder).expect("an earlier run's folder is removed");
        }
        std::fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    #[test]
    fn a_file_is_moved_to_a_free_name() {
        let folder = folder("free");
        let (source, target) = (folder.join("a.txt"), folder.join("b.txt"));
        std::fs::write(&source, "a").expect("the file can be written");

        move_no_replace(&source, &target).expect("the rename goes through");

        assert!(!source.exists(), "the source is gone");
        assert_eq!(std::fs::read_to_string(&target).expect("the target is there"), "a");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_file_the_target_names_already_is_never_replaced() {
        let folder = folder("taken");
        let (source, target) = (folder.join("a.txt"), folder.join("b.txt"));
        std::fs::write(&source, "a").expect("the file can be written");
        std::fs::write(&target, "b").expect("the file can be written");

        let moved = move_no_replace(&source, &target);

        assert_eq!(moved.map_err(|error| error.kind()), Err(io::ErrorKind::AlreadyExists));
        assert_eq!(std::fs::read_to_string(&source).expect("the source is there"), "a");
        assert_eq!(std::fs::read_to_string(&target).expect("the target is there"), "b");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    /// A file whose source and target paths are both longer than the 260 characters of `MAX_PATH` is moved.
    #[test]
    fn a_file_whose_paths_pass_max_path_is_moved() {
        let folder = folder("long");
        let deep = folder.join("a".repeat(150)).join("b".repeat(150));
        std::fs::create_dir_all(&deep).expect("the deep folder can be made");
        let (source, target) = (deep.join("source.txt"), deep.join("target.txt"));
        assert!(source.as_os_str().len() > 260 && target.as_os_str().len() > 260, "{}", source.display());
        std::fs::write(&source, "a").expect("the file can be written");

        move_no_replace(&source, &target).expect("the rename goes through");

        assert!(!source.exists(), "the source is gone");
        assert_eq!(std::fs::read_to_string(&target).expect("the target is there"), "a");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_path_holding_a_nul_is_refused() {
        let moved = move_no_replace(Path::new("a\0b"), Path::new("c"));

        assert_eq!(moved.map_err(|error| error.kind()), Err(io::ErrorKind::InvalidInput));
    }
}
