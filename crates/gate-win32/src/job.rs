//! Named job objects: a job holds a command and every process it starts, and can end them all at once.

use std::io;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_MORE_DATA, GetLastError, SetLastError};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation, JobObjectBasicAndIoAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};

use crate::launch::struct_size;
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

/// A job object, named or anonymous. One made by [`Job::create`] or [`Job::create_breakaway`] has no other limits, and
/// nothing is killed when it is dropped, which only closes this handle; one made by [`Job::create_kill_on_close`] or
/// [`Job::anonymous_kill_on_close`] ends every process in it when its last handle is closed. A process in a job cannot
/// break away from it, unless the job was made by [`Job::create_breakaway`] or
/// [`Job::anonymous_kill_on_close_breakaway`].
pub struct Job {
    handle: OwnedHandle,
    name: String,
    kill_on_close: bool,
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
        Job::created(Some(name))
    }

    /// Creates the job `name`, or an anonymous one, which no other process can open by a name, for `None`.
    fn created(name: Option<&str>) -> io::Result<Job> {
        let wide_name = name.map(wide);
        let name_pointer = wide_name.as_ref().map_or(std::ptr::null(), Vec::as_ptr);
        // SAFETY: `SetLastError` takes no pointers. It clears the last error so that a stale `ERROR_ALREADY_EXISTS`
        // cannot pass for this call's.
        unsafe { SetLastError(0) };
        // SAFETY: `name_pointer` is null or points to `wide_name`, which is NUL-terminated and outlives the call; null
        // security attributes are allowed.
        let created = unsafe { CreateJobObjectW(std::ptr::null(), name_pointer) };
        // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
        let error = unsafe { GetLastError() };
        // SAFETY: a success is a fresh handle nothing else owns, to a new job or to the one the name already held.
        let handle = unsafe { own_handle(created) }?;
        let name = name.unwrap_or_default();
        if error == ERROR_ALREADY_EXISTS {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("a job named {name} exists already")));
        }
        Ok(Job {
            handle,
            name: name.to_owned(),
            kill_on_close: false,
        })
    }

    /// Creates the job `name` as [`Job::create`] does, then has it end every process in it when its last handle is
    /// closed (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`): once this handle is dropped, or the process holding it ends, no
    /// process in the job outlives it unless another handle to the job is open. A [`crate::PipedLaunch`] started in
    /// it gives the command no handle to it, so the command cannot keep its own job open.
    ///
    /// # Errors
    ///
    /// As [`Job::create`]; and the system's error when the limit cannot be set, the job's handle closed again.
    pub fn create_kill_on_close(name: &str) -> io::Result<Job> {
        Job::create_limited(Some(name), JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
    }

    /// Creates an anonymous job that ends every process in it when its last handle is closed, as
    /// [`Job::create_kill_on_close`] does: having no name, no other process can open it, or take its name first.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be created, or its limit cannot be set.
    pub fn anonymous_kill_on_close() -> io::Result<Job> {
        Job::create_limited(None, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
    }

    /// Creates an anonymous job as [`Job::anonymous_kill_on_close`] does, and lets a process in it create one outside
    /// it (`JOB_OBJECT_LIMIT_BREAKAWAY_OK`), as a [`crate::PipedLaunch`] started from inside it needs: the process it
    /// creates is not ended with the job.
    ///
    /// # Errors
    ///
    /// As [`Job::anonymous_kill_on_close`].
    pub fn anonymous_kill_on_close_breakaway() -> io::Result<Job> {
        Job::create_limited(None, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK)
    }

    /// Creates the job `name` as [`Job::create`] does, with nothing killed when it is dropped, and lets a process in it
    /// create one outside it with `CREATE_BREAKAWAY_FROM_JOB` (`JOB_OBJECT_LIMIT_BREAKAWAY_OK`): the process it creates
    /// is not in the job, as a packaged app a process of the job starts is not.
    ///
    /// # Errors
    ///
    /// As [`Job::create`]; and the system's error when the limit cannot be set, the job's handle closed again.
    pub fn create_breakaway(name: &str) -> io::Result<Job> {
        Job::create_limited(Some(name), JOB_OBJECT_LIMIT_BREAKAWAY_OK)
    }

    /// Creates the job `name`, or an anonymous one for `None`, with the limits `flags`; it ends every process in it when
    /// its last handle is closed only when they hold `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    fn create_limited(name: Option<&str>, flags: u32) -> io::Result<Job> {
        let mut job = Job::created(name)?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = flags;
        // SAFETY: the handle is open for the whole call, with the `JOB_OBJECT_SET_ATTRIBUTES` access a new job's
        // handle has; `limits` is a whole extended limit structure of the size passed, read only.
        let set = unsafe {
            SetInformationJobObject(
                job.handle.raw(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                struct_size::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        job.kill_on_close = flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0;
        Ok(job)
    }

    /// Whether it ends every process in it when its last handle is closed ([`Job::create_kill_on_close`]).
    pub(crate) fn kills_on_close(&self) -> bool {
        self.kill_on_close
    }

    /// The name it was created by; empty for an anonymous job.
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

    /// Whether the process behind `process`, open with `PROCESS_QUERY_LIMITED_INFORMATION`, is in the job, or in a job
    /// nested inside it (`IsProcessInJob`).
    pub(crate) fn holds(&self, process: &OwnedHandle) -> io::Result<bool> {
        holds(&self.handle, process)
    }

    /// The CPU time the job has used, every process ever in it counted, and the processes it holds now.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be queried.
    pub fn accounting(&self) -> io::Result<Accounting> {
        accounting(&self.handle)
    }

    /// The bytes every process ever in the job has written, to files, pipes and devices alike, those that have exited
    /// and those in the jobs nested inside it included.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be queried.
    pub fn written_bytes(&self) -> io::Result<u64> {
        written_bytes(&self.handle)
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

/// The bytes every process ever in the job behind `job` has written: its I/O accounting's write transfer count. A
/// handle without `JOB_OBJECT_QUERY` access gets the system's error.
pub(crate) fn written_bytes(job: &OwnedHandle) -> io::Result<u64> {
    let mut information = JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION::default();
    let length = u32::try_from(size_of_val(&information)).map_err(io::Error::other)?;
    // SAFETY: the handle is open for the whole call, and one without the `JOB_OBJECT_QUERY` access the call asks for
    // only makes it fail; `information` is writable for `length` bytes, and no returned length is asked for.
    let read = unsafe {
        QueryInformationJobObject(
            job.raw(),
            JobObjectBasicAndIoAccountingInformation,
            (&raw mut information).cast(),
            length,
            std::ptr::null_mut(),
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(information.IoInfo.WriteTransferCount)
}

/// Whether the process behind `process` is in the job behind `job`, or in a job nested inside it (`IsProcessInJob`). A
/// process handle without `PROCESS_QUERY_LIMITED_INFORMATION` access, or a job handle without `JOB_OBJECT_QUERY`
/// access, gets the system's error.
pub(crate) fn holds(job: &OwnedHandle, process: &OwnedHandle) -> io::Result<bool> {
    let mut inside = 0;
    // SAFETY: both handles are open for the whole call, and one without the access the call asks for only makes it
    // fail; `inside` is writable.
    if unsafe { IsProcessInJob(process.raw(), job.raw(), &raw mut inside) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(inside != 0)
}

/// Ends every process in the job behind `job`, and in the jobs nested inside it, with `exit_code`. A handle without
/// `JOB_OBJECT_TERMINATE` access gets the system's error.
pub(crate) fn terminate(job: &OwnedHandle, exit_code: u32) -> io::Result<()> {
    // SAFETY: the handle is open for the whole call, and one without the `JOB_OBJECT_TERMINATE` access the call asks
    // for only makes it fail.
    let terminated = unsafe { TerminateJobObject(job.raw(), exit_code) };
    if terminated == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
