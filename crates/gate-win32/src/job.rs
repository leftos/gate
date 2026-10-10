//! Named job objects: a job holds a command and every process it starts, and can end them all at once.

use std::io;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_MORE_DATA, GetLastError, SetLastError};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectBasicProcessIdList, QueryInformationJobObject,
    TerminateJobObject,
};

use crate::{own_handle, wide};

/// A named job object with no limits: a process in it cannot break away, and nothing is killed when the job is
/// dropped, which only closes this handle.
pub struct Job {
    handle: OwnedHandle,
    name: String,
}

impl Job {
    /// Creates the job `name` (such as `Local\gate-job-<pid>`), with no limits and no kill-on-close.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::AlreadyExists`] when a job of that name exists already, whose handle is
    /// closed again at once; the system's error when the job cannot be created.
    pub fn create(name: &str) -> io::Result<Job> {
        let wide_name = wide(name);
        // SAFETY: `SetLastError` takes no pointers. It clears the last error so that a stale `ERROR_ALREADY_EXISTS`
        // cannot pass for this call's.
        unsafe { SetLastError(0) };
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; null security attributes are allowed.
        let created = unsafe { CreateJobObjectW(std::ptr::null(), wide_name.as_ptr()) };
        // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
        let error = unsafe { GetLastError() };
        // SAFETY: a success is a fresh handle nothing else owns, to a new job or to the one the name already held.
        let handle = unsafe { own_handle(created) }?;
        if error == ERROR_ALREADY_EXISTS {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("a job named {name} exists already")));
        }
        Ok(Job {
            handle,
            name: name.to_owned(),
        })
    }

    /// The name it was created by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Puts the process behind `process` in the job, and with it every process it starts from then on.
    pub(crate) fn assign(&self, process: &OwnedHandle) -> io::Result<()> {
        // SAFETY: both handles are open for the whole call; the process handle `CreateProcessW` returned has the
        // `PROCESS_SET_QUOTA` and `PROCESS_TERMINATE` access the call asks for.
        let assigned = unsafe { AssignProcessToJobObject(self.handle.raw(), process.raw()) };
        if assigned == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// The ids of every process in the job now, those in the jobs nested inside it included.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be queried.
    pub fn process_ids(&self) -> io::Result<Vec<u32>> {
        let first = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList) / size_of::<usize>();
        let mut capacity = 64;
        loop {
            let mut buffer = vec![0_usize; first + capacity];
            let length = u32::try_from(size_of_val(buffer.as_slice())).map_err(io::Error::other)?;
            // SAFETY: the handle is open for the whole call, with the `JOB_OBJECT_QUERY` access a created job has;
            // `buffer` is writable for `length` bytes and aligned as the list is, and no returned length is asked for.
            let read = unsafe {
                QueryInformationJobObject(
                    self.handle.raw(),
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr().cast(),
                    length,
                    std::ptr::null_mut(),
                )
            } != 0;
            // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
            let error = if read { 0 } else { unsafe { GetLastError() } };
            if !read && error != ERROR_MORE_DATA {
                return Err(io::Error::from_raw_os_error(error.cast_signed()));
            }
            // SAFETY: `buffer` is aligned as the list is and longer than it, and the call wrote its two counts.
            let list = unsafe { &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
            let assigned = list.NumberOfAssignedProcesses as usize;
            let listed = list.NumberOfProcessIdsInList as usize;
            if read && listed >= assigned {
                let ids = buffer
                    .get(first..first + listed)
                    .ok_or_else(|| io::Error::other("the job listed more process ids than its buffer holds"))?;
                return ids.iter().map(|&id| u32::try_from(id).map_err(io::Error::other)).collect();
            }
            capacity = (capacity * 2).max(assigned + 16);
        }
    }

    /// Ends every process in the job, and in the jobs nested inside it, with `exit_code`.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be terminated.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: the handle is open for the whole call, with the `JOB_OBJECT_TERMINATE` access a created job has.
        let terminated = unsafe { TerminateJobObject(self.handle.raw(), exit_code) };
        if terminated == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
}
