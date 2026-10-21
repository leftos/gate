//! What a command's job remembers of each process that joined it, for the run's release to say what it ran, when it ran
//! and the CPU it used: its command line and program, read as it joins while it lives, its times read again as it
//! ends, and, once it has ended, kept as plain data only when it is worth naming ([`Book`]).

use std::collections::BTreeMap;
use std::io;
use std::os::windows::io::BorrowedHandle;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, MutexGuard, PoisonError};

use proc_table::win32::OwnedHandle;
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows_sys::Win32::Foundation::{FILETIME, UNICODE_STRING};
use windows_sys::Win32::System::Threading::GetProcessTimes;

use crate::scrub::{journal_image, journal_text};

/// How many times the command line is asked for before a line that keeps outgrowing its buffer is an error.
const LINE_ATTEMPTS: usize = 4;
/// The bytes the first ask for the command line offers: a line of up to about 250 characters fits.
const FIRST_LINE_BYTES: usize = 512;
/// The most ended members a job's [`Book`] keeps.
pub(crate) const BOOK_CAP: usize = 256;
/// The CPU, in 100 ns ticks, from which a process is worth naming: 1 s.
const CPU_FLOOR: u64 = 10_000_000;
/// The life, in 100 ns ticks, from which a process is worth naming: 10 s.
const LIFE_FLOOR: u64 = 100_000_000;

/// When a process was created and exited, and the CPU it has used, as `GetProcessTimes` gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessSpan {
    /// When it was created, in 100 ns ticks since 1601 as a `FILETIME` counts them.
    pub created: u64,
    /// When it exited, in the same ticks; `None` while it runs.
    pub exited: Option<u64>,
    /// Its user and kernel time, in 100 ns ticks.
    pub cpu: u64,
}

/// One process of a command's job, as its release can name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TracedMember {
    /// Its command line as the journal keeps it: redacted, its control and direction characters spaces, and cut to
    /// 200 characters. The text is the process's own, which it can rewrite, so it names what the
    /// process says it runs. Its program's file name when the line cannot be read or is empty, `pid <pid>` when neither
    /// can be.
    pub cmd: String,
    /// The full path of its program as the system gives it (`QueryFullProcessImageNameW`), its control and direction
    /// characters spaces and cut to its last 1,024 characters; `None` when it cannot be read.
    pub image: Option<String>,
    /// Its times.
    pub span: ProcessSpan,
}

/// What names a member, as the naming thread reads it after the member has joined: nothing until it has, and its
/// image before its command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Named {
    pid: u32,
    /// Its command line as [`TracedMember::cmd`] keeps it; `None` until read, or when it cannot be read or is empty.
    cmd: Option<String>,
    /// Its program's path as [`TracedMember::image`] keeps it; `None` until read, or when it cannot be read.
    image: Option<String>,
}

impl Named {
    /// The process `pid`, not named yet.
    pub(crate) fn unnamed(pid: u32) -> Named {
        Named { pid, cmd: None, image: None }
    }

    /// It with its process's times: its command line, else its image's file name, else `pid <pid>`.
    pub(crate) fn with(&self, span: ProcessSpan) -> TracedMember {
        let file_name = || {
            let name = Path::new(self.image.as_deref()?).file_name()?;
            Some(journal_text("", &name.to_string_lossy()))
        };
        TracedMember {
            cmd: self.cmd.clone().or_else(file_name).unwrap_or_else(|| format!("pid {}", self.pid)),
            image: self.image.clone(),
            span,
        }
    }
}

/// A member for the naming thread to name ([`name_each`]): its key in [`Traces`], and a copy of the handle its job's
/// memory opened it with.
pub(crate) struct Naming<K> {
    pub(crate) key: K,
    pub(crate) process: OwnedHandle,
}

// SAFETY: a process handle is a kernel object reference with no thread affinity: the calls made through it
// (`QueryFullProcessImageNameW`, `NtQueryInformationProcess`) may come from any thread. The one owner closes it once.
unsafe impl<K: Send> Send for Naming<K> {}

/// What a job's memory keeps of its members for their release, shared with the naming thread, which holds no handle
/// of the job's: the names of the running members, the book of those that have ended, and whether the job has been
/// dropped.
#[derive(Debug)]
pub(crate) struct Traces<K> {
    running: BTreeMap<K, Named>,
    book: Book<K>,
    closed: bool,
}

impl<K> Default for Traces<K> {
    fn default() -> Self {
        Traces {
            running: BTreeMap::new(),
            book: Book::default(),
            closed: false,
        }
    }
}

impl<K: Ord + Copy> Traces<K> {
    /// Notes the running member `key`, the process `pid`, to be named.
    pub(crate) fn joined(&mut self, key: K, pid: u32) {
        self.running.insert(key, Named::unnamed(pid));
    }

    /// Notes the member `key`, the process `pid`, which had ended with `span` as it joined, in the book; whether the
    /// book keeps it, and so whether it is to be named.
    pub(crate) fn joined_ended(&mut self, key: K, pid: u32, span: ProcessSpan) -> bool {
        self.book.keep(key, Named::unnamed(pid), span)
    }

    /// Moves the running member `key`, which has ended, to the book with its last times `span`; with none read, it is
    /// dropped.
    pub(crate) fn ended(&mut self, key: K, span: Option<ProcessSpan>) {
        if let (Some(named), Some(span)) = (self.running.remove(&key), span) {
            self.book.keep(key, named, span);
        }
    }

    /// Drops the running member `key`, whose end could not be read.
    pub(crate) fn forget(&mut self, key: K) {
        self.running.remove(&key);
    }

    /// What names the running member `key`.
    pub(crate) fn running(&self, key: &K) -> Option<&Named> {
        self.running.get(key)
    }

    /// The ended members the book keeps.
    pub(crate) fn ended_members(&self) -> impl Iterator<Item = TracedMember> + '_ {
        self.book.ended.iter().map(|ended| ended.named.with(ended.span))
    }

    /// Tells the naming thread the job has been dropped: it names nothing more.
    pub(crate) fn close(&mut self) {
        self.closed = true;
    }

    /// Changes what names the member `key` by `change`, running or in the book; whether it is still kept.
    fn name(&mut self, key: K, change: impl FnOnce(&mut Named)) -> bool {
        if let Some(named) = self.running.get_mut(&key) {
            change(named);
            return true;
        }
        match self.book.ended.iter_mut().find(|ended| ended.key == key) {
            Some(ended) => {
                change(&mut ended.named);
                true
            }
            None => false,
        }
    }
}

/// The traces, for one change or one read.
pub(crate) fn lock<K>(traces: &Mutex<Traces<K>>) -> MutexGuard<'_, Traces<K>> {
    // A panic under the lock leaves no member half named: each change is one call.
    traces.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The naming thread: names each member `joins` hands it in `traces`, its image path first, then its command line, read
/// from the process and made fit for the journal at once, so the whole line is never kept. The program's stem for the
/// redaction is the image's, else argv\[0\]'s. It reads nothing under the lock, and names nothing more once the job
/// is dropped; it ends when `joins` closes, dropping the rest unread.
pub(crate) fn name_each<K: Ord + Copy>(traces: &Mutex<Traces<K>>, joins: &Receiver<Naming<K>>) {
    for Naming { key, process } in joins {
        if lock(traces).closed {
            return;
        }
        // SAFETY: the handle is open for the whole borrow, which ends with this call.
        let path = crate::image_path_of(unsafe { BorrowedHandle::borrow_raw(process.raw()) }).ok();
        let image = path.as_deref().map(|path| journal_image(&path.to_string_lossy()));
        let program = path
            .as_deref()
            .and_then(Path::file_stem)
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !lock(traces).name(key, |named| named.image = image) {
            continue;
        }
        let cmd = command_line(&process)
            .ok()
            .filter(|line| !line.trim().is_empty())
            .map(|line| journal_text(&program, &line));
        lock(traces).name(key, |named| named.cmd = cmd);
    }
}

/// An ended member the book keeps.
#[derive(Debug)]
struct Ended<K> {
    key: K,
    named: Named,
    span: ProcessSpan,
}

/// The members of a job that have ended and are worth naming, as plain data: at most [`BOOK_CAP`], those with the most
/// CPU.
#[derive(Debug)]
struct Book<K> {
    ended: Vec<Ended<K>>,
}

impl<K> Default for Book<K> {
    fn default() -> Self {
        Book { ended: Vec::new() }
    }
}

impl<K> Book<K> {
    /// Keeps the member `key`, named by `named`, which has ended with `span`, when it is worth naming
    /// ([`worth_naming`]), and drops it at once when it is not; once [`BOOK_CAP`] are kept, the one with the least CPU,
    /// it or one kept, is dropped. Whether it is kept.
    fn keep(&mut self, key: K, named: Named, span: ProcessSpan) -> bool {
        if !worth_naming(&span, span.created) {
            return false;
        }
        let member = Ended { key, named, span };
        if self.ended.len() < BOOK_CAP {
            self.ended.push(member);
            return true;
        }
        let least = self.ended.iter_mut().min_by_key(|kept| kept.span.cpu);
        match least.filter(|least| least.span.cpu < member.span.cpu) {
            Some(least) => {
                *least = member;
                true
            }
            None => false,
        }
    }
}

/// Whether the process of `span` is worth naming: it used at least 1 CPU-second, or lived at least 10 s up to its exit,
/// or to `now` (`FILETIME` ticks) while it runs.
#[must_use]
pub fn worth_naming(span: &ProcessSpan, now: u64) -> bool {
    let life = span.exited.unwrap_or(now).saturating_sub(span.created);
    span.cpu >= CPU_FLOOR || life >= LIFE_FLOOR
}

/// When the process behind `process`, open with query access, was created and exited and the CPU it has used,
/// readable after it has exited for as long as the handle is open.
///
/// # Errors
///
/// The system's error when the times cannot be read.
pub(crate) fn span_of(process: &OwnedHandle) -> io::Result<ProcessSpan> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the handle is open for the whole call with query access, and every pointer is to a writable FILETIME.
    let read = unsafe { GetProcessTimes(process.raw(), &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    // A process that still runs has no exit time: its FILETIME is zero.
    let exited = Some(ticks(exited)).filter(|&exited| exited != 0);
    Ok(ProcessSpan {
        created: ticks(created),
        exited,
        cpu: ticks(kernel) + ticks(user),
    })
}

/// The command line the process behind `process` was started with (`NtQueryInformationProcess` with
/// `ProcessCommandLineInformation`), which `PROCESS_QUERY_LIMITED_INFORMATION` access alone reads.
///
/// # Errors
///
/// An error naming the `NTSTATUS` when the line cannot be read, or saying the system's answer did not lie inside the
/// buffer it was given.
fn command_line(process: &OwnedHandle) -> io::Result<String> {
    let mut bytes = FIRST_LINE_BYTES;
    for _ in 0..LINE_ATTEMPTS {
        // `u64`s, so the buffer is aligned as the `UNICODE_STRING` the call writes at its start is.
        let mut buffer = vec![0_u64; bytes.div_ceil(size_of::<u64>())];
        let length = u32::try_from(size_of_val(buffer.as_slice())).map_err(io::Error::other)?;
        let mut needed = 0_u32;
        // SAFETY: the handle is open for the whole call with the query access the call asks for; `buffer` is writable
        // for `length` bytes, and `needed` is a writable u32; all outlive the call.
        let status = unsafe {
            NtQueryInformationProcess(
                process.raw(),
                ProcessCommandLineInformation,
                buffer.as_mut_ptr().cast(),
                length,
                &raw mut needed,
            )
        };
        if status < 0 && needed > length {
            bytes = usize::try_from(needed).map_err(io::Error::other)?;
            continue;
        }
        if status < 0 {
            return Err(io::Error::other(format!(
                "NtQueryInformationProcess(ProcessCommandLineInformation) failed with NTSTATUS 0x{:08x}",
                status.cast_unsigned()
            )));
        }
        return line_in(&buffer);
    }
    Err(io::Error::other(format!(
        "the command line kept outgrowing its buffer after {LINE_ATTEMPTS} attempts"
    )))
}

/// The command line `buffer` holds: the `UNICODE_STRING` at its start, whose characters must lie inside it.
fn line_in(buffer: &[u64]) -> io::Result<String> {
    // SAFETY: the buffer is aligned for a `UNICODE_STRING` and longer than one, and the call wrote one at its start.
    let string = unsafe { &*buffer.as_ptr().cast::<UNICODE_STRING>() };
    let units = usize::from(string.Length) / size_of::<u16>();
    if units == 0 {
        return Ok(String::new());
    }
    let start = buffer.as_ptr() as usize;
    let end = start + size_of_val(buffer);
    let text = string.Buffer as usize;
    if text < start + size_of::<UNICODE_STRING>() || text + units * size_of::<u16>() > end {
        return Err(io::Error::other("the command line the system gave lies outside its buffer"));
    }
    // SAFETY: the characters were just checked to lie inside `buffer`, which outlives the slice and is aligned for
    // `u16`s, as the system wrote them.
    let characters = unsafe { std::slice::from_raw_parts(string.Buffer, units) };
    Ok(String::from_utf16_lossy(characters))
}

/// A `FILETIME`'s two halves as one count of 100 ns ticks.
fn ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose child cannot be started or read must fail loudly")]
mod tests {
    use std::io::Write;
    use std::process::{Child, Command, Stdio};

    use proc_table::win32::OwnedHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    use std::sync::{Mutex, mpsc};

    use super::{BOOK_CAP, Book, Named, Naming, ProcessSpan, Traces, lock, name_each, span_of};

    /// The word the child's command line carries, so the line read back is known to be its own.
    const MARKER: &str = "GATE_TRACED_MARKER=";
    /// A second in 100 ns ticks.
    const SECOND: u64 = 10_000_000;

    /// A `cmd.exe` that waits for a line on its standard input, then exits.
    fn waiting_child() -> Child {
        let shell = crate::system_directory().expect("the system folder can be read").join("cmd.exe");
        let mut command = Command::new(shell);
        command
            .args(["/d", "/c", "set", "/p", MARKER])
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        crate::spawn(&mut command).expect("cmd.exe can be started")
    }

    /// The process `pid`, opened with the access a job's memory opens its members with.
    fn opened(pid: u32) -> OwnedHandle {
        // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
        unsafe { crate::own_handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)) }.expect("the child can be opened")
    }

    /// Ends the waiting child by giving it its line, and waits for its exit.
    fn end(mut child: Child) {
        let mut stdin = child.stdin.take().expect("the child's input is piped");
        stdin.write_all(b"x\r\n").expect("the child takes its line");
        drop(stdin);
        child.wait().expect("the child can be waited for");
    }

    /// What names an ended member called `name`.
    fn named(name: &str) -> Named {
        Named {
            pid: 1,
            cmd: Some(name.to_owned()),
            image: None,
        }
    }

    /// The span of an ended member that started at 0 and lived `life` seconds, using `cpu` CPU-seconds.
    fn ended(life: u64, cpu: u64) -> ProcessSpan {
        ProcessSpan {
            created: 0,
            exited: Some(life * SECOND),
            cpu: cpu * SECOND,
        }
    }

    /// The commands of the members `book` keeps.
    fn kept(book: &Book<u32>) -> Vec<&str> {
        book.ended.iter().filter_map(|ended| ended.named.cmd.as_deref()).collect()
    }

    /// `traces` with each of `processes` handed to the naming thread's work, run here until its queue closes.
    fn name_all(traces: &Mutex<Traces<u32>>, processes: Vec<(u32, OwnedHandle)>) {
        let (joins, queue) = mpsc::sync_channel(processes.len());
        for (key, process) in processes {
            assert!(joins.try_send(Naming { key, process }).is_ok(), "the queue has room");
        }
        drop(joins);
        name_each(traces, &queue);
    }

    #[test]
    fn a_running_child_is_named_by_its_command_line_and_image() {
        let child = waiting_child();
        let process = opened(child.id());
        let traces = Mutex::new(Traces::default());
        lock(&traces).joined(7, child.id());

        name_all(&traces, vec![(7, opened(child.id()))]);
        let span = span_of(&process).expect("the running child's times can be read");
        let member = lock(&traces).running(&7).expect("the member is kept").with(span);
        end(child);

        assert!(member.cmd.contains(MARKER), "the line names the child's own words: {member:?}");
        let image = member.image.expect("the child's image can be read");
        assert!(image.to_ascii_lowercase().ends_with(r"\cmd.exe"), "{image}");
        assert!(std::path::Path::new(&image).is_absolute(), "{image}");
        assert!(span.created > 0, "{span:?}");
        assert_eq!(span.exited, None, "a running child has no exit time: {span:?}");
    }

    #[test]
    fn an_unnamed_member_is_called_by_its_image_s_file_name_else_by_its_pid() {
        let span = ended(1, 1);
        let mut named = Named::unnamed(42);
        assert_eq!(named.with(span).cmd, "pid 42", "nothing read");

        named.image = Some(r"C:\tools\builder.exe".to_owned());
        assert_eq!(named.with(span).cmd, "builder.exe", "the image read, the line not");
    }

    #[test]
    fn a_closed_job_s_members_are_left_unnamed() {
        let child = waiting_child();
        let pid = child.id();
        let traces = Mutex::new(Traces::default());
        lock(&traces).joined(7, pid);
        lock(&traces).close();

        name_all(&traces, vec![(7, opened(pid))]);
        end(child);

        assert_eq!(lock(&traces).running(&7), Some(&Named::unnamed(pid)));
    }

    #[test]
    fn a_child_s_times_stay_readable_through_the_kept_handle_after_it_exits() {
        let child = waiting_child();
        let process = opened(child.id());
        end(child);

        let span = span_of(&process).expect("the exited child's times can be read through the kept handle");
        let exited = span.exited.expect("an exited child has an exit time");

        assert!(exited >= span.created, "it exited after it was created: {span:?}");
    }

    #[test]
    fn an_ended_member_under_both_floors_is_dropped_at_once_and_one_over_either_kept() {
        let mut book = Book::default();

        assert!(!book.keep(1, named("quiet"), ended(9, 0)));
        assert!(book.keep(2, named("busy"), ended(2, 1)));
        assert!(book.keep(3, named("long"), ended(10, 0)));

        assert_eq!(kept(&book), ["busy", "long"]);
    }

    #[test]
    fn past_256_the_member_with_the_least_cpu_is_dropped() {
        let mut book = Book::default();

        for cpu in 1..=300 {
            book.keep(u32::try_from(cpu).expect("a small key"), named(&format!("tool {cpu}")), ended(1, cpu));
        }
        let newcomer = book.keep(301, named("least"), ended(1, 1));

        assert_eq!(book.ended.len(), BOOK_CAP);
        let least = book.ended.iter().map(|member| member.span.cpu).min().expect("the book keeps members");
        assert_eq!(least, (300 - 256 + 1) * SECOND, "the 256 with the most CPU are kept");
        assert!(!newcomer && !kept(&book).contains(&"least"), "a newcomer with the least CPU is dropped");
    }
}
