//! The live machine behind the state's seams: the slot mutexes by the names `gate.ps1` gives them and a
//! process's working directory from its PEB, with proc-table's [`Win32ProcessTable`] for the process table.

use std::path::PathBuf;

use jiff::Timestamp;
use proc_table::Win32ProcessTable;
use proc_table::win32::{OwnedHandle, open_process, read_parameters_string};
use windows_sys::Win32::System::Threading::{IsWow64Process, OpenMutexW, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, SYNCHRONIZATION_SYNCHRONIZE};

use crate::model::{CwdReader, DashboardState, MutexProbe, StateInputs};
use crate::state::dashboard_state;

/// The offset of the `CurrentDirectory` path's `UNICODE_STRING` in 64-bit `RTL_USER_PROCESS_PARAMETERS`.
const PARAMS_CURRENT_DIRECTORY: usize = 0x38;

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

/// The inputs of the state of this machine now, read from its environment and its processor count.
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
        // handle is never waited on, so the mutex is not taken. The handle is fresh and nothing else owns it, so the guard
        // alone closes it, at once.
        unsafe { OwnedHandle::new(OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide.as_ptr())) }.is_some()
    }
}

fn is_wow64(process: &OwnedHandle) -> bool {
    let mut wow64 = 0;
    // SAFETY: the process handle is open and `wow64` is a writable BOOL that outlives the call.
    let asked = unsafe { IsWow64Process(process.raw(), &raw mut wow64) };
    asked != 0 && wow64 != 0
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot read this process must fail loudly")]
mod tests {
    use jiff::Timestamp;
    use proc_table::win32::OwnedHandle;
    use windows_sys::Win32::System::Threading::CreateMutexW;

    use super::{Win32CwdReader, Win32MutexProbe, trim_directory};
    use crate::model::{CwdReader, MutexProbe};

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
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call; null attributes are allowed. The handle
        // is fresh and nothing else owns it, so the guard alone closes it.
        let held = unsafe { OwnedHandle::new(CreateMutexW(std::ptr::null(), 0, wide.as_ptr())) }.expect("the test mutex can be created");
        assert!(Win32MutexProbe.is_open(&name));
        drop(held);
        assert!(!Win32MutexProbe.is_open(&name));
    }
}
