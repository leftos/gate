//! The system's process list, as `NtQuerySystemInformation(SystemProcessInformation)` gives it: every process with its
//! id, its parent's id, its image name, its creation time and its CPU times, read in one call that needs no access to
//! any process. The offsets are x64.

use std::io;
use std::mem::offset_of;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
use windows_sys::Win32::Foundation::{STATUS_INFO_LENGTH_MISMATCH, UNICODE_STRING};
use windows_sys::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

/// What the process list is called in an error.
pub const PROCESS_LIST: &str = "NtQuerySystemInformation(SystemProcessInformation)";
/// Where an entry of the process list keeps its creation, user and kernel times (each an `i64` of 100 ns ticks), inside
/// the `Reserved1` bytes of `SYSTEM_PROCESS_INFORMATION`.
const CREATE_TIME_AT: usize = 32;
const USER_TIME_AT: usize = 40;
const KERNEL_TIME_AT: usize = 48;
/// Where an entry keeps the link to the next one, and its image name.
const NEXT_AT: usize = offset_of!(SYSTEM_PROCESS_INFORMATION, NextEntryOffset);
const NAME_AT: usize = offset_of!(SYSTEM_PROCESS_INFORMATION, ImageName);
const NAME_BUFFER_AT: usize = NAME_AT + offset_of!(UNICODE_STRING, Buffer);
/// Where an entry keeps its process id.
const PID_AT: usize = offset_of!(SYSTEM_PROCESS_INFORMATION, UniqueProcessId);
/// Where an entry keeps the id of the process that started it (`InheritedFromUniqueProcessId`), in its `Reserved2`.
const PARENT_PID_AT: usize = offset_of!(SYSTEM_PROCESS_INFORMATION, Reserved2);
/// The bytes of an entry this reader reads: everything up to the end of the parent's id.
const ENTRY_READ: usize = PARENT_PID_AT + size_of::<usize>();
/// What the buffer grows by over the size the system last asked for, as the list can grow between two calls.
const LIST_SLACK: usize = 64 * 1024;
/// How many times the list is asked for before a list that keeps outgrowing its buffer is an error.
const LIST_ATTEMPTS: usize = 8;

/// The size in bytes of the last buffer the process list fit in, which the next read starts at, so a steady machine
/// asks for the list once a read.
static LIST_SIZE: AtomicUsize = AtomicUsize::new(LIST_SLACK);

/// One entry of the process list, as [`walk`] decodes it; the times are `i64`s of 100 ns ticks, as the list holds them.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    /// The process id.
    pub pid: usize,
    /// The id of the process that started it.
    pub parent_pid: usize,
    /// The image name, UTF-16 in the list's byte order; empty for the idle process.
    pub name: &'a [u8],
    /// When it was created, in 100 ns ticks since 1601; zero for the idle and system processes.
    pub create: i64,
    /// Its kernel time.
    pub kernel: i64,
    /// Its user time.
    pub user: i64,
}

/// The system's process list, as `NtQuerySystemInformation` writes it, in a buffer aligned for its entries; the read
/// starts at the size the last list fit in.
///
/// # Errors
///
/// An error naming the call and its status when the list cannot be read, or when it keeps outgrowing its buffer.
pub fn process_list() -> io::Result<Vec<u64>> {
    let mut list: Vec<u64> = vec![0; LIST_SIZE.load(Ordering::Relaxed).div_ceil(size_of::<u64>())];
    for _ in 0..LIST_ATTEMPTS {
        let bytes = size_of_val(list.as_slice());
        let size = u32::try_from(bytes).map_err(io::Error::other)?;
        let mut needed = 0_u32;
        // SAFETY: the buffer is writable for `size` bytes, aligned for the entries the call writes, and `needed` is
        // writable; the call writes at most `size` bytes.
        let status = unsafe { NtQuerySystemInformation(SystemProcessInformation, list.as_mut_ptr().cast(), size, &raw mut needed) };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            let bytes = usize::try_from(needed).map_err(io::Error::other)? + LIST_SLACK;
            list.resize(bytes.div_ceil(size_of::<u64>()), 0);
            continue;
        }
        if status < 0 {
            return Err(io::Error::other(format!("{PROCESS_LIST} failed with status 0x{status:08X}")));
        }
        LIST_SIZE.store(bytes, Ordering::Relaxed);
        return Ok(list);
    }
    Err(io::Error::other(format!(
        "{PROCESS_LIST} kept outgrowing its buffer (status 0x{STATUS_INFO_LENGTH_MISMATCH:08X}) after {LIST_ATTEMPTS} attempts"
    )))
}

/// The bytes of the process list `list`.
#[must_use]
pub fn as_bytes(list: &[u64]) -> &[u8] {
    // SAFETY: the bytes are those of `list`, which outlives the slice, and any byte is a valid `u8`.
    unsafe { std::slice::from_raw_parts(list.as_ptr().cast::<u8>(), size_of_val(list)) }
}

/// The entries of the process list `bytes` that `keep` keeps, walked by each entry's link to the next and checked
/// against the buffer at every step. An entry that cannot be decoded (one of its fields, or its image name, leaves the
/// buffer) is skipped, and the walk goes on by its link.
///
/// # Errors
///
/// An error naming the entry when the list cannot be walked: an entry whose link to the next cannot be read, is shorter
/// than an entry, or leaves the address space.
pub fn walk<'a>(bytes: &'a [u8], keep: impl Fn(&Entry<'a>) -> bool) -> io::Result<Vec<Entry<'a>>> {
    let mut kept = Vec::new();
    let mut at = 0_usize;
    loop {
        let next = usize::try_from(read_u32(bytes, at + NEXT_AT)?).map_err(io::Error::other)?;
        if next != 0 && next < ENTRY_READ {
            return Err(outside(at, bytes.len()));
        }
        if let Ok(entry) = entry_at(bytes, at)
            && keep(&entry)
        {
            kept.push(entry);
        }
        if next == 0 {
            return Ok(kept);
        }
        at = at.checked_add(next).ok_or_else(|| outside(at, bytes.len()))?;
    }
}

/// The entry at `at` of the process list `bytes`.
fn entry_at(bytes: &[u8], at: usize) -> io::Result<Entry<'_>> {
    Ok(Entry {
        pid: read_usize(bytes, at + PID_AT)?,
        parent_pid: read_usize(bytes, at + PARENT_PID_AT)?,
        name: image_name(bytes, at)?,
        create: read_i64(bytes, at + CREATE_TIME_AT)?,
        kernel: read_i64(bytes, at + KERNEL_TIME_AT)?,
        user: read_i64(bytes, at + USER_TIME_AT)?,
    })
}

/// The image name of the entry at `at` of the process list `bytes`, whose characters the system wrote into the same
/// buffer: read from there, bounds-checked, and empty for the idle process, which has none.
fn image_name(bytes: &[u8], at: usize) -> io::Result<&[u8]> {
    let length = usize::from(read_u16(bytes, at + NAME_AT)?);
    if length == 0 {
        return Ok(&[]);
    }
    let start = read_usize(bytes, at + NAME_BUFFER_AT)?.wrapping_sub(bytes.as_ptr().addr());
    start
        .checked_add(length)
        .and_then(|end| bytes.get(start..end))
        .ok_or_else(|| outside(at, bytes.len()))
}

/// The `N` bytes at `at` of the process list `bytes`, or the error of a read that would leave it.
fn read_bytes<const N: usize>(bytes: &[u8], at: usize) -> io::Result<[u8; N]> {
    at.checked_add(N)
        .and_then(|end| bytes.get(at..end))
        .and_then(|slice| <[u8; N]>::try_from(slice).ok())
        .ok_or_else(|| outside(at, bytes.len()))
}

/// The `u16` at `at` of the process list `bytes`.
fn read_u16(bytes: &[u8], at: usize) -> io::Result<u16> {
    read_bytes(bytes, at).map(u16::from_le_bytes)
}

/// The `u32` at `at` of the process list `bytes`.
fn read_u32(bytes: &[u8], at: usize) -> io::Result<u32> {
    read_bytes(bytes, at).map(u32::from_le_bytes)
}

/// The `usize` (a pointer or a process id) at `at` of the process list `bytes`.
fn read_usize(bytes: &[u8], at: usize) -> io::Result<usize> {
    read_bytes::<{ size_of::<usize>() }>(bytes, at).map(usize::from_le_bytes)
}

/// The `i64` at `at` of the process list `bytes`.
fn read_i64(bytes: &[u8], at: usize) -> io::Result<i64> {
    read_bytes(bytes, at).map(i64::from_le_bytes)
}

/// The error of a walk of the process list that would leave its `size`-byte buffer at `at`.
fn outside(at: usize, size: usize) -> io::Error {
    io::Error::other(format!(
        "{PROCESS_LIST}: the entry at byte {at} leaves the {size}-byte buffer; the list layout is not the one expected"
    ))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a list that cannot be read must fail the test loudly")]
mod tests {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle as _;
    use std::process::{Command, Stdio};
    use std::ptr;

    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Security::Authorization::{SE_KERNEL_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION};
    use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, PROCESS_QUERY_LIMITED_INFORMATION};

    use super::{as_bytes, process_list, walk};
    use crate::win32::{open_process, parent_pid_of};

    /// A `FILETIME`'s two halves as one count of 100 ns ticks.
    fn ticks(time: FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }

    /// The time now on the system clock, in 100 ns ticks since 1601.
    fn now() -> u64 {
        let mut now = FILETIME::default();
        // SAFETY: the pointer is to a writable FILETIME that outlives the call, which cannot fail.
        unsafe { GetSystemTimeAsFileTime(&raw mut now) };
        ticks(now)
    }

    /// The process list's entries for `pid`, read now.
    fn listed(pid: u32) -> Vec<(usize, usize, i64)> {
        let list = process_list().expect("the process list reads unelevated");
        let pid = usize::try_from(pid).expect("a pid fits a usize");
        walk(as_bytes(&list), |entry| entry.pid == pid)
            .expect("the list walks")
            .iter()
            .map(|entry| (entry.pid, entry.parent_pid, entry.create))
            .collect()
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "gate_win32::spawn depends on this crate; a test starts its child directly: what it checks is the child itself"
    )]
    fn the_list_gives_the_start_of_a_process_this_user_cannot_open() {
        // cmd.exe reads its piped stdin, so it runs until the test ends it.
        let mut child = Command::new("cmd.exe")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("cmd.exe starts");
        let handle = child.as_raw_handle();
        let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: the child's handle is open with full access until `child` drops; each pointer is a writable FILETIME.
        let read = unsafe { GetProcessTimes(handle, &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) };
        assert_ne!(read, 0, "the child's start can be read through its own handle");
        // An empty DACL: no new handle is granted to this user, as csrss.exe and wininit.exe grant none to a user that is
        // not elevated, though the system still lists them and their starts.
        let mut acl = ACL::default();
        let size = u32::try_from(size_of::<ACL>()).expect("an ACL's size fits a u32");
        // SAFETY: `acl` is a writable ACL of `size` bytes.
        assert_ne!(unsafe { InitializeAcl(&raw mut acl, size, ACL_REVISION) }, 0, "the empty ACL is built");
        // SAFETY: the handle is open with WRITE_DAC; the DACL points to a valid ACL that outlives the call.
        let set = unsafe {
            SetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &raw const acl,
                ptr::null(),
            )
        };
        assert_eq!(set, 0, "the child's DACL is emptied");
        assert!(
            open_process(child.id(), PROCESS_QUERY_LIMITED_INFORMATION).is_none(),
            "this user can no longer open the child (a session holding SeDebugPrivilege could, and proves nothing here)"
        );

        let found = listed(child.id());
        child.kill().expect("the child is ended through the handle that started it");
        child.wait().expect("the child is waited for");

        let &(_, _, create) = found.first().expect("the list holds the child");
        assert_eq!(found.len(), 1, "the child is listed once");
        assert_eq!(
            u64::try_from(create).ok(),
            Some(ticks(created)),
            "the list gives the start of a process this user cannot open, as its own handle reads it"
        );
    }

    #[test]
    fn the_walk_decodes_this_process_from_the_real_list() {
        let list = process_list().expect("the process list reads unelevated");
        let pid = usize::try_from(std::process::id()).expect("a pid fits a usize");
        let found = walk(as_bytes(&list), |entry| entry.pid == pid).expect("the list walks");
        let now = now();
        assert_eq!(found.len(), 1, "this process is listed once");
        let me = found.first().expect("this process is listed");

        let exe = std::env::current_exe().expect("the test executable has a path");
        let file = exe.file_name().expect("the path names a file").to_string_lossy().into_owned();
        let units: Vec<u16> = me.name.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit)).collect();
        let name = String::from_utf16_lossy(&units);
        assert!(
            name.eq_ignore_ascii_case(&file),
            "the image name {name:?} is the test executable's {file:?}"
        );

        let created = u64::try_from(me.create).expect("a create time is not negative");
        assert!(created > 0 && created <= now, "created at {created}, now {now}");
        let me_open = open_process(std::process::id(), PROCESS_QUERY_LIMITED_INFORMATION).expect("this process opens");
        let parent = parent_pid_of(&me_open).expect("this process's parent reads");
        assert_eq!(
            u32::try_from(me.parent_pid).ok(),
            Some(parent),
            "the list's parent is the one the process's own record holds"
        );
        // CPU is charged in whole 15.625 ms clock ticks, so a young process can hold more CPU than wall age: the list is
        // checked against `GetProcessTimes` of this process instead, read after it.
        let (mut own_created, mut exited, mut kernel, mut user) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: the pseudo-handle of this process needs no closing; each pointer is a writable FILETIME.
        let read = unsafe { GetProcessTimes(GetCurrentProcess(), &raw mut own_created, &raw mut exited, &raw mut kernel, &raw mut user) };
        assert_ne!(read, 0, "this process's times read");
        assert_eq!(created, ticks(own_created), "the list's create time is the one GetProcessTimes reads");
        let cpu = u64::try_from(me.kernel + me.user).expect("CPU time is not negative");
        let own_cpu = ticks(kernel) + ticks(user);
        assert!(
            cpu <= own_cpu,
            "the list's {cpu} ticks of CPU, read first, exceed the {own_cpu} read after"
        );
    }
}
