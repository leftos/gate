//! The machine's load over a short window, which a run holding the machine alone writes to its log at its start and
//! its end, as evidence of what else ran while it measured: the cores busy outside the gate's own tree, the busiest
//! processes outside it and what no listed process accounts for, the GPU's 3D use per adapter, and the claims of
//! other gates under the same state root. Every value that cannot be read becomes a line saying so; nothing here
//! fails the run.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Once;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use gate_state::v1::{self, Claim};
use gate_win32::{AdapterUse, Gpu3dWindow, Interrupt, Job, ProcessTimes, SystemTimes, process_times, system_times};
use proc_table::{ProcessInfo, Win32ProcessTable};

use crate::console::Console;
use crate::slots::{Liveness, epoch_ms, gate_liveness, is_claim_file};

/// How long a window measures, unless the suite's seam shortens it.
pub(crate) const WINDOW: Duration = Duration::from_secs(3);
/// How long past the window the GPU's reading may take before its line says it timed out.
const GPU_GRACE: Duration = Duration::from_secs(5);
/// How many processes a window lists at most, busiest first.
const LISTED: usize = 8;
/// The cores a process must use, strictly more than, to be listed.
const LISTED_FLOOR: f64 = 0.01;
/// `FILETIME` ticks (100 ns) in a second.
const TICKS_PER_SECOND: f64 = 10_000_000.0;
/// The idle process, whose time is the processors' idle time and never a load.
const IDLE_PID: u32 = 0;
/// The characters of another gate's command its load line shows at most, so the line stays far shorter than the
/// console tail's cut and the tail still finds the block.
const COMMAND_SHOWN: usize = 200;
/// The name of the thread that reads the GPU's 3D use.
const GPU_THREAD: &str = "gate-gpu-load";

/// Which end of the held machine a window measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum When {
    /// After the holds line, before the command starts.
    Start,
    /// After the command and what it left have ended, before the verdict.
    End,
}

impl When {
    fn as_str(self) -> &'static str {
        match self {
            When::Start => "start",
            When::End => "end",
        }
    }
}

/// The gate a window is measured for: its own pid and start, the state root its claims are under, the job of its
/// command once it has one, the window's length, and the console interrupt that ends the window early.
pub(crate) struct Measuring<'a> {
    pub pid: u32,
    pub start_ms: u64,
    pub root: &'a Path,
    pub job: Option<&'a Job>,
    pub window: Duration,
    pub interrupt: Option<Interrupt>,
}

/// One process's CPU over a window.
#[derive(Clone, Debug, PartialEq)]
struct ProcessLoad {
    name: String,
    pid: u32,
    cores: f64,
}

/// What one claim file of the state root says, as the window's lines show it.
#[derive(Clone, Debug, PartialEq)]
enum ClaimLine {
    /// Another gate's claim: its pid, the slot's kind and index, and its command.
    Held {
        pid: u32,
        kind: &'static str,
        index: u32,
        command: String,
    },
    /// A claims folder or file that could not be read, and why.
    Unreadable(String),
}

/// What a window measured, as its lines are made from.
struct Sample {
    seconds: f64,
    processors: usize,
    /// The machine's busy cores, or why they cannot be read.
    busy: Result<f64, String>,
    /// Every process whose CPU could be read at both ends of the window, the gate's own tree among them.
    processes: Vec<ProcessLoad>,
    /// The pids of the gate's own tree: the gate and its job's processes.
    own: HashSet<u32>,
    /// Each adapter's 3D use, or why it cannot be read.
    gpu: Result<Vec<AdapterUse>, String>,
    claims: Vec<ClaimLine>,
}

/// Measures the machine's load over `measuring`'s window and gathers its lines as notes for the log, never said on the
/// console: the window's whole cost is its length. The lines gathered, so the log's tail can leave them out.
pub(crate) fn log_load(when: When, measuring: &Measuring<'_>, processors: usize, console: &mut Console) -> Vec<String> {
    let mut own = HashSet::from([measuring.pid]);
    let listed_before = own_job(measuring.job, &mut own);
    let measured = measure(measuring.window, measuring.interrupt);
    let listed_after = own_job(measuring.job, &mut own);
    let mut problems: Vec<String> = listed_before.err().or(listed_after.err()).into_iter().collect();
    problems.extend(measured.problems);
    let live = |pid, start_ms| !matches!(gate_liveness(pid, start_ms), Liveness::Gone);
    let sample = Sample {
        seconds: measured.seconds,
        processors,
        busy: measured.busy,
        processes: measured.processes,
        own,
        gpu: measured.gpu,
        claims: claim_lines(read_claims(measuring.root), (measuring.pid, measuring.start_ms), live),
    };
    let mut said = lines(when, &sample);
    said.extend(problems.iter().map(|problem| format!("gate: load at {}: {problem}", when.as_str())));
    for line in &said {
        console.note(line);
    }
    said
}

/// Adds the pids of `job`'s processes to `own`, or says why they cannot be listed.
fn own_job(job: Option<&Job>, own: &mut HashSet<u32>) -> Result<(), String> {
    let Some(job) = job else { return Ok(()) };
    let pids = job
        .process_ids()
        .map_err(|error| format!("cannot list this gate's job: {error}; its processes count as outside this gate"))?;
    own.extend(pids);
    Ok(())
}

/// What the machine did over one window, and what could not be read of it.
struct Measured {
    seconds: f64,
    busy: Result<f64, String>,
    processes: Vec<ProcessLoad>,
    gpu: Result<Vec<AdapterUse>, String>,
    problems: Vec<String>,
}

/// Starts the GPU's query, reads every process's and the system's CPU times, waits `window` or until `interrupt` is
/// raised, reads them again and then reads the GPU's query, which has until the window plus [`GPU_GRACE`].
fn measure(window: Duration, interrupt: Option<Interrupt>) -> Measured {
    let deadline = Instant::now() + window + GPU_GRACE;
    let gpu = GpuReading::start();
    let mut problems = Vec::new();
    let before = match Win32ProcessTable.try_snapshot_without_command_lines() {
        Ok(listed) => first_reads(listed),
        Err(error) => {
            problems.push(format!("cannot list the processes: {error}"));
            Vec::new()
        }
    };
    let system_before = system_times();
    let clock = Instant::now();
    wait_window(window, interrupt, &mut problems);
    let system_after = system_times();
    let seconds = clock.elapsed().as_secs_f64();
    let processes = before.into_iter().filter_map(FirstRead::again).collect();
    Measured {
        seconds,
        busy: busy_share(system_before, system_after),
        processes,
        gpu: gpu.and_then(|reading| reading.finish(deadline)),
        problems,
    }
}

/// Waits `window`, or until `interrupt` is raised; a wait on the interrupt that fails is added to `problems`, and the
/// window is then waited out.
fn wait_window(window: Duration, interrupt: Option<Interrupt>, problems: &mut Vec<String>) {
    let Some(interrupt) = interrupt else {
        thread::sleep(window);
        return;
    };
    let clock = Instant::now();
    if let Err(error) = interrupt.wait(window) {
        problems.push(format!("cannot wait on the interrupt: {error}; the window was waited out"));
        thread::sleep(window.saturating_sub(clock.elapsed()));
    }
}

/// One process's CPU times at the window's start, and when they were read.
struct FirstRead {
    name: String,
    pid: u32,
    times: ProcessTimes,
    at: Instant,
}

impl FirstRead {
    /// The process's CPU over the time between its two reads, when it can be read again and is the same process.
    fn again(self) -> Option<ProcessLoad> {
        let now = process_times(self.pid).ok().flatten().filter(|now| now.created == self.times.created)?;
        let seconds = self.at.elapsed().as_secs_f64();
        Some(ProcessLoad {
            name: self.name,
            pid: self.pid,
            cores: gained_cores(self.times.cpu, now.cpu, seconds),
        })
    }
}

/// The first read of each process of `listed` whose times can be read, the idle process left out.
fn first_reads(listed: Vec<ProcessInfo>) -> Vec<FirstRead> {
    listed
        .into_iter()
        .filter(|process| process.pid != IDLE_PID)
        .filter_map(|process| {
            let times = process_times(process.pid).ok().flatten()?;
            Some(FirstRead {
                name: process.name,
                pid: process.pid,
                times,
                at: Instant::now(),
            })
        })
        .collect()
}

/// The GPU's 3D use read on a thread of its own, so a PDH call that hangs never holds the run past its deadline: the
/// thread starts the query, then finishes it when told to.
struct GpuReading {
    finish: Sender<()>,
    read: Receiver<Result<Vec<AdapterUse>, String>>,
}

impl GpuReading {
    /// Starts the thread and waits at most [`GPU_GRACE`] for its query to start.
    fn start() -> Result<GpuReading, String> {
        let (finish, told) = mpsc::channel::<()>();
        let (send, read) = mpsc::channel();
        let (started, has_started) = mpsc::channel::<()>();
        quiet_gpu_panics();
        thread::Builder::new()
            .name(GPU_THREAD.to_owned())
            .spawn(move || {
                // A panic drops the channels without a reading, which `finish` says as a line; it is caught so it ends
                // only this thread's reading.
                let _ = panic::catch_unwind(AssertUnwindSafe(|| read_gpu(&started, &told, &send)));
            })
            .map_err(|error| format!("cannot start its thread: {error}"))?;
        // A start that takes longer is said by `finish`, which then times out.
        let _ = has_started.recv_timeout(GPU_GRACE);
        Ok(GpuReading { finish, read })
    }

    /// Tells the thread to finish its query and waits for its reading until `deadline`; past it, the thread is left
    /// running and the reading is `timed out`.
    fn finish(self, deadline: Instant) -> Result<Vec<AdapterUse>, String> {
        // A thread whose start failed has sent its error already and reads nothing more.
        let _ = self.finish.send(());
        match self.read.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(reading) => reading,
            Err(RecvTimeoutError::Timeout) => Err("timed out".to_owned()),
            Err(RecvTimeoutError::Disconnected) => Err("its thread ended without a reading".to_owned()),
        }
    }
}

/// The GPU thread's work: starts the query and says so on `started`, then finishes it once `told` and sends the
/// reading on `send`.
fn read_gpu(started: &Sender<()>, told: &Receiver<()>, send: &Sender<Result<Vec<AdapterUse>, String>>) {
    let window = Gpu3dWindow::start();
    // A window that stopped waiting for the start no longer reads this; nothing is lost.
    let _ = started.send(());
    let reading = match window {
        // A window that went away without asking for the reading wants none.
        Ok(window) if told.recv().is_ok() => window.finish(),
        Ok(_) => return,
        Err(error) => Err(error),
    };
    // A window past its deadline no longer reads this; the reading is dropped.
    let _ = send.send(reading.map_err(|error| error.to_string()));
}

/// Leaves a panic of the GPU thread unsaid on the console, where the load is never said: its window says it in the log
/// as a reading its thread ended without. Any other thread's panic is said as before.
fn quiet_gpu_panics() {
    static QUIET: Once = Once::new();
    QUIET.call_once(|| {
        let said = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if thread::current().name() != Some(GPU_THREAD) {
                said(info);
            }
        }));
    });
}

/// The share of the processors' time spent busy between `before` and `after`, from 0 to 1, or why it cannot be known.
fn busy_share(before: io::Result<SystemTimes>, after: io::Result<SystemTimes>) -> Result<f64, String> {
    let (before, after) = match (before, after) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(error), _) | (_, Err(error)) => return Err(format!("cannot read the system's CPU times: {error}")),
    };
    let total = (after.kernel + after.user).saturating_sub(before.kernel + before.user);
    let idle = after.idle.saturating_sub(before.idle);
    if total == 0 {
        return Err("the system's CPU times did not move over the window".to_owned());
    }
    Ok(ticks_f64(total.saturating_sub(idle)) / ticks_f64(total))
}

/// The cores a process used gaining `after - before` ticks of CPU over `seconds`.
fn gained_cores(before: u64, after: u64, seconds: f64) -> f64 {
    if seconds <= 0.0 {
        return 0.0;
    }
    ticks_f64(after.saturating_sub(before)) / TICKS_PER_SECOND / seconds
}

/// A count of ticks as a float; a count past 2^53 loses its lowest digits, which no window's gain reaches.
#[expect(clippy::cast_precision_loss, reason = "a window's ticks stay far below 2^53")]
fn ticks_f64(ticks: u64) -> f64 {
    ticks as f64
}

/// Every claim file of the root's claims folder, by file name, each read or why it could not be; a folder that does
/// not exist holds none.
fn read_claims(root: &Path) -> Vec<Result<Claim, String>> {
    let dir = v1::slots_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![Err(format!("cannot read {}: {error}", dir.display()))],
    };
    let mut paths = BTreeMap::new();
    let mut read = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) if entry.file_name().to_str().is_some_and(is_claim_file) => {
                paths.insert(entry.file_name(), entry.path());
            }
            Ok(_) => {}
            Err(error) => read.push(Err(format!("cannot read {}: {error}", dir.display()))),
        }
    }
    for path in paths.into_values() {
        match v1::read_claim(&path) {
            Ok(claim) => read.push(Ok(claim)),
            // A claim deleted since the listing is a gate that released; it holds nothing.
            Err(_) if !path.exists() => {}
            Err(error) => read.push(Err(format!("cannot read {}: {error}", path.display()))),
        }
    }
    read
}

/// The lines of the claims `read` whose gate still runs, as `live` judges a gate by its pid and start in epoch
/// milliseconds (a gate that cannot be judged counts as live), those of the gate `own` left out. Each command is shown
/// as [`shown_command`] gives it, so each claim is one short line.
fn claim_lines(read: Vec<Result<Claim, String>>, own: (u32, u64), live: impl Fn(u32, u64) -> bool) -> Vec<ClaimLine> {
    read.into_iter()
        .filter_map(|claim| match claim {
            Ok(claim) => {
                let gate = (claim.pid, epoch_ms(claim.process_start));
                (gate != own && live(gate.0, gate.1)).then(|| ClaimLine::Held {
                    pid: claim.pid,
                    kind: claim.kind.as_str(),
                    index: claim.index,
                    command: shown_command(&claim.command),
                })
            }
            Err(problem) => Some(ClaimLine::Unreadable(problem)),
        })
        .collect()
}

/// `command` as one line of at most [`COMMAND_SHOWN`] characters: its line breaks become spaces, and a longer one is
/// cut after a whole character and ends in `…`.
fn shown_command(command: &str) -> String {
    let one_line = command.replace(['\r', '\n'], " ");
    if one_line.chars().count() <= COMMAND_SHOWN {
        return one_line;
    }
    one_line.chars().take(COMMAND_SHOWN - 1).chain(['…']).collect()
}

/// The lines of `sample` measured at `when`: the busy cores, the busiest processes outside the gate's own tree and the
/// remainder no listed process accounts for, the GPU's 3D use, and the other gates' claims.
fn lines(when: When, sample: &Sample) -> Vec<String> {
    let at = format!("gate: load at {}", when.as_str());
    let mut lines = cpu_lines(&at, sample);
    match &sample.gpu {
        Ok(adapters) => lines.extend(
            adapters
                .iter()
                .map(|adapter| format!("{at}: GPU 3D {:.0}% on adapter {}", adapter.percent, adapter.luid)),
        ),
        Err(why) => lines.push(format!("{at}: GPU 3D: counter unavailable ({why})")),
    }
    if sample.claims.is_empty() {
        lines.push(format!("{at}: other gates: none"));
    }
    lines.extend(sample.claims.iter().map(|claim| match claim {
        ClaimLine::Held { pid, kind, index, command } => format!("{at}: other gate {pid} holds {kind} {index}: {command}"),
        ClaimLine::Unreadable(problem) => format!("{at}: other gates: {problem}"),
    }));
    lines
}

/// The CPU lines of `sample`, each starting `at`: the headline, the busiest processes outside the gate's own tree and
/// the unattributed remainder; when the machine's busy cores cannot be read, the line saying why and the processes.
fn cpu_lines(at: &str, sample: &Sample) -> Vec<String> {
    let (own, outside): (Vec<&ProcessLoad>, Vec<&ProcessLoad>) = sample.processes.iter().partition(|process| sample.own.contains(&process.pid));
    let mut busiest: Vec<&ProcessLoad> = outside.iter().copied().filter(|process| process.cores > LISTED_FLOOR).collect();
    busiest.sort_by(|a, b| b.cores.total_cmp(&a.cores).then(a.pid.cmp(&b.pid)));
    busiest.truncate(LISTED);
    let listed = busiest
        .iter()
        .map(|process| format!("{at}:   {} {} {:.2} cores", process.name, process.pid, process.cores));
    let Ok(share) = sample.busy.as_ref() else {
        let why = sample.busy.as_ref().err().map_or("", String::as_str);
        return std::iter::once(format!("{at}: {why}")).chain(listed).collect();
    };
    let own_cores: f64 = own.iter().map(|process| process.cores).sum();
    let listed_cores: f64 = busiest.iter().map(|process| process.cores).sum();
    let busy = (share * processors_f64(sample.processors) - own_cores).max(0.0);
    let unattributed = (busy - listed_cores).max(0.0);
    let headline = format!(
        "{at} over {:.1} s: {busy:.2} of {} cores busy outside this gate",
        sample.seconds, sample.processors
    );
    std::iter::once(headline)
        .chain(listed)
        .chain(std::iter::once(format!("{at}:   unattributed {unattributed:.2} cores")))
        .collect()
}

/// A processor count as a float.
#[expect(clippy::cast_precision_loss, reason = "a processor count stays far below 2^53")]
fn processors_f64(processors: usize) -> f64 {
    processors as f64
}

#[cfg(test)]
mod tests {
    use gate_state::v1::ClaimKind;
    use jiff::Timestamp;

    use super::*;

    fn process(name: &str, pid: u32, cores: f64) -> ProcessLoad {
        ProcessLoad {
            name: name.to_owned(),
            pid,
            cores,
        }
    }

    fn sample(processes: Vec<ProcessLoad>, busy: Result<f64, String>) -> Sample {
        Sample {
            seconds: 3.04,
            processors: 16,
            busy,
            processes,
            own: HashSet::from([100, 101]),
            gpu: Ok(vec![AdapterUse {
                luid: "0x00000000_0x00013ec4".to_owned(),
                percent: 12.6,
            }]),
            claims: Vec::new(),
        }
    }

    fn claim(pid: u32, start: Timestamp, kind: ClaimKind, index: u32) -> Claim {
        Claim {
            kind,
            index,
            pid,
            process_start: start,
            started: start,
            cwd: r"C:\work".to_owned(),
            command: "cargo build".to_owned(),
            log: r"C:\work\.tmp\b.log".to_owned(),
            timeout_seconds: 60.0,
            remote: None,
            host: None,
            key: None,
            expected: None,
        }
    }

    #[test]
    fn a_load_window_says_each_line_in_its_exact_text() {
        let mut sampled = sample(vec![process("cargo.exe", 7, 1.5), process("idle.exe", 8, 0.004)], Ok(0.125));
        sampled.claims = vec![ClaimLine::Held {
            pid: 42,
            kind: "heavy",
            index: 2,
            command: "cargo test".to_owned(),
        }];
        assert_eq!(
            lines(When::Start, &sampled),
            [
                "gate: load at start over 3.0 s: 2.00 of 16 cores busy outside this gate",
                "gate: load at start:   cargo.exe 7 1.50 cores",
                "gate: load at start:   unattributed 0.50 cores",
                "gate: load at start: GPU 3D 13% on adapter 0x00000000_0x00013ec4",
                "gate: load at start: other gate 42 holds heavy 2: cargo test",
            ]
        );
    }

    #[test]
    fn a_load_window_lists_the_eight_busiest_processes_busiest_first() {
        let processes = (1..=10).map(|pid| process(&format!("p{pid}.exe"), pid, f64::from(pid) / 10.0)).collect();
        let listed = lines(When::End, &sample(processes, Ok(0.5)));
        let names: Vec<&str> = listed
            .iter()
            .skip(1)
            .take(LISTED)
            .map(|line| line.split_whitespace().nth(4).unwrap_or(""))
            .collect();
        assert_eq!(names, ["p10.exe", "p9.exe", "p8.exe", "p7.exe", "p6.exe", "p5.exe", "p4.exe", "p3.exe"]);
        assert_eq!(
            listed[LISTED + 1],
            "gate: load at end:   unattributed 2.80 cores",
            "the listed lines and the remainder add up to the headline"
        );
    }

    #[test]
    fn a_load_window_lists_no_process_at_or_under_a_hundredth_of_a_core() {
        let listed = lines(When::End, &sample(vec![process("a.exe", 1, 0.01), process("b.exe", 2, 0.011)], Ok(0.0)));
        assert_eq!(listed[1], "gate: load at end:   b.exe 2 0.01 cores");
        assert_eq!(listed[2], "gate: load at end:   unattributed 0.00 cores");
    }

    #[test]
    fn the_unattributed_load_is_never_below_zero() {
        let listed = lines(When::End, &sample(vec![process("a.exe", 1, 3.0)], Ok(0.0625)));
        assert_eq!(listed[0], "gate: load at end over 3.0 s: 1.00 of 16 cores busy outside this gate");
        assert_eq!(listed[2], "gate: load at end:   unattributed 0.00 cores");
    }

    #[test]
    fn the_load_of_the_gate_s_own_tree_is_neither_listed_nor_counted() {
        let processes = vec![
            process("gate.exe", 100, 2.0),
            process("cargo.exe", 101, 1.0),
            process("other.exe", 5, 0.5),
        ];
        let listed = lines(When::End, &sample(processes, Ok(0.25)));
        assert_eq!(listed[0], "gate: load at end over 3.0 s: 1.00 of 16 cores busy outside this gate");
        assert_eq!(listed[1], "gate: load at end:   other.exe 5 0.50 cores");
        assert_eq!(listed[2], "gate: load at end:   unattributed 0.50 cores");
    }

    #[test]
    fn a_load_window_without_the_system_s_times_says_why_and_still_lists_the_processes() {
        let listed = lines(
            When::End,
            &sample(vec![process("a.exe", 1, 0.5)], Err("cannot read the system's CPU times: x".to_owned())),
        );
        assert_eq!(listed[0], "gate: load at end: cannot read the system's CPU times: x");
        assert_eq!(listed[1], "gate: load at end:   a.exe 1 0.50 cores");
        assert!(listed[2].starts_with("gate: load at end: GPU 3D "), "{}", listed[2]);
    }

    #[test]
    fn a_load_window_without_the_gpu_counter_says_it_is_unavailable_and_why() {
        let mut sampled = sample(Vec::new(), Ok(0.0));
        sampled.gpu = Err("PdhAddEnglishCounterW returned 0xC0000BB8".to_owned());
        let listed = lines(When::Start, &sampled);
        assert_eq!(
            listed[2],
            "gate: load at start: GPU 3D: counter unavailable (PdhAddEnglishCounterW returned 0xC0000BB8)"
        );
    }

    #[test]
    fn a_load_window_with_no_other_claim_says_none() {
        let listed = lines(When::Start, &sample(Vec::new(), Ok(0.0)));
        assert_eq!(listed.last().map(String::as_str), Some("gate: load at start: other gates: none"));
    }

    #[test]
    fn the_load_s_claim_lines_leave_out_the_gate_s_own_claims_only() {
        let own_start = Timestamp::from_second(1_700_000_000).unwrap_or(Timestamp::UNIX_EPOCH);
        let other_start = Timestamp::from_second(1_700_000_100).unwrap_or(Timestamp::UNIX_EPOCH);
        let read = vec![
            Ok(claim(100, own_start, ClaimKind::Critical, 0)),
            Ok(claim(100, own_start, ClaimKind::Light, 1)),
            Ok(claim(100, other_start, ClaimKind::Heavy, 1)),
            Ok(claim(200, own_start, ClaimKind::Remote, 3)),
            Err("cannot read C:\\s\\gate-heavy-2.json: bad".to_owned()),
        ];
        let lines = claim_lines(read, (100, epoch_ms(own_start)), |_, _| true);
        let held = |pid, kind, index| ClaimLine::Held {
            pid,
            kind,
            index,
            command: "cargo build".to_owned(),
        };
        assert_eq!(
            lines,
            [
                held(100, "heavy", 1),
                held(200, "remote", 3),
                ClaimLine::Unreadable("cannot read C:\\s\\gate-heavy-2.json: bad".to_owned()),
            ]
        );
    }

    #[test]
    fn the_load_s_claim_lines_leave_out_the_claims_of_gates_no_longer_running() {
        let start = Timestamp::from_second(1_700_000_000).unwrap_or(Timestamp::UNIX_EPOCH);
        let read = vec![Ok(claim(300, start, ClaimKind::Heavy, 0)), Ok(claim(400, start, ClaimKind::Light, 2))];
        let lines = claim_lines(read, (100, 0), |pid, start_ms| pid == 400 && start_ms == epoch_ms(start));
        assert_eq!(
            lines,
            [ClaimLine::Held {
                pid: 400,
                kind: "light",
                index: 2,
                command: "cargo build".to_owned(),
            }],
            "the dead gate 300's claim is left out"
        );
    }

    #[test]
    fn another_gate_s_command_with_line_breaks_stays_one_load_line() {
        let start = Timestamp::from_second(1_700_000_000).unwrap_or(Timestamp::UNIX_EPOCH);
        let mut broken = claim(300, start, ClaimKind::Heavy, 0);
        broken.command = "cmd /c \"echo a\r\necho b\"\nc".to_owned();
        let mut sampled = sample(Vec::new(), Ok(0.0));
        sampled.claims = claim_lines(vec![Ok(broken)], (100, 0), |_, _| true);
        let said = lines(When::End, &sampled);
        assert_eq!(
            said.last().map(String::as_str),
            Some("gate: load at end: other gate 300 holds heavy 0: cmd /c \"echo a  echo b\" c")
        );
        assert!(said.iter().all(|line| !line.contains(['\r', '\n'])), "{said:#?}");
    }

    #[test]
    fn another_gate_s_long_command_is_cut_to_a_short_load_line() {
        let start = Timestamp::from_second(1_700_000_000).unwrap_or(Timestamp::UNIX_EPOCH);
        let mut long = claim(300, start, ClaimKind::Heavy, 0);
        long.command = "é".repeat(10_000);
        let mut sampled = sample(Vec::new(), Ok(0.0));
        sampled.claims = claim_lines(vec![Ok(long)], (100, 0), |_, _| true);
        let said = lines(When::End, &sampled);
        let last = said.last().map_or("", String::as_str);
        assert!(last.chars().count() < 4096, "{last}");
        assert!(last.ends_with('…'), "{last}");
        let shown = format!("gate: load at end: other gate 300 holds heavy 0: {}…", "é".repeat(COMMAND_SHOWN - 1));
        assert_eq!(last, shown, "the cut keeps whole characters");
    }

    #[test]
    fn the_load_s_busy_share_is_the_processors_time_less_their_idle_time() {
        let before = SystemTimes {
            idle: 100,
            kernel: 300,
            user: 100,
        };
        let after = SystemTimes {
            idle: 400,
            kernel: 800,
            user: 400,
        };
        assert_eq!(busy_share(Ok(before), Ok(after)), Ok(0.625));
        assert!(busy_share(Ok(before), Ok(before)).is_err());
        assert_eq!(gained_cores(0, 15_000_000, 3.0), 0.5);
    }
}
