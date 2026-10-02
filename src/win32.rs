//! The live machine behind the state's seams, read the way the gate dashboard reads it: the process table from a
//! `ToolHelp` snapshot and each process's PEB, the slot mutexes by name, and a process's working directory from its PEB.

use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;
use std::ptr::{self, with_exposed_provenance};

use jiff::Timestamp;
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, IsWow64Process, OpenMutexW, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ, SYNCHRONIZATION_SYNCHRONIZE,
};

use crate::model::{CwdReader, DashboardState, MutexProbe, ProcessInfo, ProcessTable, StateInputs};
use crate::state::dashboard_state;

/// FILETIME ticks (100 ns since 1601-01-01 UTC) at the Unix epoch.
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;
/// The offset of `ProcessParameters` in a 64-bit PEB.
const PEB_PROCESS_PARAMETERS: usize = 0x20;
/// The offset of the `CurrentDirectory` path's `UNICODE_STRING` in 64-bit `RTL_USER_PROCESS_PARAMETERS`.
const PARAMS_CURRENT_DIRECTORY: usize = 0x38;
/// The offset of the `CommandLine` `UNICODE_STRING` in 64-bit `RTL_USER_PROCESS_PARAMETERS`.
const PARAMS_COMMAND_LINE: usize = 0x70;
/// The size of a 64-bit `UNICODE_STRING`: a `u16` length in bytes, a `u16` capacity, padding, and the buffer pointer.
const UNICODE_STRING_SIZE: usize = 16;

/// The machine's processes, from a `ToolHelp` snapshot, with each one's start time and command line where its process
/// can be opened.
#[derive(Clone, Copy, Debug)]
pub struct Win32ProcessTable;

/// The slot mutexes in the session's `Local\` namespace, opened only to see that they exist.
#[derive(Clone, Copy, Debug)]
pub struct Win32MutexProbe;

/// A process's working directory, read from its PEB: 64-bit processes this user can open only.
#[derive(Clone, Copy, Debug)]
pub struct Win32CwdReader;

/// Why the live inputs could not be read.
#[derive(Debug, thiserror::Error)]
pub enum LiveInputsError {
    /// An environment variable the state needs is unset or empty.
    #[error("the environment variable {0} is not set; Windows sets it at logon, so run from a signed-in user session")]
    MissingVariable(&'static str),
    /// The processor count the pools' default counts follow could not be read.
    #[error("could not read the processor count: {0}")]
    Processors(#[source] std::io::Error),
}

/// The inputs of the state of this machine now, read as the gate dashboard reads them.
///
/// The machine's name comes from `COMPUTERNAME`, the processor count from the processors this process may run on (as
/// .NET's `Environment.ProcessorCount` counts them), and the gate's folder from `LOCALAPPDATA`. The test-only
/// `GATE_TEST_SLOT_COUNTS` and `GATE_TEST_SLOT_PREFIX` are honoured when set and not empty.
///
/// # Errors
///
/// [`LiveInputsError::MissingVariable`] when `COMPUTERNAME` or `LOCALAPPDATA` is unset or empty, and
/// [`LiveInputsError::Processors`] when the processor count cannot be read.
pub fn live_inputs() -> Result<StateInputs, LiveInputsError> {
    let machine = required_variable("COMPUTERNAME")?.to_string_lossy().into_owned();
    let local_app_data = PathBuf::from(required_variable("LOCALAPPDATA")?);
    let processors = std::thread::available_parallelism().map_err(LiveInputsError::Processors)?.get();
    Ok(StateInputs {
        now: Timestamp::now(),
        machine,
        processors,
        local_app_data,
        slot_counts_override: optional_variable("GATE_TEST_SLOT_COUNTS").map(PathBuf::from),
        slot_prefix: optional_variable("GATE_TEST_SLOT_PREFIX").map_or_else(String::new, |prefix| prefix.to_string_lossy().into_owned()),
    })
}

/// The state of this machine now: [`dashboard_state`] over [`live_inputs`] and the Win32 process table, mutexes and
/// working directories. It blocks on the process snapshot and the file reads, so an async caller runs it on a blocking
/// thread.
///
/// # Errors
///
/// The [`LiveInputsError`] of [`live_inputs`].
pub fn live_state() -> Result<DashboardState, LiveInputsError> {
    Ok(dashboard_state(&live_inputs()?, &Win32ProcessTable, &Win32MutexProbe, &Win32CwdReader))
}

fn required_variable(name: &'static str) -> Result<std::ffi::OsString, LiveInputsError> {
    optional_variable(name).ok_or(LiveInputsError::MissingVariable(name))
}

/// The variable's value, or `None` when it is unset or empty, as PowerShell's `if ($env:NAME)` reads it.
fn optional_variable(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

impl ProcessTable for Win32ProcessTable {
    fn snapshot(&self) -> Vec<ProcessInfo> {
        let mut processes = Vec::new();
        let Ok(entry_size) = u32::try_from(size_of::<PROCESSENTRY32W>()) else {
            return processes;
        };
        // SAFETY: CreateToolhelp32Snapshot takes no pointers; its result is checked by the guard before any use.
        let Some(snapshot) = OwnedHandle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return processes;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: entry_size,
            ..PROCESSENTRY32W::default()
        };
        // SAFETY: the snapshot handle is open, and `entry` is a writable PROCESSENTRY32W whose dwSize is set.
        let mut more = unsafe { Process32FirstW(snapshot.0, &raw mut entry) } != 0;
        while more {
            processes.push(process_info(&entry));
            // SAFETY: as for Process32FirstW above; the snapshot stays open until this loop ends.
            more = unsafe { Process32NextW(snapshot.0, &raw mut entry) } != 0;
        }
        processes
    }
}

/// One snapshot entry with its start time and command line, each `None` when it cannot be read.
fn process_info(entry: &PROCESSENTRY32W) -> ProcessInfo {
    let pid = entry.th32ProcessID;
    let name_length = entry.szExeFile.iter().position(|unit| *unit == 0).unwrap_or(entry.szExeFile.len());
    let (created, command_line) = if let Some(process) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ) {
        (creation_time(&process), read_parameters_string(&process, PARAMS_COMMAND_LINE))
    } else {
        // A process this user may query but not read still has a start time.
        (
            open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).and_then(|process| creation_time(&process)),
            None,
        )
    };
    ProcessInfo {
        pid,
        parent_pid: entry.th32ParentProcessID,
        name: String::from_utf16_lossy(&entry.szExeFile[..name_length]),
        command_line,
        created,
    }
}

impl CwdReader for Win32CwdReader {
    fn current_dir(&self, pid: u32) -> Option<String> {
        let process = open_process(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
        if is_wow64(&process) {
            return None;
        }
        let directory = read_parameters_string(&process, PARAMS_CURRENT_DIRECTORY)?;
        Some(trim_directory(directory))
    }
}

/// The directory without its trailing backslashes, unless it is three characters or fewer, such as `C:\`.
fn trim_directory(directory: String) -> String {
    if directory.encode_utf16().count() > 3 {
        directory.trim_end_matches('\\').to_owned()
    } else {
        directory
    }
}

impl MutexProbe for Win32MutexProbe {
    fn is_open(&self, name: &str) -> bool {
        let wide: Vec<u16> = format!("Local\\{name}").encode_utf16().chain([0]).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call. Only SYNCHRONIZE is asked for and the
        // handle is never waited on, so the mutex is not taken; the guard closes it at once.
        let handle = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide.as_ptr()) };
        OwnedHandle::new(handle).is_some()
    }
}

/// An open handle, closed when the guard drops.
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    /// The guard of a handle a Win32 call returned, or `None` when the call failed (null or `INVALID_HANDLE_VALUE`).
    fn new(handle: HANDLE) -> Option<Self> {
        (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Self(handle))
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was open when the guard was made, the guard owns it, and it is closed only here, once.
        unsafe { CloseHandle(self.0) };
    }
}

fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<OwnedHandle> {
    // SAFETY: OpenProcess takes no pointers; a failure returns null, which the guard refuses.
    OwnedHandle::new(unsafe { OpenProcess(access, 0, pid) })
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
    // The gate dashboard reads WMI's CreationDate, which holds whole microseconds, so the 100 ns digit is dropped.
    Timestamp::from_microsecond((ticks - FILETIME_UNIX_EPOCH).div_euclid(10)).ok()
}

fn is_wow64(process: &OwnedHandle) -> bool {
    let mut wow64 = 0;
    // SAFETY: the process handle is open and `wow64` is a writable BOOL that outlives the call.
    let asked = unsafe { IsWow64Process(process.0, &raw mut wow64) };
    asked != 0 && wow64 != 0
}

/// The text of the `UNICODE_STRING` at `offset` in the process's 64-bit `RTL_USER_PROCESS_PARAMETERS`, or `None`
/// when any read fails or the string is empty.
fn read_parameters_string(process: &OwnedHandle, offset: usize) -> Option<String> {
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
    use jiff::Timestamp;
    use windows_sys::Win32::System::Threading::CreateMutexW;

    use super::{OwnedHandle, Win32CwdReader, Win32MutexProbe, Win32ProcessTable, trim_directory};
    use crate::model::{CwdReader, MutexProbe, ProcessTable};

    #[test]
    fn process_table_lists_this_process() {
        let me = Win32ProcessTable
            .snapshot()
            .into_iter()
            .find(|process| process.pid == std::process::id())
            .expect("the snapshot lists this process");
        let exe = std::env::current_exe().expect("this test binary has a path");
        let stem = exe.file_stem().expect("the test binary has a file stem").to_string_lossy();
        assert!(!me.name.is_empty());
        assert!(me.created.is_some());
        let command_line = me.command_line.expect("this process's command line can be read");
        assert!(command_line.contains(stem.as_ref()), "{command_line} names {stem}");
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

    #[test]
    fn cwd_reader_reads_this_process_dir() {
        let expected = std::env::current_dir().expect("this process has a working directory");
        let expected = trim_directory(expected.to_string_lossy().into_owned());
        assert_eq!(Win32CwdReader.current_dir(std::process::id()), Some(expected));
    }

    #[test]
    fn cwd_reader_unknown_pid_is_none() {
        assert_eq!(Win32CwdReader.current_dir(u32::MAX), None);
    }

    #[test]
    fn mutex_probe_sees_a_held_mutex() {
        let name = format!("ops-tower-test-{}-{}", std::process::id(), Timestamp::now().as_nanosecond());
        let wide: Vec<u16> = format!("Local\\{name}").encode_utf16().chain([0]).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call; null attributes are allowed.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        let held = OwnedHandle::new(handle).expect("the test mutex can be created");
        assert!(Win32MutexProbe.is_open(&name));
        drop(held);
        assert!(!Win32MutexProbe.is_open(&name));
    }
}
