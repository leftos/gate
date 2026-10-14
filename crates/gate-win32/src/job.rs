//! Named job objects: a job holds a command and every process it starts, and can end them all at once.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use jiff::Timestamp;
use proc_table::filetime_to_timestamp;
use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ALREADY_EXISTS, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, SetLastError, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectAssociateCompletionPortInformation,
    JobObjectBasicAccountingInformation, JobObjectBasicAndIoAccountingInformation, JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::SystemServices::{JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS, JOB_OBJECT_MSG_EXIT_PROCESS, JOB_OBJECT_MSG_NEW_PROCESS};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

use crate::launch::struct_size;
use crate::times::file_time_now;
use crate::{own_handle, start_and_exit_of, timestamp_of, wide};

/// The completion key a job's drop posts to its port to wake its drain thread; the job's own messages carry the key 0
/// the port was associated with.
const CLOSE_KEY: usize = 1;

/// The longest the drain thread waits for a message before it looks again, in milliseconds: a port found empty for that
/// long lets the members that have exited settle, and the job's drop is seen within it.
const DRAIN_WAIT_MS: u32 = 100;

/// How many of the members a memory could not read its trouble names; the rest it counts.
const UNREAD_NAMED: usize = 3;

/// What a job has used and holds: its CPU time and its live processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accounting {
    /// The user and kernel time of every process that has ever been in the job, those that have exited and those in
    /// the jobs nested inside it included, in 100 ns ticks.
    pub cpu: u64,
    /// The processes in the job now.
    pub active_processes: u32,
}

/// A member a job remembers that has ended ([`Job::departed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Departure {
    /// Its pid.
    pub pid: u32,
    /// Its start, read while its process object lived: with its pid, it tells the member from a later holder of the pid.
    pub start: Timestamp,
    /// The first moment it can have started a process as a member: the later of its start and the job's creation, for a
    /// process put in the job from outside it.
    pub from: Timestamp,
    /// Its exit, the last moment it can have started a process.
    pub until: Timestamp,
}

/// The members a job remembers that have ended, with why what it remembers may lack a member, when it may
/// ([`Job::departed`]).
pub type Departures = (Vec<Departure>, Option<String>);

/// A job object, named or anonymous. One made by [`Job::create`] or [`Job::create_breakaway`] has no other limits, and
/// nothing is killed when it is dropped, which only closes this handle; one made by [`Job::create_kill_on_close`] or
/// [`Job::anonymous_kill_on_close`] ends every process in it when its last handle is closed. A process in a job cannot
/// break away from it, unless the job was made by [`Job::create_breakaway`] or
/// [`Job::anonymous_kill_on_close_breakaway`].
///
/// A job told to [`Job::remember`] before any process is put in it remembers the processes that join it: a completion
/// port reports each one, and a thread of the job's own takes each report as it comes and remembers the process with
/// the stretch of time in which it can have started a process, so that [`Job::departed`] still names a member that
/// started and ended between two looks at the job.
pub struct Job {
    handle: OwnedHandle,
    name: String,
    kill_on_close: bool,
    /// When the job was created, as the process table gives start times: no member started before it.
    created_at: Timestamp,
    /// What it remembers of the processes that joined it, once told to.
    memory: Option<Memory>,
}

/// What a job remembers of the processes that joined it, and the thread that remembers them.
struct Memory {
    shared: Arc<Shared>,
    /// The thread that takes the port's messages, until the job's drop ends it.
    drainer: Option<JoinHandle<()>>,
}

/// What a remembering job shares with its drain thread.
struct Shared {
    /// A copy of the job's handle, for the thread to tell whether the job holds a process the port reported.
    job: OwnedHandle,
    /// The completion port the job posts its messages to.
    port: OwnedHandle,
    /// Set by the job's drop: the drain thread ends at its next look at the port.
    closing: AtomicBool,
    remembered: Mutex<Remembered>,
}

/// The members the drain thread has found, and what keeps the memory from being whole.
#[derive(Default)]
struct Remembered {
    /// Each process the port has reported joining the job and not yet forgotten, in the order reported, each once.
    members: Vec<Member>,
    /// Why the drain thread stopped, once it has on a failure: nothing that joins the job is remembered after it.
    stopped: Option<String>,
    /// The members dropped from the memory, or never remembered, because their times could not be read.
    unread: Unread,
}

/// The members a memory could not read: how many, and a line for each of the first [`UNREAD_NAMED`].
#[derive(Debug, Default, PartialEq, Eq)]
struct Unread {
    count: usize,
    named: Vec<String>,
}

/// A process the port reported joining the job, and the stretch of time in which it can have started a process.
#[derive(Debug, PartialEq, Eq)]
struct Member<H = OwnedHandle> {
    pid: u32,
    /// Its start, read while its process object lived.
    start: Timestamp,
    /// The later of its start and the job's creation: a process put in the job from outside it was no member before.
    from: Timestamp,
    until: Until<H>,
}

/// Until when a member can have started a process.
#[derive(Debug, PartialEq, Eq)]
enum Until<H> {
    /// It still runs: its handle is kept open, so that no other process can take its pid, until it has exited.
    Running(H),
    /// Its exit, read from its process.
    Ended(Timestamp),
}

/// A message taken from a job's completion port.
#[derive(Debug, PartialEq, Eq)]
enum Message {
    /// The process of this id joined the job.
    NewProcess(u32),
    /// The process of this id, in the job, exited.
    Exited(u32),
    /// Another of the job's messages.
    Other,
    /// The job's drop asks the drain thread to end.
    Close,
    /// No message came within the wait.
    Empty,
}

/// What the drain thread finds at the pid of a process the port reported joining the job.
#[derive(Debug, PartialEq, Eq)]
enum Opened<H> {
    /// The process itself, in the job, running or exited while its process object lives.
    Held(H),
    /// A process outside the job, which has taken the pid since the member ended.
    Holder(H),
    /// A process holds the pid, but whether it is the member cannot be told.
    Unknown,
    /// No process holds the pid.
    Gone,
}

/// Why the drain thread remembers a process the port reported joining the job as no member: it takes no one.
#[derive(Debug, PartialEq, Eq)]
enum Missed {
    /// It had ended and its process object was gone, or a process outside the job held its pid, when the drain thread
    /// opened it: its start is unknown, and without it no stretch of time bounds what it started.
    Gone,
    /// A process holds its pid, but whether it is the member cannot be told.
    Unknown,
    /// Its start or its exit could not be read, for this reason.
    Unread(String),
}

/// What a member's process times say of its end.
#[derive(Debug, PartialEq, Eq)]
enum Settled {
    Running,
    Ended(Timestamp),
    /// The times could not be read, for this reason.
    Unread(String),
}

// SAFETY: a job handle, a completion port handle and a process handle are kernel object references with no thread
// affinity: every call made through them (`QueryInformationJobObject`, `AssignProcessToJobObject`,
// `TerminateJobObject`, `GetQueuedCompletionStatus`, `PostQueuedCompletionStatus`, `IsProcessInJob`,
// `GetProcessTimes`) may come from any thread, and the kernel serialises them on the object. Each handle is closed
// once, when the one owner holding it drops or forgets the member it belongs to, so moving the owner to another thread
// cannot close it twice.
unsafe impl Send for Job {}

// SAFETY: every `&self` method only passes the handles to the thread-safe calls above, reads the name and the creation
// time, which are never changed after creation, or reaches the remembered members through their mutex, under which
// alone a member's handle is closed; so sharing one between threads is sound.
unsafe impl Sync for Job {}

// SAFETY: as for `Job`: the job copy and the port are only passed to the thread-safe calls above and closed once, when
// the last `Arc` holding them drops; the members, with their handles, are reached only through the mutex.
unsafe impl Send for Shared {}

// SAFETY: as for `Send`; no `&Shared` closes a handle but under the mutex.
unsafe impl Sync for Shared {}

impl Drop for Job {
    fn drop(&mut self) {
        let Some(memory) = &mut self.memory else {
            return;
        };
        // The job posts nothing more; a failure leaves the association to end when the port's last handle closes.
        let _ = associate(&self.handle, std::ptr::null_mut());
        memory.shared.closing.store(true, Ordering::Release);
        // Wakes the thread at once; without the message, it sees `closing` when its current wait ends.
        // SAFETY: the port is open for the whole call, and no `OVERLAPPED` is passed.
        let _ = unsafe { PostQueuedCompletionStatus(memory.shared.port.raw(), 0, CLOSE_KEY, std::ptr::null()) };
        if let Some(drainer) = memory.drainer.take() {
            // A drain thread that panicked has ended too.
            let _ = drainer.join();
        }
    }
}

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
        let created_at = now()?;
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
            created_at,
            memory: None,
        })
    }

    /// Has the job remember every process that joins it from now on, through a completion port associated with it now,
    /// before any process is put in it, and a thread that takes each of the port's messages as it comes; both stay until
    /// the job is dropped. A job that remembers already is left as it is.
    ///
    /// # Errors
    ///
    /// The system's error when the port cannot be created or associated with the job, the job's handle cannot be
    /// copied for the thread, or the thread cannot be started.
    pub fn remember(&mut self) -> io::Result<()> {
        if self.memory.is_some() {
            return Ok(());
        }
        // SAFETY: an invalid file handle with no existing port asks for a new port, which takes no pointers. A success is
        // a fresh handle nothing else owns.
        let port = unsafe { own_handle(CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1)) }?;
        let shared = Arc::new(Shared {
            job: duplicate(&self.handle)?,
            port,
            closing: AtomicBool::new(false),
            remembered: Mutex::new(Remembered::default()),
        });
        associate(&self.handle, shared.port.raw())?;
        let drained = Arc::clone(&shared);
        let created_at = self.created_at;
        let spawned = std::thread::Builder::new()
            .name("job-memory".to_owned())
            .spawn(move || drained.drain(created_at));
        let drainer = match spawned {
            Ok(drainer) => drainer,
            Err(error) => {
                // A failure leaves the association to end when the port's last handle closes, at this return.
                let _ = associate(&self.handle, std::ptr::null_mut());
                return Err(error);
            }
        };
        self.memory = Some(Memory {
            shared,
            drainer: Some(drainer),
        });
        Ok(())
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
        // SAFETY: both handles are open for the whole call; the process handle `CreateProcessW` or
        // `CreateProcessAsUserW` returned has the `PROCESS_SET_QUOTA` and `PROCESS_TERMINATE` access the call asks for.
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

    /// Each remembered member that has ended, in the order the port reported them, with why the memory may lack a
    /// member: its drain thread stopped on a failure or a panic, or a member's times could not be read. A member found
    /// to have exited has its exit kept and its handle closed. Empty for a job not told to [`Job::remember`].
    ///
    /// A member that had ended and whose process object was gone, or whose pid a process outside the job held, when the
    /// drain thread opened it is not remembered, and not said: its start is unknown, so no stretch of time bounds what
    /// it started, and a process under its pid may be a stranger's. The system does not guarantee it posts every
    /// message either ("messages are intended only as notifications and their delivery to the completion port is not
    /// guaranteed", `JOBOBJECT_ASSOCIATE_COMPLETION_PORT`), so a process the job held may be missing from what it
    /// remembers.
    #[must_use]
    pub fn departed(&self) -> Departures {
        let Some(memory) = &self.memory else {
            return (Vec::new(), None);
        };
        let finished = memory.drainer.as_ref().is_some_and(JoinHandle::is_finished);
        let mut remembered = memory.shared.lock();
        remembered.settle();
        let ended = remembered.members.iter().filter_map(Member::departed).collect();
        (ended, trouble(remembered.stopped.as_deref(), finished, &remembered.unread))
    }

    /// Forgets each remembered member named in `gone` by its pid, its start and its exit: a look has taken what it
    /// started, and a process that has ended starts nothing more. Nothing of a member forgotten is kept.
    pub fn forget(&self, gone: &[(u32, Timestamp, Timestamp)]) {
        if let Some(memory) = &self.memory {
            memory
                .shared
                .lock()
                .members
                .retain(|member| !member.last().is_some_and(|last| gone.contains(&(member.pid, member.start, last))));
        }
    }
}

impl Shared {
    /// The remembered members, for one change or one read.
    fn lock(&self) -> MutexGuard<'_, Remembered> {
        // A panic under the lock leaves no member half added or half removed: each change is one call.
        self.remembered.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The drain thread: remembers each process the port reports joining the job, the job having been created at
    /// `created_at`, until the job's drop asks it to end; a failure is kept for [`Job::departed`] to say. A member's exit
    /// message, and each wait that finds no message, keep the exits of the members that have ended, closing their
    /// handles.
    fn drain(&self, created_at: Timestamp) {
        let take = |wait| {
            if self.closing.load(Ordering::Acquire) {
                Ok(Message::Close)
            } else {
                take_message(&self.port, wait)
            }
        };
        let found = |pid| match member(&self.job, pid, created_at) {
            Ok(member) => self.lock().add(member),
            Err(missed) => self.lock().missed(pid, missed),
        };
        let exited = |pid| self.lock().exited(pid);
        if let Err(error) = drain_loop(take, found, exited, || self.lock().settle()) {
            self.lock().stopped = Some(error.to_string());
        }
    }
}

impl Remembered {
    /// Adds `member`, unless the same member, by its pid and start, is remembered already.
    fn add(&mut self, member: Member) {
        if !self.members.iter().any(|known| known.key() == member.key()) {
            self.members.push(member);
        }
    }

    /// Notes the process `pid` the port reported joining the job, which is remembered as no member for `missed`: one
    /// whose times could not be read is said in [`Remembered::unread`]; one found gone, or whose holder cannot be told,
    /// is not.
    fn missed(&mut self, pid: u32, missed: Missed) {
        if let Missed::Unread(why) = missed {
            self.unread.add(format!("the times of member {pid} could not be read: {why}"));
        }
    }

    /// Keeps the exit of each running member that has exited, closing its handle; one whose exit cannot be read is
    /// dropped, and said in [`Remembered::unread`].
    fn settle(&mut self) {
        self.settle_where(|_| true);
    }

    /// Keeps the exit of the running member `pid`, which the port reported exiting, closing its handle, as
    /// [`Remembered::settle`] does; no other member is touched.
    fn exited(&mut self, pid: u32) {
        self.settle_where(|member| member.pid == pid);
    }

    /// Settles each running member `chosen` picks, as [`Remembered::settle`] says.
    fn settle_where(&mut self, chosen: impl Fn(&Member) -> bool) {
        let unread = &mut self.unread;
        self.members.retain_mut(|member| {
            if !chosen(member) {
                return true;
            }
            let Until::Running(process) = &member.until else {
                return true;
            };
            // SAFETY: the handle has `PROCESS_QUERY_LIMITED_INFORMATION` access and stays open for the whole call.
            match settled(unsafe { start_and_exit_of(process.raw()) }) {
                Settled::Running => true,
                Settled::Ended(exit) => {
                    member.until = Until::Ended(exit);
                    true
                }
                Settled::Unread(why) => {
                    unread.add(format!("the exit of member {} could not be read: {why}", member.pid));
                    false
                }
            }
        });
    }
}

impl Unread {
    /// Counts one more member that could not be read, keeping its `line` while fewer than [`UNREAD_NAMED`] are kept.
    fn add(&mut self, line: String) {
        self.count += 1;
        if self.named.len() < UNREAD_NAMED {
            self.named.push(line);
        }
    }
}

impl<H> Member<H> {
    /// Its last moment, once it has ended; `None` while it runs.
    fn last(&self) -> Option<Timestamp> {
        match &self.until {
            Until::Running(_) => None,
            Until::Ended(last) => Some(*last),
        }
    }

    /// Its identity: its pid and its start.
    fn key(&self) -> (u32, Timestamp) {
        (self.pid, self.start)
    }

    /// It as a member that has ended; `None` while it runs.
    fn departed(&self) -> Option<Departure> {
        Some(Departure {
            pid: self.pid,
            start: self.start,
            from: self.from,
            until: self.last()?,
        })
    }
}

/// The drain thread's loop: takes each message ([`take_message`], waiting at most [`DRAIN_WAIT_MS`]), hands each
/// process reported joining the job to `found` and each reported exiting to `exited`, and runs `idle` after each wait
/// that found no message. Returns when a message asks it to close.
fn drain_loop(
    mut take: impl FnMut(u32) -> io::Result<Message>,
    mut found: impl FnMut(u32),
    mut exited: impl FnMut(u32),
    mut idle: impl FnMut(),
) -> io::Result<()> {
    loop {
        match take(DRAIN_WAIT_MS)? {
            Message::Close => return Ok(()),
            Message::Empty => idle(),
            Message::NewProcess(pid) => found(pid),
            Message::Exited(pid) => exited(pid),
            Message::Other => {}
        }
    }
}

/// The next message on `port`, waiting at most `wait` milliseconds for one.
fn take_message(port: &OwnedHandle, wait: u32) -> io::Result<Message> {
    let mut message = 0_u32;
    let mut key = 0_usize;
    let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
    // SAFETY: the port is open for the whole call, and every pointer is to a writable local.
    let taken = unsafe { GetQueuedCompletionStatus(port.raw(), &raw mut message, &raw mut key, &raw mut overlapped, wait) };
    if taken == 0 {
        // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
        let error = unsafe { GetLastError() };
        // No message taken and a timeout: the port is empty.
        if overlapped.is_null() && error == WAIT_TIMEOUT {
            return Ok(Message::Empty);
        }
        return Err(io::Error::from_raw_os_error(error.cast_signed()));
    }
    if key == CLOSE_KEY {
        return Ok(Message::Close);
    }
    if ![
        JOB_OBJECT_MSG_NEW_PROCESS,
        JOB_OBJECT_MSG_EXIT_PROCESS,
        JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS,
    ]
    .contains(&message)
    {
        return Ok(Message::Other);
    }
    // A job's message carries the process id where an I/O completion carries its `OVERLAPPED`.
    let pid = u32::try_from(overlapped.addr()).map_err(io::Error::other)?;
    Ok(if message == JOB_OBJECT_MSG_NEW_PROCESS {
        Message::NewProcess(pid)
    } else {
        Message::Exited(pid)
    })
}

/// The process `pid` the port reported joining the job behind `job`, which was created at `created_at`, as
/// [`remembered_as`] remembers it.
fn member(job: &OwnedHandle, pid: u32, created_at: Timestamp) -> Result<Member, Missed> {
    // SAFETY: the handle passed is the one `open_any` opened with `PROCESS_QUERY_LIMITED_INFORMATION` access, borrowed,
    // so open, for the whole call.
    let times = |process: &OwnedHandle| unsafe { start_and_exit_of(process.raw()) };
    remembered_as(pid, kept(open_any(pid), |process| holds(job, process)), times, created_at)
}

/// The process `pid` the port reported joining the job created at `created_at`, `opened` at its pid: while the job
/// holds it, a member from the later of its start and the job's creation until its exit, as its `times` say them, its
/// handle kept while it runs; else, or when its times cannot be read, why it is no member. A member found gone takes no
/// one: its start is unknown, and a window guessed for it could hold a process a stranger that held its pid started.
fn remembered_as<H>(
    pid: u32,
    opened: Opened<H>,
    times: impl FnOnce(&H) -> io::Result<(u64, u64)>,
    created_at: Timestamp,
) -> Result<Member<H>, Missed> {
    let process = match opened {
        Opened::Held(process) => process,
        Opened::Holder(_) | Opened::Gone => return Err(Missed::Gone),
        Opened::Unknown => return Err(Missed::Unknown),
    };
    let times = times(&process);
    let start = start_of(&times)?;
    let until = match settled(times) {
        Settled::Running => Until::Running(process),
        Settled::Ended(exit) => Until::Ended(exit),
        Settled::Unread(why) => return Err(Missed::Unread(why)),
    };
    Ok(Member {
        pid,
        start,
        from: start.max(created_at),
        until,
    })
}

/// The start a process's `times`, its start and its exit as FILETIME ticks, give.
fn start_of(times: &io::Result<(u64, u64)>) -> Result<Timestamp, Missed> {
    match times {
        Ok((start, _)) => timestamp_of(*start).ok_or_else(|| Missed::Unread("the start time is out of range".to_owned())),
        Err(error) => Err(Missed::Unread(error.to_string())),
    }
}

/// What the drain thread finds of a process the port reported joining the job, `opened` at its pid: the process,
/// while the job `holds` it; the process that took its pid, while the job does not; [`Opened::Unknown`] when the pid
/// could not be opened, or whether the job holds what holds it cannot be told; [`Opened::Gone`] when no process holds
/// it.
fn kept<H>(opened: io::Result<Option<H>>, holds: impl FnOnce(&H) -> io::Result<bool>) -> Opened<H> {
    let process = match opened {
        Ok(Some(process)) => process,
        Ok(None) => return Opened::Gone,
        Err(_) => return Opened::Unknown,
    };
    match holds(&process) {
        Ok(true) => Opened::Held(process),
        Ok(false) => Opened::Holder(process),
        Err(_) => Opened::Unknown,
    }
}

/// What a process's `times`, its start and its exit as FILETIME ticks, say of its end.
fn settled(times: io::Result<(u64, u64)>) -> Settled {
    match times {
        // A process that still runs has no exit time: its FILETIME is zero.
        Ok((_, 0)) => Settled::Running,
        Ok((_, exit)) => timestamp_of(exit).map_or_else(|| Settled::Unread("the exit time is out of range".to_owned()), Settled::Ended),
        Err(error) => Settled::Unread(error.to_string()),
    }
}

/// Why a memory may lack a member: its drain thread `stopped` on a failure, or ended on a panic, `finished` while the
/// job is still in use, and the members whose times could not be read, `unread`, the first few named and the rest
/// counted; `None` when nothing is missing.
fn trouble(stopped: Option<&str>, finished: bool, unread: &Unread) -> Option<String> {
    let stop = stopped
        .map(|why| format!("its drain thread stopped: {why}"))
        .or_else(|| finished.then(|| "its drain thread ended on a panic".to_owned()));
    let more = unread.count.saturating_sub(unread.named.len());
    let rest = (more > 0).then(|| format!("{more} more member(s) could not be read"));
    let all: Vec<String> = stop.into_iter().chain(unread.named.iter().cloned()).chain(rest).collect();
    (!all.is_empty()).then(|| all.join("; "))
}

/// The process `pid`, running or exited while its process object lives, opened with `PROCESS_QUERY_LIMITED_INFORMATION`
/// access; `None` when no process has that pid.
fn open_any(pid: u32) -> io::Result<Option<OwnedHandle>> {
    // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
    match unsafe { own_handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)) } {
        Ok(process) => Ok(Some(process)),
        // `OpenProcess` reports a pid no process has as `ERROR_INVALID_PARAMETER`.
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER.cast_signed()) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Now, as the process table gives start times.
fn now() -> io::Result<Timestamp> {
    i64::try_from(file_time_now())
        .ok()
        .and_then(filetime_to_timestamp)
        .ok_or_else(|| io::Error::other("the system time is outside the range a timestamp holds"))
}

/// A copy of `handle`, with the same access, that this process alone owns.
fn duplicate(handle: &OwnedHandle) -> io::Result<OwnedHandle> {
    let mut copy: HANDLE = std::ptr::null_mut();
    // SAFETY: both process handles are this process's pseudo-handle, which is always open; `handle` stays open for the
    // whole call, and `copy` is writable. The copy is not inheritable.
    let copied = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle.raw(),
            GetCurrentProcess(),
            &raw mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if copied == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a success is a fresh handle nothing else owns.
    unsafe { own_handle(copy) }
}

/// Associates the completion port `port` with the job behind `job`, which posts every message of the job to it from
/// now on; a null `port` ends the job's association. The system's error when it cannot be set.
fn associate(job: &OwnedHandle, port: HANDLE) -> io::Result<()> {
    let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
        CompletionKey: std::ptr::null_mut(),
        CompletionPort: port,
    };
    // SAFETY: the job's handle is open for the whole call, with the `JOB_OBJECT_SET_ATTRIBUTES` access a new job's handle
    // has, and `port` is the caller's open port or null; `association` is a whole structure of the size passed, read
    // only.
    let set = unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectAssociateCompletionPortInformation,
            (&raw const association).cast(),
            struct_size::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>(),
        )
    };
    if set == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
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

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot start its member must fail loudly")]
mod tests;
