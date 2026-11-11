//! The watch: a command with no output and CPU under the floor for the stall window, the stretches it was starved of a
//! processor left out, is killed with the STALLED line, its job's processes terminated and the run failed with status
//! 124, and a run that passes journals its load-adjusted seconds.

use std::fs;
use std::io::Read as _;
use std::os::windows::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use gate_win32::{EnvironmentBlock, Job, Launch, LaunchOutput, StoppableProcess};
use regex::Regex;
use serde_json::Value;
use windows_sys::Win32::Foundation::{CloseHandle, DBG_CONTINUE, DBG_EXCEPTION_NOT_HANDLED};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CREATE_PROCESS_DEBUG_EVENT, ContinueDebugEvent, DEBUG_EVENT, DebugActiveProcess, DebugActiveProcessStop, DebugSetProcessKillOnExit,
    EXCEPTION_DEBUG_EVENT, LOAD_DLL_DEBUG_EVENT, WaitForDebugEvent,
};

use crate::harness::pin_to_one_core_at_normal;
use crate::support::{
    CORE_SECOND, Case, Escapee, FAKE_TOOL, UNTIL_KILLED, assert_in_order, copy_tool, cpu_ticks, exit_of, log_texts, process_lines, release,
    run_words, said_pid, shown, start, tool, wait_until, wait_until_closed,
};

#[test]
fn a_process_started_with_breakaway_leaves_a_job_that_allows_it() {
    let case = Case::new("a_process_started_with_breakaway_leaves_a_job_that_allows_it");
    let job = Job::create_breakaway(&format!(r"Local\gate-suite-{}-breakaway", std::process::id())).expect("the job is created");
    let escapee = Escapee::new(&case);
    let line = escapee
        .command("0")
        .iter()
        .map(|word| format!(r#""{word}""#))
        .collect::<Vec<_>>()
        .join(" ");
    let launch = Launch {
        application: PathBuf::from(FAKE_TOOL),
        command_line: line.encode_utf16().collect(),
        directory: case.folder(),
        environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
        output: LaunchOutput::Pipes,
        priority: None,
    };

    let mut suspended = launch.start_suspended().expect("the command starts");
    let (_out, err) = suspended.take_output().expect("the command's pipes are made");
    // The escapee's lines on the command's stderr, read as they come, so the pipe never fills.
    let said = Arc::new(Mutex::new(String::new()));
    let _reader = {
        let said = Arc::clone(&said);
        thread::spawn(move || {
            let mut err = err;
            let mut chunk = [0_u8; 4096];
            while let Ok(read @ 1..) = err.read(&mut chunk) {
                said.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push_str(&String::from_utf8_lossy(&chunk[..read]));
            }
        })
    };
    let running = suspended.assign_and_resume(&job).expect("the command is assigned and resumed");
    let ended = || !matches!(running.wait(Duration::ZERO), Ok(None));
    let (_, away) = escapee.open(ended);
    let parent = StoppableProcess::open(running.pid())
        .expect("the command is opened")
        .expect("it runs: it waits on the stop file");

    let log = || said.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert!(parent.in_job(&job).expect("the command's job is read"), "the command is in the job");
    assert!(
        !away.in_job(&job).expect("the escapee's job is read"),
        "the escapee left the job: {}",
        log()
    );
    escapee.stop();
    assert_eq!(running.wait(Duration::from_secs(600)).expect("the wait answers"), Some(0), "{}", log());
}

/// The words of a gate with a 2 s stall window and a 100 ms sample, whose command's job lets a process in it create
/// one outside it.
const AWAY_SEAMS: [&str; 5] = ["--test-stall-ms", "2000", "--test-sample-ms", "100", "--test-job-breakaway"];

#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_command_whose_only_work_is_a_child_outside_its_job_is_not_stalled() {
    let case = Case::new("a_command_whose_only_work_is_a_child_outside_its_job_is_not_stalled");
    let escapee = Escapee::new(&case);
    let command = escapee.command("2");
    let words: Vec<&str> = command.iter().map(String::as_str).collect();

    let mut running = case.start(&run_words("out.log", &AWAY_SEAMS, &words));
    let (pid, _held) = escapee.open(|| !running.is_running());
    // Eight core-seconds on two threads take at least 4 s: two stall windows in which the command itself only waits.
    wait_until("the escapee using 8 core-seconds, or the gate ending", || {
        cpu_ticks(pid) >= 8 * CORE_SECOND || !running.is_running()
    });
    escapee.stop();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "the escapee's CPU is the quiet command's progress: {outcome:?}");
    assert!(!outcome.stderr.contains("STALLED"), "{outcome:?}");
}

#[test]
fn a_command_whose_child_outside_its_job_idles_is_still_stalled() {
    let case = Case::new("a_command_whose_child_outside_its_job_idles_is_still_stalled");
    // Neither the command nor its child outside the job polls or spins: what either does is no progress, so the stall
    // kill ends the command, and the escapee's guard ends the child.
    let escapee = Escapee::new(&case);
    let command = escapee.idle_command();
    let words: Vec<&str> = command.iter().map(String::as_str).collect();

    let outcome = case.run(&run_words("out.log", &AWAY_SEAMS, &words));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
    assert_eq!(release(&case)["verdict"], "stalled", "{outcome:?}");
}

/// A child a case's command starts in a job of its own nested in the command's, with fake-tool's `spawn-in-job`, as
/// Godot's console wrapper starts Godot: spinning until its stop file exists, or, idle, sleeping without ever looking at
/// it. Its stop file is written on every path out of the case, a panic included; an idle one ends with the job of the
/// command that holds its own job's only handle.
struct InJob {
    pid_file: PathBuf,
    stop: PathBuf,
}

impl InJob {
    /// The child whose pid file and stop file are `in-job.pid` and `stop` in `case`'s folder.
    fn new(case: &Case) -> InJob {
        InJob {
            pid_file: case.folder().join("in-job.pid"),
            stop: case.folder().join("stop"),
        }
    }

    /// The words of a command that starts the child spinning on `threads` threads (`0`: sleeping), then waits for it to
    /// end without output.
    fn command(&self, threads: &str) -> Vec<String> {
        [FAKE_TOOL, "spawn-in-job", &shown(&self.pid_file), threads, &shown(&self.stop)]
            .map(str::to_owned)
            .to_vec()
    }

    /// The child's pid and the process, once the pid file holds its whole line, checked to be the process that started
    /// then. Fails at once, rather than at the hang guard, when `ended` says the command has ended without writing it.
    #[expect(clippy::panic, reason = "a command that ended before it wrote the pid file fails the case saying so")]
    fn open(&self, mut ended: impl FnMut() -> bool) -> (u32, StoppableProcess) {
        let mut said = None;
        wait_until(
            &format!("{} holding a pid and a start, or the command ending", self.pid_file.display()),
            || {
                said = fs::read_to_string(&self.pid_file).ok().and_then(|text| {
                    let (pid, start) = text.strip_suffix('\n')?.split_once(' ')?;
                    Some((pid.parse::<u32>().ok()?, start.parse::<i64>().ok()?))
                });
                said.is_some() || ended()
            },
        );
        let Some((pid, start)) = said else {
            panic!("the command ended before it wrote {}", self.pid_file.display());
        };
        let process = StoppableProcess::open(pid)
            .expect("the child is opened")
            .expect("it runs: it spins until its stop file exists, or sleeps");
        let created = process.created().expect("the child's start is read");
        assert_eq!(
            Some(created),
            proc_table::filetime_to_timestamp(start),
            "pid {pid} is the child spawn-in-job started"
        );
        (pid, process)
    }

    /// Writes the child's stop file.
    fn stop(&self) {
        fs::write(&self.stop, "").expect("the child's stop file can be written");
    }
}

impl Drop for InJob {
    fn drop(&mut self) {
        // Fails only when the case's folder is gone, and with it anything that would read the file.
        let _ = fs::write(&self.stop, "");
    }
}

/// The words of a gate with a 3 s stall window and a 100 ms sample.
const IN_JOB_SEAMS: [&str; 4] = ["--test-stall-ms", "3000", "--test-sample-ms", "100"];

#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_command_whose_only_work_is_a_child_in_a_job_of_its_own_is_not_stalled() {
    let case = Case::new("a_command_whose_only_work_is_a_child_in_a_job_of_its_own_is_not_stalled");
    // GATE-1090: the child's job is nested in the command's, so the command's job counts the child's CPU.
    let child = InJob::new(&case);
    let command = child.command("2");
    let words: Vec<&str> = command.iter().map(String::as_str).collect();

    let mut running = case.start(&run_words("out.log", &IN_JOB_SEAMS, &words));
    let (pid, _held) = child.open(|| !running.is_running());
    // Eight core-seconds on two threads take at least 4 s: more than a stall window in which the command itself only waits.
    wait_until("the child using 8 core-seconds, or the gate ending", || {
        cpu_ticks(pid) >= 8 * CORE_SECOND || !running.is_running()
    });
    child.stop();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "the child's CPU is the quiet command's progress: {outcome:?}");
    assert!(!outcome.stderr.contains("STALLED"), "{outcome:?}");
    let release = release(&case);
    assert!(
        release["cpuSeconds"].as_f64().is_some_and(|seconds| seconds >= 8.0),
        "the job's CPU counts the child's: {release}"
    );
}

#[test]
fn a_stall_kill_ends_a_child_in_a_job_of_its_own() {
    let case = Case::new("a_stall_kill_ends_a_child_in_a_job_of_its_own");
    // GATE-1090: the child sleeps, so neither it nor the command does anything that is progress; the stall kill ends the
    // command's job, and with it the job nested in it.
    let child = InJob::new(&case);
    let command = child.command("0");
    let words: Vec<&str> = command.iter().map(String::as_str).collect();

    let mut running = case.start(&run_words("out.log", &IN_JOB_SEAMS, &words));
    let (pid, process) = child.open(|| !running.is_running());
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
    assert_eq!(
        Escapee::exit_code_once_ended(&process),
        Some(124),
        "the child {pid} in a job of its own was ended by the kill with the kill's status: {outcome:?}"
    );
    let log_path = case.folder().join("out.log");
    wait_until_closed(&log_path);
    let lines = process_lines(&log_path);
    let ended = format!("exit pid={pid} code=124 ");
    assert!(
        lines.iter().any(|line| line.starts_with(&ended)),
        "the log has the child's {ended:?} line: {lines:#?}"
    );
}

/// The longest the case waits for the attach breakpoint, for a debug event, or for its release.
const BREAKPOINT_HOLD_GUARD: Duration = Duration::from_secs(600);

/// The exception code of a breakpoint (`STATUS_BREAKPOINT`).
const STATUS_BREAKPOINT: u32 = 0x8000_0003;

/// A process kept from running as a starved one is: this case, as its debugger, holds its attach breakpoint, so none
/// of its threads runs, a terminate's included; one ended meanwhile has no exit time and an unsignalled handle until
/// this is dropped, which continues the breakpoint and detaches.
struct BreakpointHeld {
    release: Option<mpsc::Sender<()>>,
    debugger: Option<thread::JoinHandle<()>>,
}

impl BreakpointHeld {
    /// Attaches to `pid` and holds its attach breakpoint, failing at the hang guard.
    fn hold(pid: u32) -> BreakpointHeld {
        let (held_tx, held) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let debugger = thread::spawn(move || hold_breakpoint(pid, &held_tx, &released));
        held.recv_timeout(BREAKPOINT_HOLD_GUARD).expect("the attach breakpoint is held");
        BreakpointHeld {
            release: Some(release),
            debugger: Some(debugger),
        }
    }
}

impl Drop for BreakpointHeld {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(debugger) = self.debugger.take() {
            let _ = debugger.join();
        }
    }
}

/// Debugs `pid` until its attach breakpoint, says so on `held`, and keeps it there until `released` lets it go.
fn hold_breakpoint(pid: u32, held: &mpsc::Sender<()>, released: &mpsc::Receiver<()>) {
    // SAFETY: takes no pointers; this thread is the debugger.
    assert_ne!(unsafe { DebugActiveProcess(pid) }, 0, "attach to {pid}");
    // SAFETY: takes no pointers; a detach never kills.
    unsafe { DebugSetProcessKillOnExit(0) };
    let guard = u32::try_from(BREAKPOINT_HOLD_GUARD.as_millis()).expect("the guard fits a u32");
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
        // SAFETY: the exception member is the one an exception event carries.
        let code =
            (event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT).then(|| unsafe { event.u.Exception.ExceptionRecord.ExceptionCode }.cast_unsigned());
        if code == Some(STATUS_BREAKPOINT) {
            held.send(()).expect("the case waits for the hold");
            // A release, or a case that failed and dropped its sender, lets it go.
            let _ = released.recv_timeout(BREAKPOINT_HOLD_GUARD);
            // SAFETY: continues the event this thread received, then detaches from the process it attached to.
            unsafe { ContinueDebugEvent(event.dwProcessId, event.dwThreadId, DBG_CONTINUE) };
            // SAFETY: takes no pointers; this thread attached to `pid`.
            unsafe { DebugActiveProcessStop(pid) };
            return;
        }
        let status = if code.is_some() { DBG_EXCEPTION_NOT_HANDLED } else { DBG_CONTINUE };
        // SAFETY: continues the event this thread received.
        unsafe { ContinueDebugEvent(event.dwProcessId, event.dwThreadId, status) };
    }
}

#[test]
fn a_stall_kill_writes_the_exit_line_of_a_child_in_a_job_of_its_own_that_ends_late() {
    let case = Case::new("a_stall_kill_writes_the_exit_line_of_a_child_in_a_job_of_its_own_that_ends_late");
    // GATE-1185: the kill ends the child with the command's job, but a child starved of a processor runs its end late; the
    // run's end waits for it, so the log has its exit line, never a `left` line. Its attach breakpoint, held, starves it.
    // The child spins, so the stall clock waits while the case attaches: its CPU is progress until the hold freezes it,
    // and under load its waiting threads count as starved, so a busy case never loses the attach to the kill.
    let child = InJob::new(&case);
    let command = child.command("1");
    let words: Vec<&str> = command.iter().map(String::as_str).collect();
    let log_path = case.folder().join("out.log");

    let mut running = case.start(&run_words("out.log", &IN_JOB_SEAMS, &words));
    let (pid, process) = child.open(|| !running.is_running());
    let held = BreakpointHeld::hold(pid);
    let left = format!("left pid={pid} ");
    let pid_text = pid.to_string();
    wait_until(&format!("the gate waiting for the ending child {pid}, giving it up, or ending"), || {
        let waiting = log_texts(&log_path)
            .lines()
            .any(|line| line.starts_with("gate: waiting for ") && line.split(|c: char| !c.is_ascii_digit()).any(|word| word == pid_text));
        waiting || process_lines(&log_path).iter().any(|line| line.starts_with(&left)) || !running.is_running()
    });
    drop(held);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
    assert_eq!(
        Escapee::exit_code_once_ended(&process),
        Some(124),
        "the child {pid} was ended by the kill: {outcome:?}"
    );
    wait_until_closed(&log_path);
    let lines = process_lines(&log_path);
    let ended = format!("exit pid={pid} code=124 ");
    assert!(
        lines.iter().any(|line| line.starts_with(&ended)),
        "the log has the child's {ended:?} line: {lines:#?}"
    );
}

/// One line of a sample trace: when the sample read the job's accounting, in milliseconds of the watch, whether it
/// counted as starved, and the command's CPU since the last progress, in microseconds.
#[derive(Debug)]
struct Traced {
    at_ms: u128,
    starved: bool,
    quiet_cpu_us: u128,
}

/// The samples the trace at `trace` holds so far, each whole line of `<at_ms> starved=<0|1> quiet_cpu_us=<n>`; a line
/// still being written is left out.
fn traced(trace: &Path) -> Vec<Traced> {
    let text = fs::read_to_string(trace).unwrap_or_default();
    text.split_inclusive('\n')
        .filter_map(|line| {
            let mut words = line.strip_suffix('\n')?.split(' ');
            let at_ms = words.next()?.parse().ok()?;
            let starved = match words.next()? {
                "starved=1" => true,
                "starved=0" => false,
                _ => return None,
            };
            let quiet_cpu_us = words.next()?.strip_prefix("quiet_cpu_us=")?.parse().ok()?;
            Some(Traced {
                at_ms,
                starved,
                quiet_cpu_us,
            })
        })
        .collect()
}

/// A normal-priority `fake-tool burn` this test process started outside any gate, burning until its stop file exists.
/// Dropped, it writes that file, then kills the burner and waits for it, so a failing case leaves no burner.
struct Burner {
    child: std::process::Child,
    stop: PathBuf,
}

impl Burner {
    /// A burner that burns until `stop` exists, inheriting this process's core pin and priority class.
    fn start(stop: &Path) -> Burner {
        let child = Command::new(FAKE_TOOL)
            .args(["burn", &shown(stop)])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("the burner starts");
        Burner {
            child,
            stop: stop.to_path_buf(),
        }
    }
}

impl Drop for Burner {
    fn drop(&mut self) {
        // Each step fails only when the burner or its folder is already gone, which leaves nothing to end.
        let _ = fs::write(&self.stop, "");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A prek a case starts outside the gate's job, as another repo's is while it holds prek's store lock: killed, and
/// waited for, when it drops, so a case that fails before its end leaves nothing running.
struct Peer {
    child: std::process::Child,
    ended: bool,
}

impl Peer {
    /// A prek at `path` that sleeps until it is killed.
    fn start(path: &Path) -> Peer {
        let child = Command::new(path).args(["sleep", UNTIL_KILLED]).spawn().expect("the peer prek starts");
        Peer { child, ended: false }
    }

    /// Ends the peer and waits for it, saying a failure to end it; the drop then has nothing left to do.
    fn end(mut self) {
        self.child.kill().expect("the peer prek is killed");
        self.child.wait().expect("the peer prek is waited for");
        self.ended = true;
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        // A case's cleanup: each step fails only when the peer is already gone, which leaves nothing to end.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[expect(clippy::panic, reason = "a wait that failed is said again with the trace read so far")]
fn a_command_starved_by_a_busier_thread_on_its_core_is_not_stalled() {
    let case = Case::new("a_command_starved_by_a_busier_thread_on_its_core_is_not_stalled");
    // GATE-1092: the command is held off its processor by a normal-priority thread on the only core it may use, so the
    // same thread waits for a processor sample after sample whatever the machine's load, getting only the starvation
    // boost (about 0.3% of a core), under the floor. It is starved, not idle: three stall windows of such samples are
    // no stall. The wait is on the samples the gate traces, never on time, and fails at once when the gate ends; a gate
    // that never sampled fills no trace and the case fails as hung.
    pin_to_one_core_at_normal();
    let stop = case.folder().join("stop");
    let trace = case.folder().join("trace.txt");
    let trace_word = shown(&trace);
    let seams = ["--test-stall-ms", "2000", "--test-sample-ms", "100", "--test-sample-trace", &trace_word];
    let stop_word = shown(&stop);
    let _burner = Burner::start(&stop);
    let command = [FAKE_TOOL, "burn", &stop_word];

    let running = case.start(&run_words("out.log", &seams, &command));
    // A wait that fails is said again with the trace read so far, the evidence of which samples counted as starved.
    let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        running.wait_until("a starved sample three stall windows in", || {
            traced(&trace).iter().any(|sample| sample.starved && sample.at_ms >= 6000)
        });
    }));
    if let Err(payload) = waited {
        let said = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        panic!("{said}\ntrace so far:\n{}", fs::read_to_string(&trace).unwrap_or_default());
    }
    fs::write(&stop, "").expect("the stop file can be written");
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: STALLED"), "{outcome:?}");
    assert_eq!(release(&case)["verdict"], "passed", "{outcome:?}");
    // The floor of a 2 s window is 20 ms of CPU: every sample that stopped the stall clock found the command under it.
    let samples = traced(&trace);
    let over: Vec<&Traced> = samples.iter().filter(|sample| sample.starved && sample.quiet_cpu_us >= 20_000).collect();
    assert!(over.is_empty(), "starved samples over the floor: {over:?}; every sample: {samples:?}");
}

/// The line a stalled ssh command says right after its STALLED line. A copy of the gate's own constant on purpose: it
/// pins the wording independently.
const SSH_STALL_LINE: &str = "gate: a command run over ssh shows no CPU on this machine, so it reads as stalled here; routed with gate run \
                              --builder <host>, it is watched by the builder's own CPU";
/// How the ssh line starts.
const SSH_LINE_START: &str = "gate: a command run over ssh";

/// The stderr line after the STALLED one of a run that stalled, `None` when the STALLED line is the last.
fn line_after_stalled(stderr: &str) -> Option<&str> {
    let mut lines = stderr.lines().skip_while(|line| !line.starts_with("gate: STALLED: "));
    assert!(lines.next().is_some(), "the run says the STALLED line: {stderr}");
    lines.next()
}

#[test]
fn a_stalled_ssh_command_says_how_to_route_it() {
    let case = Case::new("a_stalled_ssh_command_says_how_to_route_it");
    copy_tool(&case, "ssh.exe");

    let outcome = case.run(&run_words(
        "out.log",
        &["--test-stall-ms", "2000", "--test-sample-ms", "100"],
        &[r".\ssh.exe", "sleep", UNTIL_KILLED],
    ));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_eq!(line_after_stalled(&outcome.stderr), Some(SSH_STALL_LINE), "{outcome:?}");
}

#[test]
fn a_timed_out_ssh_command_says_no_ssh_line() {
    let case = Case::new("a_timed_out_ssh_command_says_no_ssh_line");
    copy_tool(&case, "ssh.exe");
    let words = [
        "run",
        "--slot",
        "critical",
        "--reason",
        "the suite's critical run",
        "--timeout",
        "1",
        "--log",
        "out.log",
        "--test-sample-ms",
        "100",
        "--",
        r".\ssh.exe",
        "sleep",
        UNTIL_KILLED,
    ];

    // A critical run's load-adjusted clock is wall time, so the 1 s ceiling comes before the stall window and the backstop.
    let outcome = case.run(&words);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: TIMED OUT"), "{outcome:?}");
    assert!(!outcome.stderr.lines().any(|line| line.starts_with(SSH_LINE_START)), "{outcome:?}");
}

#[test]
fn a_stalled_command_not_named_ssh_says_no_ssh_line() {
    let case = Case::new("a_stalled_command_not_named_ssh_says_no_ssh_line");
    copy_tool(&case, "sshd.exe");

    let outcome = case.run(&run_words(
        "out.log",
        &["--test-stall-ms", "2000", "--test-sample-ms", "100"],
        &[r".\sshd.exe", "sleep", UNTIL_KILLED],
    ));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_ne!(line_after_stalled(&outcome.stderr), Some(SSH_STALL_LINE), "{outcome:?}");
    assert!(!outcome.stderr.lines().any(|line| line.starts_with(SSH_LINE_START)), "{outcome:?}");
}

#[test]
fn a_quiet_command_is_stalled_beside_a_busy_compiler_server_outside_its_job() {
    let case = Case::new("a_quiet_command_is_stalled_beside_a_busy_compiler_server_outside_its_job");
    let msbuild = tool(&case, "MSBuild.exe");
    let compiler = tool(&case, "VBCSCompiler.exe");
    let stop = case.folder().join("stop-compiler");
    // The case starts the compiler server itself, so it runs outside the gate's job, and it burns a core for the whole
    // run. It is no process of the run, so its CPU is no progress for the quiet build: only the stall kill ends the
    // command, which sleeps until killed, so a watch that counted the server would hold the stall off until the hang
    // guard and the case would fail.
    let mut command = Command::new(&compiler);
    case.prepare_without_state_dir(&mut command, &["burn", &shown(&stop), "sleep", UNTIL_KILLED]);
    let server = start(command);
    server.wait_for_stdout("burning");
    let burned_before = cpu_ticks(server.id());
    let seams = ["--test-stall-ms", "2000", "--test-sample-ms", "100", "--test-stall-from-output"];

    let outcome = case.run(&run_words("out.log", &seams, &[&msbuild, "out", "building", "sleep", UNTIL_KILLED]));
    let burned_by_the_kill = cpu_ticks(server.id());
    fs::write(&stop, "").expect("the server's stop file can be written");

    assert!(
        burned_by_the_kill > burned_before,
        "the server burned while the gate ran ({burned_before} then {burned_by_the_kill} ticks): {outcome:?}"
    );
    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
    assert_eq!(release(&case)["verdict"], "stalled", "{outcome:?}");
}

#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_stall_kill_ends_a_child_outside_the_job_whose_launcher_has_ended() {
    let case = Case::new("a_stall_kill_ends_a_child_outside_the_job_whose_launcher_has_ended");
    // The launcher lives a second, so a sample sees the escapee under it; then nothing in the job is its parent, and
    // the command idles until the stall kill.
    let escapee = Escapee::new(&case);
    let command = escapee.launched_command();
    let words: Vec<&str> = command.iter().map(String::as_str).collect();
    let seams = ["--test-stall-ms", "3000", "--test-sample-ms", "100", "--test-job-breakaway"];

    let mut running = case.start(&run_words("out.log", &seams, &words));
    let (pid, away) = escapee.open(|| !running.is_running());
    let launcher = escapee.launcher_ended();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
    assert!(
        matches!(away.wait(Duration::ZERO), Ok(true)),
        "the escapee {pid}, whose launcher had ended, was ended by the kill: {outcome:?}"
    );
    assert!(
        matches!((launcher.exited(), away.exited()), (Ok(left), Ok(ended)) if left < ended),
        "the launcher had ended before the kill ended the escapee {pid}: {outcome:?}"
    );
}

#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_stall_kill_ends_a_child_outside_the_job_whose_launcher_exited_at_once() {
    let case = Case::new("a_stall_kill_ends_a_child_outside_the_job_whose_launcher_exited_at_once");
    // The launcher ends as soon as it has started the escapee, long before the watch's first 5 s sample: no sample
    // sees the escapee under a process of the job, and the command idles until the stall kill.
    let escapee = Escapee::new(&case);
    let command = escapee.instant_launched_command();
    let words: Vec<&str> = command.iter().map(String::as_str).collect();
    let seams = ["--test-stall-ms", "3000", "--test-job-breakaway"];

    let mut running = case.start(&run_words("out.log", &seams, &words));
    let (pid, away) = escapee.open(|| !running.is_running());
    let launcher = escapee.launcher_ended();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
    assert_eq!(
        Escapee::exit_code_once_ended(&away),
        Some(124),
        "the escapee {pid}, whose launcher exited at once, was ended by the kill with the kill's status: {outcome:?}"
    );
    assert!(
        matches!((launcher.exited(), away.exited()), (Ok(left), Ok(ended)) if left < ended),
        "the launcher had ended before the kill ended the escapee {pid}: {outcome:?}"
    );
}

#[test]
fn a_silent_idle_command_is_killed_as_stalled() {
    let case = Case::new("a_silent_idle_command_is_killed_as_stalled");
    let seams = ["--test-stall-ms", "1500", "--test-sample-ms", "100", "--test-stall-from-output"];
    let pid_file = case.folder().join("command.pid");

    // `sleep` blocks without a tick of CPU, where `wait` would poll. The window opens at the command's first output byte,
    // its `hi` line; its own pid goes to a file before that, without output, so the line the case reads comes before the
    // window closes.
    let outcome = case.run(&run_words(
        "out.log",
        &seams,
        &[FAKE_TOOL, "pid-to", &shown(&pid_file), "out", "hi", "sleep", UNTIL_KILLED],
    ));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let stalled = Regex::new(&format!(
        concat!(
            r"(?m)^gate: STALLED: no output and \d+\.\d{{2}} s of CPU \(under the \d+\.\d{{2}} s floor\) ",
            r"for \d+ s not waiting for a processor \(machine free \d+% on average\); ",
            r"killed {} pid-to {} out hi sleep {} and its children$"
        ),
        regex::escape(FAKE_TOOL),
        regex::escape(&shown(&pid_file)),
        UNTIL_KILLED
    ))
    .expect("the pattern is valid");
    let found = stalled.find(&outcome.stderr);
    assert!(found.is_some(), "no STALLED line on stderr: {outcome:?}");
    let line = found.expect("checked above").as_str();
    let after = ["gate: terminated the job's 1 process(es)", "gate: FAILED (status 124)"];
    assert_in_order(&outcome.stderr, &[&[line][..], &after[..]].concat());
    let log = log_texts(&case.folder().join("out.log"));
    assert_in_order(&log, &[&["hi\n", line][..], &after[..]].concat());

    let release = release(&case);
    assert_eq!(
        (&release["verdict"], &release["status"]),
        (&Value::from("stalled"), &Value::from(124)),
        "{release}"
    );
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("the command wrote its pid before its first output")
        .trim()
        .parse()
        .expect("the pid file holds a pid");
    // A terminated process leaves its job before its object is signalled, so its end is waited for, not read once.
    wait_until(&format!("the killed command {pid} ending"), || {
        gate_win32::process_start_of(pid).is_ok_and(|start| start.is_none())
    });
}

#[test]
fn a_stalled_run_leaves_a_stall_capture_naming_its_threads() {
    let case = Case::new("a_stalled_run_leaves_a_stall_capture_naming_its_threads");
    let seams = ["--test-stall-ms", "1500", "--test-sample-ms", "100", "--test-stall-from-output"];

    let outcome = case.run(&run_words("out.log", &seams, &[FAKE_TOOL, "pid", "out", "hi", "sleep", UNTIL_KILLED]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let capture_path = case.folder().join("out.log.stall.txt");
    assert_in_order(
        &outcome.stderr,
        &[
            "gate: STALLED: ",
            &format!("gate: stall capture written to {} (", capture_path.display()),
            "gate: diagnostics written to ",
        ],
    );
    let capture = fs::read_to_string(&capture_path).expect("the stall capture was written beside the log");
    assert!(
        capture.starts_with("gate stall capture: "),
        "the capture starts with its header: {capture}"
    );
    let pid = said_pid(&log_texts(&case.folder().join("out.log")), "pid ");
    let named = Regex::new(&format!(r"(?mi)^process {pid} parent \d+ fake-tool\.exe$")).expect("the pattern is valid");
    assert!(named.is_match(&capture), "the capture names the command {pid}: {capture}");
    let waiting = Regex::new(r"(?m)^  thread \d+ state \S+ wait \S+ waited \d+ ms start \S+$").expect("the pattern is valid");
    assert!(waiting.is_match(&capture), "the capture lists a thread with its wait: {capture}");
}

#[test]
fn a_stalled_prek_leaves_a_copy_of_prek_s_log() {
    let case = Case::new("a_stalled_prek_leaves_a_copy_of_prek_s_log");
    copy_tool(&case, "prek-copies-own-log.exe");
    let prek = shown(&case.folder().join("prek-copies-own-log.exe"));
    let prek_log = case.local_app_data().join("prek").join("prek.log");
    fs::create_dir_all(prek_log.parent().expect("the log has a folder")).expect("prek's log folder is made");
    let text = format!(
        "2026-11-11T05:57:00.393465Z DEBUG Git root: {}\nprek: waiting for the hook to exit\n",
        case.folder().display()
    );
    fs::write(&prek_log, &text).expect("prek's log is written");
    // Its own image name, so prek's of another case, of another suite module, or of this PC are invisible to it.
    let seams = [
        "--test-stall-ms",
        "1500",
        "--test-sample-ms",
        "100",
        "--test-stall-from-output",
        "--test-prek-image",
        "prek-copies-own-log.exe",
    ];

    let outcome = case.run(&run_words("out.log", &seams, &[&prek, "out", "hi", "sleep", UNTIL_KILLED]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let copy = case.folder().join("out.log.stall.prek.log");
    assert_in_order(
        &outcome.stderr,
        &[
            "gate: STALLED: ",
            &format!("gate: stall capture copied prek's log to {}", copy.display()),
            "gate: diagnostics written to ",
        ],
    );
    assert_eq!(
        fs::read_to_string(&copy).expect("prek's log was copied beside the log"),
        text,
        "the copy holds prek's log"
    );
    assert!(
        !outcome.stderr.contains("this run's prek is waiting while another prek runs on this PC"),
        "the run's prek waits on no other prek, so no hold line is said: {outcome:?}"
    );
}

#[test]
fn a_stalled_prek_does_not_copy_another_run_s_prek_log() {
    let case = Case::new("a_stalled_prek_does_not_copy_another_run_s_prek_log");
    copy_tool(&case, "prek-another-log.exe");
    let prek = shown(&case.folder().join("prek-another-log.exe"));
    let prek_log = case.local_app_data().join("prek").join("prek.log");
    fs::create_dir_all(prek_log.parent().expect("the log has a folder")).expect("prek's log folder is made");
    let elsewhere = case.folder().with_file_name("another-repo");
    let text = format!(
        "2026-11-11T05:57:00.393465Z DEBUG Git root: {}\nprek: waiting for the hook to exit\n",
        elsewhere.display()
    );
    fs::write(&prek_log, &text).expect("prek's log is written");
    let seams = [
        "--test-stall-ms",
        "1500",
        "--test-sample-ms",
        "100",
        "--test-stall-from-output",
        "--test-prek-image",
        "prek-another-log.exe",
    ];

    let outcome = case.run(&run_words("out.log", &seams, &[&prek, "out", "hi", "sleep", UNTIL_KILLED]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[
            "gate: STALLED: ",
            "gate: stall capture did not copy prek's log: it belongs to another prek run, not this run's folder",
            "gate: diagnostics written to ",
        ],
    );
    assert!(
        !case.folder().join("out.log.stall.prek.log").exists(),
        "another run's prek log is not copied"
    );
}

#[test]
fn a_prek_waiting_while_another_prek_runs_is_held_until_that_prek_ends() {
    let case = Case::new("a_prek_waiting_while_another_prek_runs_is_held_until_that_prek_ends");
    copy_tool(&case, "prek-held.exe");
    let prek = shown(&case.folder().join("prek-held.exe"));
    // A prek outside this run's job, as another repo's is while it holds prek's store lock. The name is this case's
    // own, so only this peer counts as the other prek.
    let peer = Peer::start(&case.folder().join("prek-held.exe"));
    let seams = [
        "--test-stall-ms",
        "1500",
        "--test-sample-ms",
        "100",
        "--test-stall-from-output",
        "--test-prek-image",
        "prek-held.exe",
    ];
    let held = "this run's prek is waiting while another prek runs on this PC";

    let running = case.start(&run_words("out.log", &seams, &[&prek, "out", "hi", "sleep", UNTIL_KILLED]));
    running.wait_for_stderr(held);

    peer.end();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[held, "gate: STALLED: "]);
    assert_eq!(outcome.stderr.matches(held).count(), 1, "the hold line is said once: {outcome:?}");
}

#[test]
fn a_timed_out_run_leaves_no_stall_capture() {
    let case = Case::new("a_timed_out_run_leaves_no_stall_capture");
    // A critical run's load-adjusted clock is wall time, so the 1 s ceiling comes before the stall window and the
    // backstop however busy the machine is.
    let words = [
        "run",
        "--slot",
        "critical",
        "--reason",
        "the suite's critical run",
        "--timeout",
        "1",
        "--log",
        "out.log",
        "--test-sample-ms",
        "100",
        "--",
        FAKE_TOOL,
        "sleep",
        UNTIL_KILLED,
    ];

    let outcome = case.run(&words);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: TIMED OUT after 1 s"), "{outcome:?}");
    assert!(!outcome.stderr.contains("stall capture"), "a ceiling kill says no capture: {outcome:?}");
    assert!(
        !case.folder().join("out.log.stall.txt").exists(),
        "a ceiling kill leaves no stall capture"
    );
}

#[test]
fn a_run_removes_an_earlier_stall_capture_beside_its_log() {
    let case = Case::new("a_run_removes_an_earlier_stall_capture_beside_its_log");
    let earlier = [case.folder().join("out.log.stall.txt"), case.folder().join("out.log.stall.prek.log")];
    for path in &earlier {
        fs::write(path, "an earlier run's capture\n").expect("the earlier capture is written");
    }

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "hi"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    for path in &earlier {
        assert!(!path.exists(), "{} was removed as the run began", path.display());
    }
}

#[test]
fn a_log_of_another_extension_keeps_its_own_stall_capture() {
    let case = Case::new("a_log_of_another_extension_keeps_its_own_stall_capture");
    // `out.txt`'s capture, under its own name and under the name a capture that replaced the log's extension would take,
    // which `out.log`'s would share.
    let others = [case.folder().join("out.txt.stall.txt"), case.folder().join("out.stall.txt")];
    for path in &others {
        fs::write(path, "out.txt's capture\n").expect("the other log's capture is written");
    }

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "hi"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    for path in &others {
        assert!(path.exists(), "{} is left alone by a run logging to out.log", path.display());
    }
}

#[test]
fn a_stall_window_from_the_first_output_does_not_count_a_quiet_start() {
    let case = Case::new("a_stall_window_from_the_first_output_does_not_count_a_quiet_start");
    // The first sleep blocks 3 s, twice the window, without a byte of output or a tick of CPU: with the window starting
    // at the command's first output, that quiet start is no stall, and the quiet after `hi` is. Without the seam the
    // command is killed in its first sleep, before it prints anything. Its own pid goes to a file before its first byte,
    // without output, so the `hi` line the case reads comes before the window closes.
    let seams = ["--test-stall-ms", "1500", "--test-sample-ms", "100", "--test-stall-from-output"];
    let pid_file = case.folder().join("command.pid");

    let outcome = case.run(&run_words(
        "out.log",
        &seams,
        &[
            FAKE_TOOL,
            "sleep",
            "3000",
            "pid-to",
            &shown(&pid_file),
            "out",
            "hi",
            "sleep",
            UNTIL_KILLED,
        ],
    ));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let stalled = Regex::new(&format!(
        concat!(
            r"(?m)^gate: STALLED: no output and \d+\.\d{{2}} s of CPU \(under the \d+\.\d{{2}} s floor\) ",
            r"for \d+ s not waiting for a processor \(machine free \d+% on average\); ",
            r"killed {} sleep 3000 pid-to {} out hi sleep {} and its children$"
        ),
        regex::escape(FAKE_TOOL),
        regex::escape(&shown(&pid_file)),
        UNTIL_KILLED
    ))
    .expect("the pattern is valid");
    let found = stalled.find(&outcome.stderr);
    assert!(found.is_some(), "no STALLED line on stderr: {outcome:?}");
    let line = found.expect("checked above").as_str();
    let after = ["gate: terminated the job's 1 process(es)", "gate: FAILED (status 124)"];
    assert_in_order(&outcome.stderr, &[&[line][..], &after[..]].concat());
    let log = log_texts(&case.folder().join("out.log"));
    assert_in_order(&log, &[&["hi\n", line][..], &after[..]].concat());

    let release = release(&case);
    assert_eq!(
        (&release["verdict"], &release["status"]),
        (&Value::from("stalled"), &Value::from(124)),
        "{release}"
    );
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("the command wrote its pid before its first output")
        .trim()
        .parse()
        .expect("the pid file holds a pid");
    // A terminated process leaves its job before its object is signalled, so its end is waited for, not read once.
    wait_until(&format!("the killed command {pid} ending"), || {
        gate_win32::process_start_of(pid).is_ok_and(|start| start.is_none())
    });
}

#[test]
fn a_stalled_command_is_killed_with_its_grandchild() {
    let case = Case::new("a_stalled_command_is_killed_with_its_grandchild");
    // Never created: the grandchild waits on it until the kill ends it.
    let go = shown(&case.folder().join("go"));
    let pid_file = case.folder().join("command.pid");
    // The window opens at the command's first output byte, its `spawned` line, written only once the grandchild is in
    // the job; its own pid goes to a file before that, without output, so nothing the case reads has to come before the
    // window closes.
    let seams = ["--test-stall-ms", "3000", "--test-sample-ms", "100", "--test-stall-from-output"];

    let outcome = case.run(&run_words(
        "out.log",
        &seams,
        &[FAKE_TOOL, "pid-to", &shown(&pid_file), "spawn-wait", &go, "sleep", UNTIL_KILLED],
    ));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let after = ["gate: terminated the job's 2 process(es)", "gate: FAILED (status 124)"];
    assert_in_order(&outcome.stderr, &[&["gate: STALLED: "][..], &after[..]].concat());
    assert_eq!(release(&case)["verdict"], "stalled");
    let log_path = case.folder().join("out.log");
    wait_until_closed(&log_path);
    let log = log_texts(&log_path);
    let command: u32 = fs::read_to_string(&pid_file)
        .expect("the command wrote its pid before its first output")
        .trim()
        .parse()
        .expect("the pid file holds a pid");
    for (pid, which) in [(command, "command"), (said_pid(&log, "spawned "), "grandchild")] {
        // A terminated process leaves its job before its object is signalled, so its end is waited for, not read once.
        wait_until(&format!("the killed {which} {pid} ending"), || {
            gate_win32::process_start_of(pid).is_ok_and(|start| start.is_none())
        });
    }
}

/// What a case that starts a packaged program ends on every path out of it, a panic before the program's pid is read
/// included: the files the processes wait on, `opened`, `go` and `release`, are written, and the program, once opened,
/// is terminated, so no process of the case waits out fake-tool's own limit.
struct Packaged {
    process: Option<StoppableProcess>,
    files: [PathBuf; 3],
}

impl Drop for Packaged {
    fn drop(&mut self) {
        // Each fails only when there is nothing left to end: the program has ended, or the case's folder is gone.
        for file in &self.files {
            let _ = fs::write(file, "");
        }
        if let Some(process) = &self.process {
            let _ = process.terminate(1);
        }
    }
}

/// `FILE_ATTRIBUTE_REPARSE_POINT`, the attribute an app execution alias has.
const REPARSE_POINT: u32 = 0x400;

/// Whether the first `pwsh.exe` on `PATH` is the Store's app execution alias: a reparse point in
/// `%LOCALAPPDATA%\Microsoft\WindowsApps`, which starts the packaged pwsh with its package identity. A `pwsh.exe` run
/// straight from the Store's install folder has no package identity, so it does not count. The alias does not open as
/// a file, so its own entry is read.
fn pwsh_is_packaged() -> bool {
    let (Some(path), Some(local)) = (std::env::var_os("PATH"), std::env::var_os("LOCALAPPDATA")) else {
        return false;
    };
    let aliases = PathBuf::from(local).join("Microsoft").join("WindowsApps");
    let first = std::env::split_paths(&path).find_map(|folder| {
        let candidate = folder.join("pwsh.exe");
        fs::symlink_metadata(&candidate).ok().map(|entry| (folder, entry))
    });
    first.is_some_and(|(folder, entry)| same_folder(&folder, &aliases) && entry.file_attributes() & REPARSE_POINT != 0)
}

/// Whether two folders are the same, as Windows compares paths: ignoring case and a trailing separator.
fn same_folder(left: &Path, right: &Path) -> bool {
    let text = |path: &Path| path.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    text(left) == text(right)
}

#[test]
#[expect(clippy::print_stderr, reason = "a case this machine cannot test says so in its output")]
fn a_stall_kill_ends_a_packaged_app_the_command_started() {
    if !pwsh_is_packaged() {
        eprintln!("skipped: pwsh on PATH is not a packaged app");
        return;
    }
    let case = Case::new("a_stall_kill_ends_a_packaged_app_the_command_started");
    // None of these exists until the case writes it: the packaged program chatters into the log until `opened`, so the
    // stall cannot come before the case holds a handle to it, then waits on `go`; the command holds it on `release`.
    let opened = case.folder().join("opened");
    let go = case.folder().join("go");
    let release_path = case.folder().join("release");
    let release_file = shown(&release_path);
    let pid_file = case.folder().join("packaged.pid");
    let mut packaged = Packaged {
        process: None,
        files: [opened.clone(), go.clone(), release_path],
    };
    let program = format!("& '{FAKE_TOOL}' chatter '{}' wait '{}'", shown(&opened), shown(&go));
    // `pwsh` on PATH is a packaged app. fake-tool, in the job and with no package identity, starts it, and Windows
    // creates it in a job of its own, outside the command's.
    let seams = ["--test-stall-ms", "3000", "--test-sample-ms", "100"];
    let command = [
        FAKE_TOOL,
        "spawn-pid",
        &shown(&pid_file),
        &release_file,
        "pwsh",
        "-NoProfile",
        "-Command",
        &program,
    ];

    let running = case.start(&run_words("out.log", &seams, &command));
    let mut pid = None;
    wait_until("the packaged program's pid", || {
        pid = fs::read_to_string(&pid_file).ok().and_then(|text| text.trim().parse::<u32>().ok());
        pid.is_some()
    });
    let pid = pid.expect("checked above");
    let process = packaged.process.insert(
        StoppableProcess::open(pid)
            .expect("the program is opened")
            .expect("it runs: it chatters until `opened`"),
    );
    fs::write(&opened, "").expect("`opened` is written");
    let outcome = running.finish();
    // A program the kill reached has ended with the gate's 124 by now; one it missed ends on its own once released,
    // with fake-tool's status 0, rather than when fake-tool's `wait` gives up.
    fs::write(&go, "").expect("`go` is written");

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_eq!(release(&case)["verdict"], "stalled");
    let killed = Regex::new(&format!(
        r"(?m)^gate: terminated the job's 1 process\(es\) and 2 that left it \(pwsh\.exe {pid}, fake-tool\.exe \d+\)$"
    ))
    .expect("the pattern is valid");
    assert!(killed.is_match(&outcome.stderr), "no kill line naming pwsh.exe {pid} first: {outcome:?}");
    wait_until(&format!("the packaged program {pid} ending"), || {
        matches!(process.wait(Duration::ZERO), Ok(true))
    });
    let code = process.exit_code().expect("its status is read");
    assert_eq!(code, 124, "the kill did not reach the packaged program {pid}");
}

#[test]
fn a_passed_run_journals_its_load_adjusted_seconds() {
    let case = Case::new("a_passed_run_journals_its_load_adjusted_seconds");

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "hi"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let release = release(&case);
    assert!(release["adjustedSeconds"].is_number(), "{release}");
    assert_eq!(release["verdict"], "passed", "{release}");
}

#[test]
fn a_quiet_command_that_ran_a_short_child_is_still_stalled_and_its_process_lines_are_no_output() {
    let case = Case::new("a_quiet_command_that_ran_a_short_child_is_still_stalled_and_its_process_lines_are_no_output");
    let seams = ["--test-stall-ms", "1500", "--test-sample-ms", "100", "--test-stall-from-output"];
    let log_path = case.folder().join("out.log");

    // The command runs one child to its end, waiting for it, and only then writes its first byte, its `pid` line, which
    // opens the stall window; then it sleeps until killed. So the child has ended before the window opens, however long
    // it waits for CPU, and the child's spawn and exit lines must not count as the command's output.
    let outcome = case.run(&run_words("out.log", &seams, &[FAKE_TOOL, "run-each", "1", "pid", "sleep", UNTIL_KILLED]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
    let text = log_texts(&log_path);
    let command = said_pid(&text, "pid ");
    let lines = process_lines(&log_path);
    let child_head = format!(" ppid={command} ");
    let child: Option<u32> = lines
        .iter()
        .find_map(|line| line.strip_prefix("spawn pid=")?.split_once(&child_head)?.0.parse().ok());
    // The child's lines are the log's either way: a `spawn pid=N ppid={command} …` line with its `exit pid=N code=0 …`
    // after it once it ran to its end, or one `spawn pid=N missed (ended before it could be opened)` line alone when it
    // ended before the job's watcher could open it.
    let ran_to_its_end = |child: u32| {
        exit_of(&lines, child)
            .and_then(|at| lines.get(at))
            .is_some_and(|line| line.starts_with(&format!("exit pid={child} code=0 ")))
    };
    let missed: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|line| line.starts_with("spawn pid=") && line.ends_with(" missed (ended before it could be opened)"))
        .collect();
    assert!(
        child.is_some_and(ran_to_its_end) || (child.is_none() && missed.len() == 1),
        "a spawn and an exit line of the command's child, which ran to its end, \
         or its one missed spawn line when it ended before it could be opened: {lines:#?}"
    );
    // The command's own bytes are its `pid` line on stdout, and it wrote nothing on stderr: every sample counts those,
    // or none yet when it came before the reader had the line, and never the process lines the log holds.
    let pid_line = format!("pid {command}\n");
    let counted = [": out 0 B, err 0 B, ".to_owned(), format!(": out {} B, err 0 B, ", pid_line.len())];
    let samples: Vec<&str> = text.lines().filter(|line| line.starts_with("gate:   sample at ")).collect();
    assert!(
        !samples.is_empty() || text.contains("gate:   ... diagnostics cut at "),
        "the block lists its samples:\n{text}"
    );
    for sample in samples {
        assert!(
            counted.iter().any(|said| sample.contains(said.as_str())),
            "the sample counts only the command's bytes, one of {counted:?}: {sample}"
        );
    }
    assert_eq!(release(&case)["verdict"], "stalled", "{outcome:?}");
}
