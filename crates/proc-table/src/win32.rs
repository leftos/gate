//! The process table on 64-bit Windows: a `ToolHelp` snapshot, each process's start time, and its command line read
//! from its PEB. [`Win32ProcessTable::snapshot_without_command_lines`] gives the same entries with no command line.
//! The handle guard, [`open_process`] and [`read_parameters_string`] are public for callers that read another PEB
//! string, such as a process's working directory.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{self, with_exposed_provenance};

use jiff::Timestamp;
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

use crate::{ProcessInfo, ProcessTable, filetime_to_timestamp};

/// The offset of `ProcessParameters` in a 64-bit PEB.
const PEB_PROCESS_PARAMETERS: usize = 0x20;
/// The offset of the `CommandLine` `UNICODE_STRING` in 64-bit `RTL_USER_PROCESS_PARAMETERS`.
const PARAMS_COMMAND_LINE: usize = 0x70;
/// The size of a 64-bit `UNICODE_STRING`: a `u16` length in bytes, a `u16` capacity, padding, and the buffer pointer.
const UNICODE_STRING_SIZE: usize = 16;

/// The machine's processes, from a `ToolHelp` snapshot, with each one's start time and command line where its process
/// can be opened. [`Win32ProcessTable::snapshot_without_command_lines`] gives the same entries with no command line.
#[derive(Clone, Copy, Debug)]
pub struct Win32ProcessTable;

impl ProcessTable for Win32ProcessTable {
    fn snapshot(&self) -> Vec<ProcessInfo> {
        walk(true)
    }
}

impl Win32ProcessTable {
    /// Every process running now, each with its pid, parent, executable name and start time: the same entries
    /// [`ProcessTable::snapshot`] lists, but with every `command_line` `None`.
    ///
    /// No process is opened for its command line, so no `PROCESS_VM_READ` is asked for and nothing of a process's PEB
    /// is read: a process this user may query but not read still contributes its start time. A caller that needs only
    /// the process tree and start times takes this snapshot.
    #[must_use]
    pub fn snapshot_without_command_lines(&self) -> Vec<ProcessInfo> {
        walk(false)
    }
}

/// Every process of one `ToolHelp` snapshot, each turned into a [`ProcessInfo`] by [`process_info`]; an empty list when
/// the snapshot itself cannot be taken. `command_lines` reads each process's command line as well as its start time.
fn walk(command_lines: bool) -> Vec<ProcessInfo> {
    let mut processes = Vec::new();
    let Ok(entry_size) = u32::try_from(size_of::<PROCESSENTRY32W>()) else {
        return processes;
    };
    // SAFETY: CreateToolhelp32Snapshot takes no pointers; its result is checked by the guard before any use. It returns
    // a fresh handle nothing else owns, so the guard alone closes it.
    let Some(snapshot) = (unsafe { OwnedHandle::new(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)) }) else {
        return processes;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: entry_size,
        ..PROCESSENTRY32W::default()
    };
    // SAFETY: the snapshot handle is open, and `entry` is a writable PROCESSENTRY32W whose dwSize is set.
    let mut more = unsafe { Process32FirstW(snapshot.0, &raw mut entry) } != 0;
    while more {
        processes.push(process_info(&entry, command_lines));
        // SAFETY: as for Process32FirstW above; the snapshot stays open until this loop ends.
        more = unsafe { Process32NextW(snapshot.0, &raw mut entry) } != 0;
    }
    processes
}

/// One snapshot entry with its start time, and its command line only when `command_lines` is set, each `None` when it
/// cannot be read.
fn process_info(entry: &PROCESSENTRY32W, command_lines: bool) -> ProcessInfo {
    let pid = entry.th32ProcessID;
    let name_length = entry.szExeFile.iter().position(|unit| *unit == 0).unwrap_or(entry.szExeFile.len());
    let readable = if command_lines {
        open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)
    } else {
        None
    };
    let (created, command_line) = match readable {
        Some(process) => (creation_time(&process), read_parameters_string(&process, PARAMS_COMMAND_LINE)),
        // A process this user may query but not read still has a start time; with no command line asked for, this is the
        // only handle opened.
        None => (
            open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).and_then(|process| creation_time(&process)),
            None,
        ),
    };
    ProcessInfo {
        pid,
        parent_pid: entry.th32ParentProcessID,
        name: String::from_utf16_lossy(&entry.szExeFile[..name_length]),
        command_line,
        created,
    }
}

/// An open handle, closed when the guard drops.
pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    /// The guard of a handle a Win32 call returned, or `None` when the call failed (null or `INVALID_HANDLE_VALUE`).
    ///
    /// # Safety
    ///
    /// `handle` is null, `INVALID_HANDLE_VALUE`, or an open handle this guard alone owns from now on: nothing else
    /// closes it and no other guard is made from it.
    #[must_use]
    pub unsafe fn new(handle: HANDLE) -> Option<Self> {
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            Some(Self(handle))
        } else {
            None
        }
    }

    /// The open handle, to pass to a Win32 call; the guard still owns it and closes it, so the caller never does.
    #[must_use]
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was open when the guard was made, the guard owns it, and it is closed only here, once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The process `pid` opened with `access`, or `None` when it cannot be opened.
#[must_use]
pub fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<OwnedHandle> {
    // SAFETY: OpenProcess takes no pointers; a failure returns null, which the guard refuses. A success is a fresh
    // handle nothing else owns, so the guard alone closes it.
    unsafe { OwnedHandle::new(OpenProcess(access, 0, pid)) }
}

/// When the process started, or `None` when its times cannot be read.
fn creation_time(process: &OwnedHandle) -> Option<Timestamp> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the process handle is open, and each argument is a writable FILETIME that outlives the call.
    let read = unsafe { GetProcessTimes(process.0, &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) };
    if read == 0 {
        return None;
    }
    let ticks = (i64::from(created.dwHighDateTime) << 32) | i64::from(created.dwLowDateTime);
    if ticks == 0 {
        return None;
    }
    filetime_to_timestamp(ticks)
}

/// The text of the `UNICODE_STRING` at `offset` in the process's 64-bit `RTL_USER_PROCESS_PARAMETERS`, or `None`
/// when any read fails or the string is empty. The process must be open with `PROCESS_VM_READ` and a query right.
#[must_use]
pub fn read_parameters_string(process: &OwnedHandle, offset: usize) -> Option<String> {
    let mut information = PROCESS_BASIC_INFORMATION::default();
    let size = u32::try_from(size_of::<PROCESS_BASIC_INFORMATION>()).ok()?;
    let mut returned = 0;
    // SAFETY: the process handle is open, `information` is a writable PROCESS_BASIC_INFORMATION of `size` bytes, and
    // `returned` is a writable u32; all outlive the call.
    let status = unsafe {
        NtQueryInformationProcess(
            process.0,
            ProcessBasicInformation,
            ptr::from_mut(&mut information).cast::<c_void>(),
            size,
            &raw mut returned,
        )
    };
    if status != 0 {
        return None;
    }
    let parameters = read_address(
        process,
        information.PebBaseAddress.expose_provenance().checked_add(PEB_PROCESS_PARAMETERS)?,
    )?;
    let header = read_memory(process, parameters.checked_add(offset)?, UNICODE_STRING_SIZE)?;
    let length = usize::from(u16::from_le_bytes([header[0], header[1]]));
    if length == 0 {
        return None;
    }
    let buffer = usize::try_from(u64::from_le_bytes(header[8..16].try_into().ok()?)).ok()?;
    let bytes = read_memory(process, buffer, length)?;
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
    Some(String::from_utf16_lossy(&units))
}

/// The 64-bit pointer stored at `address` in the process.
fn read_address(process: &OwnedHandle, address: usize) -> Option<usize> {
    let bytes = read_memory(process, address, 8)?;
    usize::try_from(u64::from_le_bytes(bytes.try_into().ok()?)).ok()
}

/// `size` bytes at `address` in the process, or `None` unless all of them were read.
fn read_memory(process: &OwnedHandle, address: usize, size: usize) -> Option<Vec<u8>> {
    let mut buffer = vec![0_u8; size];
    let mut read = 0;
    // SAFETY: the process handle is open, `buffer` has `size` writable bytes and `read` is a writable usize, all
    // outliving the call. The address is in the other process: the kernel reads it and fails the call on a bad one.
    let copied = unsafe {
        ReadProcessMemory(
            process.0,
            with_exposed_provenance::<c_void>(address),
            buffer.as_mut_ptr().cast::<c_void>(),
            size,
            &raw mut read,
        )
    };
    (copied != 0 && read == size).then_some(buffer)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot read this process must fail loudly")]
mod tests {
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, GetLastError, INVALID_HANDLE_VALUE, SetLastError};

    use super::{OwnedHandle, Win32ProcessTable};
    use crate::ProcessTable;

    #[test]
    fn refused_handle_keeps_the_last_error() {
        for handle in [ptr::null_mut(), INVALID_HANDLE_VALUE] {
            // SAFETY: SetLastError only sets this thread's last-error code.
            unsafe { SetLastError(ERROR_FILE_NOT_FOUND) };
            // SAFETY: the handle is null or INVALID_HANDLE_VALUE, which the guard refuses.
            let guard = unsafe { OwnedHandle::new(handle) };
            // SAFETY: GetLastError only reads this thread's last-error code.
            let error = unsafe { GetLastError() };
            assert!(guard.is_none(), "{handle:?} is refused");
            assert_eq!(error, ERROR_FILE_NOT_FOUND, "refusing {handle:?} keeps the thread's last error");
        }
    }

    #[test]
    fn process_table_lists_this_process() {
        let me = Win32ProcessTable
            .snapshot()
            .into_iter()
            .find(|process| process.pid == std::process::id())
            .expect("the snapshot lists this process");
        let exe = std::env::current_exe().expect("this test binary has a path");
        let stem = exe.file_stem().expect("the test binary has a file stem").to_string_lossy();
        assert_ne!(me.name, "");
        assert!(me.created.is_some());
        let command_line = me.command_line.expect("this process's command line can be read");
        assert!(command_line.contains(stem.as_ref()), "{command_line} names {stem}");
    }

    #[test]
    fn snapshot_without_command_lines_keeps_this_process_parent_and_start_time() {
        let full = Win32ProcessTable.snapshot();
        let brief = Win32ProcessTable.snapshot_without_command_lines();
        assert!(
            brief.iter().all(|process| process.command_line.is_none()),
            "no entry of a command-line-free snapshot carries a command line"
        );
        let me = std::process::id();
        let seen = full.iter().find(|process| process.pid == me).expect("the snapshot lists this process");
        let mine = brief
            .iter()
            .find(|process| process.pid == me)
            .expect("the command-line-free snapshot lists this process");
        assert_eq!(mine.parent_pid, seen.parent_pid);
        assert_eq!(mine.created, seen.created, "the same start time as the full snapshot gives");
        assert!(mine.created.is_some(), "this process's start time can be read without its command line");
    }

    #[test]
    fn created_is_whole_microseconds() {
        let created = Win32ProcessTable
            .snapshot()
            .into_iter()
            .find(|process| process.pid == std::process::id())
            .and_then(|process| process.created)
            .expect("this process's start time can be read");
        assert_eq!(
            created.as_nanosecond() % 1_000,
            0,
            "{created} is whole microseconds, as WMI's CreationDate is"
        );
    }
}
