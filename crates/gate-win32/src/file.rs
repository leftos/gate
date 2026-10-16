//! File calls the standard library does not make: a rename that never replaces the file its target names, an open of
//! a name inside an open folder, a folder made there with the security it is to have from the instant it exists, a
//! listing and a tree delete through an open folder's handle, an open that reads an
//! open file through its reparse point's filter, an open file's id, whether two open files are one, and whether an open
//! file is on a network volume ([`on_network_volume`]).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
    FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformationEx, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, GENERIC_READ, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
    FILE_FULL_DIR_INFO, FILE_GENERIC_READ, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_REMOTE_PROTOCOL_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfoEx, FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileIdInfo,
    FileRemoteProtocolInfo, GetFileInformationByHandleEx, MoveFileExW, ReOpenFile, SYNCHRONIZE, SetFileInformationByHandle,
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
        // SAFETY: the call reads nothing but the status it is given.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code.cast_signed()));
    }
    // SAFETY: a success is a fresh handle nothing else owns; the returned file owns and closes it from now on.
    Ok(unsafe { File::from_raw_handle(handle) })
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
    // Eight-byte units, so each entry the system writes is aligned for its structure.
    let mut buffer = vec![0_u64; DIRECTORY_BUFFER_UNITS];
    let size = u32::try_from(size_of_val(buffer.as_slice())).map_err(io::Error::other)?;
    let mut names = Vec::new();
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        // SAFETY: the handle is open for the whole call, and `buffer` is writable for `size` bytes.
        let read = unsafe { GetFileInformationByHandleEx(folder.as_raw_handle(), class, buffer.as_mut_ptr().cast(), size) };
        if read == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES.cast_signed()) {
                return Ok(names);
            }
            return Err(error);
        }
        class = FileFullDirectoryInfo;
        entry_names(&buffer, &mut names)?;
    }
}

/// The 64 KiB the directory listing reads into at once, in eight-byte units.
const DIRECTORY_BUFFER_UNITS: usize = 8 * 1024;

/// Adds to `names` the name of each entry the system wrote in `buffer`, `.` and `..` left out. Each entry is read only
/// as far as its name ends, so a short last entry near the buffer's end is read whole.
fn entry_names(buffer: &[u64], names: &mut Vec<OsString>) -> io::Result<()> {
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
            names.push(OsString::from_wide(&wide));
        }
        if next == 0 {
            return Ok(());
        }
        offset += usize::try_from(next).map_err(io::Error::other)?;
    }
}

/// The little-endian `u32` at byte `at` of a directory entry's `header`.
fn u32_at(header: &[u8], at: usize) -> io::Result<u32> {
    let bytes = header
        .get(at..at + size_of::<u32>())
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .ok_or_else(|| io::Error::other("a directory entry's header is cut short"))?;
    Ok(u32::from_le_bytes(bytes))
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

/// What others may do with a file or folder while a handle holds it open, [`open_beneath`]'s or another open's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Share {
    /// Read, write and delete it, a rename included.
    All,
    /// Read and write it, but neither delete nor rename it, a folder's own name included, until the handle is closed.
    NoDelete,
    /// Read and delete it, but not write it, so what a reader hashed stays what the handle holds.
    NoWrite,
}

impl Share {
    /// The share mode of an open that lets others do this, as `CreateFileW` and `OpenOptions::share_mode` take it.
    #[must_use]
    pub fn mode(self) -> u32 {
        match self {
            Share::All => FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            Share::NoDelete => FILE_SHARE_READ | FILE_SHARE_WRITE,
            Share::NoWrite => FILE_SHARE_READ | FILE_SHARE_DELETE,
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

/// Whether the open files `first` and `second` are one file: the same volume serial number and the same 128-bit file
/// id.
///
/// # Errors
///
/// The system's error when either handle's file id cannot be read, as on a file system that keeps none.
pub fn same_file(first: &File, second: &File) -> io::Result<bool> {
    Ok(file_id(first)? == file_id(second)?)
}

/// The volume serial number and the 128-bit file id of the open file or folder `file`: two handles give the same pair
/// only when they are of one file.
///
/// # Errors
///
/// The system's error when the id cannot be read, as on a file system that keeps none.
pub fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
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
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    };

    use super::{
        Make, Share, delete_opened, delete_tree_opened, move_no_replace, names_beneath, open_beneath, open_beneath_as, rename_beneath,
        rename_beneath_no_replace, same_file, verbatim,
    };

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
