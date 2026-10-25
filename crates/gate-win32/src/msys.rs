//! A process's parent as the MSYS runtime (Git Bash's, or Cygwin's) records it, read from the runtime's named kernel
//! objects: a `winpid.{windows pid}` symbolic link whose target is the process's MSYS pid, and a `cygpid.{msys pid}`
//! section holding the runtime's record of that process, both in an object directory under `\BaseNamedObjects` whose
//! name starts `msys-` (`cygwin1S` for Cygwin's). The runtime emulates fork, so an MSYS process's Windows parent is
//! often a stub that has already exited while its MSYS parent lives; this reads the parent the runtime keeps. Any
//! account can make objects in the runtime's directory, so a link or a record is trusted only when it is owned as this
//! process's token owns what it creates.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{NtOpenDirectoryObject, NtOpenSymbolicLinkObject, NtQueryDirectoryObject, NtQuerySymbolicLinkObject};
use windows_sys::Win32::Foundation::{
    ERROR_SUCCESS, HANDLE, LocalFree, NTSTATUS, STATUS_NO_MORE_ENTRIES, STATUS_OBJECT_NAME_NOT_FOUND, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::token::{own_object_owners, sid_string};

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// Opens a named section; `windows-sys` binds it only under a feature this crate does not take.
    fn NtOpenSection(section: *mut HANDLE, access: u32, attributes: *const OBJECT_ATTRIBUTES) -> NTSTATUS;
    /// Maps a view of an open section into a process; `windows-sys` binds it only under a feature this crate does not
    /// take.
    fn NtMapViewOfSection(
        section: HANDLE,
        process: HANDLE,
        base: *mut *mut core::ffi::c_void,
        zero_bits: usize,
        commit_size: usize,
        offset: *mut i64,
        view_size: *mut usize,
        inherit: i32,
        allocation_type: u32,
        protection: u32,
    ) -> NTSTATUS;
    /// Unmaps a view [`NtMapViewOfSection`] mapped; `windows-sys` binds it only under a feature this crate does not
    /// take.
    fn NtUnmapViewOfSection(process: HANDLE, base: *const core::ffi::c_void) -> NTSTATUS;
}

/// `DIRECTORY_QUERY`: list an object directory.
const DIRECTORY_QUERY: u32 = 0x1;
/// `DIRECTORY_TRAVERSE`: open names inside an object directory.
const DIRECTORY_TRAVERSE: u32 = 0x2;
/// `SYMBOLIC_LINK_QUERY`: read a symbolic link's target.
const SYMBOLIC_LINK_QUERY: u32 = 0x1;
/// `SECTION_MAP_READ`: map a section for reading.
const SECTION_MAP_READ: u32 = 0x4;
/// `ViewUnmap`: the view is not mapped into child processes.
const VIEW_UNMAP: i32 = 2;
/// `PAGE_READONLY`.
const PAGE_READONLY: u32 = 0x2;
/// The object directory that holds the runtime's directories.
const NAMED_OBJECTS: &str = r"\BaseNamedObjects";
/// The starts of the runtime directories' names: Git Bash's MSYS runtime, then Cygwin's.
const RUNTIME_PREFIXES: [&str; 2] = ["msys-", "cygwin1S"];
/// The bytes listed of `\BaseNamedObjects` per directory query.
const LISTING_BYTES: usize = 64 * 1024;
/// The UTF-16 units a `winpid` link's target is read into: a decimal pid needs far fewer.
const LINK_UNITS: usize = 64;
/// The bytes of a `cygpid` record read: `pid` (i32) at 0, `process_state` (u32) at 4, `ppid` (i32) at 8 and
/// `dwProcessId` (u32) at 20, as msys2-runtime's `pinfo.h` lays them out.
const RECORD_BYTES: usize = 24;
/// The `process_state` bit that marks an ended process (`PID_EXITED`).
const EXITED: u32 = 0x4000_0000;

/// What the MSYS runtime records of a Windows process's parent ([`msys_parent`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsysParent {
    /// No MSYS runtime record names this Windows pid.
    NotMsys,
    /// An MSYS process started by a non-MSYS one (ppid 1 or less): its Windows parent is its real parent.
    FromWindows,
    /// Its MSYS parent lives: that parent's Windows pid.
    Alive(u32),
    /// Its MSYS parent's record is gone or marks it ended.
    Gone,
    /// A record was found but could not be read or does not agree with itself.
    Unreadable,
}

/// The parent the MSYS runtime records for the Windows process `windows_pid`: its `winpid` link looked for in every
/// runtime directory under `\BaseNamedObjects`, its own `cygpid` record checked against both pids, then its MSYS
/// parent's record read. Any failure to open, list, read or map what was looked for, but a parent record that is
/// missing, is [`MsysParent::Unreadable`].
#[must_use]
pub fn msys_parent(windows_pid: u32) -> MsysParent {
    read_parent(windows_pid).unwrap_or(MsysParent::Unreadable)
}

/// A runtime object that could not be read, or a record that does not agree with itself.
struct Unreadable;

/// One process's record in a `cygpid` section.
struct Record {
    /// The MSYS pid.
    pid: i32,
    /// The runtime's state bits of the process.
    state: u32,
    /// The MSYS pid of its parent.
    ppid: i32,
    /// Its Windows pid.
    windows_pid: u32,
}

/// [`msys_parent`] with its failures not yet folded.
fn read_parent(windows_pid: u32) -> Result<MsysParent, Unreadable> {
    let root = open(None, NAMED_OBJECTS, DIRECTORY_QUERY | DIRECTORY_TRAVERSE, NtOpenDirectoryObject)?.ok_or(Unreadable)?;
    let owners = own_object_owners().map_err(|_| Unreadable)?;
    for name in runtime_directories(&root)? {
        // A runtime directory that will not open is skipped, so no directory another account made can stop the look.
        let Ok(Some(directory)) = open(Some(&root), &name, DIRECTORY_QUERY | DIRECTORY_TRAVERSE, NtOpenDirectoryObject) else {
            continue;
        };
        let link_name = format!("winpid.{windows_pid}");
        let Some(link) = open_owned(&directory, &link_name, SYMBOLIC_LINK_QUERY, NtOpenSymbolicLinkObject, &owners)? else {
            continue;
        };
        let msys_pid = parse_pid(&link_target(&link)?)?;
        return judged(&directory, msys_pid, windows_pid, &owners);
    }
    Ok(MsysParent::NotMsys)
}

/// The parent the records in the runtime `directory` give the process whose MSYS pid is `msys_pid` and whose Windows
/// pid is `windows_pid`, trusting only the records `owners` own.
fn judged(directory: &OwnedHandle, msys_pid: i32, windows_pid: u32, owners: &[String]) -> Result<MsysParent, Unreadable> {
    let own = record(directory, msys_pid, owners)?.ok_or(Unreadable)?;
    if own.pid != msys_pid || own.windows_pid != windows_pid {
        return Err(Unreadable);
    }
    if own.ppid <= 1 {
        return Ok(MsysParent::FromWindows);
    }
    let Some(parent) = record(directory, own.ppid, owners)? else {
        return Ok(MsysParent::Gone);
    };
    if parent.pid != own.ppid {
        return Err(Unreadable);
    }
    if parent.state & EXITED != 0 {
        return Ok(MsysParent::Gone);
    }
    Ok(MsysParent::Alive(parent.windows_pid))
}

/// The NT call that opens a named object of one type.
type Opener = unsafe extern "system" fn(*mut HANDLE, u32, *const OBJECT_ATTRIBUTES) -> NTSTATUS;

/// The object `name` opened with `access` by `opener`, inside `root` when given: `None` when no object has the name.
fn open(root: Option<&OwnedHandle>, name: &str, access: u32, opener: Opener) -> Result<Option<OwnedHandle>, Unreadable> {
    let wide: Vec<u16> = name.encode_utf16().collect();
    let length = u16::try_from(size_of_val(wide.as_slice())).map_err(|_| Unreadable)?;
    let object_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        // The system only reads the name.
        Buffer: wide.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).map_err(|_| Unreadable)?,
        RootDirectory: root.map_or(ptr::null_mut(), AsRawHandle::as_raw_handle),
        ObjectName: &raw const object_name,
        Attributes: 0,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle = ptr::null_mut();
    // SAFETY: the name's buffer and the attributes outlive the call, the attributes' root is an open directory handle
    // `root` owns (or none), and `handle` is writable.
    let status = unsafe { opener(&raw mut handle, access, &raw const attributes) };
    if status == STATUS_OBJECT_NAME_NOT_FOUND {
        return Ok(None);
    }
    if status < 0 {
        return Err(Unreadable);
    }
    // SAFETY: the call succeeded, so `handle` is an open handle this process owns and nothing else closes.
    Ok(Some(unsafe { OwnedHandle::from_raw_handle(handle) }))
}

/// The object `name` in the runtime `directory`, opened with `access` by `opener`, when one of `owners` owns it: `None`
/// when no object has the name, when it may not be opened to read its owner, or when its owner cannot be read or is
/// another, as another account can make objects in the runtime's directory.
fn open_owned(directory: &OwnedHandle, name: &str, access: u32, opener: Opener, owners: &[String]) -> Result<Option<OwnedHandle>, Unreadable> {
    let opened = match open(Some(directory), name, access | READ_CONTROL, opener) {
        Ok(opened) => opened,
        Err(Unreadable) if denied(directory, name, access, opener) => None,
        Err(Unreadable) => return Err(Unreadable),
    };
    Ok(opened.filter(|object| owner_of(object).is_some_and(|owner| trusted(&owner, owners))))
}

/// Whether `name` in `directory` exists but may not be opened with `READ_CONTROL`, which the owner of an object always
/// has: it opens with `access` alone.
fn denied(directory: &OwnedHandle, name: &str, access: u32, opener: Opener) -> bool {
    matches!(open(Some(directory), name, access, opener), Ok(Some(_)))
}

/// Whether the owner `owner`, a SID's string form, is one of `owners`.
fn trusted(owner: &str, owners: &[String]) -> bool {
    owners.iter().any(|trusted| trusted == owner)
}

/// The owner of the open kernel object `object`, as a SID's string form: `None` when it cannot be read.
fn owner_of(object: &OwnedHandle) -> Option<String> {
    let mut owner: PSID = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the handle is open for the whole call with `READ_CONTROL`, and `owner` and `descriptor` are writable; no
    // other part is asked for.
    let error = unsafe {
        GetSecurityInfo(
            object.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return None;
    }
    // SAFETY: `owner` points into `descriptor`, which is freed only after this.
    let text = unsafe { sid_string(owner) }.ok();
    // SAFETY: the descriptor was allocated by `GetSecurityInfo` with `LocalAlloc`, and nothing uses it after this.
    unsafe { LocalFree(descriptor) };
    text
}

/// The names of the runtime directories in the open object directory `root`: its subdirectories whose names start
/// with one of [`RUNTIME_PREFIXES`].
fn runtime_directories(root: &OwnedHandle) -> Result<Vec<String>, Unreadable> {
    // `u64`s, so the buffer is aligned as the entries the call writes are.
    let mut buffer = vec![0_u64; LISTING_BYTES / size_of::<u64>()];
    let length = u32::try_from(size_of_val(buffer.as_slice())).map_err(|_| Unreadable)?;
    let mut context = 0_u32;
    let mut restart = true;
    let mut names = Vec::new();
    loop {
        let mut returned = 0_u32;
        // SAFETY: the handle is open with query access for the whole call; `buffer` is writable for `length` bytes, and
        // `context` and `returned` are writable u32s; all outlive the call.
        let status = unsafe {
            NtQueryDirectoryObject(
                root.as_raw_handle(),
                buffer.as_mut_ptr().cast(),
                length,
                false,
                restart,
                &raw mut context,
                &raw mut returned,
            )
        };
        restart = false;
        if status == STATUS_NO_MORE_ENTRIES {
            return Ok(names);
        }
        if status < 0 {
            return Err(Unreadable);
        }
        names.extend(
            listed_directories(&buffer)?
                .into_iter()
                .filter(|name| RUNTIME_PREFIXES.iter().any(|prefix| name.starts_with(prefix))),
        );
    }
}

/// One entry of a directory listing, as `NtQueryDirectoryObject` writes it (`OBJECT_DIRECTORY_INFORMATION`).
#[repr(C)]
struct DirectoryEntry {
    /// The object's name.
    name: UNICODE_STRING,
    /// The name of the object's type.
    type_name: UNICODE_STRING,
}

/// The names of the directories among the entries `buffer` holds, up to the empty entry that ends them.
fn listed_directories(buffer: &[u64]) -> Result<Vec<String>, Unreadable> {
    let entries = size_of_val(buffer) / size_of::<DirectoryEntry>();
    let mut names = Vec::new();
    for index in 0..entries {
        // SAFETY: the buffer is aligned for a `DirectoryEntry` and holds `entries` of them whole; the call wrote its
        // entries, then an empty one, at its start, and the rest is zeroed or written.
        let entry = unsafe { &*buffer.as_ptr().cast::<DirectoryEntry>().add(index) };
        if entry.name.Length == 0 && entry.name.Buffer.is_null() {
            return Ok(names);
        }
        if text_in(buffer, &entry.type_name)? == "Directory" {
            names.push(text_in(buffer, &entry.name)?);
        }
    }
    Err(Unreadable)
}

/// The text of `string`, whose characters must lie inside `buffer`.
fn text_in(buffer: &[u64], string: &UNICODE_STRING) -> Result<String, Unreadable> {
    let units = usize::from(string.Length) / size_of::<u16>();
    if units == 0 {
        return Ok(String::new());
    }
    let start = buffer.as_ptr() as usize;
    let end = start + size_of_val(buffer);
    let text = string.Buffer as usize;
    let text_end = (units * size_of::<u16>()).checked_add(text);
    if text < start || text_end.is_none_or(|text_end| text_end > end) {
        return Err(Unreadable);
    }
    // SAFETY: the characters were just checked to lie inside `buffer`, which outlives the slice and is aligned for
    // `u16`s, as the system wrote them.
    let characters = unsafe { std::slice::from_raw_parts(string.Buffer, units) };
    Ok(String::from_utf16_lossy(characters))
}

/// The target of the open symbolic link `link`.
fn link_target(link: &OwnedHandle) -> Result<String, Unreadable> {
    let mut units = [0_u16; LINK_UNITS];
    let mut target = UNICODE_STRING {
        Length: 0,
        MaximumLength: u16::try_from(size_of_val(&units)).map_err(|_| Unreadable)?,
        Buffer: units.as_mut_ptr(),
    };
    // SAFETY: the handle is open with query access for the whole call, and `target`'s buffer is writable for its
    // maximum length; both outlive the call, and the returned length is optional.
    let status = unsafe { NtQuerySymbolicLinkObject(link.as_raw_handle(), &raw mut target, ptr::null_mut()) };
    if status < 0 {
        return Err(Unreadable);
    }
    let written = units.get(..usize::from(target.Length) / size_of::<u16>()).ok_or(Unreadable)?;
    Ok(String::from_utf16_lossy(written))
}

/// The MSYS pid a `winpid` link's target names: decimal digits only.
fn parse_pid(target: &str) -> Result<i32, Unreadable> {
    if target.is_empty() || !target.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Unreadable);
    }
    target.parse().map_err(|_| Unreadable)
}

/// A view of a section mapped into this process, unmapped when dropped.
struct View {
    /// The view's first byte.
    base: *mut core::ffi::c_void,
    /// The view's length in bytes.
    size: usize,
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: `base` is a view this process mapped and has not unmapped; the status is not checked, as there is
        // nothing left to do if the system refuses.
        unsafe {
            NtUnmapViewOfSection(GetCurrentProcess(), self.base);
        }
    }
}

/// The record of the MSYS process `msys_pid` in the runtime `directory`: `None` when it has no `cygpid` section that
/// one of `owners` owns ([`open_owned`]).
fn record(directory: &OwnedHandle, msys_pid: i32, owners: &[String]) -> Result<Option<Record>, Unreadable> {
    let Some(section) = open_owned(directory, &format!("cygpid.{msys_pid}"), SECTION_MAP_READ, NtOpenSection, owners)? else {
        return Ok(None);
    };
    let mut base = ptr::null_mut();
    let mut size = 0_usize;
    // SAFETY: the section is open with map-read access for the whole call; the base and size are writable and start
    // empty, so the system picks the address and maps the whole section, read-only, into this process.
    let status = unsafe {
        NtMapViewOfSection(
            section.as_raw_handle(),
            GetCurrentProcess(),
            &raw mut base,
            0,
            0,
            ptr::null_mut(),
            &raw mut size,
            VIEW_UNMAP,
            0,
            PAGE_READONLY,
        )
    };
    if status < 0 || base.is_null() {
        return Err(Unreadable);
    }
    let view = View { base, size };
    if view.size < RECORD_BYTES {
        return Err(Unreadable);
    }
    // The bytes are copied rather than read in place: a section another process maps can change under the read or
    // lose its pages, which a copy reports as a failure instead of a fault.
    let mut bytes = [0_u8; RECORD_BYTES];
    let mut copied = 0_usize;
    // SAFETY: the view is mapped into this process for at least `RECORD_BYTES` bytes, `bytes` is writable for as many,
    // and `copied` is a writable usize; the call faults on neither, reporting an unreadable page as a failure.
    let read = unsafe { ReadProcessMemory(GetCurrentProcess(), view.base, bytes.as_mut_ptr().cast(), RECORD_BYTES, &raw mut copied) };
    if read == 0 || copied != RECORD_BYTES {
        return Err(Unreadable);
    }
    let field = |offset: usize| -> [u8; 4] {
        let mut field = [0_u8; 4];
        field.copy_from_slice(&bytes[offset..offset + 4]);
        field
    };
    Ok(Some(Record {
        pid: i32::from_ne_bytes(field(0)),
        state: u32::from_ne_bytes(field(4)),
        ppid: i32::from_ne_bytes(field(8)),
        windows_pid: u32::from_ne_bytes(field(20)),
    }))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a token that cannot be read must fail the test loudly")]
mod tests {
    use super::{MsysParent, msys_parent, parse_pid, trusted};
    use crate::current_user_sid;
    use crate::token::own_object_owners;

    #[test]
    fn the_test_process_is_not_an_msys_process() {
        assert_eq!(msys_parent(std::process::id()), MsysParent::NotMsys);
    }

    #[test]
    fn a_pid_is_decimal_digits_within_i32() {
        assert_eq!(parse_pid("4242").ok(), Some(4242));
        assert_eq!(parse_pid("007").ok(), Some(7), "leading zeros still name the pid");
        assert_eq!(parse_pid("2147483647").ok(), Some(i32::MAX));
        assert!(parse_pid("").is_err(), "an empty target");
        assert!(parse_pid("12a").is_err(), "a target with a letter");
        assert!(parse_pid("-5").is_err(), "a signed target");
        assert!(parse_pid(" 5").is_err(), "a target with a space");
        assert!(parse_pid("2147483648").is_err(), "a target past i32");
    }

    #[test]
    fn only_an_object_owned_as_this_token_creates_them_is_trusted() {
        let owners = own_object_owners().expect("the token's owners can be read");
        let user = current_user_sid().expect("the token's user can be read");
        assert!(trusted(&user, &owners), "the current user {user} owns what it creates: {owners:?}");
        assert!(!trusted("S-1-5-19", &owners), "LocalService owns nothing this token creates: {owners:?}");
    }
}
