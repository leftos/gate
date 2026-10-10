//! The watch of a running command: a sample of the system's and the job's CPU, the job's processes and the log's size
//! every sample period, the load-adjusted clock, the stall, the ceiling and the backstop they drive, the diagnostics a
//! kill gathers before it terminates the job, and the end of the wait: the command's exit, a kill as stalled, timed out
//! or past the backstop, or a Ctrl+C.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use gate_args::{RunRequest, Slot};
use gate_watchdog::{
    Config, JOB_LINES, Machine, OUTSIDE_LINES, ProcessFacts, Report, Sample, SampleFacts, SampleHistory, SystemTimes, Ticks, Watch, WslVm,
    failed_block, lines,
};
use gate_win32::{Interrupt, Job, ProcessTimes, Running, Waited};
use jiff::Timestamp;
use proc_table::{ProcessInfo, ProcessTable, ProcessTree, Win32ProcessTable, filetime_to_timestamp};

use crate::console::Console;
use crate::log::Log;
use crate::slots::Pool;

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
    sampler_works: bool,
    /// Whether every sample fails (`--test-sampler-fail`).
    sampler_fail: bool,
    /// Whether gathering a kill's diagnostics fails (`--test-diagnostics-fail`).
    diagnostics_fail: bool,
    /// The samples taken so far, which `--test-interrupt-after-samples` counts.
    samples: u64,
    /// After how many samples the gate raises the interrupt itself (`--test-interrupt-after-samples`).
    interrupt_after: Option<u64>,
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
}

impl<'a> Watcher<'a> {
    /// Begins the watch of `run`, which `request` started at `start` on a machine of `processors` logical processors,
    /// from the system's and the job's CPU times read now.
    ///
    /// # Errors
    ///
    /// The system's error when those times cannot be read.
    pub(crate) fn begin(request: &RunRequest, run: Watching<'a>, start: &Start, processors: usize) -> io::Result<Watcher<'a>> {
        let system = system_times()?;
        let job_cpu = Ticks(run.job.accounting()?.cpu);
        let config = Config {
            timeout: request.timeout,
            stall: request
                .test_seams
                .stall_ms
                .map_or(Duration::from_secs(request.stall), Duration::from_millis),
        };
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
            watch: Watch::new(config, machine(request.slot, processors), system, job_cpu),
            history: SampleHistory::default(),
            sampler_works: true,
            sampler_fail: request.test_seams.sampler_fail,
            diagnostics_fail: request.test_seams.diagnostics_fail,
            samples: 0,
            interrupt_after: request.test_seams.interrupt_after_samples,
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
        if !self.sampler_works {
            self.watch.wall_only(at, false);
            return;
        }
        match self.sample(at) {
            Ok((sample, log_bytes, err_bytes)) => {
                self.watch.sample(&sample);
                self.record(&sample, log_bytes, err_bytes);
            }
            Err(error) => {
                self.sampler_works = false;
                console.err(&self.watch.sampler_failed(&error.to_string()));
            }
        }
    }

    /// Keeps one sample for the diagnostics block a kill writes. No build server, WSL VM or build client is watched
    /// yet, so every sample says there were none.
    fn record(&mut self, sample: &Sample, log_bytes: u64, err_bytes: u64) {
        self.history.push(SampleFacts {
            wall_seconds: sample.at.as_secs_f64(),
            adjusted_seconds: self.watch.adjusted_seconds(),
            // The sample just taken leaves the share it found, so this is `Some`; 100 is the watch's answer while
            // nothing has been counted.
            free_percent: self.watch.last_free_percent().unwrap_or(100),
            log_bytes,
            err_bytes,
            job_cpu: sample.job_cpu,
            wsl_vm: WslVm::NotWatched,
            processes: u32::try_from(sample.pids.len()).unwrap_or(u32::MAX),
            new_servers_cpu: Ticks(0),
            shared_servers_cpu: Ticks(0),
            build_client_in_job: false,
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
            servers: if cut { None } else { Some(Vec::new()) },
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

    /// The facts of one sample at `at`: the system's and the job's CPU, the job's processes created since the command
    /// started, and the size of the log and its `.err` file, the two kept apart for the diagnostics block. Under
    /// `--test-sampler-fail` every sample fails.
    fn sample(&self, at: Duration) -> io::Result<(Sample, u64, u64)> {
        if self.sampler_fail {
            return Err(io::Error::other(SAMPLER_FAIL));
        }
        let log_bytes = size(self.log.path())?;
        let err_bytes = size(self.log.err_path())?;
        let sample = Sample {
            at,
            system: system_times()?,
            job_cpu: Ticks(self.job.accounting()?.cpu),
            output_size: log_bytes + err_bytes,
            pids: self.pids()?,
            paused: false,
        };
        Ok((sample, log_bytes, err_bytes))
    }

    /// The job's processes created at or after the command's start; one that exited since the job listed it is left
    /// out.
    fn pids(&self) -> io::Result<BTreeSet<u32>> {
        members(self.job.process_ids()?, self.since, gate_win32::process_times)
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
    use std::io;

    use gate_args::Slot;
    use gate_win32::ProcessTimes;

    use super::{ACCESS_DENIED, machine, members};

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
