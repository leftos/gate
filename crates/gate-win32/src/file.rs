//! File calls the standard library does not make: a rename that never replaces the file its target names, an open of
//! a name inside an open folder, a folder made there with the security it is to have from the instant it exists, a
//! listing and a tree delete through an open folder's handle, an open that reads an
//! open file through its reparse point's filter, an open file's id, whether two open files are one, and whether an open
//! file is on a network volume ([`on_network_volume`]).

use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::ptr;
use std::thread;
use std::time::{Duration, SystemTime};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
    FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformationEx, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION, GENERIC_READ,
    INVALID_HANDLE_VALUE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_DELETE_PENDING, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_FULL_DIR_INFO, FILE_GENERIC_READ, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_REMOTE_PROTOCOL_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfoEx,
    FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileIdInfo, FileRemoteProtocolInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, MoveFileExW, ReOpenFile, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use crate::security::Descriptor;

/// Opens `name`, one file or folder name, inside the open folder `folder` for reading, as itself: a link or a junction
/// so named is opened as the link, not followed. The name is looked up in the folder the handle holds, whatever its
/// path names by now, and matched in any letter case unless that folder is case-sensitive, as a path open matches it.
/// The new handle shares what `share` says.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::NotFound`] when nothing in the folder has that name, of kind
/// [`io::ErrorKind::InvalidInput`], before any system call, for a name that is not one name ([`one_name`]) or is too
/// long to pass, else the system's error.
pub fn open_beneath(folder: &File, name: &str, share: Share) -> io::Result<File> {
    open_beneath_as(folder, name, FILE_GENERIC_READ, share, Make::Open)
}

/// What [`open_beneath_as`] does with its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Make {
    /// Opens what has the name, a file or a folder, as itself.
    Open,
    /// Makes a new file of the name; anything already there, a link included, is an error of kind
    /// [`io::ErrorKind::AlreadyExists`].
    NewFile,
    /// Opens the folder of the name as itself, or makes one when nothing has the name; a file there is the system's
    /// error. A junction there is opened as itself, never followed, for the caller to refuse.
    Folder,
    /// Makes a new folder of the name; anything already there is an error of kind [`io::ErrorKind::AlreadyExists`].
    NewFolder,
}

/// Opens `name`, one file or folder name, inside the open folder `folder` with the access `access` (plus
/// `SYNCHRONIZE`) as itself, as [`open_beneath`] does, or makes it there as `make` says. The new handle shares what
/// `share` says.
///
/// # Errors
///
/// As [`open_beneath`]'s, and the system's error when the name cannot be made or opened with that access.
pub fn open_beneath_as(folder: &File, name: &str, access: u32, share: Share, make: Make) -> io::Result<File> {
    one_name(name)?;
    let wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    open_wide(
        folder,
        &wide,
        Opening {
            access,
            share,
            make,
            security: None,
        },
    )
}

/// Makes a new folder `name`, one name, inside the open folder `folder` with the security `security` names, its owner,
/// DACL and label included, so the folder is never seen with any other security, not even the one its folder hands
/// down; opened with the access `access` (plus `SYNCHRONIZE`), sharing what `share` says.
///
/// # Errors
///
/// As [`open_beneath_as`]'s with [`Make::NewFolder`]: an error of kind [`io::ErrorKind::AlreadyExists`] when anything
/// has the name, and the system's error when the folder cannot be made with that security, such as access denied for
/// an owner this process may not set.
pub fn make_folder_beneath(folder: &File, name: &str, access: u32, share: Share, security: &Descriptor) -> io::Result<File> {
    one_name(name)?;
    let wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    open_wide(
        folder,
        &wide,
        Opening {
            access,
            share,
            make: Make::NewFolder,
            security: Some(security),
        },
    )
}

/// Makes a new file `name`, one name, inside the open folder `folder` with the security `security` names, its owner,
/// DACL and label included, so the file is never seen with any other security, not even the one its folder hands
/// down; opened with the access `access` (plus `SYNCHRONIZE`), sharing what `share` says.
///
/// # Errors
///
/// As [`open_beneath_as`]'s with [`Make::NewFile`]: an error of kind [`io::ErrorKind::AlreadyExists`] when anything
/// has the name, and the system's error when the file cannot be made with that security.
pub fn make_file_beneath(folder: &File, name: &str, access: u32, share: Share, security: &Descriptor) -> io::Result<File> {
    one_name(name)?;
    let wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    open_wide(
        folder,
        &wide,
        Opening {
            access,
            share,
            make: Make::NewFile,
            security: Some(security),
        },
    )
}

/// How [`open_wide`] opens or makes its name: with what access (plus `SYNCHRONIZE`), sharing what, making what, and
/// the security a new file or folder is made with, none for what its folder hands down.
#[derive(Clone, Copy)]
struct Opening<'a> {
    access: u32,
    share: Share,
    make: Make,
    security: Option<&'a Descriptor>,
}

/// [`open_beneath_as`] of the UTF-16 `name`, which is taken as it is, unchecked: one name a listing gave, or none at
/// all, which opens `folder` itself again.
fn open_wide(folder: &File, name: &[u16], opening: Opening<'_>) -> io::Result<File> {
    open_wide_status(folder, name, opening)?.map_err(status_error)
}

/// [`open_wide`] for [`delete_fresh_beneath`]'s walk: `None` when the entry is gone, a file whose delete is pending (a
/// legacy delete while another handle holds it, which the open refuses with `STATUS_DELETE_PENDING`) included, as it
/// goes with that handle and cannot be opened by anyone.
fn open_walked(folder: &File, name: &[u16], opening: Opening<'_>) -> io::Result<Option<File>> {
    match open_wide_status(folder, name, opening)? {
        Ok(file) => Ok(Some(file)),
        Err(STATUS_DELETE_PENDING) => Ok(None),
        Err(status) => match status_error(status) {
            error if error.kind() == io::ErrorKind::NotFound => Ok(None),
            error => Err(error),
        },
    }
}

/// The system's error for the failing NT `status`, as `GetLastError` would give it.
fn status_error(status: NTSTATUS) -> io::Error {
    // SAFETY: the call reads nothing but the status it is given.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code.cast_signed())
}

/// [`open_wide`] before its failing NT status is converted: the inner error is the status `NtCreateFile` gave, the
/// outer one a name or a structure the call could not be made with.
fn open_wide_status(folder: &File, name: &[u16], opening: Opening<'_>) -> io::Result<Result<File, NTSTATUS>> {
    let Opening {
        access,
        share,
        make,
        security,
    } = opening;
    let (disposition, options) = match make {
        Make::Open => (FILE_OPEN, 0),
        Make::NewFile => (FILE_CREATE, FILE_NON_DIRECTORY_FILE),
        Make::Folder => (FILE_OPEN_IF, FILE_DIRECTORY_FILE),
        Make::NewFolder => (FILE_CREATE, FILE_DIRECTORY_FILE),
    };
    let length = u16::try_from(size_of_val(name)).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name is too long"))?;
    let object_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        // The system only reads the name.
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).map_err(io::Error::other)?,
        RootDirectory: folder.as_raw_handle(),
        ObjectName: &raw const object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: security.map_or(ptr::null(), |security| security.as_ptr().cast_const().cast()),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle = ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: the name's buffer, the attributes, the security descriptor (if any, borrowed for the call) and the status
    // block outlive the call, the attributes' root is the open handle `folder` owns, and `handle` is writable;
    // synchronous I/O and `SYNCHRONIZE` make a handle std reads from and writes to as to any other.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            access | SYNCHRONIZE,
            &raw const attributes,
            &raw mut status_block,
            ptr::null(),
            0,
            share.mode(),
            disposition,
            options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Ok(Err(status));
    }
    // SAFETY: a success is a fresh handle nothing else owns; the returned file owns and closes it from now on.
    Ok(Ok(unsafe { File::from_raw_handle(handle) }))
}

/// Renames the open file `file`, opened with `DELETE` access, to `name`, one name, inside the open folder `folder` on
/// the same volume, by handle: `NtSetInformationFile` with `FileRenameInformationEx`, the held folder its root, so no
/// path is looked up and no link on the way is followed. (`SetFileInformationByHandle` with `FileRenameInfoEx` and a
/// root folder failed with `ERROR_INVALID_PARAMETER` on Windows 11, so the native call is made.) A file or a link
/// already of that name is replaced as itself
/// (POSIX semantics: its name is free at once, even while another handle holds it open sharing deletion); a folder
/// there is not.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::InvalidInput`] for a name that is not one name ([`one_name`]) or too long, else
/// the system's error, such as access denied for a folder at the name or a sharing violation for a file held open
/// without delete sharing.
pub fn rename_beneath(file: &File, folder: &File, name: &str) -> io::Result<()> {
    renamed(
        file,
        folder,
        name,
        RENAME_REPLACE_IF_EXISTS | RENAME_POSIX_SEMANTICS | RENAME_IGNORE_READONLY_ATTRIBUTE,
    )
}

/// [`rename_beneath`], except that nothing already of that name, a file or a link included, is replaced.
///
/// # Errors
///
/// As [`rename_beneath`]'s, and an error of kind [`io::ErrorKind::AlreadyExists`] when anything has the name.
pub fn rename_beneath_no_replace(file: &File, folder: &File, name: &str) -> io::Result<()> {
    renamed(file, folder, name, RENAME_POSIX_SEMANTICS)
}

/// [`rename_beneath`] with the rename flags `flags`.
fn renamed(file: &File, folder: &File, name: &str, flags: u32) -> io::Result<()> {
    one_name(name)?;
    let wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    let name_bytes = size_of_val(wide.as_slice());
    let size = size_of::<FILE_RENAME_INFORMATION>() + name_bytes;
    // Eight-byte units, so the buffer is aligned for the structure's handle.
    let mut buffer = vec![0_u64; size.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let length = u32::try_from(name_bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name is too long"))?;
    let total = u32::try_from(size).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name is too long"))?;
    // SAFETY: `buffer` is zeroed, aligned for `FILE_RENAME_INFORMATION` and holds `size` bytes, the structure and the name's
    // units after its `FileName` start; the copy writes `wide.len()` units from there, inside the buffer.
    unsafe {
        (*info).Anonymous.Flags = flags;
        (*info).RootDirectory = folder.as_raw_handle();
        (*info).FileNameLength = length;
        ptr::copy_nonoverlapping(wide.as_ptr(), (&raw mut (*info).FileName).cast::<u16>(), wide.len());
    }
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: both handles are open for the whole call, the buffer holds `total` bytes of a filled-in structure, and
    // the status block is writable.
    let status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &raw mut status_block,
            buffer.as_ptr().cast(),
            total,
            FileRenameInformationEx,
        )
    };
    if status < 0 {
        // SAFETY: the call reads nothing but the status it is given.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code.cast_signed()));
    }
    Ok(())
}

/// `FILE_RENAME_FLAG_REPLACE_IF_EXISTS`: a file already of the name is replaced.
const RENAME_REPLACE_IF_EXISTS: u32 = 0x1;
/// `FILE_RENAME_FLAG_POSIX_SEMANTICS`: a replaced file's name is free at once, even while it is held open.
const RENAME_POSIX_SEMANTICS: u32 = 0x2;
/// `FILE_RENAME_FLAG_IGNORE_READONLY_ATTRIBUTE`: a read-only file at the name is replaced as any other.
const RENAME_IGNORE_READONLY_ATTRIBUTE: u32 = 0x40;
/// `FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE`: a read-only file is deleted as any other.
const DISPOSITION_IGNORE_READONLY_ATTRIBUTE: u32 = 0x10;

/// Deletes the open file or empty folder `file`, opened with `DELETE` access, as itself, by handle:
/// `SetFileInformationByHandle` with `FileDispositionInfoEx` and POSIX semantics, so its name is free at once even while
/// another handle holds it open sharing deletion, a read-only file as any other. A link so opened is deleted, not what
/// it leads to.
///
/// # Errors
///
/// The system's error, such as access denied for a handle without `DELETE` access, or the folder-not-empty error.
pub fn delete_opened(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS | DISPOSITION_IGNORE_READONLY_ATTRIBUTE,
    };
    let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).map_err(io::Error::other)?;
    // SAFETY: the handle is open for the whole call, and `info` is a filled-in structure of `size` bytes.
    let deleted = unsafe { SetFileInformationByHandle(file.as_raw_handle(), FileDispositionInfoEx, (&raw const info).cast(), size) };
    if deleted == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The names of every entry inside the open folder `folder`, opened with list access (`FILE_GENERIC_READ` holds it),
/// read through the handle: `GetFileInformationByHandleEx` with the full directory classes, so no path is looked up.
/// `.` and `..` are left out. Each name is kept as the system gave it, so one that is not valid UTF-16 is listed too.
///
/// # Errors
///
/// The system's error, such as an invalid-parameter error for a handle that is not a folder's.
pub fn names_beneath(folder: &File) -> io::Result<Vec<OsString>> {
    Ok(entries_beneath(folder)?.into_iter().map(|entry| entry.name).collect())
}

/// One entry of a folder's listing as the full directory class gives it: its name and its attributes.
struct Listed {
    name: OsString,
    attributes: u32,
}

/// Every entry inside the open folder `folder`, as [`names_beneath`] lists them, each with what the listing says of it.
fn entries_beneath(folder: &File) -> io::Result<Vec<Listed>> {
    // Eight-byte units, so each entry the system writes is aligned for its structure.
    let mut buffer = vec![0_u64; DIRECTORY_BUFFER_UNITS];
    let size = u32::try_from(size_of_val(buffer.as_slice())).map_err(io::Error::other)?;
    let mut entries = Vec::new();
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        // SAFETY: the handle is open for the whole call, and `buffer` is writable for `size` bytes.
        let read = unsafe { GetFileInformationByHandleEx(folder.as_raw_handle(), class, buffer.as_mut_ptr().cast(), size) };
        if read == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES.cast_signed()) {
                return Ok(entries);
            }
            return Err(error);
        }
        class = FileFullDirectoryInfo;
        listed_in(&buffer, &mut entries)?;
    }
}

/// The 64 KiB the directory listing reads into at once, in eight-byte units.
const DIRECTORY_BUFFER_UNITS: usize = 8 * 1024;

/// Adds to `entries` each entry the system wrote in `buffer`, `.` and `..` left out. Each entry is read only as far as
/// its name ends, so a short last entry near the buffer's end is read whole.
fn listed_in(buffer: &[u64], entries: &mut Vec<Listed>) -> io::Result<()> {
    const DOT: u16 = b'.' as u16;
    // SAFETY: the bytes of `buffer`, which it holds for as long as the slice lives; a byte needs no alignment.
    let raw = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size_of_val(buffer)) };
    let past = || io::Error::other("a directory entry runs past the listing's buffer");
    let name_offset = std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName);
    let mut offset = 0_usize;
    loop {
        let header = raw.get(offset..offset + name_offset).ok_or_else(past)?;
        let next = u32_at(header, std::mem::offset_of!(FILE_FULL_DIR_INFO, NextEntryOffset))?;
        let length = usize::try_from(u32_at(header, std::mem::offset_of!(FILE_FULL_DIR_INFO, FileNameLength))?).map_err(io::Error::other)?;
        let name_start = offset + name_offset;
        let name_bytes = raw.get(name_start..name_start + length).ok_or_else(past)?;
        let wide: Vec<u16> = name_bytes.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        if wide != [DOT] && wide != [DOT, DOT] {
            entries.push(Listed {
                name: OsString::from_wide(&wide),
                attributes: u32_at(header, std::mem::offset_of!(FILE_FULL_DIR_INFO, FileAttributes))?,
            });
        }
        if next == 0 {
            return Ok(());
        }
        offset += usize::try_from(next).map_err(io::Error::other)?;
    }
}

/// The `N` bytes at byte `at` of a directory entry's `header`.
fn bytes_at<const N: usize>(header: &[u8], at: usize) -> io::Result<[u8; N]> {
    header
        .get(at..at + N)
        .and_then(|bytes| <[u8; N]>::try_from(bytes).ok())
        .ok_or_else(|| io::Error::other("a directory entry's header is cut short"))
}

/// The little-endian `u32` at byte `at` of a directory entry's `header`.
fn u32_at(header: &[u8], at: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(bytes_at(header, at)?))
}

/// Deletes the open file or folder `top`, opened with `DELETE` access and with read sharing given, as itself: a folder
/// of its own emptied beneath its handle first, then each deleted by its handle ([`delete_opened`]). The walk is
/// iterative, so a tree of any depth is deleted: each entry is opened as itself beneath the folder it is in, by the
/// name the listing gave, with `DELETE` and attribute access alone and held without delete sharing; a folder of its
/// own is opened again with list access only while it is listed, and emptied the same way before it is deleted. An
/// entry gone between the listing and its open counts as deleted. A link or a junction is deleted as itself, never
/// followed, so nothing outside `top` is reached.
///
/// # Errors
///
/// The first system error met, naming the entry it was met on, such as a sharing violation for an entry another
/// process holds without delete sharing; what was deleted before it stays deleted.
pub fn delete_tree_opened(top: &File) -> io::Result<()> {
    if !is_own_folder(top)? {
        return delete_opened(top);
    }
    let mut stack = vec![Level {
        folder: None,
        names: listed(top)?,
    }];
    while let Some(level) = stack.last_mut() {
        let Some(name) = level.names.pop() else {
            if let Some(Level {
                folder: Some((folder, name)),
                ..
            }) = stack.pop()
            {
                delete_opened(&folder).map_err(|error| named(&name, &error))?;
            }
            continue;
        };
        let parent = level.folder.as_ref().map_or(top, |(folder, _)| folder);
        let Some(entry) = opened_entry(parent, &name)? else {
            continue;
        };
        if is_own_folder(&entry).map_err(|error| named(&name, &error))? {
            let names = listed(&entry).map_err(|error| named(&name, &error))?;
            stack.push(Level {
                folder: Some((entry, name)),
                names,
            });
        } else {
            delete_opened(&entry).map_err(|error| named(&name, &error))?;
        }
    }
    delete_opened(top)
}

/// A folder [`delete_tree_opened`] is emptying: the folder, with its name (`None` for the top one, which the caller
/// holds), and the names in it not yet deleted.
struct Level {
    folder: Option<(File, OsString)>,
    names: Vec<OsString>,
}

/// The entry `name` of the open folder `parent`, opened as itself with `DELETE` and attribute access and held without
/// delete sharing; `None` when it is gone. Else the error, naming the entry.
fn opened_entry(parent: &File, name: &OsStr) -> io::Result<Option<File>> {
    let wide: Vec<u16> = name.encode_wide().collect();
    let opening = Opening {
        access: DELETE | FILE_READ_ATTRIBUTES,
        share: Share::NoDelete,
        make: Make::Open,
        security: None,
    };
    match open_wide(parent, &wide, opening) {
        Ok(entry) => Ok(Some(entry)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(named(name, &error)),
    }
}

/// The names in the open folder `folder`, listed through a second handle of it with list access alone, closed once
/// listed.
fn listed(folder: &File) -> io::Result<Vec<OsString>> {
    let opening = Opening {
        access: FILE_LIST_DIRECTORY,
        share: Share::All,
        make: Make::Open,
        security: None,
    };
    names_beneath(&open_wide(folder, &[], opening)?)
}

/// Whether the open `file` is a folder of its own: a folder, and no reparse point.
fn is_own_folder(file: &File) -> io::Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}

/// `error`, of its kind, saying it was met deleting the entry `name`.
fn named(name: &OsStr, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("cannot delete {}: {error}", name.to_string_lossy()))
}

/// Which files [`delete_fresh_beneath`] deletes, and how hard it tries.
#[derive(Clone, Copy, Debug)]
pub struct Fresh {
    /// A file is fresh when the later of its creation and last-write times, as its own handle gives them, is at or
    /// after this time; `None` makes every file fresh.
    pub since: Option<SystemTime>,
    /// How many times an open another process's handle refuses with a sharing violation is tried in all before the
    /// walk stops on it: a fresh file's open for deletion, and a folder's open to be held while the walk is inside it.
    pub tries: u32,
    /// How long the walk waits between two tries.
    pub wait: Duration,
}

/// Where [`delete_fresh_beneath`] stopped: the entry's path below the top folder, empty for the top folder itself,
/// and the system's error met on it.
#[derive(Debug)]
pub struct NotDeleted {
    pub path: PathBuf,
    pub error: io::Error,
}

/// Deletes every fresh file ([`Fresh`]) at any depth below the open folder `top`, by handle descent alone, judging each
/// entry's kind by what its folder's listing says of it (its attributes), and each file's freshness by its own times
/// ([`written_since`]), so every entry is opened beneath the folder it is in, by the name the listing gave, as itself,
/// never through a reparse point. A fresh file is opened with `DELETE` access and deleted as itself ([`delete_opened`]), so a hard link
/// loses only the name below `top`; one that is a folder by the time it is opened is left. A folder of its own is
/// opened and held without delete sharing while the walk is inside it, so it cannot be renamed out of `top`, listed,
/// descended into, and stays, emptied or not; a folder that is a reparse point (a junction or a folder link) is neither
/// descended into nor deleted; an entry named `.git` in any letter case, file or folder, is skipped at every depth with
/// all it holds. A sharing violation on either open is tried again as `fresh` says; an entry gone between the listing
/// and its open is skipped. How many files were deleted.
///
/// The listing's times are never used: NTFS keeps a stale time in the listing for a name the file was not written
/// through, such as a second hard link to it, so each file is opened with attribute access alone and judged by its
/// own times.
///
/// # Errors
///
/// The first entry that could not be listed, opened or deleted, with the system's error; what was deleted before it
/// stays deleted.
pub fn delete_fresh_beneath(top: &File, fresh: Fresh) -> Result<usize, NotDeleted> {
    let stopped = |path: &Path| {
        let path = path.to_path_buf();
        move |error| NotDeleted { path, error }
    };
    let mut deleted = 0;
    let mut stack = vec![FreshLevel {
        folder: None,
        path: PathBuf::new(),
        entries: listed_entries(top).map_err(stopped(Path::new("")))?,
    }];
    while let Some(level) = stack.last_mut() {
        let Some(entry) = level.entries.pop() else {
            stack.pop();
            continue;
        };
        if is_git_name(&entry.name) {
            continue;
        }
        let path = level.path.join(&entry.name);
        let parent = level.folder.as_ref().unwrap_or(top);
        match fresh_visit(parent, &entry, fresh).map_err(stopped(&path))? {
            Visit::Descend(folder, entries) => stack.push(FreshLevel {
                folder: Some(folder),
                path,
                entries,
            }),
            Visit::Deleted => deleted += 1,
            Visit::Kept => {}
        }
    }
    Ok(deleted)
}

/// A folder [`delete_fresh_beneath`] walks: the folder, held without delete sharing (`None` for the top one, which the
/// caller holds), its path below the top folder, and the entries in it not yet visited.
struct FreshLevel {
    folder: Option<File>,
    path: PathBuf,
    entries: Vec<Listed>,
}

/// What [`fresh_visit`] did with an entry.
enum Visit {
    /// A folder of its own, held without delete sharing, and the entries in it.
    Descend(File, Vec<Listed>),
    /// A fresh file, deleted.
    Deleted,
    /// Anything else, left as it is: a file that is not fresh, a folder that is a reparse point, or an entry gone.
    Kept,
}

/// Visits the listed `entry` of the open folder `parent` for [`delete_fresh_beneath`].
fn fresh_visit(parent: &File, entry: &Listed, fresh: Fresh) -> io::Result<Visit> {
    let wide: Vec<u16> = entry.name.encode_wide().collect();
    if entry.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        if entry.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(Visit::Kept);
        }
        let Some(folder) = held_folder(parent, &wide, fresh)? else {
            return Ok(Visit::Kept);
        };
        // The listing may be older than the entry: one that is no folder of its own by now is left as it is.
        if !is_own_folder(&folder)? {
            return Ok(Visit::Kept);
        }
        let entries = entries_beneath(&folder)?;
        return Ok(Visit::Descend(folder, entries));
    }
    if let Some(since) = fresh.since
        && !written_since(parent, &wide, since)?
    {
        return Ok(Visit::Kept);
    }
    Ok(if deleted_retrying(parent, &wide, fresh)? {
        Visit::Deleted
    } else {
        Visit::Kept
    })
}

/// Whether the file `name` of the open folder `parent` was made or written at or after `since`, by the later of its
/// creation and last-write times read through its own handle: opened beneath `parent` as itself, never followed, with
/// attribute access alone, which no other process's sharing refuses. Not when it is gone.
fn written_since(parent: &File, name: &[u16], since: SystemTime) -> io::Result<bool> {
    let opening = Opening {
        access: FILE_READ_ATTRIBUTES,
        share: Share::All,
        make: Make::Open,
        security: None,
    };
    let Some(file) = open_walked(parent, name, opening)? else {
        return Ok(false);
    };
    written_at_or_after(&file.metadata()?, since)
}

/// Whether the file `metadata` describes was made or written at or after `since`, by the later of its creation and
/// last-write times.
fn written_at_or_after(metadata: &Metadata, since: SystemTime) -> io::Result<bool> {
    Ok(metadata.created()?.max(metadata.modified()?) >= since)
}

/// The entries in the open folder `folder`, listed through a second handle of it with list access alone, closed once
/// listed.
fn listed_entries(folder: &File) -> io::Result<Vec<Listed>> {
    let opening = Opening {
        access: FILE_LIST_DIRECTORY,
        share: Share::All,
        make: Make::Open,
        security: None,
    };
    entries_beneath(&open_wide(folder, &[], opening)?)
}

/// The folder `name` of the open folder `parent`, opened as itself to be listed and held without delete sharing, as
/// the work folder is held; `None` when it is gone. A sharing violation is tried again after `fresh.wait`, up to
/// `fresh.tries` tries in all.
fn held_folder(parent: &File, name: &[u16], fresh: Fresh) -> io::Result<Option<File>> {
    let opening = Opening {
        access: FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        share: Share::NoDelete,
        make: Make::Open,
        security: None,
    };
    retried_open(parent, name, opening, fresh)
}

/// Deletes the file `name` of the open folder `parent` as itself: opened beneath it with `DELETE` access and deleted
/// through that handle ([`delete_opened`]); a sharing violation on the open is tried again after `fresh.wait`, up to
/// `fresh.tries` tries in all. Whether it was deleted: not when it is gone, or is a folder by now.
fn deleted_retrying(parent: &File, name: &[u16], fresh: Fresh) -> io::Result<bool> {
    let opening = Opening {
        access: DELETE | FILE_READ_ATTRIBUTES,
        share: Share::All,
        make: Make::Open,
        security: None,
    };
    let Some(file) = retried_open(parent, name, opening, fresh)? else {
        return Ok(false);
    };
    deleted_if_fresh(&file, fresh.since)
}

/// Deletes the open file `file`, opened with `DELETE` and `FILE_READ_ATTRIBUTES` access, through that handle
/// ([`delete_opened`]) when its own times, read through the same handle, say it was made or written at or after
/// `since` ([`written_at_or_after`]), so a stale file renamed over the name after a first look is kept. Whether it was
/// deleted: not when it is a folder by now, or is not fresh.
fn deleted_if_fresh(file: &File, since: Option<SystemTime>) -> io::Result<bool> {
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Ok(false);
    }
    if let Some(since) = since
        && !written_at_or_after(&metadata, since)?
    {
        return Ok(false);
    }
    delete_opened(file)?;
    Ok(true)
}

/// The entry `name` of the open folder `parent` opened with `opening`, `None` when it is gone; a sharing violation is
/// tried again after `fresh.wait`, up to `fresh.tries` tries in all.
fn retried_open(parent: &File, name: &[u16], opening: Opening<'_>, fresh: Fresh) -> io::Result<Option<File>> {
    let mut tried = 1;
    loop {
        match open_walked(parent, name, opening) {
            Ok(opened) => return Ok(opened),
            Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION.cast_signed()) && tried < fresh.tries => {
                thread::sleep(fresh.wait);
                tried += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether `name` is `.git` in any letter case.
fn is_git_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| name.eq_ignore_ascii_case(".git"))
}

/// What others may do with a file or folder while a handle holds it open, [`open_beneath`]'s or another open's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Share {
    /// Read, write and delete it, a rename included.
    All,
    /// Read and write it, but neither delete nor rename it, a folder's own name included, until the handle is closed.
    NoDelete,
    /// Read and delete it, but not write it, so what a reader hashed stays what the handle holds.
    NoWrite,
    /// Nothing: no other open of it at all until the handle is closed, so an open that finds another handle already
    /// there fails with a sharing violation.
    None,
}

impl Share {
    /// The share mode of an open that lets others do this, as `CreateFileW` and `OpenOptions::share_mode` take it.
    #[must_use]
    pub fn mode(self) -> u32 {
        match self {
            Share::All => FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            Share::NoDelete => FILE_SHARE_READ | FILE_SHARE_WRITE,
            Share::NoWrite => FILE_SHARE_READ | FILE_SHARE_DELETE,
            Share::None => 0,
        }
    }
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

/// Whether the open files `first` and `second` give the same pair of volume serial number and file id ([`file_id`]):
/// one file.
///
/// # Errors
///
/// The system's error when either handle's pair cannot be read, and [`file_id`]'s error for a zero id.
pub fn same_file(first: &File, second: &File) -> io::Result<bool> {
    Ok(file_id(first)? == file_id(second)?)
}

/// The volume serial number and the 128-bit file id of the open file or folder `file`: two handles of one file give
/// the same pair, and, but for the file index below, two of different files never do.
///
/// The id is the 128-bit one `FileIdInfo` gives, which a Dev Drive and other `ReFS` volumes need. A file system
/// that does not serve that class (FAT and exFAT) gives its 32-bit volume serial number and 64-bit file index
/// instead, each widened into the same pair. Which of the two a file gives is the file system's, so two reads of one
/// volume's files always take the same one. Such a file system may give the index 0 for every file, so a zero id
/// does not tell its files apart: this refuses a zero id from either source, and no caller ever compares one.
///
/// # Errors
///
/// The system's error when neither can be read; an error of kind [`io::ErrorKind::Unsupported`] when the file system
/// gives an id of all zeros.
pub fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
    identity_by_either(id_info(file), || index_info(file))
}

/// The pair [`file_id`] gives for a file whose `FileIdInfo` read gave `by_id`, its index read by `by_index` only when
/// the file system does not serve that class ([`serves_no_id_info`]); an id of all zeros is refused.
fn identity_by_either(
    by_id: io::Result<FILE_ID_INFO>,
    by_index: impl FnOnce() -> io::Result<BY_HANDLE_FILE_INFORMATION>,
) -> io::Result<(u64, [u8; 16])> {
    let (volume, file) = identity_read(by_id, by_index)?;
    if file == [0; 16] {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the file system gives the file no usable file id",
        ));
    }
    Ok((volume, file))
}

/// The pair `by_id` gives, or, when the file system does not serve `FileIdInfo`, the one `by_index` gives, widened.
fn identity_read(by_id: io::Result<FILE_ID_INFO>, by_index: impl FnOnce() -> io::Result<BY_HANDLE_FILE_INFORMATION>) -> io::Result<(u64, [u8; 16])> {
    match by_id {
        Ok(information) => Ok((information.VolumeSerialNumber, information.FileId.Identifier)),
        Err(error) if serves_no_id_info(&error) => {
            let information = by_index()?;
            let index = (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
            let mut file = [0_u8; 16];
            file[..8].copy_from_slice(&index.to_le_bytes());
            Ok((u64::from(information.dwVolumeSerialNumber), file))
        }
        Err(error) => Err(error),
    }
}

/// Whether a `FileIdInfo` read failed with `error` because the file system does not serve that class:
/// `ERROR_INVALID_PARAMETER`, `ERROR_NOT_SUPPORTED` or `ERROR_INVALID_FUNCTION`.
fn serves_no_id_info(error: &io::Error) -> bool {
    [ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_INVALID_FUNCTION]
        .iter()
        .any(|unserved| error.raw_os_error() == Some(unserved.cast_signed()))
}

/// The `FileIdInfo` class of the open file `file`.
fn id_info(file: &File) -> io::Result<FILE_ID_INFO> {
    let mut information = FILE_ID_INFO::default();
    let size = u32::try_from(size_of::<FILE_ID_INFO>()).map_err(io::Error::other)?;
    // SAFETY: the handle `file` owns is open for the whole call, and `information` is a writable `FILE_ID_INFO` of
    // `size` bytes, the buffer the `FileIdInfo` class fills.
    let read = unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), FileIdInfo, (&raw mut information).cast(), size) };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(information)
}

/// The volume serial number and file index of the open file `file`, as `GetFileInformationByHandle` gives them.
fn index_info(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle `file` owns is open for the whole call, and `information` is a writable
    // `BY_HANDLE_FILE_INFORMATION`, the buffer the call fills.
    let read = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut information) };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(information)
}

/// Whether the open file or folder `file` is on a network volume: whether the system gives its remote protocol
/// information (`FileRemoteProtocolInfo`), which it gives only for a file a network redirector serves; a local file
/// system fails the call with `ERROR_INVALID_PARAMETER`.
///
/// # Errors
///
/// The system's error when the call fails with any other error, so a caller never takes a failed query for a local
/// volume.
pub fn on_network_volume(file: &File) -> io::Result<bool> {
    let mut information = FILE_REMOTE_PROTOCOL_INFO::default();
    let size = u32::try_from(size_of::<FILE_REMOTE_PROTOCOL_INFO>()).map_err(io::Error::other)?;
    // SAFETY: the handle `file` owns is open for the whole call, and `information` is a writable
    // `FILE_REMOTE_PROTOCOL_INFO` of `size` bytes, the buffer the `FileRemoteProtocolInfo` class fills.
    let read = unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), FileRemoteProtocolInfo, (&raw mut information).cast(), size) };
    if read != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    // A local NTFS volume fails the query with ERROR_INVALID_PARAMETER (87): measured on folders of C:, which holds the
    // temp folder, and of X:. The documentation names no error for a local file.
    if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER.cast_signed()) {
        Ok(false)
    } else {
        Err(error)
    }
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
    use std::ffi::OsString;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_READ_ATTRIBUTES, FileDispositionInfo, READ_CONTROL, SetFileInformationByHandle,
    };

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED};
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, FILE_ID_128, FILE_ID_INFO};

    use super::{
        Fresh, Make, Share, delete_fresh_beneath, delete_opened, delete_tree_opened, deleted_if_fresh, identity_by_either, make_file_beneath,
        move_no_replace, names_beneath, open_beneath, open_beneath_as, rename_beneath, rename_beneath_no_replace, same_file, verbatim,
    };

    #[test]
    fn the_delete_judges_freshness_again_through_the_handle_it_deletes_by() {
        let folder = folder("fresh-judged-by-delete-handle");
        let (stale, fresh) = (folder.join("stale.txt"), folder.join("fresh.txt"));
        std::fs::write(&stale, "stale").expect("the stale file can be written");
        std::fs::write(&fresh, "fresh").expect("the fresh file can be written");
        let opened = |path: &Path| {
            OpenOptions::new()
                .access_mode(DELETE | FILE_READ_ATTRIBUTES)
                .share_mode(Share::All.mode())
                .open(path)
                .expect("the file can be opened to delete")
        };
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);

        let stale_deleted = deleted_if_fresh(&opened(&stale), Some(later)).expect("a stale file is judged, not an error");
        let fresh_deleted = deleted_if_fresh(&opened(&fresh), Some(earlier)).expect("the fresh file is deleted");

        assert!(!stale_deleted, "a file the delete handle's times say is stale is not deleted");
        assert!(stale.exists(), "the stale file stays");
        assert!(fresh_deleted, "a file the delete handle's times say is fresh is deleted");
        assert!(!fresh.exists(), "the fresh file goes");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_file_whose_delete_is_pending_counts_as_gone() {
        let folder = folder("fresh-delete-pending");
        let (pending, fresh_file) = (folder.join("pending.txt"), folder.join("fresh.txt"));
        std::fs::write(&pending, "pending").expect("the pending file can be written");
        std::fs::write(&fresh_file, "fresh").expect("the fresh file can be written");
        let holder = File::open(&pending).expect("a second handle holds the pending file");
        let marker = OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(Share::All.mode())
            .open(&pending)
            .expect("the pending file can be opened to delete");
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).expect("the structure is small");
        // SAFETY: the handle is open for the whole call, and `info` is a filled-in structure of `size` bytes.
        let marked = unsafe { SetFileInformationByHandle(marker.as_raw_handle(), FileDispositionInfo, (&raw const info).cast(), size) };
        assert_ne!(marked, 0, "the legacy delete is set: {}", io::Error::last_os_error());
        drop(marker);
        assert!(pending.exists(), "the name stays while the holder's handle is open");
        let fresh = Fresh {
            since: Some(std::time::SystemTime::now() - std::time::Duration::from_secs(60)),
            tries: 1,
            wait: std::time::Duration::ZERO,
        };

        let deleted = delete_fresh_beneath(&held(&folder), fresh).expect("a file whose delete is pending does not refuse the walk");

        assert_eq!(deleted, 1, "the fresh file is the one deleted; the pending one counts as gone");
        assert!(!fresh_file.exists(), "the fresh file goes");
        drop(holder);
        assert!(!pending.exists(), "the pending file goes with its last handle");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }
    use crate::security::{Descriptor, is_protected_file, protected_descriptor, security_text};
    use crate::token_facts;

    /// A read of a file's index that a case expects never to be asked for.
    fn unasked() -> io::Result<BY_HANDLE_FILE_INFORMATION> {
        Err(io::Error::other("the file index was read"))
    }

    #[test]
    fn a_file_system_without_file_id_info_gives_its_serial_and_file_index() {
        let by_index = BY_HANDLE_FILE_INFORMATION {
            dwVolumeSerialNumber: 0x1234_5678,
            nFileIndexHigh: 0x0102_0304,
            nFileIndexLow: 0x0506_0708,
            ..BY_HANDLE_FILE_INFORMATION::default()
        };
        let mut index = [0_u8; 16];
        index[..8].copy_from_slice(&0x0102_0304_0506_0708_u64.to_le_bytes());
        for unserved in [ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_INVALID_FUNCTION] {
            let by_id = Err(io::Error::from_raw_os_error(unserved.cast_signed()));

            let identity = identity_by_either(by_id, || Ok(by_index)).expect("the file index is read");

            assert_eq!(identity, (0x1234_5678, index), "the serial and the index, widened, for error {unserved}");
        }
    }

    #[test]
    fn file_id_info_is_used_when_the_file_system_gives_it() {
        let by_id = FILE_ID_INFO {
            VolumeSerialNumber: 0x0abc_def0_1234_5678,
            FileId: FILE_ID_128 { Identifier: [7; 16] },
        };

        assert_eq!(
            identity_by_either(Ok(by_id), unasked).expect("the id is read"),
            (0x0abc_def0_1234_5678, [7; 16])
        );
        let denied = identity_by_either(Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED.cast_signed())), unasked)
            .expect_err("a read that failed for another reason fails");
        assert_eq!(
            denied.raw_os_error(),
            Some(ERROR_ACCESS_DENIED.cast_signed()),
            "its own error, the index unread: {denied}"
        );
    }

    #[test]
    fn a_zero_file_id_from_either_source_is_refused() {
        let zero_index = BY_HANDLE_FILE_INFORMATION {
            dwVolumeSerialNumber: 0x1234_5678,
            ..BY_HANDLE_FILE_INFORMATION::default()
        };
        let by_id = Err(io::Error::from_raw_os_error(ERROR_NOT_SUPPORTED.cast_signed()));
        let from_index = identity_by_either(by_id, || Ok(zero_index)).expect_err("a zero file index names no one file");
        let zero_id = FILE_ID_INFO {
            VolumeSerialNumber: 0x0abc_def0_1234_5678,
            FileId: FILE_ID_128 { Identifier: [0; 16] },
        };
        let from_id = identity_by_either(Ok(zero_id), unasked).expect_err("a zero file id names no one file");

        for refused in [from_index, from_id] {
            assert_eq!(refused.kind(), io::ErrorKind::Unsupported, "{refused}");
            assert_eq!(refused.to_string(), "the file system gives the file no usable file id");
        }
    }

    /// Makes a junction at `link` leading to the folder `target`.
    #[expect(clippy::disallowed_methods, reason = "a test's one mklink takes no spawn lock")]
    fn junction(link: &Path, target: &Path) {
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd.exe runs");
        assert!(made.status.success(), "a junction needs no privilege: {made:?}");
    }

    #[test]
    fn names_beneath_lists_every_entry_of_the_held_folder_and_no_dot_names() {
        let folder = folder("names-beneath");
        std::fs::write(folder.join("a.txt"), "a").expect("the file can be written");
        std::fs::create_dir(folder.join("sub")).expect("the folder can be made");
        let many: Vec<String> = (0..600).map(|index| format!("{index:04}-{}", "n".repeat(100))).collect();
        for name in &many {
            std::fs::write(folder.join("sub").join(name), "").expect("the file can be written");
        }

        let text = |names: Vec<OsString>| {
            let mut names: Vec<String> = names.into_iter().map(|name| name.to_string_lossy().into_owned()).collect();
            names.sort();
            names
        };
        let top = text(names_beneath(&held(&folder)).expect("the folder can be listed"));
        let sub = text(names_beneath(&held(&folder.join("sub"))).expect("the folder can be listed"));

        assert_eq!(top, ["a.txt", "sub"]);
        assert_eq!(sub, many, "a listing past one buffer's worth is whole");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn delete_tree_opened_deletes_the_held_folder_and_a_junction_in_it_as_itself() {
        let folder = folder("delete-tree");
        let (stage, outside) = (folder.join("stage"), folder.join("outside"));
        std::fs::create_dir_all(stage.join(r"a\b")).expect("the folders can be made");
        std::fs::create_dir(&outside).expect("the outside folder can be made");
        std::fs::write(stage.join(r"a\b\c.txt"), "c").expect("the file can be written");
        std::fs::write(stage.join("d.txt"), "d").expect("the file can be written");
        std::fs::write(outside.join("keep.txt"), "keep").expect("the outside file can be written");
        junction(&stage.join(r"a\link"), &outside);
        let held = OpenOptions::new()
            .access_mode(FILE_GENERIC_READ | DELETE)
            .share_mode(Share::NoDelete.mode())
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&stage)
            .expect("the folder can be opened for deletion");

        delete_tree_opened(&held).expect("the folder and its contents are deleted");

        drop(held);
        assert!(!stage.exists(), "the held folder itself is deleted");
        assert!(outside.join("keep.txt").exists(), "nothing the junction leads to is deleted");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn the_clear_never_deletes_through_a_hard_link_the_name_outside_work() {
        let folder = folder("fresh-hard-link");
        let (work, outside) = (folder.join("work"), folder.join("outside"));
        std::fs::create_dir_all(work.join("bin")).expect("the work folder can be made");
        std::fs::create_dir(&outside).expect("the outside folder can be made");
        std::fs::write(outside.join("target.txt"), "kept").expect("the outside file can be written");
        std::fs::hard_link(outside.join("target.txt"), work.join(r"bin\link.txt")).expect("the hard link can be made");
        let fresh = Fresh {
            since: Some(std::time::SystemTime::now() - std::time::Duration::from_secs(60)),
            tries: 1,
            wait: std::time::Duration::ZERO,
        };

        let deleted = delete_fresh_beneath(&held(&work), fresh).expect("the fresh files are deleted");

        assert_eq!(deleted, 1, "the fresh link is the one file below the work folder");
        assert!(!work.join(r"bin\link.txt").exists(), "the work folder's name goes");
        assert_eq!(
            std::fs::read_to_string(outside.join("target.txt")).expect("the outside name is still there"),
            "kept",
            "the name outside the work folder stays, its bytes whole"
        );
        assert!(work.join("bin").is_dir(), "a folder is never deleted");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_file_written_through_its_other_hard_link_name_is_fresh_under_both_names() {
        use std::os::windows::fs::FileTimesExt as _;
        let folder = folder("fresh-written-through-link");
        let (written, other) = (folder.join("written.txt"), folder.join("other.txt"));
        std::fs::write(&written, "old").expect("the file can be written");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let backdate = |path: &Path| {
            let times = std::fs::FileTimes::new().set_modified(old).set_created(old);
            OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|file| file.set_times(times))
                .expect("the file's times can be set");
        };
        backdate(&written);
        std::fs::hard_link(&written, &other).expect("the hard link can be made");
        backdate(&other);
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(1);
        OpenOptions::new()
            .append(true)
            .open(&written)
            .and_then(|mut file| file.write_all(b" and new"))
            .expect("the file can be written through one name");
        let fresh = Fresh {
            since: Some(since),
            tries: 1,
            wait: std::time::Duration::ZERO,
        };

        let deleted = delete_fresh_beneath(&held(&folder), fresh).expect("the fresh files are deleted");

        assert!(!written.exists(), "the name written through goes");
        assert!(
            !other.exists(),
            "the other name of the same written file goes too, its listing's times stale"
        );
        assert_eq!(deleted, 2, "both names are deleted");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

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

        let mut opened = open_beneath(&held(&folder), "Siblings.TOML", Share::All).expect("the name is found in another case");
        let mut text = String::new();
        opened.read_to_string(&mut text).expect("the file can be read");

        assert_eq!(text, "a");
        drop(opened);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_missing_name_is_not_found() {
        let folder = folder("missing");

        let opened = open_beneath(&held(&folder), "siblings.toml", Share::All);

        assert_eq!(opened.map(drop).map_err(|error| error.kind()), Err(io::ErrorKind::NotFound));
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_junction_at_the_name_is_opened_as_itself() {
        let folder = folder("junction");
        std::fs::create_dir(folder.join("target")).expect("the junction's target can be made");
        junction(&folder.join("link"), &folder.join("target"));

        let opened = open_beneath(&held(&folder), "link", Share::All).expect("the junction can be opened");

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

        let opened = open_beneath(&held(&folder), "link.txt", Share::All).expect("the symlink can be opened");

        assert!(is_link_itself(&opened), "the symlink is opened as itself, not followed");
        drop(opened);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_name_that_is_not_one_name_is_refused() {
        let folder = folder("names");
        let held = held(&folder);
        for name in ["", ".", "..", r"a\b", "a/b", "a:b", "a\0b"] {
            let opened = open_beneath(&held, name, Share::All);
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
    fn a_folder_opened_without_delete_sharing_cannot_be_renamed() {
        let folder = folder("no-delete");
        std::fs::create_dir(folder.join("b")).expect("the folder can be made");

        let opened = open_beneath(&held(&folder), "b", Share::NoDelete).expect("the folder can be opened");
        let renamed = std::fs::rename(folder.join("b"), folder.join("c"));

        assert_eq!(
            renamed.map_err(|error| error.raw_os_error()),
            Err(Some(32)),
            "a sharing violation while held"
        );
        drop(opened);
        std::fs::rename(folder.join("b"), folder.join("c")).expect("the folder can be renamed once let go");
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
        let folder = crate::test_support::temp_dir().join(format!("gate-win32-file-{name}-{}", std::process::id()));
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

    /// The file `path` opened for reading with `DELETE` access, every sharing given, as a rename or a delete by
    /// handle needs it.
    fn deletable(path: &Path) -> File {
        OpenOptions::new()
            .access_mode(FILE_GENERIC_READ | DELETE)
            .share_mode(Share::All.mode())
            .open(path)
            .expect("the file can be opened for deletion")
    }

    #[test]
    fn a_new_file_is_made_beneath_and_one_already_there_is_refused() {
        let folder = folder("new-file");
        let held = held(&folder);

        let mut made = open_beneath_as(&held, "a.txt", FILE_GENERIC_WRITE, Share::NoDelete, Make::NewFile).expect("a free name is made");
        made.write_all(b"a").expect("the new file can be written");
        drop(made);
        let again = open_beneath_as(&held, "a.txt", FILE_GENERIC_WRITE, Share::NoDelete, Make::NewFile);

        assert_eq!(again.map(drop).map_err(|error| error.kind()), Err(io::ErrorKind::AlreadyExists));
        assert_eq!(std::fs::read_to_string(folder.join("a.txt")).expect("the file is there"), "a");
        drop(held);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_file_is_made_new_beneath_with_its_security_and_one_already_there_is_refused() {
        let folder = folder("new-file-security");
        let held = held(&folder);
        let security = Descriptor::from_sddl("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)").expect("the descriptor can be made");

        let made = make_file_beneath(&held, "lock", FILE_GENERIC_READ | READ_CONTROL, Share::NoDelete, &security).expect("a free name is made");
        let text = security_text(&made).expect("its security can be read");
        let foreign = is_protected_file(&made).expect("its security can be read");
        drop(made);
        let again = make_file_beneath(&held, "lock", FILE_GENERIC_READ, Share::NoDelete, &security);
        let facts = token_facts().expect("this process's token can be read");
        let protected = protected_descriptor(&facts).expect("the protected descriptor can be made");
        let made = make_file_beneath(&held, "protected", FILE_GENERIC_READ | READ_CONTROL, Share::NoDelete, &protected).expect("a free name is made");

        assert!(
            is_protected_file(&made).expect("its security can be read"),
            "a file made protected reads as protected: {}",
            security_text(&made).unwrap_or_default()
        );
        assert!(!foreign, "a file made with another DACL is not protected");
        drop(made);
        assert!(text.starts_with("D:P"), "the DACL given, not the folder's: {text}");
        assert!(text.contains("(A;;FA;;;WD)"), "the DACL given, not the folder's: {text}");
        assert_eq!(again.map(drop).map_err(|error| error.kind()), Err(io::ErrorKind::AlreadyExists));
        drop(held);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_folder_is_made_or_opened_beneath_and_a_junction_there_is_opened_as_itself() {
        let folder = folder("make-folder");
        let held = held(&folder);
        std::fs::create_dir(folder.join("target")).expect("the junction's target can be made");
        junction(&folder.join("link"), &folder.join("target"));

        let new = open_beneath_as(&held, "new", FILE_GENERIC_READ, Share::NoDelete, Make::Folder).expect("a folder is made");
        let old = open_beneath_as(&held, "new", FILE_GENERIC_READ, Share::All, Make::Folder).expect("the folder is opened");
        let link = open_beneath_as(&held, "link", FILE_GENERIC_READ, Share::All, Make::Folder).expect("the junction is opened");

        let again = open_beneath_as(&held, "new", FILE_GENERIC_READ, Share::All, Make::NewFolder);
        let fresh = open_beneath_as(&held, "fresh", FILE_GENERIC_READ, Share::All, Make::NewFolder).expect("a free name is made a folder");

        assert_eq!(
            again.map(drop).map_err(|error| error.kind()),
            Err(io::ErrorKind::AlreadyExists),
            "a new folder takes no used name"
        );
        assert!(folder.join("fresh").is_dir(), "the new folder is made");
        drop(fresh);
        assert!(folder.join("new").is_dir(), "the folder is made");
        assert!(
            same_file(&new, &old).expect("the ids can be read"),
            "the second open finds the first's folder"
        );
        assert!(is_link_itself(&link), "the junction is opened as itself, not followed");
        drop((new, old, link, held));
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_rename_beneath_no_replace_leaves_an_entry_at_its_name_and_says_it_exists() {
        let folder = folder("rename-no-replace");
        std::fs::write(folder.join("a.txt"), "a").expect("the file can be written");
        std::fs::write(folder.join("b.txt"), "b").expect("the file can be written");
        let into = held(&folder);

        let refused = rename_beneath_no_replace(&deletable(&folder.join("a.txt")), &into, "b.txt").expect_err("a file at the name is not replaced");

        assert_eq!(refused.kind(), std::io::ErrorKind::AlreadyExists, "{refused}");
        assert_eq!(
            std::fs::read_to_string(folder.join("b.txt")).expect("the file at the name is still there"),
            "b"
        );
        rename_beneath_no_replace(&deletable(&folder.join("a.txt")), &into, "c.txt").expect("a free name is taken");
        assert_eq!(std::fs::read_to_string(folder.join("c.txt")).expect("the file is renamed"), "a");
        drop(into);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_rename_beneath_lands_in_the_held_folder_whatever_its_path_names_by_now() {
        let folder = folder("rename");
        std::fs::create_dir(folder.join("into")).expect("the folder can be made");
        std::fs::write(folder.join("a.txt"), "a").expect("the file can be written");
        let into = OpenOptions::new()
            .read(true)
            .share_mode(Share::All.mode())
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(folder.join("into"))
            .expect("the folder can be held");
        std::fs::rename(folder.join("into"), folder.join("moved")).expect("the held folder, sharing deletion, can be renamed");
        std::fs::create_dir(folder.join("into")).expect("another folder takes the old name");

        rename_beneath(&deletable(&folder.join("a.txt")), &into, "b.txt").expect("the rename goes through");

        assert_eq!(
            std::fs::read_to_string(folder.join(r"moved\b.txt")).expect("the file is in the held folder"),
            "a"
        );
        assert!(!folder.join(r"into\b.txt").exists(), "nothing went where the path now leads");
        drop(into);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    #[expect(clippy::print_stderr, reason = "a skipped case says why it skipped")]
    fn a_rename_beneath_replaces_a_file_or_a_link_at_its_name_as_itself() {
        let folder = folder("rename-replace");
        std::fs::write(folder.join("a.txt"), "a").expect("the file can be written");
        std::fs::write(folder.join("b.txt"), "b").expect("the file can be written");
        let held = held(&folder);

        rename_beneath(&deletable(&folder.join("a.txt")), &held, "b.txt").expect("the file is replaced");
        assert_eq!(std::fs::read_to_string(folder.join("b.txt")).expect("the file is there"), "a");

        std::fs::write(folder.join("c.txt"), "c").expect("the file can be written");
        std::fs::write(folder.join("target.txt"), "target").expect("the file can be written");
        if let Err(error) = std::os::windows::fs::symlink_file(folder.join("target.txt"), folder.join("link.txt")) {
            eprintln!("skipped the link half: a file symlink cannot be made here (no privilege, or developer mode off): {error}");
        } else {
            rename_beneath(&deletable(&folder.join("c.txt")), &held, "link.txt").expect("the link is replaced");
            let link = std::fs::symlink_metadata(folder.join("link.txt")).expect("the name is there");
            assert!(!link.file_type().is_symlink(), "the link itself is replaced");
            assert_eq!(std::fs::read_to_string(folder.join("target.txt")).expect("the target is there"), "target");
        }
        drop(held);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_read_only_file_is_replaced_by_a_rename_beneath_and_deleted_by_handle() {
        let folder = folder("read-only");
        let held = held(&folder);
        let read_only = |path: &Path| {
            let mut permissions = std::fs::metadata(path).expect("the file is there").permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(path, permissions).expect("the file can be made read-only");
        };
        std::fs::write(folder.join("a.txt"), "a").expect("the file can be written");
        std::fs::write(folder.join("b.txt"), "b").expect("the file can be written");
        read_only(&folder.join("b.txt"));

        rename_beneath(&deletable(&folder.join("a.txt")), &held, "b.txt").expect("a read-only target is replaced");
        assert_eq!(std::fs::read_to_string(folder.join("b.txt")).expect("the file is there"), "a");

        read_only(&folder.join("b.txt"));
        delete_opened(&deletable(&folder.join("b.txt"))).expect("a read-only file is deleted");
        assert!(!folder.join("b.txt").exists(), "the read-only file is gone");
        drop(held);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_name_deleted_by_handle_is_free_at_once_while_another_handle_holds_it() {
        let folder = folder("delete");
        let path = folder.join("a.txt");
        std::fs::write(&path, "a").expect("the file can be written");
        let other = OpenOptions::new()
            .read(true)
            .share_mode(Share::All.mode())
            .open(&path)
            .expect("another handle holds the file");

        delete_opened(&deletable(&path)).expect("the delete goes through");

        assert!(!path.exists(), "the name is gone");
        std::fs::write(&path, "b").expect("the name is free while the other handle is open");
        drop(other);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_path_holding_a_nul_is_refused() {
        let moved = move_no_replace(Path::new("a\0b"), Path::new("c"));

        assert_eq!(moved.map_err(|error| error.kind()), Err(io::ErrorKind::InvalidInput));
    }
}
