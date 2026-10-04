//! The live machine behind the state's seams: the slot mutexes by the names `gate.ps1` gives them and a
//! process's working directory from its PEB; the process table comes from the caller.

use std::path::PathBuf;

use jiff::Timestamp;
use proc_table::ProcessInfo;
use proc_table::win32::{OwnedHandle, open_process, read_parameters_string};
use windows_sys::Win32::System::Threading::{IsWow64Process, OpenMutexW, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, SYNCHRONIZATION_SYNCHRONIZE};

use crate::journal::{History, JournalCache};
use crate::model::{CwdReader, MutexProbe, StateInputs};

/// The offset of the `CurrentDirectory` path's `UNICODE_STRING` in 64-bit `RTL_USER_PROCESS_PARAMETERS`.
const PARAMS_CURRENT_DIRECTORY: usize = 0x38;
/// How far a live process's start time may be from the `processStart` a journal line records, in nanoseconds: the
/// tolerance a claim's start time gets.
const PROCESS_START_TOLERANCE_NANOS: i128 = 2_000_000_000;

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

/// Why the gate journal's history could not be read.
#[derive(Debug, thiserror::Error)]
pub enum LiveHistoryError {
    /// The inputs naming the journal's folder and prefix could not be read.
    #[error(transparent)]
    Inputs(#[from] LiveInputsError),
    /// A journal file exists but could not be read.
    #[error("could not read the gate journal in {dir}: {source}")]
    Journal {
        /// The folder holding the journal.
        dir: PathBuf,
        /// What the read met.
        #[source]
        source: std::io::Error,
    },
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

/// The gate journal's history on this machine now, read through `cache` from `%LOCALAPPDATA%\gate` with the
/// `GATE_TEST_SLOT_PREFIX` [`live_inputs`] reads. An open run's gate is live when its pid is in `processes`, the
/// caller's process snapshot, and started within 2 s of the `processStart` the journal records. It blocks on the file
/// reads, so an async caller runs it on a blocking thread.
///
/// # Errors
///
/// The [`LiveInputsError`] of [`live_inputs`], or [`LiveHistoryError::Journal`] when a journal file exists but cannot
/// be read.
pub fn live_history(cache: &mut JournalCache, processes: &[ProcessInfo]) -> Result<History, LiveHistoryError> {
    let inputs = live_inputs()?;
    let dir = inputs.local_app_data.join("gate");
    let is_live = |pid: u32, process_start: &str| {
        let Ok(recorded) = process_start.parse::<Timestamp>() else {
            return false;
        };
        processes
            .iter()
            .rfind(|process| process.pid == pid)
            .and_then(|process| process.created)
            .is_some_and(|created| (created.as_nanosecond() - recorded.as_nanosecond()).abs() < PROCESS_START_TOLERANCE_NANOS)
    };
    cache
        .read(&dir, &inputs.slot_prefix, inputs.now, is_live)
        .map_err(|source| LiveHistoryError::Journal { dir, source })
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
