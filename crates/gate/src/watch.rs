//! The watch of a running command: a sample of the system's and the job's CPU, the job's processes and the log's size
//! every sample period, the load-adjusted clock, the stall, the ceiling and the backstop they drive, the diagnostics a
//! kill gathers before it terminates the job, and the end of the wait: the command's exit, a kill as stalled, timed out
//! or past the backstop, or a Ctrl+C.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

use gate_args::{RunRequest, Slot};
use gate_state::{RunTrace, SlotKind};
use gate_watchdog::{
    Config, Cores, JOB_LINES, Kill, Machine, OUTSIDE_LINES, ProcessFacts, Report, SSH_STALL_LINE, Sample, SampleFacts, SampleHistory, SystemTimes,
    Ticks, Watch, WslVm, failed_block, is_ssh_command, is_wsl_command, lines, wsl_unavailable_line,
};
use gate_win32::{Interrupt, Job, Priority, ProcessTimes, Running, Waited};
use jiff::Timestamp;
use proc_table::{ProcessInfo, ProcessTable, ProcessTree, Win32ProcessTable, filetime_to_timestamp};

use crate::boxside::forward::OutputBytes;
use crate::console::Console;
use crate::durations::{Usual, hang_hint};
use crate::escaped::{self, Departed, Look, Recorded};
use crate::late::Late;
use crate::nesting::Parent;
use crate::pause::PauseReader;
use crate::resolve::ResolvedRoot;
use crate::slots::{Pool, Takes, ThisGate, epoch_ms};
use crate::trace;

/// How long the watch waits for the command between two samples.
const SAMPLE_PERIOD: Duration = Duration::from_secs(5);
/// The phase a local command's hang hint names.
const RUNNING_HERE: &str = "running here";
/// `ERROR_ACCESS_DENIED`, as a raw OS error.
const ACCESS_DENIED: i32 = 5;
/// The failure every sample has under `--test-sampler-fail`.
const SAMPLER_FAIL: &str = "--test-sampler-fail makes every sample fail (test seam only)";
/// The delay a gathering of the kill's diagnostics may take before it is cut, as the PowerShell gate's budget is.
const DIAGNOSTICS_BUDGET_MS: u128 = 2000;
/// The failure every gathering of the kill's diagnostics has under `--test-diagnostics-fail`.
const DIAGNOSTICS_FAIL: &str = "--test-diagnostics-fail makes the diagnostics fail (test seam only)";

/// How the wait for a watched command ended.
pub(crate) enum Watched {
    /// The command exited with this code.
    Exited(u32),
    /// The command was killed as stalled, timed out or past the backstop: the line that says so, the line said right
    /// after it on stderr and in the log but kept out of the kill's journal text (the routing hint of a stalled ssh
    /// command), the processes its job held as the kill was found, and the diagnostics block gathered for it.
    Killed {
        line: String,
        hint: Option<&'static str>,
        processes: u32,
        diagnostics: Vec<String>,
    },
    /// The console was interrupted while the command ran.
    Interrupted,
}

/// What one run's watch reads and counts.
pub(crate) struct Watcher<'a> {
    job: &'a Job,
    running: &'a Running,
    output: OutputBytes,
    /// The command's start as a `FILETIME`, read before it was created: a process of the job created before it is
    /// left out of the sample's processes.
    since: u64,
    started: Instant,
    period: Duration,
    interrupt: Option<Interrupt>,
    /// The command's words joined by spaces, as the kill line names it.
    spoken: String,
    /// The line a stalled kill says after its STALLED line: how to route an ssh command ([`is_ssh`]), `None` for any
    /// other command.
    stall_hint: Option<&'static str>,
    watch: Watch,
    /// The samples taken, as the diagnostics block lists them, oldest first.
    history: SampleHistory,
    /// The processes outside the job the last sample counted, by pid and start.
    counted_escaped: Vec<(u32, u64)>,
    /// The processes outside the job the samples have recorded, which the run's kill ends.
    recorded: &'a RefCell<Recorded>,
    sampler_works: bool,
    /// Why the job's memory of its ended members failed, once a sample has said so.
    memory_said: Option<String>,
    /// Whether every sample fails (`--test-sampler-fail`).
    sampler_fail: bool,
    /// Whether gathering a kill's diagnostics fails (`--test-diagnostics-fail`).
    diagnostics_fail: bool,
    /// The milliseconds a gathering of the kill's diagnostics may take before it is cut
    /// (`--test-diagnostics-budget-ms`), the gate's own budget unless the suite sets one.
    diagnostics_budget_ms: u128,
    /// The samples taken so far, which `--test-interrupt-after-samples` counts.
    samples: u64,
    /// After how many samples the gate raises the interrupt itself (`--test-interrupt-after-samples`).
    interrupt_after: Option<u64>,
    vm: VmReader<VmRead>,
    /// The pause markers of the gates under this one: a gate under it waiting behind a critical request is the run's
    /// progress.
    pause: PauseReader<'a>,
    /// The watch of the parent gate whose slot covers this one, and the slot it takes once that parent ends ([`Late`]);
    /// `None` for any other run.
    late: Option<Late<'a>>,
    /// When the last sample ended, or the watch began: the next sample comes a whole period after it, and while the late
    /// take waits for a slot, the watch wakes on its poll in between.
    sampled: Instant,
    /// The slot kind the command runs as ([`Watching::runs_as`]), and the gate's own kind, which it runs as once a late
    /// take gave it a slot of that kind.
    runs_as: Slot,
    kind: Slot,
    processors: usize,
    /// The hang hint of a run with a usual p90 ([`Watching::usual`]), which a run says once.
    hang: Option<HangHint>,
    /// The most processes the job held at one sample; `None` before the first.
    peak: Option<u32>,
    /// The priority class the command started in, read as the watch began; `None` when it could not be read.
    priority: Option<Priority>,
}

/// The usual p90 a run's hang hint compares with, and whether the hint was said.
#[derive(Clone, Copy, Debug)]
struct HangHint {
    usual: Usual,
    said: bool,
}

/// The CPU time in 100 ns ticks of the processes outside the job under its members, by pid and start, as a sample
/// carries them.
type EscapedCpu = BTreeMap<(u32, u64), u64>;

/// A read of the WSL VM's cumulative CPU in 100 ns ticks, `None` when no VM runs.
type VmRead = fn() -> io::Result<Option<u64>>;

/// How the watch reads the WSL VM's CPU, and whether it has said that a read failed, which it says once a run.
struct VmReader<R> {
    read: R,
    said: bool,
}

impl<R: FnMut() -> io::Result<Option<u64>>> VmReader<R> {
    /// A reader that has said nothing yet.
    fn new(read: R) -> Self {
        VmReader { read, said: false }
    }

    /// The VM's CPU as the watch begins: a read that fails leaves no baseline and says nothing, since the first
    /// sample whose own read fails says it.
    fn baseline(&mut self) -> Option<u64> {
        (self.read)().ok().flatten()
    }

    /// One sample's read, and the line to say when it is the first of the run that failed; a failure never fails the
    /// sample, and the next sample reads again.
    fn sample(&mut self) -> (io::Result<Option<u64>>, Option<String>) {
        let read = (self.read)();
        let line = match &read {
            Err(error) if !self.said => {
                self.said = true;
                Some(wsl_unavailable_line(&error.to_string()))
            }
            _ => None,
        };
        (read, line)
    }
}

/// The facts a watch begins from, read as the command starts: when it began, by both clocks.
pub(crate) struct Start {
    pub since: u64,
    pub started: Instant,
}

impl Start {
    /// Now, read just before the command is created.
    pub(crate) fn now() -> Start {
        Start {
            since: gate_win32::file_time_now(),
            started: Instant::now(),
        }
    }
}

/// The command's run, as the watch is told it: what it watches and how.
pub(crate) struct Watching<'a> {
    pub job: &'a Job,
    pub running: &'a Running,
    /// The bytes the command's readers have read from its stdout and stderr: the only output the stall counts.
    pub output: OutputBytes,
    /// The run's ceiling in whole seconds, as its take resolved it.
    pub ceiling: u64,
    /// The usual p90 of the run's take, past 3x which the watch says its hang hint once; `None` without history.
    pub usual: Option<Usual>,
    pub interrupt: Option<Interrupt>,
    /// The slot kind the command runs as, whose machine the watch measures it on: critical under a critical parent's
    /// slot, else the gate's own.
    pub runs_as: Slot,
    /// The state root whose pause markers name the gates waiting under this one.
    pub root: &'a ResolvedRoot,
    /// This gate, whose name the pause markers of the gates under it carry.
    pub gate: &'a ThisGate,
    /// The parent whose marker this gate hands its command, when that parent's slot covers it: the gates under that
    /// command name their pause markers after the parent.
    pub hands_on: Option<&'a Parent>,
    /// Where the samples record the processes outside the job, which the run's kill reads.
    pub recorded: &'a RefCell<Recorded>,
    /// The watch of the parent gate whose slot covers a heavy or light run, which takes a slot of the run's kind once
    /// that parent ends; `None` for any other run.
    pub late: Option<Late<'a>>,
}

/// One sample pass's gathered facts: the sample itself, the bytes of the command's stdout and stderr, the two kept apart
/// for the diagnostics block, and whether the pass's WSL VM read failed, which that block reports.
struct Gathered {
    sample: Sample,
    out_bytes: u64,
    err_bytes: u64,
    vm_failed: bool,
}

impl<'a> Watcher<'a> {
    /// Begins the watch of `run`, which `request` started at `start` on a machine of `processors` logical processors,
    /// from the system's and the job's CPU times read now, and the WSL VM's for a wsl command.
    ///
    /// # Errors
    ///
    /// The system's error when those times cannot be read.
    pub(crate) fn begin(request: &RunRequest, run: Watching<'a>, start: &Start, processors: usize) -> io::Result<Watcher<'a>> {
        let Watching {
            job,
            running,
            output,
            ceiling,
            usual,
            interrupt,
            runs_as,
            root,
            gate,
            hands_on,
            recorded,
            late,
        } = run;
        let system = system_times()?;
        let job_cpu = Ticks(job.accounting()?.cpu);
        let config = config(request, ceiling);
        // A command that is not a wsl one reads nothing.
        let read: VmRead = if config.wsl { gate_win32::wsl_vm_cpu } else { || Ok(None) };
        let mut vm = VmReader::new(read);
        let vm_cpu = vm.baseline();
        let spoken: Vec<String> = request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect();
        Ok(Watcher {
            job,
            running,
            output,
            since: start.since,
            started: start.started,
            period: request.test_seams.sample_ms.map_or(SAMPLE_PERIOD, Duration::from_millis),
            interrupt,
            spoken: spoken.join(" "),
            stall_hint: is_ssh(request).then_some(SSH_STALL_LINE),
            watch: Watch::new(config, machine(runs_as, processors), system, job_cpu, vm_cpu),
            history: SampleHistory::default(),
            counted_escaped: Vec::new(),
            recorded,
            sampler_works: true,
            memory_said: None,
            sampler_fail: request.test_seams.sampler_fail,
            diagnostics_fail: request.test_seams.diagnostics_fail,
            diagnostics_budget_ms: request.test_seams.diagnostics_budget_ms.map_or(DIAGNOSTICS_BUDGET_MS, u128::from),
            samples: 0,
            interrupt_after: request.test_seams.interrupt_after_samples,
            vm,
            pause: PauseReader::new(root, gate, hands_on),
            late,
            sampled: Instant::now(),
            runs_as,
            kind: request.slot,
            processors,
            hang: usual.map(|usual| HangHint { usual, said: false }),
            peak: None,
            // A class the handle cannot read, or one Windows never answers with, is not said.
            priority: running.priority().ok().flatten(),
        })
    }

    /// Where the run's time went, read now: the job's CPU and the processes it ever held (both left out when its
    /// accounting cannot be read), the most it held at one sample, the class the command started in, the machine's
    /// mean free share once an unpaused sample counted one, and the processes the job remembers that are worth naming
    /// ([`trace::children`]).
    pub(crate) fn trace(&self) -> RunTrace {
        let accounting = self.job.accounting().ok();
        RunTrace {
            cpu_seconds: accounting.map(|accounting| trace::ticks_as_seconds(accounting.cpu)),
            processes: accounting.map(|accounting| accounting.total_processes),
            peak_processes: self.peak,
            priority: self.priority.map(priority_name),
            load_free_share: self.watch.free_share(),
            children: trace::children(self.job.traced(), self.since, gate_win32::file_time_now()),
            phases: Vec::new(),
        }
    }

    /// Waits for the command to exit, sampling it every period, until it exits, is killed or the console is
    /// interrupted. A sample that fails is said once, and the watch goes on by wall time alone. Each wake also looks at
    /// the covering parent gate, or takes a pass of the wait for a slot once it has ended ([`Late::step`]); while that
    /// wait goes on, the watch wakes on its poll, and a wait still going when the watch ends ends with it.
    ///
    /// # Errors
    ///
    /// The system's error when the wait fails, or when the job's processes cannot be counted for a kill.
    pub(crate) fn until_end(&mut self, console: &mut Console) -> io::Result<Watched> {
        let watched = self.watch_to_end(console);
        if let Some(late) = &mut self.late {
            late.end(console);
        }
        watched
    }

    /// The loop of [`Watcher::until_end`].
    fn watch_to_end(&mut self, console: &mut Console) -> io::Result<Watched> {
        loop {
            let polling = self.late.as_ref().and_then(Late::polling);
            match self.wait(slice(self.period, self.sampled.elapsed(), polling))? {
                Waited::Exited(code) => {
                    self.end(console);
                    return Ok(Watched::Exited(code));
                }
                Waited::Interrupted => {
                    self.end(console);
                    return Ok(Watched::Interrupted);
                }
                Waited::Running => {}
            }
            if self.late.as_mut().is_some_and(|late| late.step(console)) {
                self.after_late_take(console);
            }
            if !sample_due(self.period, self.sampled.elapsed(), polling.is_some()) {
                continue;
            }
            self.take_sample(console);
            self.sampled = Instant::now();
            self.samples += 1;
            if let Some(interrupt) = self.interrupt.filter(|_| self.interrupt_after == Some(self.samples)) {
                interrupt.raise();
            }
            self.hint_if_hung(console);
            if let Some(watched) = self.kill(console)? {
                return Ok(watched);
            }
        }
    }

    /// Runs the command as the gate's own kind once a late take gave it a slot of that kind, or a heavy or light gate
    /// stands in for its ended parent, when a critical parent's slot had it run as critical ([`own_kind_after_take`]):
    /// the watch measures it on the machine of its own kind from the next sample on, and its processes still at High
    /// are lowered to below normal, as a command of its kind starts. A process that cannot be lowered is said.
    fn after_late_take(&mut self, console: &mut Console) {
        let Some(kind) = own_kind_after_take(self.runs_as, self.kind) else {
            return;
        };
        self.runs_as = kind;
        self.watch.run_on(machine(kind, self.processors));
        let pids = match self.job.process_ids() {
            Ok(pids) => pids,
            Err(error) => {
                console.err(&format!("gate: cannot list the command's processes to lower them from High: {error}"));
                return;
            }
        };
        for (pid, error) in lowered(&pids, gate_win32::priority_of, gate_win32::set_priority) {
            console.err(&format!("gate: could not lower the command's process {pid} from High: {error}"));
        }
    }

    /// The load-adjusted seconds the watch counted, unrounded; `None` once the sampler has failed, as they are unknown.
    pub(crate) fn adjusted_seconds(&self) -> Option<f64> {
        self.sampler_works.then(|| self.watch.adjusted_seconds())
    }

    /// Says the hang hint once, on the first sample at which the run has been running 3x its usual p90: the wall time
    /// since the command started less the time paused, as the backstop counts it, which is also how long its phase,
    /// [`RUNNING_HERE`], has run. It kills nothing.
    fn hint_if_hung(&mut self, console: &mut Console) {
        let running = self.started.elapsed().saturating_sub(self.watch.paused_time());
        let Some(hang) = self.hang.as_mut().filter(|hang| !hang.said) else {
            return;
        };
        let running = running.as_secs_f64();
        if let Some(hint) = hang_hint(running, hang.usual, RUNNING_HERE, running) {
            hang.said = true;
            console.err(&hint);
        }
    }

    /// The wall time the watch counted as paused, while a gate under this one waited behind a critical request.
    pub(crate) fn paused_time(&self) -> Duration {
        self.watch.paused_time()
    }

    /// The command's average, peak and free cores ([`Watch::cores`]); `None` before an unpaused sample, once the
    /// sampler has failed, and when its CPU could not be read at its end.
    pub(crate) fn cores(&self) -> Option<Cores> {
        self.watch.cores()
    }

    /// Brings the watch to the command's end: the job's accounting and the CPU of the processes outside the job the last
    /// sample counted are read once more, so the CPU since the last sample counts toward the cores, and the stretch
    /// since that sample is counted at the share it found. A job accounting read that fails leaves the cores unknown and
    /// is noted in the log only, since the run's verdict does not depend on it.
    fn end(&mut self, console: &mut Console) {
        if self.sampler_works {
            match self.final_read() {
                Ok((job_cpu, escaped)) => {
                    self.watch.final_cpu(job_cpu, &escaped);
                }
                Err(error) => {
                    self.watch.cores_unread();
                    console.note(&format!("gate: cannot read the command's CPU at its end: {error}; its cores are unknown"));
                }
            }
        }
        self.watch.exited(self.started.elapsed());
    }

    /// The job's total CPU now, and that of the processes outside the job the last sample counted that still run
    /// ([`escaped_cpu`]); only the job's accounting failing fails the read.
    fn final_read(&self) -> io::Result<(Ticks, EscapedCpu)> {
        let job_cpu = Ticks(self.job.accounting()?.cpu);
        let escaped = escaped_cpu(&self.counted_escaped, gate_win32::process_times);
        Ok((job_cpu, escaped))
    }

    /// Waits at most `timeout` for the command's exit, or for the interrupt when it was installed.
    fn wait(&self, timeout: Duration) -> io::Result<Waited> {
        match self.interrupt {
            Some(interrupt) => self.running.wait_or_interrupt(timeout, interrupt),
            None => Ok(self.running.wait(timeout)?.map_or(Waited::Running, Waited::Exited)),
        }
    }

    /// Takes one sample, or moves the clock by wall time once the sampler has failed. The sample is stamped twice:
    /// `began` before any of its reads, so the stall does not count the time its own reads took as the command's quiet
    /// time, and `at` with the system times and the job's accounting, so the interval covers the stretch those two
    /// measure.
    fn take_sample(&mut self, console: &mut Console) {
        let began = self.started.elapsed();
        // A gate under this one waiting behind a critical request is progress, and the wall-only path counts it too.
        let paused = self.paused(console);
        if !self.sampler_works {
            // A failed pass reads nothing: the stretch it answers for is stamped once, at its own end.
            self.watch.wall_only(self.started.elapsed(), paused);
            return;
        }
        match self.sample(console, began, paused) {
            Ok(Gathered {
                sample,
                out_bytes,
                err_bytes,
                vm_failed,
            }) => {
                // Only a sample that came back whole becomes the one the command's end reads again; one that failed
                // part way leaves the last good sample's processes in place.
                self.counted_escaped = sample.escaped.keys().copied().collect();
                self.watch.sample(&sample);
                self.record(&sample, out_bytes, err_bytes, vm_failed);
            }
            Err(error) => {
                self.sampler_works = false;
                console.err(&self.watch.sampler_failed(&error.to_string()));
                // The pause read this pass answers for the stretch the failed sample would have measured.
                self.watch.wall_only(self.started.elapsed(), paused);
            }
        }
    }

    /// Keeps one sample for the diagnostics block a kill writes: the sample's own figures, the WSL VM's CPU the watch
    /// found for a wsl command (`vm_failed` when this sample could not read it).
    fn record(&mut self, sample: &Sample, out_bytes: u64, err_bytes: u64, vm_failed: bool) {
        let wsl_vm = match (self.watch.last_vm_percent(), vm_failed) {
            (None, _) => WslVm::NotWatched,
            (Some(_), true) => WslVm::Unavailable,
            (Some(percent), false) => WslVm::Percent(percent),
        };
        // The sample just taken leaves the share it found, so this is `Some`; 100 is the watch's answer while nothing has
        // been counted.
        let figures = (self.watch.adjusted_seconds(), self.watch.last_free_percent().unwrap_or(100));
        self.history.push(sample_facts(sample, figures, (out_bytes, err_bytes), wsl_vm));
    }

    /// The kill due now, as stalled, timed out or past the backstop, or `None`; a command that exited as the kill was
    /// found keeps its own status, checked again once the diagnostics are gathered.
    fn kill(&mut self, console: &mut Console) -> io::Result<Option<Watched>> {
        let now = self.started.elapsed();
        let Some(kill) = self.watch.kill_check(now) else {
            return Ok(None);
        };
        if let Some(exited) = self.exit_if_ended(console)? {
            return Ok(Some(exited));
        }
        let diagnostics = self.diagnostics();
        if let Some(exited) = self.exit_if_ended(console)? {
            return Ok(Some(exited));
        }
        let processes = self.job.accounting()?.active_processes;
        let stalled = matches!(kill, Kill::Stalled { .. });
        Ok(Some(Watched::Killed {
            line: kill.line(&self.spoken),
            hint: self.stall_hint.filter(|_| stalled),
            processes,
            diagnostics,
        }))
    }

    /// The command's own status once it has exited, or `None` while it still runs; the watch is brought to its end.
    fn exit_if_ended(&mut self, console: &mut Console) -> io::Result<Option<Watched>> {
        match self.running.wait(Duration::ZERO)? {
            Some(code) => {
                self.end(console);
                Ok(Some(Watched::Exited(code)))
            }
            None => Ok(None),
        }
    }

    /// The diagnostics block for the kill found now: the facts gathered within the budget, or the heading and the
    /// failed line when the gathering failed.
    fn diagnostics(&self) -> Vec<String> {
        self.gather().map_or_else(|message| failed_block(&message), |report| lines(&report))
    }

    /// Everything the block shows, gathered now, or the message the block ends with when it failed.
    fn gather(&self) -> Result<Report, String> {
        if self.diagnostics_fail {
            return Err(DIAGNOSTICS_FAIL.to_owned());
        }
        let started = Instant::now();
        let before = self.job.process_ids().map_err(|error| error.to_string())?;
        let tree = ProcessTree::new(Win32ProcessTable.snapshot());
        // The snapshot can take a while: a process that joined the job meanwhile is a member of it like any other.
        let after = self.job.process_ids().map_err(|error| error.to_string())?;
        let ids: Vec<u32> = before.into_iter().chain(after).collect();
        // A member whose children the memory may lack is said by the samples and the kill's look, not by the block.
        let (departed, _) = escaped::departed(self.job);
        let (job, job_unread) = job_facts(&tree, &ids, started, self.diagnostics_budget_ms);
        let (outside, outside_unread) = self.outside_facts(&tree, &ids, &departed, started);
        let cut = out_of_time(started, self.diagnostics_budget_ms);
        Ok(Report {
            job,
            job_unread,
            outside,
            outside_unread,
            samples: self.history.to_vec(),
            cut_at_ms: if cut { Some(cut_at(started)) } else { None },
        })
    }

    /// The live descendants of the job's members and of the command that are not members themselves, breadth first,
    /// then the children of each `departed` member, none of them, as [`escaped::departed_children`] finds them, with
    /// their descendants, then each process outside the job the samples recorded that `tree` still lists as itself,
    /// with its descendants, as the kill ends them though the process that started it has ended; read up to the
    /// block's cap, with the number the cap or the budget left unread.
    fn outside_facts(&self, tree: &ProcessTree, ids: &[u32], departed: &[Departed], started: Instant) -> (Vec<ProcessFacts>, usize) {
        let children = tree.children();
        let mut seen: HashSet<u32> = ids.iter().copied().collect();
        let mut found: Vec<&ProcessInfo> = Vec::new();
        let roots = ids.iter().copied().chain(std::iter::once(self.running.pid()));
        for root in roots {
            found.extend(
                children
                    .descendants(root)
                    .into_iter()
                    .filter(|info| seen.insert(info.pid) && self.after_start(info)),
            );
        }
        for child in escaped::departed_children(tree, departed) {
            let under = std::iter::once(child).chain(children.descendants(child.pid));
            found.extend(under.filter(|info| seen.insert(info.pid) && self.after_start(info)));
        }
        for recorded in self.recorded.borrow().processes(tree) {
            let under = std::iter::once(recorded).chain(children.descendants(recorded.pid));
            found.extend(under.filter(|info| seen.insert(info.pid) && self.after_start(info)));
        }
        let mut facts = Vec::new();
        for (index, info) in found.iter().enumerate() {
            if index >= OUTSIDE_LINES || out_of_time(started, self.diagnostics_budget_ms) {
                return (facts, found.len() - index);
            }
            facts.push(process_facts(info.pid, info));
        }
        (facts, 0)
    }

    /// Whether `info` started at or after the command did.
    fn after_start(&self, info: &ProcessInfo) -> bool {
        start_instant(self.since).is_some_and(|since| info.created.is_some_and(|created| created >= since))
    }

    /// Whether a gate under this one waits behind a critical request now ([`PauseReader::sample`]); a read that fails
    /// counts as not paused, and the first of the watch is said, as the WSL VM's unreadable line is.
    fn paused(&mut self, console: &mut Console) -> bool {
        let job = self.job;
        let (paused, line) = self.pause.sample(epoch_ms(Timestamp::now()), &|| job.process_ids());
        if let Some(line) = line {
            console.err(&line);
        }
        paused
    }

    /// The facts of one sample that began at `began`: the bytes the command wrote to its stdout and its stderr, the
    /// two kept apart for the diagnostics block and no gate line among them, the job's processes created since the command started, the processes outside the job
    /// under its members and their CPU, the WSL VM's CPU for a wsl command (its unreadable line said once, when it
    /// first fails), and last the system's times and the job's accounting, stamped with the watch's end time: the
    /// interval the watch counts them over is exactly the stretch the end stamp closes. Under `--test-sampler-fail`
    /// every sample fails.
    fn sample(&mut self, console: &mut Console, began: Duration, paused: bool) -> io::Result<Gathered> {
        if self.sampler_fail {
            return Err(io::Error::other(SAMPLER_FAIL));
        }
        let (out_bytes, err_bytes) = self.output.read();
        let looked = escaped::now().ok_or_else(|| io::Error::other(escaped::NO_CLOCK))?;
        let before = self.job.process_ids()?;
        let started = members(before.iter().copied(), self.since, gate_win32::process_times)?;
        let processes = Win32ProcessTable.snapshot_without_command_lines();
        // The snapshot takes a while: a process that joined the job meanwhile is a member, not one that left it.
        let after = self.job.process_ids()?;
        let (departed, trouble) = escaped::departed(self.job);
        if let Some(line) = escaped::memory_line(&mut self.memory_said, trouble.as_deref()) {
            console.err(&line);
        }
        let tree = ProcessTree::new(processes);
        let nested_gate = holds_nested_gate(&tree, &started, std::process::id());
        let look = Look {
            before,
            after,
            since: looked,
            tree: &tree,
            departed: departed.clone(),
        };
        let escaped = outside_the_job(&mut self.recorded.borrow_mut(), look, gate_win32::process_times);
        // What the members that had ended before the look began started is recorded now, and they start nothing more;
        // a snapshot that failed recorded nothing under them, so they are kept.
        escaped::forget(self.job, &departed, looked, &tree);
        let (vm, line) = self.vm.sample();
        if let Some(line) = line {
            console.err(&line);
        }
        let system = system_times()?;
        let accounting = self.job.accounting()?;
        let job_cpu = Ticks(accounting.cpu);
        let at = self.started.elapsed();
        let active = accounting.active_processes;
        self.peak = Some(self.peak.map_or(active, |peak| peak.max(active)));
        let sample = Sample {
            began,
            at,
            system,
            job_cpu,
            output_size: out_bytes + err_bytes,
            pids: started.into_keys().collect(),
            paused,
            nested_gate,
            escaped,
            vm_cpu: vm.as_ref().ok().copied().flatten(),
        };
        Ok(Gathered {
            sample,
            out_bytes,
            err_bytes,
            vm_failed: vm.is_err(),
        })
    }
}

/// What one sample finds outside the job at `look` and records in `recorded` ([`Recorded::record`]): the CPU of each
/// recorded process as `times` reads it.
fn outside_the_job(recorded: &mut Recorded, look: Look<'_>, times: impl Fn(u32) -> io::Result<Option<ProcessTimes>>) -> EscapedCpu {
    recorded.record(look);
    recorded.cpu(times)
}

/// The facts the diagnostics block keeps of `sample`: its own figures, the watch's load-adjusted seconds and the free
/// share it found (`figures`), the bytes of the command's stdout and stderr, and the WSL VM's CPU.
fn sample_facts(sample: &Sample, figures: (f64, u32), sizes: (u64, u64), wsl_vm: WslVm) -> SampleFacts {
    let ((adjusted_seconds, free_percent), (out_bytes, err_bytes)) = (figures, sizes);
    SampleFacts {
        wall_seconds: sample.at.as_secs_f64(),
        adjusted_seconds,
        free_percent,
        out_bytes,
        err_bytes,
        job_cpu: sample.job_cpu,
        wsl_vm,
        processes: u32::try_from(sample.pids.len()).unwrap_or(u32::MAX),
        escaped_cpu: Ticks(sample.escaped.values().copied().sum()),
    }
}

/// The CPU `times` reads now for each of the processes outside the job the last sample `counted`, by pid and start. One
/// that has exited, whose times cannot be read, or whose pid a process with another start has taken since is skipped.
fn escaped_cpu(counted: &[(u32, u64)], times: impl Fn(u32) -> io::Result<Option<ProcessTimes>>) -> EscapedCpu {
    let mut escaped = BTreeMap::new();
    for &(pid, created) in counted {
        if let Ok(Some(read)) = times(pid)
            && read.created == created
        {
            escaped.insert((pid, created), read.cpu);
        }
    }
    escaped
}

/// What a run of `request` is watched for: its `ceiling` in whole seconds as its take resolved it, its stall window, and
/// whether it runs through WSL, as a command whose first word names `wsl` does.
fn config(request: &RunRequest, ceiling: u64) -> Config {
    Config {
        timeout: ceiling,
        stall: request
            .test_seams
            .stall_ms
            .map_or(Duration::from_secs(request.stall), Duration::from_millis),
        stall_from_output: request.test_seams.stall_from_output,
        wsl: request.command.first().is_some_and(|word| is_wsl_command(&word.to_string_lossy())),
    }
}

/// Whether `request`'s command runs through ssh, as a command whose first word names `ssh` does: its work runs on the
/// other machine, whose CPU this watch cannot see.
fn is_ssh(request: &RunRequest) -> bool {
    request.command.first().is_some_and(|word| is_ssh_command(&word.to_string_lossy()))
}

/// How long the watch waits for the command on this pass, `since` the last sample ended: what is left of the sample
/// period, or while a late take waits for a slot (`polling`), its poll when that is shorter; rounded up to a whole
/// millisecond, the wait's own unit, so a wait that ends a fraction of a millisecond early is not followed by one of 0.
fn slice(period: Duration, since: Duration, polling: Option<Duration>) -> Duration {
    let left = period.saturating_sub(since);
    let slice = polling.map_or(left, |poll| poll.min(left));
    let millis = slice.as_nanos().div_ceil(1_000_000);
    Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
}

/// Whether a pass samples, `since` the last sample ended: every pass that waited the rest of the period, and while a
/// late take waits for a slot (`polling`), only the first pass once a whole period has passed.
fn sample_due(period: Duration, since: Duration, polling: bool) -> bool {
    !polling || since >= period
}

/// The kind a command run as `runs_as` runs as once a late take gave the gate `kind` a slot of its own kind: its own,
/// when a critical parent's slot had it run as critical; `None` when it ran as its own kind already.
fn own_kind_after_take(runs_as: Slot, kind: Slot) -> Option<Slot> {
    (runs_as != kind).then_some(kind)
}

/// Lowers each process of `pids` that `class_of` reads at High to below normal through `set`; the processes that
/// could not be read or lowered, with why. A process that has ended meanwhile is passed over.
fn lowered(
    pids: &[u32],
    class_of: impl Fn(u32) -> io::Result<Option<Priority>>,
    set: impl Fn(u32, Priority) -> io::Result<Option<()>>,
) -> Vec<(u32, io::Error)> {
    let mut failed = Vec::new();
    for &pid in pids {
        let lowered = match class_of(pid) {
            Ok(Some(Priority::High)) => set(pid, Priority::BelowNormal).map(|_| ()),
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = lowered {
            failed.push((pid, error));
        }
    }
    failed
}

/// Whether the gathering that began at `started` has used up its `budget_ms` milliseconds.
fn out_of_time(started: Instant, budget_ms: u128) -> bool {
    started.elapsed().as_millis() >= budget_ms
}

/// The milliseconds the gathering that began at `started` is cut at.
fn cut_at(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The command's start as a process's start time reads it, or `None` when it is outside a [`Timestamp`]'s range.
fn start_instant(since: u64) -> Option<Timestamp> {
    i64::try_from(since).ok().and_then(filetime_to_timestamp)
}

/// The job's processes of `ids` read in pid order, and the number the cap or the budget left unread: only the ones the
/// snapshot still has, since a pid it no longer lists has no facts to show.
fn job_facts(tree: &ProcessTree, ids: &[u32], started: Instant, budget_ms: u128) -> (Vec<ProcessFacts>, usize) {
    let mut sorted: Vec<u32> = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut facts = Vec::new();
    for (index, &pid) in sorted.iter().enumerate() {
        if index >= JOB_LINES || out_of_time(started, budget_ms) {
            return (facts, sorted[index..].iter().filter(|&&pid| tree.get(pid).is_some()).count());
        }
        // A pid the snapshot no longer lists has exited since the job listed it, so it has no facts to show.
        if let Some(info) = tree.get(pid) {
            facts.push(process_facts(pid, info));
        }
    }
    (facts, 0)
}

/// One process's facts for the diagnostics block, each part `None` when the block shows `?` for it: a process that
/// exited since the snapshot, or one this user may not read.
fn process_facts(pid: u32, info: &ProcessInfo) -> ProcessFacts {
    ProcessFacts {
        pid,
        parent: Some(info.parent_pid),
        name: info.name.clone(),
        cpu: gate_win32::process_times(pid).ok().flatten().map(|times| Ticks(times.cpu)),
        working_set_bytes: gate_win32::working_set(pid).ok().flatten(),
        command_line: info.command_line.clone(),
    }
}

/// Whether the job's `members`, each by its pid and the creation time its own times gave (100 ns ticks since 1601), hold
/// a gate other than the watching gate `own`: the snapshot `tree` lists the pid as `gate.exe`, in any case, with that
/// same start. A member that ended and whose pid a newer process took before the snapshot is not taken for a gate, nor
/// is one the snapshot does not list or whose start it cannot read. A nested gate is progress while it lives; a process
/// outside the job counts for nothing here.
fn holds_nested_gate(tree: &ProcessTree, members: &BTreeMap<u32, u64>, own: u32) -> bool {
    members.iter().filter(|&(&pid, _)| pid != own).any(|(&pid, &created)| {
        tree.get(pid)
            .is_some_and(|info| info.created.is_some() && info.created == start_instant(created) && escaped::is_gate(&info.name))
    })
}

/// Of the job's process `ids`, those `times` reads as created at or after `since`, each with that creation time in
/// 100 ns ticks since 1601.
fn members(
    ids: impl IntoIterator<Item = u32>,
    since: u64,
    times: impl Fn(u32) -> io::Result<Option<ProcessTimes>>,
) -> io::Result<BTreeMap<u32, u64>> {
    let mut pids = BTreeMap::new();
    for pid in ids {
        let read = match times(pid) {
            // A process the job created can always be queried; a denied pid was reused by another user's or a
            // protected process, so it is no member, as the PowerShell gate skips any process it cannot open.
            Err(error) if error.raw_os_error() == Some(ACCESS_DENIED) => None,
            read => read?,
        };
        if let Some(times) = read.filter(|times| times.created >= since) {
            pids.insert(pid, times.created);
        }
    }
    Ok(pids)
}

/// The machine a command of `slot` runs on: a critical command is High priority, a light one is measured against the
/// light pool's threads where the machine has them, and a no-slot one against a single core.
pub(crate) fn machine(slot: Slot, processors: usize) -> Machine {
    let cores = u32::try_from(processors).unwrap_or(u32::MAX);
    let demand = match Takes::of(slot) {
        Takes::Pool(pool @ Pool::Local(SlotKind::Light)) => processors.min(pool.threads()),
        Takes::Nothing => 1,
        Takes::Pool(_) | Takes::Machine => processors,
    };
    Machine::new(cores, u32::try_from(demand).unwrap_or(u32::MAX), slot == Slot::Critical)
}

/// The word a release's `priority` gives `priority`.
fn priority_name(priority: Priority) -> &'static str {
    match priority {
        Priority::Idle => "idle",
        Priority::BelowNormal => "belowNormal",
        Priority::Normal => "normal",
        Priority::AboveNormal => "aboveNormal",
        Priority::High => "high",
        Priority::RealTime => "realTime",
    }
}

/// The system's CPU times, as the watchdog takes them.
pub(crate) fn system_times() -> io::Result<SystemTimes> {
    let times = gate_win32::system_times()?;
    Ok(SystemTimes {
        idle: times.idle,
        kernel: times.kernel,
        user: times.user,
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a sample that cannot be read must fail the test loudly")]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::io;
    use std::path::PathBuf;
    use std::time::Duration;

    use gate_args::{Ceiling, RunRequest, Slot, Spelling, TestSeams};
    use gate_watchdog::wsl_unavailable_line;
    use gate_win32::ProcessTimes;

    use gate_win32::Priority;

    use super::{
        ACCESS_DENIED, VmReader, config, holds_nested_gate, is_ssh, lowered, machine, members, own_kind_after_take, sample_due, sample_facts, slice,
    };

    /// The test moment `second` seconds past 10:00 on the test day.
    fn at(second: u32) -> jiff::Timestamp {
        format!("2026-10-01T10:00:0{second}Z").parse().expect("a test timestamp parses")
    }

    /// The snapshot entry of `pid`, running `name` since second `second`.
    fn listed(pid: u32, name: &str, second: u32) -> proc_table::ProcessInfo {
        proc_table::ProcessInfo {
            pid,
            parent_pid: 1,
            name: name.to_owned(),
            command_line: None,
            created: Some(at(second)),
        }
    }

    /// The `FILETIME` ticks of the test moment `second`, as a process's own times give its start.
    fn filetime(second: u32) -> u64 {
        u64::try_from(at(second).as_microsecond() * 10 + 116_444_736_000_000_000).expect("a test moment is after 1601")
    }

    #[test]
    fn only_a_member_listed_as_a_gate_with_its_own_start_other_than_this_one_is_a_nested_gate() {
        // This gate is pid 9. Member 6 started at second 1, ended, and a newer gate took its pid at second 2, before the
        // snapshot; member 8's start the snapshot cannot read.
        let mut unknown = listed(8, "gate.exe", 1);
        unknown.created = None;
        let tree = proc_table::ProcessTree::new(vec![
            listed(2, "gate.exe", 1),
            listed(3, "GATE.EXE", 1),
            listed(4, "gate.exe", 1),
            listed(5, "pwsh.exe", 1),
            listed(6, "gate.exe", 2),
            unknown,
            listed(9, "gate.exe", 1),
        ]);
        let holds = |members: &[u32]| {
            let started = members.iter().map(|&pid| (pid, filetime(1))).collect();
            holds_nested_gate(&tree, &started, 9)
        };

        assert!(holds(&[2]), "a member gate");
        assert!(holds(&[3]), "its image named in any case");
        assert!(!holds(&[5]), "a member that is no gate");
        assert!(!holds(&[7]), "a member the snapshot does not list");
        assert!(!holds(&[6]), "a newer gate that took an ended member's pid");
        assert!(!holds(&[8]), "a member whose start the snapshot cannot read");
        assert!(!holds(&[9]), "this gate itself");
        assert!(!holds(&[]), "pid 4, a gate outside the job, is no member");
    }

    #[test]
    fn a_sample_s_escaped_cpu_reaches_its_diagnostics_facts() {
        let sample = gate_watchdog::Sample {
            began: std::time::Duration::from_secs(9),
            at: std::time::Duration::from_secs(10),
            system: gate_watchdog::SystemTimes { idle: 0, kernel: 0, user: 0 },
            job_cpu: gate_watchdog::Ticks(1),
            output_size: 0,
            pids: BTreeSet::from([1]),
            paused: false,
            nested_gate: false,
            escaped: std::collections::BTreeMap::from([((2, 10), 15_000_000), ((3, 20), 10_000_000)]),
            vm_cpu: None,
        };

        let facts = sample_facts(&sample, (4.0, 50), (0, 0), gate_watchdog::WslVm::NotWatched);

        assert_eq!(facts.escaped_cpu, gate_watchdog::Ticks(25_000_000), "both escapees' CPU");
    }

    /// A heavy run of `command`.
    fn request(command: &[&str]) -> RunRequest {
        RunRequest {
            slot: Slot::Heavy,
            timeout: Ceiling::Given(60),
            log: PathBuf::from("out.log"),
            stall: 60,
            tail: 0,
            no_markers: false,
            local: false,
            alone: false,
            builder: None,
            reason: None,
            bring_tracked: Vec::new(),
            env: Vec::new(),
            state_dir: None,
            cd: None,
            command: command.iter().map(OsString::from).collect(),
            test_seams: TestSeams::default(),
            print_request: false,
            spelling: Spelling::Cli,
        }
    }

    #[test]
    fn a_command_run_through_wsl_is_watched_as_a_wsl_one() {
        assert!(config(&request(&["wsl", "-e", "true"]), 60).wsl, "wsl by name");
        assert!(
            config(&request(&["C:\\Windows\\System32\\wsl.exe", "-e", "true"]), 60).wsl,
            "wsl.exe by path"
        );
        assert!(!config(&request(&["cargo", "test"]), 60).wsl, "any other command is not a wsl one");
    }

    #[test]
    fn a_command_run_through_ssh_is_watched_as_an_ssh_one() {
        assert!(is_ssh(&request(&["ssh", "host", "make"])), "ssh by name");
        assert!(
            is_ssh(&request(&["C:\\Windows\\System32\\OpenSSH\\ssh.exe", "host", "make"])),
            "ssh.exe by path"
        );
        assert!(
            !is_ssh(&request(&["git", "fetch", "ssh"])),
            "a later word naming ssh is not an ssh command"
        );
        assert!(!is_ssh(&request(&["wsl", "ssh", "host"])), "a wsl command running ssh is a wsl one");
    }

    /// A VM reader that reads `reads` in turn, then no VM.
    fn scripted(reads: Vec<io::Result<Option<u64>>>) -> VmReader<impl FnMut() -> io::Result<Option<u64>>> {
        let mut reads = reads.into_iter();
        VmReader::new(move || reads.next().unwrap_or(Ok(None)))
    }

    #[test]
    fn the_vm_unavailable_line_is_said_once_at_the_first_failed_read() {
        let mut vm = scripted(vec![
            Err(io::Error::other("first")),
            Err(io::Error::other("second")),
            Ok(Some(5)),
            Err(io::Error::other("third")),
        ]);
        let samples: Vec<(bool, Option<String>)> = (0..4)
            .map(|_| {
                let (read, line) = vm.sample();
                (read.is_err(), line)
            })
            .collect();
        assert_eq!(
            samples,
            vec![(true, Some(wsl_unavailable_line("first"))), (true, None), (false, None), (true, None)],
            "every failure reads as one, and only the first is said"
        );
    }

    #[test]
    fn a_failed_baseline_followed_by_a_good_sample_says_nothing() {
        let mut vm = scripted(vec![Err(io::Error::other("begin")), Ok(Some(5))]);
        assert_eq!(vm.baseline(), None, "a failed read leaves no baseline");
        let (read, line) = vm.sample();
        assert_eq!(read.ok(), Some(Some(5)));
        assert_eq!(line, None, "the begin read's failure is not said once a sample reads");
    }

    #[test]
    fn a_light_command_asks_for_the_light_pool_s_threads_where_the_machine_has_them() {
        let light = machine(Slot::Light, 16);
        assert_eq!((light.cores, light.demand, light.high), (16, 2, false));
        let small = machine(Slot::Light, 1);
        assert_eq!((small.cores, small.demand, small.high), (1, 1, false));
    }

    #[test]
    fn a_heavy_command_asks_for_every_core_and_a_critical_one_is_high() {
        let heavy = machine(Slot::Heavy, 16);
        assert_eq!((heavy.cores, heavy.demand, heavy.high), (16, 16, false));
        let critical = machine(Slot::Critical, 16);
        assert_eq!((critical.cores, critical.demand, critical.high), (16, 16, true));
    }

    #[test]
    fn a_no_slot_command_asks_for_one_core() {
        let none = machine(Slot::None, 16);
        assert_eq!((none.cores, none.demand, none.high), (16, 1, false));
    }

    #[test]
    fn a_watch_wakes_on_the_late_take_s_poll_and_still_samples_once_a_period() {
        let period = Duration::from_secs(5);
        let poll = Duration::from_secs(2);
        assert_eq!(slice(period, Duration::ZERO, None), period, "no late wait: a whole period after a sample");
        assert_eq!(
            slice(period, Duration::from_secs(1), None),
            Duration::from_secs(4),
            "no late wait: what is left of the period, so a wait that took the slot does not stretch it"
        );
        assert!(sample_due(period, Duration::ZERO, false), "no late wait: every pass samples");
        assert_eq!(slice(period, Duration::ZERO, Some(poll)), poll, "the poll, while a period is ahead");
        assert_eq!(
            slice(period, Duration::from_secs(4), Some(poll)),
            Duration::from_secs(1),
            "cut to what is left of the period"
        );
        assert_eq!(slice(period, Duration::from_secs(6), Some(poll)), Duration::ZERO, "a sample already due");
        assert!(
            !sample_due(period, Duration::from_secs(4), true),
            "a pass inside the period does not sample"
        );
        assert!(sample_due(period, period, true), "the first pass a whole period on samples");
    }

    #[test]
    fn a_slice_is_rounded_up_to_a_whole_millisecond_so_no_wait_of_0_ms_repeats() {
        let period = Duration::from_secs(5);
        let poll = Duration::from_secs(2);
        let almost = Duration::from_micros(4_999_700);
        assert_eq!(slice(period, almost, Some(poll)), Duration::from_millis(1), "300 µs left waits 1 ms");
        assert_eq!(slice(period, almost, None), Duration::from_millis(1));
        let after = slice(period, almost, Some(poll));
        assert!(sample_due(period, almost + after, true), "the pass after that wait samples");
    }

    #[test]
    fn only_a_command_run_as_critical_switches_to_its_own_kind_at_a_late_take() {
        assert_eq!(own_kind_after_take(Slot::Critical, Slot::Heavy), Some(Slot::Heavy));
        assert_eq!(own_kind_after_take(Slot::Critical, Slot::Light), Some(Slot::Light));
        assert_eq!(own_kind_after_take(Slot::Heavy, Slot::Heavy), None, "it ran as its own kind already");
        assert_eq!(own_kind_after_take(Slot::Light, Slot::Light), None);
    }

    #[test]
    fn only_the_command_s_processes_at_high_are_lowered_and_each_failure_is_kept() {
        let set_calls = std::cell::RefCell::new(Vec::new());
        let class_of = |pid: u32| match pid {
            1 | 4 => Ok(Some(Priority::High)),
            2 => Ok(Some(Priority::Normal)),
            3 => Ok(None),
            _ => Err(io::Error::other("cannot read")),
        };
        let set = |pid: u32, priority: Priority| {
            set_calls.borrow_mut().push((pid, priority));
            if pid == 4 { Err(io::Error::other("denied")) } else { Ok(Some(())) }
        };
        let failed: Vec<u32> = lowered(&[1, 2, 3, 4, 5], class_of, set).into_iter().map(|(pid, _)| pid).collect();
        assert_eq!(failed, [4, 5], "the one that could not be set and the one that could not be read");
        assert_eq!(
            set_calls.into_inner(),
            [(1, Priority::BelowNormal), (4, Priority::BelowNormal)],
            "only the processes at High are moved, to below normal"
        );
    }

    #[test]
    fn a_pid_denied_to_the_gate_is_no_member_but_another_failure_fails_the_sample() {
        let times = |pid: u32| match pid {
            1 => Ok(Some(ProcessTimes { created: 10, cpu: 0 })),
            2 => Ok(Some(ProcessTimes { created: 5, cpu: 0 })),
            3 => Ok(None),
            4 => Err(io::Error::from_raw_os_error(ACCESS_DENIED)),
            _ => Err(io::Error::other("the times cannot be read")),
        };

        let found = members([1, 2, 3, 4], 10, times).expect("a denied pid is no failure");
        assert_eq!(
            found.into_keys().collect::<BTreeSet<u32>>(),
            BTreeSet::from([1]),
            "created since, still running, and readable"
        );
        assert!(members([1, 9], 10, times).is_err(), "any other failure fails the sample");
    }
}
