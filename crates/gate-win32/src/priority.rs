//! A process's priority class, read and set: what a watch reads and lowers the command's processes still at High
//! with, once a late take has the command run as its own kind.

use std::io;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, REALTIME_PRIORITY_CLASS, SetPriorityClass,
};

use crate::own_handle;

/// A process's priority class: the class `GetPriorityClass` reads and `SetPriorityClass` sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Priority {
    /// `IDLE_PRIORITY_CLASS`.
    Idle,
    /// `BELOW_NORMAL_PRIORITY_CLASS`.
    BelowNormal,
    /// `NORMAL_PRIORITY_CLASS`.
    Normal,
    /// `ABOVE_NORMAL_PRIORITY_CLASS`.
    AboveNormal,
    /// `HIGH_PRIORITY_CLASS`.
    High,
    /// `REALTIME_PRIORITY_CLASS`.
    RealTime,
}

impl Priority {
    /// The Win32 constant that asks for the class.
    const fn constant(self) -> u32 {
        match self {
            Priority::Idle => IDLE_PRIORITY_CLASS,
            Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            Priority::Normal => NORMAL_PRIORITY_CLASS,
            Priority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            Priority::High => HIGH_PRIORITY_CLASS,
            Priority::RealTime => REALTIME_PRIORITY_CLASS,
        }
    }

    /// The class the Win32 constant `class` stands for, `None` for a value Windows never answers with.
    fn of(class: u32) -> Option<Priority> {
        [
            Priority::Idle,
            Priority::BelowNormal,
            Priority::Normal,
            Priority::AboveNormal,
            Priority::High,
            Priority::RealTime,
        ]
        .into_iter()
        .find(|candidate| candidate.constant() == class)
    }
}

impl crate::Running {
    /// The command's priority class, read through the handle its launch holds (`GetPriorityClass`), so no process is
    /// opened by its pid; `None` for a class Windows never answers with.
    ///
    /// # Errors
    ///
    /// The system's error when the class cannot be read.
    pub fn priority(&self) -> io::Result<Option<Priority>> {
        // SAFETY: the handle is open for as long as `self` holds it, with the query access the creation of a process
        // gives its handle.
        let class = unsafe { GetPriorityClass(self.process.raw()) };
        if class == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Priority::of(class))
    }
}

/// The priority class of the process `pid`; `None` when no process of that pid runs. The process is opened with
/// `PROCESS_QUERY_INFORMATION`, as .NET's `Process.PriorityClass` getter opens it, so a class that getter cannot read,
/// such as an elevated process's, is an error here too.
///
/// # Errors
///
/// The system's error when the process exists but cannot be queried.
pub fn priority_of(pid: u32) -> io::Result<Option<Priority>> {
    let Some(process) = open(pid, PROCESS_QUERY_INFORMATION)? else {
        return Ok(None);
    };
    // SAFETY: the handle is open for the whole call, with the query access the call asks for.
    let class = unsafe { GetPriorityClass(process.raw()) };
    if class == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Priority::of(class))
}

/// Sets the priority class of the process `pid` to `priority`; `None` when no process of that pid runs, which leaves
/// nothing to set.
///
/// # Errors
///
/// The system's error when the process exists but its class cannot be set, as when this process may not set the class
/// of that one.
pub fn set_priority(pid: u32, priority: Priority) -> io::Result<Option<()>> {
    let Some(process) = open(pid, PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION)? else {
        return Ok(None);
    };
    // SAFETY: the handle is open for the whole call, with the set access the call asks for.
    let set = unsafe { SetPriorityClass(process.raw(), priority.constant()) };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(()))
}

/// The process `pid` opened with `access`, or `None` when no process has that pid.
///
/// # Errors
///
/// The system's error when the process exists but cannot be opened with that access.
fn open(pid: u32, access: u32) -> io::Result<Option<OwnedHandle>> {
    // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
    match unsafe { own_handle(OpenProcess(access, 0, pid)) } {
        Ok(handle) => Ok(Some(handle)),
        // `OpenProcess` reports a pid no process has as `ERROR_INVALID_PARAMETER`.
        Err(error) if error.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() => Ok(None),
        Err(error) => Err(error),
    }
}
