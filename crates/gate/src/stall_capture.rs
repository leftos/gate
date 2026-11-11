//! The stall capture: what a command the watchdog kills as STALLED was doing, read before its job is terminated and
//! saved beside the log, so the evidence of the hang outlives the kill. Each live process of the run's job is listed
//! with its threads, each thread's state, wait reason, time waited and Win32 start address; when one of them is
//! `prek.exe` and prek's log is this run's own, the end of that log is copied too. The capture only reads: it stops,
//! injects into and waits on nothing, and it is cut once its budget is spent.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use proc_table::system_list::{self, Entry, Thread};
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::console::Console;
use crate::run::is_within;

/// The milliseconds a stall capture may take before it is cut, so the kill it comes before is never held long.
pub(crate) const STALL_CAPTURE_BUDGET_MS: u128 = 2000;
/// The most processes a capture lists, and the most threads it lists of one process.
const PROCESS_LINES: usize = 40;
const THREAD_LINES: usize = 256;
/// The `KTHREAD_STATE` of a thread that waits, the only state whose wait reason and wait time are current.
const WAITING: u32 = 5;
/// The image whose log a capture copies, compared ignoring ASCII case, unless `--test-prek-image` names another.
pub(crate) const PREK_IMAGE: &str = "prek.exe";
/// The most bytes copied from the end of prek's log.
const PREK_LOG_BYTES: u64 = 1024 * 1024;
/// The most bytes read from the start of prek's log to find the git root it names.
const PREK_LOG_HEAD_BYTES: u64 = 64 * 1024;
/// The mark prek's log puts before the git root of the run that wrote it.
const PREK_ROOT_MARK: &str = " DEBUG Git root: ";
/// What a capture says when prek's log is another run's, and so is not copied.
const ANOTHER_RUN_LINE: &str = "gate: stall capture did not copy prek's log: it belongs to another prek run, not this run's folder";
/// What a capture says when prek's log names no git root, and so is not copied.
const NO_ROOT_LINE: &str = "gate: stall capture did not copy prek's log: it names no git root, so it cannot be tied to this run";

/// A stall capture gathered in memory: its text, one line each, the processes and threads it describes, and, when
/// the job held prek, this run's prek log or why it is not copied.
pub(crate) struct Capture {
    lines: Vec<String>,
    processes: usize,
    threads: usize,
    prek_log: Option<PrekLog>,
}

/// The prek a stall capture looks for: the run's working directory, which the log it copies must name or sit under,
/// and the image name the gate treats as prek.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PrekLook<'a> {
    /// The run's working directory.
    pub(crate) folder: &'a Path,
    /// The image name of prek, which the snapshot's entries are matched against, ignoring ASCII case.
    pub(crate) image: &'a str,
}

/// The prek log a stall capture found: this run's own, its tail bytes, or why nothing is copied.
#[derive(Debug, PartialEq, Eq)]
enum PrekLog {
    /// This run's log, its last [`PREK_LOG_BYTES`].
    Own(Vec<u8>),
    /// Another prek run's log.
    AnotherRun,
    /// A log that names no git root.
    NoRoot,
    /// A log that could not be read, and why.
    Unread(String),
}

/// Where a thread began, as a capture says it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Start {
    /// Inside a module of its process: the module's name and the offset into it.
    InModule(String, usize),
    /// At an address no module of its process holds, or one whose modules could not be listed.
    Address(usize),
    /// Unknown: the thread could not be opened, or its start address read.
    Unread,
}

/// One thread as a capture describes it.
#[derive(Clone, Debug)]
struct ThreadView {
    tid: usize,
    state: u32,
    wait_reason: u32,
    waited_ms: u64,
    start: Start,
}

/// One process as a capture describes it: its threads read, at most [`THREAD_LINES`], of `thread_count` in all.
#[derive(Clone, Debug)]
struct ProcessView {
    pid: usize,
    parent: usize,
    image: String,
    threads: Vec<ThreadView>,
    thread_count: usize,
    /// Why its modules could not be listed, which leaves its threads' starts unresolved; `None` when they were.
    modules_unread: Option<String>,
}

/// The capture file beside the log at `log`: `<log>.stall.txt`.
pub(crate) fn capture_path(log: &Path) -> PathBuf {
    beside(log, ".stall.txt")
}

/// The copy of prek's log beside the log at `log`: `<log>.stall.prek.log`.
pub(crate) fn prek_copy_path(log: &Path) -> PathBuf {
    beside(log, ".stall.prek.log")
}

/// The file whose name is the log at `log`'s whole name followed by `suffix`, as the PowerShell gate names its
/// `<log>.err`, so logs that share a stem never share a capture.
fn beside(log: &Path, suffix: &str) -> PathBuf {
    let mut name = log.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The stall capture of the job whose members `members` reads, killed after `quiet` without progress, gathered within
/// `budget_ms` milliseconds. Only a process both member reads, taken before and after the process list, name is
/// described: one that joined or left meanwhile, or whose id was reused, is left out. `look` names the run's working
/// directory, which prek's log must name, or sit under, to be this run's, and the image name it treats as prek.
///
/// # Errors
///
/// Why the capture could not be gathered: the job's members or the process list could not be read.
pub(crate) fn capture(members: &dyn Fn() -> io::Result<Vec<u32>>, quiet: Duration, budget_ms: u128, look: PrekLook<'_>) -> Result<Capture, String> {
    let started = Instant::now();
    let unread = |error: io::Error| format!("cannot read the job's processes: {error}");
    let before: HashSet<u32> = members().map_err(unread)?.into_iter().collect();
    let list = system_list::process_list().map_err(|error| error.to_string())?;
    let after = members().map_err(unread)?;
    let ids: HashSet<usize> = after
        .into_iter()
        .filter(|id| before.contains(id))
        .filter_map(|id| usize::try_from(id).ok())
        .collect();
    let entries = system_list::walk(system_list::as_bytes(&list), |entry| ids.contains(&entry.pid)).map_err(|error| error.to_string())?;
    let mut capture = Capture::new(header(Timestamp::now(), std::process::id(), quiet));
    let mut cut = capture.describe(&entries, view_of, started, budget_ms)?;
    if !cut && entries.iter().any(|entry| image_of(entry).eq_ignore_ascii_case(look.image)) {
        cut = elapsed_ms(started) >= budget_ms;
        if !cut {
            capture.prek_log = Some(prek_log_tail(look.folder));
        }
    }
    if cut {
        capture.lines.push(format!("capture cut at {} ms", elapsed_ms(started)));
    }
    Ok(capture)
}

/// Writes `captured` beside the log at `log` and says so: the file written, with what it describes, and prek's log
/// copied; or why the capture, a write or prek's log failed. A failure changes nothing of the kill.
pub(crate) fn save(captured: Result<Capture, String>, log: &Path, console: &mut Console) {
    let capture = match captured {
        Ok(capture) => capture,
        Err(why) => return console.err(&failed(&why)),
    };
    let path = capture_path(log);
    match fs::write(&path, capture.text()) {
        Ok(()) => console.err(&format!(
            "gate: stall capture written to {} ({} processes, {} threads)",
            path.display(),
            capture.processes,
            capture.threads
        )),
        Err(error) => console.err(&failed(&format!("cannot write {}: {error}", path.display()))),
    }
    match capture.prek_log {
        Some(PrekLog::Own(bytes)) => {
            let copy = prek_copy_path(log);
            match fs::write(&copy, bytes) {
                Ok(()) => console.err(&format!("gate: stall capture copied prek's log to {}", copy.display())),
                Err(error) => console.err(&failed(&format!("cannot write {}: {error}", copy.display()))),
            }
        }
        Some(PrekLog::AnotherRun) => console.err(ANOTHER_RUN_LINE),
        Some(PrekLog::NoRoot) => console.err(NO_ROOT_LINE),
        Some(PrekLog::Unread(why)) => console.err(&failed(&why)),
        None => {}
    }
}

/// The line that says a capture failed, and why.
fn failed(why: &str) -> String {
    format!("gate: stall capture failed: {why}")
}

impl Capture {
    /// A capture that holds only its `header` line.
    fn new(header: String) -> Capture {
        Capture {
            lines: vec![header],
            processes: 0,
            threads: 0,
            prek_log: None,
        }
    }

    /// The capture's text: its lines, each ended by a line feed.
    fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }

    /// Adds the processes of `items`, each described by `view`, up to [`PROCESS_LINES`] and then a line counting the
    /// rest, the budget checked before each; whether the budget cut it.
    fn describe<P>(
        &mut self,
        items: &[P],
        view: impl Fn(&P) -> Result<ProcessView, String>,
        started: Instant,
        budget_ms: u128,
    ) -> Result<bool, String> {
        for (index, item) in items.iter().enumerate() {
            if index == PROCESS_LINES {
                self.lines.push(format!("... and {} more", items.len() - index));
                break;
            }
            if elapsed_ms(started) >= budget_ms {
                return Ok(true);
            }
            self.add(&view(item)?);
        }
        Ok(false)
    }

    /// Adds the lines of `process`: its own, why its modules were unread when they were, then one per thread it read
    /// and a line counting the threads past the cap.
    fn add(&mut self, process: &ProcessView) {
        self.lines.push(process_line(process));
        if let Some(why) = &process.modules_unread {
            self.lines.push(format!("  modules unread: {why}"));
        }
        self.lines.extend(process.threads.iter().map(thread_line));
        let more = process.thread_count.saturating_sub(process.threads.len());
        if more > 0 {
            self.lines.push(format!("  ... and {more} more"));
        }
        self.processes += 1;
        self.threads += process.threads.len();
    }
}

/// The capture's first line: when it was taken, by which gate, after how long without progress.
fn header(now: Timestamp, gate_pid: u32, quiet: Duration) -> String {
    format!(
        "gate stall capture: {now}, gate pid {gate_pid}, quiet {} s",
        quiet.as_secs_f64().round_ties_even()
    )
}

/// The line that names `process`.
fn process_line(process: &ProcessView) -> String {
    format!("process {} parent {} {}", process.pid, process.parent, process.image)
}

/// The line that describes `thread`; its wait reason and time waited only while it waits, as they are stale otherwise.
fn thread_line(thread: &ThreadView) -> String {
    let start = match &thread.start {
        Start::InModule(name, offset) => format!("{name}+0x{offset:x}"),
        Start::Address(address) => format!("0x{address:x}"),
        Start::Unread => "?".to_owned(),
    };
    let state = state_name(thread.state).map_or_else(|| thread.state.to_string(), str::to_owned);
    let wait = if thread.state == WAITING {
        let reason = wait_reason_name(thread.wait_reason).map_or_else(|| thread.wait_reason.to_string(), str::to_owned);
        format!(" wait {reason} waited {} ms", thread.waited_ms)
    } else {
        String::new()
    };
    format!("  thread {} state {state}{wait} start {start}", thread.tid)
}

/// The process of the list's `entry` as a capture describes it, its threads' start addresses read now.
fn view_of(entry: &Entry<'_>) -> Result<ProcessView, String> {
    let threads = entry.threads().map_err(|error| error.to_string())?;
    let pid = u32::try_from(entry.pid).map_err(|_| format!("process id {} does not fit 32 bits", entry.pid))?;
    // A process whose modules cannot be listed still has its threads' addresses said, unresolved, and why.
    let (modules, modules_unread) = match gate_win32::process_modules(pid) {
        Ok(modules) => (modules, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    Ok(ProcessView {
        pid: entry.pid,
        parent: entry.parent_pid,
        image: image_of(entry),
        threads: capped(&threads, |thread| thread_view(pid, thread, &modules)),
        thread_count: threads.len(),
        modules_unread,
    })
}

/// The views of the first [`THREAD_LINES`] of `threads`, each made by `view`: the only place the thread cap applies.
fn capped<T>(threads: &[T], view: impl Fn(&T) -> ThreadView) -> Vec<ThreadView> {
    threads.iter().take(THREAD_LINES).map(view).collect()
}

/// The list's `thread` of process `pid` as a capture describes it, its start address read now and resolved against
/// `modules`.
fn thread_view(pid: u32, thread: &Thread, modules: &[gate_win32::Module]) -> ThreadView {
    let start = match u32::try_from(thread.tid).ok().map(|tid| gate_win32::thread_start_address(pid, tid)) {
        Some(Ok(address)) => gate_win32::resolve_address(modules, address)
            .map_or(Start::Address(address), |(module, offset)| Start::InModule(module.name.clone(), offset)),
        _ => Start::Unread,
    };
    ThreadView {
        tid: thread.tid,
        state: thread.state,
        wait_reason: thread.wait_reason,
        waited_ms: gate_win32::waited_ms(thread.wait_time),
        start,
    }
}

/// The image name of the list's `entry`.
fn image_of(entry: &Entry<'_>) -> String {
    let units: Vec<u16> = entry.name.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit)).collect();
    String::from_utf16_lossy(&units)
}

/// The last [`PREK_LOG_BYTES`] of `%LOCALAPPDATA%\prek\prek.log` when the log is this run's own, or why it is not this
/// run's to copy.
fn prek_log_tail(folder: &Path) -> PrekLog {
    match prek_log_read(folder) {
        Ok(bytes) => PrekLog::Own(bytes),
        Err(why) => why,
    }
}

/// The last [`PREK_LOG_BYTES`] of prek's log when it is this run's own, opened so prek can go on writing, renaming or
/// deleting it; or why it is not this run's to copy.
fn prek_log_read(folder: &Path) -> Result<Vec<u8>, PrekLog> {
    let local =
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| PrekLog::Unread("LOCALAPPDATA is not set, so prek's log cannot be found".to_owned()))?;
    let path = PathBuf::from(local).join("prek").join("prek.log");
    let unread = |error: io::Error| PrekLog::Unread(format!("cannot read prek's log {}: {error}", path.display()));
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&path)
        .map_err(unread)?;
    let first = file.metadata().map_err(unread)?.len();
    // The head names the run whose prek wrote the log; the tail is copied only when that run is this one.
    let mut head = Vec::new();
    file.by_ref().take(PREK_LOG_HEAD_BYTES).read_to_end(&mut head).map_err(unread)?;
    if let Some(other) = prek_log_owner(&String::from_utf8_lossy(&head), folder) {
        return Err(other);
    }
    file.seek(SeekFrom::Start(first.saturating_sub(PREK_LOG_BYTES))).map_err(unread)?;
    let mut bytes = Vec::new();
    file.by_ref().take(PREK_LOG_BYTES).read_to_end(&mut bytes).map_err(unread)?;
    // A log shorter than it was when this read began was truncated by a new prek run meanwhile, so it is that run's.
    if truncated(first, file.metadata().map_err(unread)?.len()) {
        return Err(PrekLog::AnotherRun);
    }
    Ok(bytes)
}

/// Whether a log whose length was `first` when a read began and `second` after it was truncated meanwhile: a new prek
/// run empties the log at its start, so the log belongs to that run, not the one being captured.
fn truncated(first: u64, second: u64) -> bool {
    second < first
}

/// Why the log whose first [`PREK_LOG_HEAD_BYTES`] are `head` is not this run's, or `None` when it is: the first line
/// holding the git root prek logs names a folder, and a folder that holds `folder` or lies inside it is this run's,
/// since a run started above the repo names a root below its own folder, as `git -C repo commit` from a parent does.
fn prek_log_owner(head: &str, folder: &Path) -> Option<PrekLog> {
    let Some(root) = head.lines().find_map(|line| line.split_once(PREK_ROOT_MARK).map(|(_, rest)| rest.trim())) else {
        return Some(PrekLog::NoRoot);
    };
    let root = root.strip_prefix("\\\\?\\").unwrap_or(root);
    if root.is_empty() {
        return Some(PrekLog::NoRoot);
    }
    let root = Path::new(root);
    if is_within(root, folder) || is_within(folder, root) {
        None
    } else {
        Some(PrekLog::AnotherRun)
    }
}

/// The milliseconds since `started`.
fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

/// The name of the `KTHREAD_STATE` value `state`, or `None` for a value this gate does not know.
fn state_name(state: u32) -> Option<&'static str> {
    Some(match state {
        0 => "Initialized",
        1 => "Ready",
        2 => "Running",
        3 => "Standby",
        4 => "Terminated",
        5 => "Waiting",
        6 => "Transition",
        7 => "DeferredReady",
        8 => "GateWaitObsolete",
        9 => "WaitingForProcessInSwap",
        _ => return None,
    })
}

/// The name of the `KWAIT_REASON` value `reason`, or `None` for a value this gate does not know.
fn wait_reason_name(reason: u32) -> Option<&'static str> {
    Some(match reason {
        0 => "Executive",
        1 => "FreePage",
        2 => "PageIn",
        3 => "PoolAllocation",
        4 => "DelayExecution",
        5 => "Suspended",
        6 => "UserRequest",
        7 => "WrExecutive",
        8 => "WrFreePage",
        9 => "WrPageIn",
        10 => "WrPoolAllocation",
        11 => "WrDelayExecution",
        12 => "WrSuspended",
        13 => "WrUserRequest",
        14 => "WrSpare0",
        15 => "WrQueue",
        16 => "WrLpcReceive",
        17 => "WrLpcReply",
        18 => "WrVirtualMemory",
        19 => "WrPageOut",
        20 => "WrRendezvous",
        21 => "WrKeyedEvent",
        22 => "WrTerminated",
        23 => "WrProcessInSwap",
        24 => "WrCpuRateControl",
        25 => "WrCalloutStack",
        26 => "WrKernel",
        27 => "WrResource",
        28 => "WrPushLock",
        29 => "WrMutex",
        30 => "WrQuantumEnd",
        31 => "WrDispatchInt",
        32 => "WrPreempted",
        33 => "WrYieldExecution",
        34 => "WrFastMutex",
        35 => "WrGuardedMutex",
        36 => "WrRundown",
        37 => "WrAlertByThreadId",
        38 => "WrDeferredPreempt",
        39 => "WrPhysicalFault",
        40 => "WrIoRing",
        41 => "WrMdlCache",
        _ => return None,
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a capture that cannot be gathered must fail the test loudly")]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use jiff::Timestamp;

    use super::{
        Capture, PREK_IMAGE, PrekLog, PrekLook, ProcessView, Start, ThreadView, capped, capture, header, prek_log_owner, process_line, thread_line,
        truncated,
    };

    /// A thread of id `tid` waiting on a user request for 1500 ms, which began at `start`.
    fn waiting(tid: usize, start: Start) -> ThreadView {
        ThreadView {
            tid,
            state: 5,
            wait_reason: 6,
            waited_ms: 1500,
            start,
        }
    }

    /// A process of id `pid` holding `threads` waiting threads, as `view_of` reads them: up to the cap.
    fn process(pid: usize, threads: usize) -> ProcessView {
        let tids: Vec<usize> = (0..threads).collect();
        ProcessView {
            pid,
            parent: 8,
            image: "fake-tool.exe".to_owned(),
            threads: capped(&tids, |tid| waiting(*tid, Start::Unread)),
            thread_count: threads,
            modules_unread: None,
        }
    }

    #[test]
    fn a_thread_that_does_not_wait_says_no_wait() {
        let running = ThreadView {
            state: 2,
            ..waiting(9, Start::Address(0x1000))
        };

        let line = thread_line(&running);

        assert_eq!(line, "  thread 9 state Running start 0x1000");
        assert!(!line.contains(" wait "), "a running thread's stale wait reason is not said: {line}");
    }

    #[test]
    fn a_process_whose_modules_are_unread_says_why() {
        let unread = ProcessView {
            modules_unread: Some("OpenProcess(12) failed: Access is denied. (os error 5)".to_owned()),
            ..process(12, 1)
        };
        let mut captured = Capture::new("header".to_owned());

        captured.add(&unread);

        assert_eq!(
            captured.lines.get(1..3),
            Some(
                &[
                    "process 12 parent 8 fake-tool.exe".to_owned(),
                    "  modules unread: OpenProcess(12) failed: Access is denied. (os error 5)".to_owned(),
                ][..]
            ),
            "the reason follows the process line: {:?}",
            captured.lines
        );
    }

    #[test]
    fn a_capture_with_no_budget_is_cut() {
        let look = PrekLook {
            folder: Path::new("C:\\dev\\run"),
            image: PREK_IMAGE,
        };
        let captured = capture(&|| Ok(vec![std::process::id()]), Duration::from_secs(3), 0, look).expect("the capture is gathered");

        let last = captured.lines.last().expect("a capture has lines");
        assert!(last.starts_with("capture cut at "), "the capture ends with its cut: {:?}", captured.lines);
        assert_eq!(captured.processes, 0, "no process is described past the budget");
        assert!(captured.prek_log.is_none(), "nothing is copied past the budget");
    }

    #[test]
    fn the_header_and_the_process_and_thread_lines_read_as_designed() {
        let now: Timestamp = "2026-11-02T03:04:05Z".parse().expect("the time parses");
        assert_eq!(
            header(now, 4321, Duration::from_millis(7400)),
            "gate stall capture: 2026-11-02T03:04:05Z, gate pid 4321, quiet 7 s"
        );
        assert_eq!(process_line(&process(12, 0)), "process 12 parent 8 fake-tool.exe");
        assert_eq!(
            thread_line(&waiting(5, Start::InModule("ntdll.dll".to_owned(), 0x1a2b))),
            "  thread 5 state Waiting wait UserRequest waited 1500 ms start ntdll.dll+0x1a2b"
        );
        assert_eq!(
            thread_line(&waiting(6, Start::Address(0x7ff6_0000_1000))),
            "  thread 6 state Waiting wait UserRequest waited 1500 ms start 0x7ff600001000"
        );
        let unknown_reason = ThreadView {
            wait_reason: 77,
            ..waiting(7, Start::Unread)
        };
        assert_eq!(thread_line(&unknown_reason), "  thread 7 state Waiting wait 77 waited 1500 ms start ?");
        let unknown_state = ThreadView {
            state: 99,
            ..waiting(8, Start::Unread)
        };
        assert_eq!(thread_line(&unknown_state), "  thread 8 state 99 start ?");
    }

    #[test]
    fn a_git_root_that_is_the_folder_is_this_run_s_log() {
        let head = "2026-11-11T05:57:00.393465Z DEBUG Git root: X:\\dev\\run\n";

        assert_eq!(prek_log_owner(head, Path::new("X:\\dev\\run")), None);
    }

    #[test]
    fn a_git_root_above_the_folder_is_this_run_s_log() {
        let head = "2026-11-11T05:57:00.393465Z DEBUG Git root: X:\\dev\n";

        assert_eq!(prek_log_owner(head, Path::new("X:\\dev\\run\\sub")), None);
    }

    #[test]
    fn a_git_root_under_the_folder_is_this_run_s_log() {
        let head = "2026-11-11T05:57:00.393465Z DEBUG Git root: X:\\dev\\run\\sub\n";

        assert_eq!(
            prek_log_owner(head, Path::new("X:\\dev\\run")),
            None,
            "a run started above the repo, as `git -C repo commit` is, is still this run's"
        );
    }

    #[test]
    fn a_git_root_with_the_verbatim_prefix_is_this_run_s_log() {
        let head = "2026-11-11T05:57:00.393465Z DEBUG Git root: \\\\?\\X:\\dev\\run\n";

        assert_eq!(prek_log_owner(head, Path::new("X:\\dev\\run")), None);
    }

    #[test]
    fn a_folder_that_only_shares_the_root_s_name_start_is_another_run_s_log() {
        let head = "2026-11-11T05:57:00.393465Z DEBUG Git root: X:\\dev\\yaat\n";

        assert_eq!(prek_log_owner(head, Path::new("X:\\dev\\yaat-server\\sub")), Some(PrekLog::AnotherRun));
    }

    #[test]
    fn a_git_root_in_another_case_with_forward_slashes_is_this_run_s_log() {
        let head = "2026-11-11T05:57:00.393465Z DEBUG Git root: x:/DEV/Run/\n";

        assert_eq!(prek_log_owner(head, Path::new("X:\\dev\\run")), None);
    }

    #[test]
    fn a_log_with_no_git_root_names_no_run() {
        let head = "2026-11-11T05:57:00.257996Z DEBUG prek: 0.5.0\nprek: running hooks\n";

        assert_eq!(prek_log_owner(head, Path::new("X:\\dev\\run")), Some(PrekLog::NoRoot));
    }

    #[test]
    fn a_git_root_line_after_another_line_is_found() {
        let head = "2026-11-11T05:57:00.257996Z DEBUG prek: 0.5.0\n2026-11-11T05:57:00.393465Z DEBUG Git root: X:\\dev\\run\n";

        assert_eq!(prek_log_owner(head, Path::new("X:\\dev\\run")), None);
    }

    #[test]
    fn a_log_that_ends_shorter_than_it_began_was_truncated() {
        assert!(truncated(100, 99), "a log shorter than it began was truncated");
        assert!(!truncated(100, 100), "a log of the same length was not");
        assert!(!truncated(100, 200), "a log that grew was not");
    }

    #[test]
    fn the_caps_count_the_processes_and_threads_left_out() {
        let mut processes: Vec<ProcessView> = vec![process(100, 260)];
        processes.extend((101..145).map(|pid| process(pid, 1)));
        let mut captured = Capture::new("header".to_owned());

        let cut = captured
            .describe(&processes, |view| Ok(view.clone()), Instant::now(), u128::MAX)
            .expect("the synthetic views describe");

        assert!(!cut, "the budget does not cut it");
        assert_eq!(captured.processes, 40, "40 processes are described");
        assert_eq!(captured.threads, 256 + 39, "256 threads of the first, one of each other");
        let after_threads = captured.lines.get(1 + 1 + 256).expect("the line after the first process's threads");
        assert_eq!(
            after_threads, "  ... and 4 more",
            "the first process's 4 threads past the cap are counted"
        );
        assert_eq!(
            captured.lines.last().map(String::as_str),
            Some("... and 5 more"),
            "the 5 processes past the cap are counted"
        );
    }
}
