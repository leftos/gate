//! Named job objects: a job holds a command and every process it starts, and can end them all at once.

use std::io;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_MORE_DATA, GetLastError, SetLastError};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList, QueryInformationJobObject, TerminateJobObject,
};

use crate::{own_handle, wide};

/// What a job has used and holds: its CPU time and its live processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accounting {
    /// The user and kernel time of every process that has ever been in the job, those that have exited and those in
    /// the jobs nested inside it included, in 100 ns ticks.
    pub cpu: u64,
    /// The processes in the job now.
    pub active_processes: u32,
}

/// A named job object with no limits: a process in it cannot break away, and nothing is killed when the job is
/// dropped, which only closes this handle.
pub struct Job {
    handle: OwnedHandle,
    name: String,
}

// SAFETY: a job handle is a kernel object reference with no thread affinity: every call made through it
// (`QueryInformationJobObject`, `AssignProcessToJobObject`, `TerminateJobObject`) may come from any thread, and the
// kernel serialises them on the object. The handle is closed once, when the one `Job` owning it drops, so moving the
// owner to another thread cannot close it twice.
unsafe impl Send for Job {}

// SAFETY: every method takes `&self` and only passes the handle to the thread-safe calls above, or reads the name,
// which is never changed after creation; no `&Job` can close the handle, so sharing one between threads is sound.
unsafe impl Sync for Job {}

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
        process_ids(&self.handle)
    }

    /// The CPU time the job has used, every process ever in it counted, and the processes it holds now.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be queried.
    pub fn accounting(&self) -> io::Result<Accounting> {
        accounting(&self.handle)
    }

    /// Ends every process in the job, and in the jobs nested inside it, with `exit_code`.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be terminated.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        terminate(&self.handle, exit_code)
    }
}

/// The ids of every process in the job behind `job` now, those in the jobs nested inside it included. A handle
/// without `JOB_OBJECT_QUERY` access gets the system's error.
pub(crate) fn process_ids(job: &OwnedHandle) -> io::Result<Vec<u32>> {
    let first = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList) / size_of::<usize>();
    let mut capacity = 64;
    loop {
        let mut buffer = vec![0_usize; first + capacity];
        let length = u32::try_from(size_of_val(buffer.as_slice())).map_err(io::Error::other)?;
        // SAFETY: the handle is open for the whole call, and one without the `JOB_OBJECT_QUERY` access the call asks
        // for only makes it fail; `buffer` is writable for `length` bytes and aligned as the list is, and no returned
        // length is asked for.
        let read = unsafe {
            QueryInformationJobObject(
                job.raw(),
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

/// The accounting of the job behind `job`: the user and kernel time of every process ever in it, and the processes it
/// holds now. A handle without `JOB_OBJECT_QUERY` access gets the system's error.
pub(crate) fn accounting(job: &OwnedHandle) -> io::Result<Accounting> {
    let mut information = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let length = u32::try_from(size_of_val(&information)).map_err(io::Error::other)?;
    // SAFETY: the handle is open for the whole call, and one without the `JOB_OBJECT_QUERY` access the call asks for
    // only makes it fail; `information` is writable for `length` bytes, and no returned length is asked for.
    let read = unsafe {
        QueryInformationJobObject(
            job.raw(),
            JobObjectBasicAccountingInformation,
            (&raw mut information).cast(),
            length,
            std::ptr::null_mut(),
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    let user = u64::try_from(information.TotalUserTime).map_err(io::Error::other)?;
    let kernel = u64::try_from(information.TotalKernelTime).map_err(io::Error::other)?;
    Ok(Accounting {
        cpu: user + kernel,
        active_processes: information.ActiveProcesses,
    })
}

/// Ends every process in the job behind `job`, and in the jobs nested inside it, with `exit_code`. A handle without
/// `JOB_OBJECT_TERMINATE` access gets the system's error.
pub(crate) fn terminate(job: &OwnedHandle, exit_code: u32) -> io::Result<()> {
    // SAFETY: the handle is open for the whole call, and one without the `JOB_OBJECT_TERMINATE` access the call asks
    // for only makes it fail.
    let terminated = unsafe { TerminateJobObject(job.raw(), exit_code) };
    if terminated == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
