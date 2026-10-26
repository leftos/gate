//! `gate stop-tree`: a gate and its command's processes stopped, a process only the gate's job still reaches, a tree
//! with no gate in it, a pid no live process has, a process already ending counted as stopped, and a case's own drop
//! ending the command its gate left.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use jiff::Timestamp;
use proc_table::{ProcessTable as _, ProcessTree, Win32ProcessTable};
use regex::Regex;
use windows_sys::Win32::Foundation::{CloseHandle, DBG_CONTINUE, DBG_EXCEPTION_NOT_HANDLED};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CREATE_PROCESS_DEBUG_EVENT, ContinueDebugEvent, DEBUG_EVENT, DebugActiveProcess, DebugSetProcessKillOnExit, EXCEPTION_DEBUG_EVENT,
    EXIT_PROCESS_DEBUG_EVENT, LOAD_DLL_DEBUG_EVENT, WaitForDebugEvent,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

use crate::support::{Case, FAKE_TOOL, USAGE, log_texts, run_words, said_pid, shown, start, wait_until, wait_until_closed};

/// Writes the go file when dropped, so a helper a failing case leaves behind ends without waiting out its limit. A
/// case declares it after its [`Case`] and before its running programs, so it lets them go before the folder goes.
struct LetGo(PathBuf);

impl Drop for LetGo {
    fn drop(&mut self) {
        // The folder may already be gone; a helper that cannot see the file still ends at its own limit.
        let _ = fs::write(&self.0, "");
    }
}

/// The pid on the `spawned <pid>` line of `log`, once the log also has `after`.
fn spawned_pid(log: &Path, after: &str) -> u32 {
    let mut pid = None;
    wait_until(&format!("{} naming its spawned helper after {after:?}", log.display()), || {
        let text = log_texts(log);
        pid = text
            .lines()
            .any(|line| line == after)
            .then(|| text.lines().find_map(|line| line.strip_prefix("spawned ")?.parse().ok()))
            .flatten();
        pid.is_some()
    });
    pid.expect("checked by the wait")
}

/// The pid of the case's command, from the `pid <pid>` line its own `pid` step printed, once the log holds it.
fn command_pid(log: &Path) -> u32 {
    let text = || log_texts(log);
    wait_until(&format!("{} naming its command's pid", log.display()), || {
        text().lines().any(|line| line.starts_with("pid "))
    });
    said_pid(&text(), "pid ")
}

/// A running process: its pid and its start, so that a later process given the same pid is not taken for it.
struct Started {
    pid: u32,
    at: Timestamp,
}

/// The running process `pid`, with its start read now.
fn started(pid: u32) -> Started {
    let at = gate_win32::process_start_of(pid)
        .expect("a process the case started can be queried")
        .expect("the process runs");
    Started { pid, at }
}

/// Returns once the process has ended: its pid no longer belongs to a running process with its start.
fn wait_gone(process: &Started) {
    wait_until(
        &format!("process {} ending", process.pid),
        || !matches!(gate_win32::process_start_of(process.pid), Ok(Some(at)) if at == process.at),
    );
}

/// The pids of the running `fake-tool` processes whose parent is `pid`.
fn fake_tool_children(pid: u32) -> Vec<u32> {
    ProcessTree::new(Win32ProcessTable.snapshot())
        .iter()
        .filter(|process| process.parent_pid == pid && process.name.eq_ignore_ascii_case("fake-tool.exe"))
        .map(|process| process.pid)
        .collect()
}

/// Asserts that `stdout` is the one summary line of stop-tree under `pid`, with `jobs` gate jobs.
fn assert_summary(stdout: &str, jobs: u32, pid: u32) {
    let pattern = format!(r"^gate: stopped {jobs} gate job\(s\) and \d+ process\(es\) under {pid}\n$");
    let summary = Regex::new(&pattern).expect("the summary pattern is valid");
    assert!(summary.is_match(stdout), "{stdout:?} does not match {pattern}");
}

#[test]
fn stop_tree_stops_a_gate_and_its_children() {
    let case = Case::new("stop_tree_stops_a_gate_and_its_children");
    let go = case.folder().join("go");
    let _let_go = LetGo(go.clone());
    let go = shown(&go);
    let log = case.folder().join("out.log");
    let gate = case.start(&run_words("out.log", &[], &[FAKE_TOOL, "out", "ready", "spawn-wait", &go, "wait", &go]));
    let helper = spawned_pid(&log, "ready");
    let command = ProcessTree::new(Win32ProcessTable.snapshot())
        .get(helper)
        .map(|process| process.parent_pid)
        .expect("the helper is running under the command");
    let gate_pid = gate.id();
    let tree = [started(gate_pid), started(command), started(helper)];

    let outcome = case.run(&["stop-tree", &gate_pid.to_string()]);
    let stopped = gate.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    assert_summary(&outcome.stdout, 1, gate_pid);
    assert_eq!(stopped.code, Some(124), "the gate ends with the stop status: {stopped:?}");
    for process in &tree {
        wait_gone(process);
    }
    wait_until_closed(&log);
}

#[test]
fn stop_tree_reaches_a_process_only_the_job_holds() {
    let case = Case::new("stop_tree_reaches_a_process_only_the_job_holds");
    let go = case.folder().join("go");
    let _let_go = LetGo(go.clone());
    let go = shown(&go);
    let log = case.folder().join("out.log");
    let gate = case.start(&run_words(
        "out.log",
        &[],
        &[FAKE_TOOL, "spawn-orphan", &go, "out", "orphaned", "wait", &go],
    ));
    let orphan = started(spawned_pid(&log, "orphaned"));

    let gate_pid = gate.id();
    let outcome = case.run(&["stop-tree", &gate_pid.to_string()]);
    let stopped = gate.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    assert_summary(&outcome.stdout, 1, gate_pid);
    assert_eq!(stopped.code, Some(124), "the gate ends with the stop status: {stopped:?}");
    wait_gone(&orphan);
    wait_until_closed(&log);
}

#[test]
fn stop_tree_of_a_tree_without_a_gate() {
    let case = Case::new("stop_tree_of_a_tree_without_a_gate");
    let go = case.folder().join("go");
    let _let_go = LetGo(go.clone());
    let go = shown(&go);
    let mut command = Command::new(FAKE_TOOL);
    command.args(["spawn-wait", &go, "err", "up", "wait", &go]);
    let tree = start(command);
    tree.wait_for_stderr("up");
    let root = tree.id();
    let helpers = fake_tool_children(root);
    assert_eq!(helpers.len(), 1, "the root runs one helper: {helpers:?}");
    let started_tree: Vec<Started> = std::iter::once(root).chain(helpers.iter().copied()).map(started).collect();

    let outcome = case.run(&["stop-tree", &root.to_string()]);
    let ended = tree.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    assert_summary(&outcome.stdout, 0, root);
    assert_eq!(ended.code, Some(124), "the root ends with the stop status: {ended:?}");
    assert!(
        ended.stdout.contains(&format!("spawned {}\n", helpers[0])),
        "the helper found is the one the root started: {ended:?}"
    );
    for process in &started_tree {
        wait_gone(process);
    }
}

#[test]
fn a_dropped_case_ends_its_gates_command() {
    let case = Case::new("a_dropped_case_ends_its_gates_command");
    let stop = case.folder().join("stop");
    let _let_go = LetGo(stop.clone());
    let stop = shown(&stop);
    let log = case.folder().join("out.log");
    let gate = case.start(&run_words("out.log", &[], &[FAKE_TOOL, "pid", "chatter", &stop]));
    let command = started(command_pid(&log));

    drop(gate);

    wait_gone(&command);
    wait_until_closed(&log);
}

#[test]
fn stop_tree_of_a_dead_pid_is_a_usage_error() {
    let case = Case::new("stop_tree_of_a_dead_pid_is_a_usage_error");
    let mut command = Command::new(FAKE_TOOL);
    command.args(["exit", "0"]);
    // Kept, unwaited, until the case ends: its handle keeps the pid from going to another process meanwhile.
    let mut exited = start(command);
    wait_until("the process exiting", || !exited.is_running());
    let pid = exited.id();

    let outcome = case.run(&["stop-tree", &pid.to_string()]);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, format!("gate: stop-tree: no live process has the id {pid}\n{USAGE}"));
}

/// How long the exit hold waits for any one debug event or release before it fails as hung.
const EXIT_HOLD_GUARD: Duration = Duration::from_secs(600);
const STATUS_BREAKPOINT: u32 = 0x8000_0003;

/// A process's end held open as its debugger holds it: every debug event but its exit is continued, and the exit is
/// held until this is dropped, so meanwhile the process is ending, listed, and its handle unsignalled.
struct ExitHeld {
    release: Option<mpsc::Sender<()>>,
    debugger: Option<thread::JoinHandle<()>>,
}

impl ExitHeld {
    /// Attaches to `pid`, ends it with `status`, and returns once its exit is held.
    fn end_and_hold(pid: u32, status: u32) -> ExitHeld {
        let (attached_tx, attached) = mpsc::channel();
        let (held_tx, held) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let debugger = thread::spawn(move || hold_exit(pid, &attached_tx, &held_tx, &released));
        attached
            .recv_timeout(EXIT_HOLD_GUARD)
            .expect("the case attaches as the process's debugger");
        // SAFETY: takes no pointers; a null handle fails the assert below.
        let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        assert!(!process.is_null(), "the case opens {pid} to end it");
        // SAFETY: `process` is open with PROCESS_TERMINATE and closed right after.
        let ended = unsafe { TerminateProcess(process, status) };
        // SAFETY: as above.
        unsafe { CloseHandle(process) };
        assert_ne!(ended, 0, "the case ends {pid}, as a job's termination does");
        held.recv_timeout(EXIT_HOLD_GUARD).expect("the process's exit is held");
        ExitHeld {
            release: Some(release),
            debugger: Some(debugger),
        }
    }
}

impl Drop for ExitHeld {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(debugger) = self.debugger.take() {
            let _ = debugger.join();
        }
    }
}

fn hold_exit(pid: u32, attached: &mpsc::Sender<()>, held: &mpsc::Sender<()>, released: &mpsc::Receiver<()>) {
    // SAFETY: takes no pointers; this thread is the debugger and waits for the events below.
    assert_ne!(unsafe { DebugActiveProcess(pid) }, 0, "attach to {pid}");
    // SAFETY: takes no pointers; this thread's end detaches rather than kills.
    unsafe { DebugSetProcessKillOnExit(0) };
    attached.send(()).expect("the case waits for the attach");
    let guard = u32::try_from(EXIT_HOLD_GUARD.as_millis()).expect("the guard fits a u32");
    loop {
        let mut event = DEBUG_EVENT::default();
        // SAFETY: `event` is a writable DEBUG_EVENT that outlives the call.
        assert_ne!(
            unsafe { WaitForDebugEvent(&raw mut event, guard) },
            0,
            "no debug event from {pid} within the hang guard"
        );
        // SAFETY: the union member read is the one the event code names; the file handles are the debugger's to close.
        unsafe {
            match event.dwDebugEventCode {
                CREATE_PROCESS_DEBUG_EVENT => CloseHandle(event.u.CreateProcessInfo.hFile),
                LOAD_DLL_DEBUG_EVENT => CloseHandle(event.u.LoadDll.hFile),
                _ => 0,
            };
        }
        if event.dwDebugEventCode == EXIT_PROCESS_DEBUG_EVENT {
            held.send(()).expect("the case waits for the hold");
            // A release, or a case that failed and dropped its sender, lets the exit go.
            let _ = released.recv_timeout(EXIT_HOLD_GUARD);
            // SAFETY: continues the event this thread received.
            unsafe { ContinueDebugEvent(event.dwProcessId, event.dwThreadId, DBG_CONTINUE) };
            return;
        }
        // SAFETY: the exception member is the one an exception event carries.
        let foreign = event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT
            && unsafe { event.u.Exception.ExceptionRecord.ExceptionCode }.cast_unsigned() != STATUS_BREAKPOINT;
        let status = if foreign { DBG_EXCEPTION_NOT_HANDLED } else { DBG_CONTINUE };
        // SAFETY: continues the event this thread received.
        unsafe { ContinueDebugEvent(event.dwProcessId, event.dwThreadId, status) };
    }
}

#[test]
fn stop_tree_counts_a_process_already_ending_as_stopped() {
    let case = Case::new("stop_tree_counts_a_process_already_ending_as_stopped");
    let go = case.folder().join("go");
    let _let_go = LetGo(go.clone());
    let go = shown(&go);
    let mut command = Command::new(FAKE_TOOL);
    command.args(["err", "up", "wait", &go]);
    let tool = start(command);
    tool.wait_for_stderr("up");
    let pid = tool.id();
    let held = ExitHeld::end_and_hold(pid, 7);

    let outcome = case.run(&["stop-tree", &pid.to_string()]);
    drop(held);
    let ended = tool.finish();

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    assert_eq!(
        outcome.stdout,
        format!("gate: stopped 0 gate job(s) and 0 process(es) under {pid}\n"),
        "an already-ending process counts as gone, not as stopped"
    );
    assert_eq!(ended.code, Some(7), "it ends with the status it was already ending with: {ended:?}");
}
