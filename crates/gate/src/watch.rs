//! The watch of a running command: a sample of the system's and the job's CPU, the job's processes and the log's size
//! every sample period, the load-adjusted clock, the stall, the ceiling and the backstop they drive, the diagnostics a
//! kill gathers before it terminates the job, and the end of the wait: the command's exit, a kill as stalled, timed out
//! or past the backstop, or a Ctrl+C.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gate_args::{RunRequest, Slot};
use gate_watchdog::{
    Config, JOB_LINES, Machine, OUTSIDE_LINES, ProcessFacts, Report, Sample, SampleFacts, SampleHistory, ServerFacts, SystemTimes, Ticks, Watch,
    WslVm, failed_block, is_wsl_command, lines, wsl_unavailable_line,
};
use gate_win32::{Interrupt, Job, ProcessTimes, Running, Waited};
use jiff::Timestamp;
use proc_table::{ProcessInfo, ProcessTable, ProcessTree, Win32ProcessTable, filetime_to_timestamp};

use crate::console::Console;
use crate::log::Log;
use crate::nesting::Parent;
use crate::pause::PauseReader;
use crate::resolve::ResolvedRoot;
use crate::servers::{Scope, Servers, counted};
use crate::slots::{Pool, ThisGate, epoch_ms};

/// How long the watch waits for the command between two samples.
const SAMPLE_PERIOD: Duration = Duration::from_secs(5);
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
    /// The command was killed as stalled, timed out or past the backstop: the line that says so, the processes its
    /// job held as the kill was found, and the diagnostics block gathered for it.
    Killed {
        line: String,
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
    log: &'a Log,
    /// The command's start as a `FILETIME`, read before it was created: a process of the job created before it is
    /// left out of the sample's processes.
    since: u64,
    started: Instant,
    period: Duration,
    interrupt: Option<Interrupt>,
    /// The command's words joined by spaces, as the kill line names it.
    spoken: String,
    watch: Watch,
    /// The samples taken, as the diagnostics block lists them, oldest first.
    history: SampleHistory,
    /// The build servers outside the job, and the command lines read for them so far.
    servers: Servers,
    /// The folder a build server's program must lie under, `None` when the scan is machine-wide
    /// (`--test-servers-under`).
    under: Option<PathBuf>,
    /// The build servers the last sample counted, each with whether it started after the command.
    counted: Vec<(u32, bool)>,
    sampler_works: bool,
    /// Whether every sample fails (`--test-sampler-fail`).
    sampler_fail: bool,
    /// Whether gathering a kill's diagnostics fails (`--test-diagnostics-fail`).
    diagnostics_fail: bool,
    /// The samples taken so far, which `--test-interrupt-after-samples` counts.
    samples: u64,
    /// After how many samples the gate raises the interrupt itself (`--test-interrupt-after-samples`).
    interrupt_after: Option<u64>,
    vm: VmReader<VmRead>,
    /// The pause markers of the gates under this one: a gate under it waiting behind a critical request is the run's
    /// progress.
    pause: PauseReader<'a>,
}

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
#[derive(Clone, Copy)]
pub(crate) struct Watching<'a> {
    pub job: &'a Job,
    pub running: &'a Running,
    pub log: &'a Log,
    pub interrupt: Option<Interrupt>,
    /// The slot kind the command runs as, whose machine the watch measures it on: critical under a critical parent's
    /// slot, else the gate's own.
    pub runs_as: Slot,
    /// The state root whose pause markers name the gates waiting under this one.
    pub root: &'a ResolvedRoot,
    /// This gate, whose name the pause markers of the gates under it carry.
    pub gate: &'a ThisGate,
    /// The parent whose marker this gate hands its command, when that parent's slot covers it or is lent to it: the
    /// gates under that command name their pause markers after the parent.
    pub hands_on: Option<&'a Parent>,
}

impl<'a> Watcher<'a> {
    /// Begins the watch of `run`, which `request` started at `start` on a machine of `processors` logical processors,
    /// from the system's and the job's CPU times read now, and the WSL VM's for a wsl command.
    ///
    /// # Errors
    ///
    /// The system's error when those times cannot be read.
    pub(crate) fn begin(request: &RunRequest, run: Watching<'a>, start: &Start, processors: usize) -> io::Result<Watcher<'a>> {
        let system = system_times()?;
        let job_cpu = Ticks(run.job.accounting()?.cpu);
        let config = config(request);
        // A command that is not a wsl one reads nothing.
        let read: VmRead = if config.wsl { gate_win32::wsl_vm_cpu } else { || Ok(None) };
        let mut vm = VmReader::new(read);
        let vm_cpu = vm.baseline();
        let spoken: Vec<String> = request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect();
        Ok(Watcher {
            job: run.job,
            running: run.running,
            log: run.log,
            since: start.since,
            started: start.started,
            period: request.test_seams.sample_ms.map_or(SAMPLE_PERIOD, Duration::from_millis),
            interrupt: run.interrupt,
            spoken: spoken.join(" "),
            watch: Watch::new(config, machine(run.runs_as, processors), system, job_cpu, vm_cpu),
            history: SampleHistory::default(),
            servers: Servers::new(),
            under: request.test_seams.servers_under.as_deref().map(absolute),
            counted: Vec::new(),
            sampler_works: true,
            sampler_fail: request.test_seams.sampler_fail,
            diagnostics_fail: request.test_seams.diagnostics_fail,
            samples: 0,
            interrupt_after: request.test_seams.interrupt_after_samples,
            vm,
            pause: PauseReader::new(run.root, run.gate, run.hands_on),
        })
    }

    /// Waits for the command to exit, sampling it every period, until it exits, is killed or the console is
    /// interrupted. A sample that fails is said once, and the watch goes on by wall time alone.
    ///
    /// # Errors
    ///
    /// The system's error when the wait fails, or when the job's processes cannot be counted for a kill.
    pub(crate) fn until_end(&mut self, console: &mut Console) -> io::Result<Watched> {
        loop {
            match self.wait(self.period)? {
                Waited::Exited(code) => {
                    self.watch.exited(self.started.elapsed());
                    return Ok(Watched::Exited(code));
                }
                Waited::Interrupted => {
                    self.watch.exited(self.started.elapsed());
                    return Ok(Watched::Interrupted);
                }
                Waited::Running => {}
            }
            self.take_sample(console);
            self.samples += 1;
            if let Some(interrupt) = self.interrupt.filter(|_| self.interrupt_after == Some(self.samples)) {
                interrupt.raise();
            }
            if let Some(watched) = self.kill()? {
                return Ok(watched);
            }
        }
    }

    /// The load-adjusted seconds the watch counted, unrounded; `None` once the sampler has failed, as they are unknown.
    pub(crate) fn adjusted_seconds(&self) -> Option<f64> {
        self.sampler_works.then(|| self.watch.adjusted_seconds())
    }

    /// The wall time the watch counted as paused, while a gate under this one waited behind a critical request.
    pub(crate) fn paused_time(&self) -> Duration {
        self.watch.paused_time()
    }

    /// Waits at most `timeout` for the command's exit, or for the interrupt when it was installed.
    fn wait(&self, timeout: Duration) -> io::Result<Waited> {
        match self.interrupt {
            Some(interrupt) => self.running.wait_or_interrupt(timeout, interrupt),
            None => Ok(self.running.wait(timeout)?.map_or(Waited::Running, Waited::Exited)),
        }
    }

    /// Takes one sample, or moves the clock by wall time once the sampler has failed.
    fn take_sample(&mut self, console: &mut Console) {
        let at = self.started.elapsed();
        // A gate under this one waiting behind a critical request is progress, and the wall-only path counts it too.
        let paused = self.paused(console);
        if !self.sampler_works {
            self.watch.wall_only(at, paused);
            return;
        }
        let (vm, line) = self.vm.sample();
        if let Some(line) = line {
            console.err(&line);
        }
        match self.sample(at, vm.as_ref().ok().copied().flatten(), paused) {
            Ok((sample, log_bytes, err_bytes)) => {
                // Only a sample that came back whole becomes the one a kill's block lists the servers of; one that
                // failed part way leaves the last good sample's servers in place.
                self.counted = counted(&sample.new_servers, &sample.shared_servers);
                self.watch.sample(&sample);
                self.record(&sample, log_bytes, err_bytes, vm.is_err());
            }
            Err(error) => {
                self.sampler_works = false;
                console.err(&self.watch.sampler_failed(&error.to_string()));
                // The pause read this pass answers for the stretch the failed sample would have measured.
                self.watch.wall_only(at, paused);
            }
        }
    }

    /// Keeps one sample for the diagnostics block a kill writes: the sample's own figures, the WSL VM's CPU the watch
    /// found for a wsl command (`vm_failed` when this sample could not read it), the CPU its build servers have used
    /// and whether the job held a build client with them.
    fn record(&mut self, sample: &Sample, log_bytes: u64, err_bytes: u64, vm_failed: bool) {
        let wsl_vm = match (self.watch.last_vm_percent(), vm_failed) {
            (None, _) => WslVm::NotWatched,
            (Some(_), true) => WslVm::Unavailable,
            (Some(percent), false) => WslVm::Percent(percent),
        };
        self.history.push(SampleFacts {
            wall_seconds: sample.at.as_secs_f64(),
            adjusted_seconds: self.watch.adjusted_seconds(),
            // The sample just taken leaves the share it found, so this is `Some`; 100 is the watch's answer while
            // nothing has been counted.
            free_percent: self.watch.last_free_percent().unwrap_or(100),
            log_bytes,
            err_bytes,
            job_cpu: sample.job_cpu,
            wsl_vm,
            processes: u32::try_from(sample.pids.len()).unwrap_or(u32::MAX),
            new_servers_cpu: Ticks(sample.new_servers.values().copied().sum()),
            shared_servers_cpu: Ticks(sample.shared_servers.values().copied().sum()),
            build_client_in_job: sample.build_client,
        });
    }

    /// The kill due now, as stalled, timed out or past the backstop, or `None`; a command that exited as the kill was
    /// found keeps its own status, checked again once the diagnostics are gathered.
    fn kill(&mut self) -> io::Result<Option<Watched>> {
        let now = self.started.elapsed();
        let Some(kill) = self.watch.kill_check(now) else {
            return Ok(None);
        };
        if let Some(exited) = self.exit_if_ended()? {
            return Ok(Some(exited));
        }
        let diagnostics = self.diagnostics();
        if let Some(exited) = self.exit_if_ended()? {
            return Ok(Some(exited));
        }
        let processes = self.job.accounting()?.active_processes;
        Ok(Some(Watched::Killed {
            line: kill.line(&self.spoken),
            processes,
            diagnostics,
        }))
    }

    /// The command's own status once it has exited, or `None` while it still runs; the watch is brought to its end.
    fn exit_if_ended(&mut self) -> io::Result<Option<Watched>> {
        match self.running.wait(Duration::ZERO)? {
            Some(code) => {
                self.watch.exited(self.started.elapsed());
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
        let (job, job_unread) = job_facts(&tree, &ids, started);
        let (outside, outside_unread) = self.outside_facts(&tree, &ids, started);
        let cut = out_of_time(started);
        Ok(Report {
            job,
            job_unread,
            outside,
            outside_unread,
            servers: if cut { None } else { Some(self.server_facts(&tree)) },
            samples: self.history.to_vec(),
            cut_at_ms: if cut { Some(cut_at(started)) } else { None },
        })
    }

    /// The live descendants of the job's members and of the command that are not members themselves, breadth first,
    /// read up to the block's cap, with the number the cap or the budget left unread.
    fn outside_facts(&self, tree: &ProcessTree, ids: &[u32], started: Instant) -> (Vec<ProcessFacts>, usize) {
        let members: HashSet<u32> = ids.iter().copied().collect();
        let mut seen = members.clone();
        let mut found: Vec<&ProcessInfo> = Vec::new();
        let roots = ids.iter().copied().chain(std::iter::once(self.running.pid()));
        for root in roots {
            found.extend(
                tree.descendants(root)
                    .into_iter()
                    .filter(|info| seen.insert(info.pid) && self.after_start(info)),
            );
        }
        let mut facts = Vec::new();
        for (index, info) in found.iter().enumerate() {
            if index >= OUTSIDE_LINES || out_of_time(started) {
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

    /// The build servers the last sample counted, read again now for the block: the name from the gathering's
    /// snapshot, `(exited)` for one that no longer holds its pid, and its CPU time read again.
    fn server_facts(&self, tree: &ProcessTree) -> Vec<ServerFacts> {
        self.counted
            .iter()
            .map(|&(pid, after)| ServerFacts {
                pid,
                name: tree.get(pid).map_or_else(|| "(exited)".to_owned(), |info| info.name.clone()),
                cpu: gate_win32::process_times(pid).ok().flatten().map(|times| Ticks(times.cpu)),
                started_after_command: after,
            })
            .collect()
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

    /// The facts of one sample at `at`: the system's and the job's CPU, the job's processes created since the command
    /// started, the build servers outside the job and the build client inside it, the WSL VM's CPU `vm_cpu` read for
    /// it, whether a gate under this one waits behind a critical request (`paused`), and the size of the log and its
    /// `.err` file, the two kept apart for the diagnostics block. Under `--test-sampler-fail` every sample fails.
    fn sample(&mut self, at: Duration, vm_cpu: Option<u64>, paused: bool) -> io::Result<(Sample, u64, u64)> {
        if self.sampler_fail {
            return Err(io::Error::other(SAMPLER_FAIL));
        }
        let log_bytes = size(self.log.path())?;
        let err_bytes = size(self.log.err_path())?;
        let pids = self.pids()?;
        let processes = Win32ProcessTable.snapshot_without_command_lines();
        let scope = Scope {
            members: &pids,
            since: self.since,
            under: self.under.as_deref(),
        };
        let found = self
            .servers
            .read(&processes, &scope, gate_win32::process_times, proc_table::command_line_of);
        let sample = Sample {
            at,
            system: system_times()?,
            job_cpu: Ticks(self.job.accounting()?.cpu),
            output_size: log_bytes + err_bytes,
            pids,
            paused,
            new_servers: found.new_servers,
            shared_servers: found.shared_servers,
            build_client: found.build_client,
            vm_cpu,
        };
        Ok((sample, log_bytes, err_bytes))
    }

    /// The job's processes created at or after the command's start; one that exited since the job listed it is left
    /// out.
    fn pids(&self) -> io::Result<BTreeSet<u32>> {
        members(self.job.process_ids()?, self.since, gate_win32::process_times)
    }
}

/// What a run of `request` is watched for: its ceiling, its stall window, and whether it runs through WSL, as a command
/// whose first word names `wsl` does.
fn config(request: &RunRequest) -> Config {
    Config {
        timeout: request.timeout,
        stall: request
            .test_seams
            .stall_ms
            .map_or(Duration::from_secs(request.stall), Duration::from_millis),
        wsl: request.command.first().is_some_and(|word| is_wsl_command(&word.to_string_lossy())),
    }
}

/// Whether the gathering that began at `started` has used up its budget.
fn out_of_time(started: Instant) -> bool {
    started.elapsed().as_millis() >= DIAGNOSTICS_BUDGET_MS
}

/// The milliseconds the gathering that began at `started` is cut at.
fn cut_at(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The command's start as a process's start time reads it, or `None` when it is outside a [`Timestamp`]'s range.
fn start_instant(since: u64) -> Option<Timestamp> {
    i64::try_from(since).ok().and_then(filetime_to_timestamp)
}

/// `path` made absolute against the gate's working directory, so a relative `--test-servers-under` still matches the
/// program a command line names; a path that cannot be made absolute is left as it was given.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The job's processes of `ids` read in pid order, and the number the cap or the budget left unread: only the ones the
/// snapshot still has, since a pid it no longer lists has no facts to show.
fn job_facts(tree: &ProcessTree, ids: &[u32], started: Instant) -> (Vec<ProcessFacts>, usize) {
    let mut sorted: Vec<u32> = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut facts = Vec::new();
    for (index, &pid) in sorted.iter().enumerate() {
        if index >= JOB_LINES || out_of_time(started) {
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

/// Of the job's process `ids`, those `times` reads as created at or after `since`.
fn members(ids: impl IntoIterator<Item = u32>, since: u64, times: impl Fn(u32) -> io::Result<Option<ProcessTimes>>) -> io::Result<BTreeSet<u32>> {
    let mut pids = BTreeSet::new();
    for pid in ids {
        let read = match times(pid) {
            // A process the job created can always be queried; a denied pid was reused by another user's or a
            // protected process, so it is no member, as the PowerShell gate skips any process it cannot open.
            Err(error) if error.raw_os_error() == Some(ACCESS_DENIED) => None,
            read => read?,
        };
        if read.is_some_and(|times| times.created >= since) {
            pids.insert(pid);
        }
    }
    Ok(pids)
}

/// The machine a command of `slot` runs on: a critical command is High priority, and a light one is measured against
/// the light pool's threads where the machine has them.
fn machine(slot: Slot, processors: usize) -> Machine {
    let cores = u32::try_from(processors).unwrap_or(u32::MAX);
    let demand = match Pool::of(slot) {
        Some(pool) if slot == Slot::Light => processors.min(pool.threads()),
        _ => processors,
    };
    Machine::new(cores, u32::try_from(demand).unwrap_or(u32::MAX), slot == Slot::Critical)
}

/// The system's CPU times, as the watchdog takes them.
fn system_times() -> io::Result<SystemTimes> {
    let times = gate_win32::system_times()?;
    Ok(SystemTimes {
        idle: times.idle,
        kernel: times.kernel,
        user: times.user,
    })
}

/// The size of the file at `path`.
fn size(path: &Path) -> io::Result<u64> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| io::Error::new(error.kind(), format!("cannot read the size of {}: {error}", path.display())))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a sample that cannot be read must fail the test loudly")]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::io;
    use std::path::PathBuf;

    use gate_args::{RunRequest, Slot, Spelling, TestSeams};
    use gate_watchdog::wsl_unavailable_line;
    use gate_win32::ProcessTimes;

    use super::{ACCESS_DENIED, VmReader, config, machine, members};

    /// A heavy run of `command`.
    fn request(command: &[&str]) -> RunRequest {
        RunRequest {
            slot: Slot::Heavy,
            timeout: 60,
            log: PathBuf::from("out.log"),
            stall: 60,
            tail: 0,
            no_markers: false,
            local: false,
            state_dir: None,
            command: command.iter().map(OsString::from).collect(),
            test_seams: TestSeams::default(),
            print_request: false,
            spelling: Spelling::Cli,
        }
    }

    #[test]
    fn a_command_run_through_wsl_is_watched_as_a_wsl_one() {
        assert!(config(&request(&["wsl", "-e", "true"])).wsl, "wsl by name");
        assert!(config(&request(&["C:\\Windows\\System32\\wsl.exe", "-e", "true"])).wsl, "wsl.exe by path");
        assert!(!config(&request(&["cargo", "test"])).wsl, "any other command is not a wsl one");
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
    fn a_pid_denied_to_the_gate_is_no_member_but_another_failure_fails_the_sample() {
        let times = |pid: u32| match pid {
            1 => Ok(Some(ProcessTimes { created: 10, cpu: 0 })),
            2 => Ok(Some(ProcessTimes { created: 5, cpu: 0 })),
            3 => Ok(None),
            4 => Err(io::Error::from_raw_os_error(ACCESS_DENIED)),
            _ => Err(io::Error::other("the times cannot be read")),
        };

        let found = members([1, 2, 3, 4], 10, times).expect("a denied pid is no failure");
        assert_eq!(found, BTreeSet::from([1]), "created since, still running, and readable");
        assert!(members([1, 9], 10, times).is_err(), "any other failure fails the sample");
    }
}
