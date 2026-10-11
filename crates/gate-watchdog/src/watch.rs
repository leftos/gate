//! The state of one watched command: the load-adjusted clock the samples advance, and the share of the machine it
//! counts at.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::share::{Machine, SystemTimes, Ticks, free_share};

/// The shortest interval a sample may measure, so two samples at the same instant still count a millisecond.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

/// The wall-time nanoseconds one tick of the CPU floor stands for: the 1% of a core that a 100 ns tick of CPU counts,
/// so 10 000.
const FLOOR_NANOS_PER_TICK: u128 = 10_000;

/// The wall-time nanoseconds one tick of the WSL VM's floor stands for: the tenth of one core over the window that a
/// 100 ns tick of the VM's CPU counts, so a thousand.
const VM_FLOOR_NANOS_PER_TICK: u128 = 1_000;

/// What kills a watched command, and when.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The ceiling on load-adjusted time, in whole seconds as the PowerShell gate's `[int]` is, so the line a kill
    /// writes can never print a different number than the one that killed.
    pub timeout: u64,
    /// The window without progress after which the command is killed as `STALLED`.
    pub stall: Duration,
    /// Whether the command runs through WSL, so the VM's CPU counts toward its progress.
    pub wsl: bool,
}

/// One sample of a watched command: the raw facts the Win32 side reads, with the time the watch had run.
#[derive(Clone, Debug)]
pub struct Sample {
    /// How long the watch had run when this sample's reads began.
    pub began: Duration,
    /// How long the watch had run at this sample, read with the system times and the job's accounting.
    pub at: Duration,
    /// The system's CPU times.
    pub system: SystemTimes,
    /// The command's job's total CPU time, every process ever in it counted.
    pub job_cpu: Ticks,
    /// The sum of the sizes of the log and its `.err` file.
    pub output_size: u64,
    /// The pid of every process in the job now.
    pub pids: BTreeSet<u32>,
    /// Whether a gate under this one waits behind a critical request.
    pub paused: bool,
    /// The build servers the command started, by pid, with the CPU time each has used in 100 ns ticks.
    pub new_servers: BTreeMap<u32, u64>,
    /// The compiler servers already running that the command's build client may be using, by pid, with the CPU time
    /// each has used in 100 ns ticks.
    pub shared_servers: BTreeMap<u32, u64>,
    /// Whether the job holds a build client, one that may hand its compile to a build server.
    pub build_client: bool,
    /// The WSL VM's cumulative CPU time in 100 ns ticks, or `None` when it is not read, or could not be.
    pub vm_cpu: Option<u64>,
}

/// The state of one watched command: the load-adjusted seconds it has accrued, the seconds it spent paused, where the
/// last progress was, whether its sampler still works, and the facts the last sample left to measure the next one
/// against.
#[derive(Clone, Debug)]
pub struct Watch {
    config: Config,
    machine: Machine,
    last_at: Duration,
    last_began: Duration,
    last_progress: Duration,
    adjusted: f64,
    paused: Duration,
    last_free: f64,
    system: SystemTimes,
    job_cpu: Ticks,
    progress_cpu: Ticks,
    output_size: u64,
    pids: BTreeSet<u32>,
    new_servers: BTreeMap<u32, u64>,
    shared_servers: BTreeMap<u32, u64>,
    vm_cpu: Option<u64>,
    vm_busy: u64,
    vm_percent: f64,
    sampled: bool,
    sampler_failed: bool,
}

impl Watch {
    /// A watch beginning now, with `system` and `job_cpu` the facts at the start of the command and `vm_cpu` the WSL
    /// VM's CPU read then, or `None` when it was not read: the clock starts at zero, the progress clock with it, and
    /// nothing has been counted or read yet. A VM baseline lets the first sample count what the VM used since the
    /// command started, as the PowerShell gate's own first reading does.
    #[must_use]
    pub fn new(config: Config, machine: Machine, system: SystemTimes, job_cpu: Ticks, vm_cpu: Option<u64>) -> Self {
        Self {
            config,
            machine,
            last_at: Duration::ZERO,
            last_began: Duration::ZERO,
            last_progress: Duration::ZERO,
            adjusted: 0.0,
            paused: Duration::ZERO,
            last_free: 1.0,
            system,
            job_cpu,
            progress_cpu: job_cpu,
            output_size: 0,
            pids: BTreeSet::new(),
            new_servers: BTreeMap::new(),
            shared_servers: BTreeMap::new(),
            vm_cpu,
            vm_busy: 0,
            vm_percent: 0.0,
            sampled: false,
            sampler_failed: false,
        }
    }

    /// Takes one sample: the interval since the last one advances the clock at the share of the machine the command
    /// could have had, or counts as paused time while a gate under this one waits behind a critical request. The
    /// interval runs between the samples' end stamps, the moment their system times and job accounting were read, and
    /// a progress sample marks its own end stamp as the last progress, since the CPU and the job's figures that show
    /// it were read next to that stamp; the stall is judged at the latest sample's begin stamp, so a watcher starved
    /// between its two stamps does not count its own read time as the command's quiet time. The sample is progress
    /// when the log's size changed either way, the job's CPU since the last progress reached 1% of one core over the
    /// stall window, a pid appeared that the last sample did not have, a build server the job's client is using gained
    /// CPU, the WSL VM passed its floor, or the run is paused.
    pub fn sample(&mut self, sample: &Sample) {
        let interval = sample.at.saturating_sub(self.last_at).max(MIN_INTERVAL);
        let new_servers = measure_cpu(&self.new_servers, &sample.new_servers);
        let shared_servers = measure_cpu(&self.shared_servers, &sample.shared_servers);
        let own_gained = own_gained(
            sample.job_cpu.saturating_sub(self.job_cpu),
            new_servers,
            shared_servers,
            sample.build_client,
        );
        let vm_working = self.vm_working(sample.vm_cpu, interval);
        if self.progressed(sample, new_servers, shared_servers, vm_working) {
            self.last_progress = sample.at;
            self.progress_cpu = sample.job_cpu;
            self.vm_busy = 0;
        }
        let free = free_share(self.machine, self.system, sample.system, own_gained, interval.as_secs_f64());
        if sample.paused {
            self.paused += interval;
        } else {
            self.adjusted += interval.as_secs_f64() * free;
        }
        self.last_free = free;
        self.sampled = true;
        self.last_at = sample.at;
        self.last_began = sample.began;
        self.system = sample.system;
        self.job_cpu = sample.job_cpu;
        self.output_size = sample.output_size;
        self.pids.clone_from(&sample.pids);
        self.new_servers.clone_from(&sample.new_servers);
        self.shared_servers.clone_from(&sample.shared_servers);
    }

    /// Whether `sample` is progress: the log changed either way, the job's CPU since the last progress reached 1% of
    /// one core over the stall window, a pid appeared that the last sample did not have, a build server the job's
    /// client is using rose, the WSL VM worked, or a gate under this one waits. A server counts only while the job
    /// holds a build client, and a server that merely appeared is no progress.
    fn progressed(&self, sample: &Sample, new_servers: CpuMove, shared_servers: CpuMove, vm_working: bool) -> bool {
        let floor = self.config.stall.as_nanos() / FLOOR_NANOS_PER_TICK;
        let grew = sample.output_size != self.output_size;
        let working = u128::from(sample.job_cpu.saturating_sub(self.progress_cpu).0) >= floor;
        let appeared = !sample.pids.is_subset(&self.pids);
        let servers = sample.build_client && (new_servers.rose || shared_servers.rose);
        grew || working || appeared || servers || sample.paused || vm_working
    }

    /// Adds the WSL VM's CPU since the last reading to the ticks accrued since the last progress, and says whether
    /// they have reached the tenth of the stall window the floor is, and keeps that CPU over `interval` as the VM's
    /// percent of one core. Nothing is counted for a command that is not a WSL one, and the first reading, and the
    /// first after one that could not be read, is only a baseline, as is a reading lower than the last one (a VM that
    /// restarted).
    fn vm_working(&mut self, read: Option<u64>, interval: Duration) -> bool {
        if !self.config.wsl {
            return false;
        }
        let floor = self.config.stall.as_nanos() / VM_FLOOR_NANOS_PER_TICK;
        let before = self.vm_cpu;
        self.vm_cpu = read;
        let (Some(before), Some(now)) = (before, read) else {
            self.vm_percent = 0.0;
            return false;
        };
        let gained = now.saturating_sub(before);
        let gained_seconds = Duration::from_nanos(gained.saturating_mul(100)).as_secs_f64();
        self.vm_percent = gained_seconds / interval.as_secs_f64() * 100.0;
        self.vm_busy = self.vm_busy.saturating_add(gained);
        u128::from(self.vm_busy) >= floor
    }

    /// Records that a sample failed, and returns the line the gate says for it.
    ///
    /// The time stays where the last good sample left it, as `Update-Watch` throws before it moves `LastAt`, so the
    /// stretch a later [`Watch::wall_only`] counts as paused starts at that sample. From here the stall and the
    /// load-adjusted clock no longer move, so [`Watch::kill_check`] reads only the backstop.
    pub fn sampler_failed(&mut self, message: &str) -> String {
        self.sampler_failed = true;
        sampler_failed_line(message)
    }

    /// One pass while the sampler is failed, at `now`: the wall time since the last pass counts as paused time while
    /// `paused` and is left out of the backstop, and either way the clock is brought up to `now`. A pass reads nothing,
    /// so its two stamps fall together.
    pub fn wall_only(&mut self, now: Duration, paused: bool) {
        let stretch = now.saturating_sub(self.last_at);
        if paused {
            self.paused += stretch;
        }
        self.last_at = now;
        self.last_began = now;
    }

    /// The command exited at `now`: the stretch since the last sample is counted at the share the last sample found.
    /// A sampler that failed leaves no share to count it at, so nothing is added after one has. An exit reads nothing,
    /// so its two stamps fall together.
    pub fn exited(&mut self, now: Duration) {
        if self.sampler_failed {
            return;
        }
        self.adjusted += now.saturating_sub(self.last_at).as_secs_f64() * self.last_free;
        self.last_at = now;
        self.last_began = now;
    }

    /// The load-adjusted seconds the clock has counted, unrounded.
    #[must_use]
    pub fn adjusted_seconds(&self) -> f64 {
        self.adjusted
    }

    /// The wall time counted as paused, while a gate under this one waited behind a critical request.
    #[must_use]
    pub fn paused_time(&self) -> Duration {
        self.paused
    }

    /// The share of the machine free on average over the wall time the ceiling counted, rounded to a whole number
    /// with halves to even: the paused seconds are left out, and 100 while nothing has been counted.
    #[must_use]
    pub fn free_percent(&self) -> f64 {
        let counted = self.last_at.saturating_sub(self.paused);
        if counted.is_zero() {
            return 100.0;
        }
        (100.0 * self.adjusted / counted.as_secs_f64()).round_ties_even()
    }

    /// The share of the machine the last sample found free, as a whole percent with halves to even: `None` before the
    /// first sample and once the sampler has failed.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "with a demand of at least 1 the share is clamped to [0.05, 1], so its whole percent is 5 to 100; a demand \
                  of 0 can make it NaN, which the cast turns into 0"
    )]
    pub fn last_free_percent(&self) -> Option<u32> {
        if !self.sampled || self.sampler_failed {
            return None;
        }
        Some((100.0 * self.last_free).round_ties_even() as u32)
    }

    /// The WSL VM's CPU over the last sample's interval, in percent of one core, so above 100 when it used more than
    /// one: 0 before the first sample, when that sample's reading or the one before it is missing, and when the VM
    /// restarted and reads lower than before. `None` for a command that is not a WSL one.
    #[must_use]
    pub fn last_vm_percent(&self) -> Option<f64> {
        self.config.wsl.then_some(self.vm_percent)
    }

    /// Whether a sample has failed, so the watch goes by wall time alone.
    #[must_use]
    pub fn sampler_has_failed(&self) -> bool {
        self.sampler_failed
    }

    /// The kill for the first of the three reasons that holds at `now`, or `None` while none does: the stall, then the
    /// ceiling, then the backstop. The stall is judged at the last sample's begin stamp, so the time a starved watcher
    /// spent in its reads, and between them and the check, is not the command's quiet time; the ceiling and the
    /// backstop read `now`. Once the sampler has failed only the backstop is read, since the stall and the load-adjusted
    /// clock no longer move.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "the ceiling is read as the double the clock counts in, as the PowerShell gate reads it"
    )]
    pub fn kill_check(&self, now: Duration) -> Option<Kill> {
        let free_percent = self.free_percent();
        if !self.sampler_failed {
            // The stall is judged at the last sample's begin stamp: a watcher starved of CPU wakes late, and the delay
            // it spent descheduled between its two stamps is not the command's quiet time.
            let judged_at = if self.sampled { self.last_began.min(now) } else { now };
            let quiet = judged_at.saturating_sub(self.last_progress);
            if quiet >= self.config.stall {
                return Some(Kill::Stalled { quiet, free_percent });
            }
            if self.adjusted >= self.config.timeout as f64 {
                return Some(Kill::TimedOut {
                    timeout: self.config.timeout,
                    wall: now,
                    free_percent,
                });
            }
        }
        if now.saturating_sub(self.paused) >= Duration::from_secs(self.config.timeout.saturating_mul(5)) {
            return Some(Kill::Backstop {
                timeout: self.config.timeout,
                wall: now,
                adjusted: self.adjusted,
                free_percent: (!self.sampler_failed).then_some(free_percent),
            });
        }
        None
    }
}

/// How far one set of processes moved between two samples: the CPU gained, the CPU of the processes the earlier sample
/// had too, whether one of them gained any, and whether one appeared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuMove {
    /// Every process's CPU gained, an appearing process's whole CPU with it.
    pub gained: u64,
    /// The CPU gained by the processes the earlier sample had too.
    pub continued: u64,
    /// Whether a process the earlier sample had too gained any CPU.
    pub rose: bool,
    /// Whether a process appeared that the earlier sample did not have.
    pub appeared: bool,
}

/// The CPU the processes of `after` gained on `before`, as the PowerShell gate's `Measure-Cpu` measures it: an appearing
/// process adds its whole CPU to `gained` alone, and only a process seen before can rise, so a server that merely
/// appeared is no progress.
#[must_use]
pub fn measure_cpu(before: &BTreeMap<u32, u64>, after: &BTreeMap<u32, u64>) -> CpuMove {
    let mut moved = CpuMove::default();
    for (&pid, &cpu) in after {
        match before.get(&pid) {
            None => {
                moved.appeared = true;
                moved.gained += cpu;
            }
            Some(&was) => {
                let delta = cpu.saturating_sub(was);
                if delta > 0 {
                    moved.rose = true;
                    moved.gained += delta;
                    moved.continued += delta;
                }
            }
        }
    }
    moved
}

/// The command's own CPU over the interval, in ticks: its job's gain, plus, while the job holds a build client, the
/// build servers the command started and the shared compiler servers' continuing CPU.
fn own_gained(job: Ticks, new_servers: CpuMove, shared_servers: CpuMove, client: bool) -> Ticks {
    if !client {
        return job;
    }
    Ticks(job.0 + new_servers.gained + shared_servers.continued)
}

/// Why a watched command was killed, with the figures its line says: the kill is decided at one sample and its line
/// written whenever the caller is ready.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kill {
    /// No output and no CPU for the stall window.
    Stalled {
        /// The seconds without progress.
        quiet: Duration,
        /// The share of the machine free on average.
        free_percent: f64,
    },
    /// The load-adjusted clock reached the ceiling.
    TimedOut {
        /// The ceiling the clock reached, in whole seconds.
        timeout: u64,
        /// The wall seconds the command has run.
        wall: Duration,
        /// The share of the machine free on average.
        free_percent: f64,
    },
    /// Five ceilings of wall time passed, the paused seconds left out.
    Backstop {
        /// The ceiling the backstop is five of, in whole seconds.
        timeout: u64,
        /// The wall seconds the command has run.
        wall: Duration,
        /// The load-adjusted seconds counted so far.
        adjusted: f64,
        /// The share of the machine free on average, or `None` once the sampler failed.
        free_percent: Option<f64>,
    },
}

impl Kill {
    /// The line the gate says for this kill, with `spoken` the command's words joined by single spaces.
    #[must_use]
    pub fn line(&self, spoken: &str) -> String {
        match *self {
            Kill::Stalled { quiet, free_percent } => format!(
                "gate: STALLED: no output and no CPU for {} s (machine free {free_percent}% on average); \
                 killed {spoken} and its children",
                quiet.as_secs_f64().round_ties_even()
            ),
            Kill::TimedOut { timeout, wall, free_percent } => format!(
                "gate: TIMED OUT after {timeout} s of load-adjusted time (wall {} s, machine free \
                 {free_percent}% on average); killed {spoken} and its children",
                wall.as_secs_f64().round_ties_even()
            ),
            Kill::Backstop {
                timeout,
                wall,
                adjusted,
                free_percent,
            } => format!(
                "gate: BACKSTOP: {} s of wall time (5 x {timeout} s ceiling, {}); killed {spoken} and its children",
                wall.as_secs_f64().round_ties_even(),
                machine_load(adjusted, free_percent)
            ),
        }
    }
}

/// The backstop line's middle clause: the load-adjusted seconds with the free share, or that the sampler failed and
/// the machine's load is unknown.
#[must_use]
fn machine_load(adjusted: f64, free_percent: Option<f64>) -> String {
    match free_percent {
        Some(free) => format!("load-adjusted {} s, machine free {free}% on average", adjusted.round_ties_even()),
        None => "machine load unknown: the sampler failed".to_string(),
    }
}

/// The line the gate says once a sample has failed: the watch goes by wall time alone from then on.
#[must_use]
pub fn sampler_failed_line(message: &str) -> String {
    format!("gate: sampler failed: {message}; watching by wall time only")
}

/// The line the gate says once the WSL VM's CPU cannot be read: every later sample tries again, and until one of them
/// succeeds the command counts only its output and its job's CPU as progress.
#[must_use]
pub fn wsl_unavailable_line(message: &str) -> String {
    format!(
        "gate: WSL VM CPU unavailable ({message}); retrying each sample, and until a read succeeds this wsl command \
         counts only output and job CPU as progress"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The system times of a machine that has been idle: a sample that reads them again sees a full share.
    const IDLE: SystemTimes = SystemTimes { idle: 0, kernel: 0, user: 0 };

    /// The times a 16-core machine reads over 10 s when half of it is busy elsewhere.
    const HALF_BUSY: SystemTimes = SystemTimes {
        idle: 800_000_000,
        kernel: 800_000_000,
        user: 800_000_000,
    };

    /// The times a 16-core machine reads over 30 s when half of it is busy elsewhere: three times [`HALF_BUSY`].
    const HALF_BUSY_30S: SystemTimes = SystemTimes {
        idle: 2_400_000_000,
        kernel: 2_400_000_000,
        user: 2_400_000_000,
    };

    /// The stall window and ceiling under test, for a command that is not a WSL one.
    fn config(stall_ms: u64, timeout_s: u64) -> Config {
        Config {
            stall: Duration::from_millis(stall_ms),
            timeout: timeout_s,
            wsl: false,
        }
    }

    /// The stall window and ceiling under test, for a WSL command.
    fn wsl_config(stall_ms: u64, timeout_s: u64) -> Config {
        Config {
            wsl: true,
            ..config(stall_ms, timeout_s)
        }
    }

    /// A watch of a 16-core machine asking a command for every core of it: a sample that reads [`IDLE`] again sees a
    /// full share.
    fn watching(config: Config) -> Watch {
        Watch::new(config, Machine::new(16, 16, false), IDLE, Ticks(0), None)
    }

    /// A sample at `ms`, stamping its begin and its end both there: `job_cpu` used by the job, `output` bytes in the
    /// log, `pids` in the job.
    fn tick(ms: u64, job_cpu: u64, output: u64, pids: &[u32], paused: bool) -> Sample {
        Sample {
            began: Duration::from_millis(ms),
            at: Duration::from_millis(ms),
            system: IDLE,
            job_cpu: Ticks(job_cpu),
            output_size: output,
            pids: pids.iter().copied().collect(),
            paused,
            new_servers: BTreeMap::new(),
            shared_servers: BTreeMap::new(),
            build_client: false,
            vm_cpu: None,
        }
    }

    /// A sample at `ms`, stamping its begin and its end both there, reading `system` as the machine's times since the
    /// watch began.
    fn sample(ms: u64, system: SystemTimes, paused: bool) -> Sample {
        Sample {
            began: Duration::from_millis(ms),
            at: Duration::from_millis(ms),
            system,
            job_cpu: Ticks(0),
            output_size: 0,
            pids: BTreeSet::new(),
            paused,
            new_servers: BTreeMap::new(),
            shared_servers: BTreeMap::new(),
            build_client: false,
            vm_cpu: None,
        }
    }

    /// A sample at `ms` of a half-busy machine, so its share over that stretch is 0.5.
    fn half_sample(ms: u64) -> Sample {
        sample(ms, HALF_BUSY, false)
    }

    /// A sample at `ms` of an idle machine whose job holds `pids`, holding a build client when `client`, with the build
    /// servers the sample found.
    fn server_tick(ms: u64, pids: &[u32], new: &[(u32, u64)], shared: &[(u32, u64)], client: bool) -> Sample {
        let mut ticked = tick(ms, 0, 0, pids, false);
        ticked.new_servers = new.iter().copied().collect();
        ticked.shared_servers = shared.iter().copied().collect();
        ticked.build_client = client;
        ticked
    }

    /// A sample at `ms` of a WSL command's watch, reading `vm_cpu` as the VM's cumulative CPU.
    fn vm_sample(ms: u64, vm_cpu: Option<u64>) -> Sample {
        let mut ticked = sample(ms, IDLE, false);
        ticked.vm_cpu = vm_cpu;
        ticked
    }

    /// The times a 16-core machine reads over one second with every core busy: no idle, all of it in kernel.
    const FULLY_BUSY: SystemTimes = SystemTimes {
        idle: 0,
        kernel: 160_000_000,
        user: 0,
    };

    /// The same machine one second later, so the second sample's interval is busy on every core too.
    const FULLY_BUSY_SECOND: SystemTimes = SystemTimes {
        idle: 0,
        kernel: 320_000_000,
        user: 0,
    };

    /// The free share the first sample of a 16-core machine busy on every core found, with `new` and `shared` servers
    /// and a build client when `client`: 1.6 core-seconds of a server over the 1 s sample is a 10% share.
    fn server_share(new: &[(u32, u64)], shared: &[(u32, u64)], client: bool) -> Option<u32> {
        let mut watch = Watch::new(config(60_000, 5000), Machine::new(16, 16, false), IDLE, Ticks(0), None);
        let mut first = server_tick(1000, &[], new, shared, client);
        first.system = FULLY_BUSY;
        watch.sample(&first);
        watch.last_free_percent()
    }

    #[test]
    fn free_percent_counts_nothing_as_100() {
        let watch = Watch::new(config(5000, 5), Machine::new(4, 4, false), IDLE, Ticks(0), None);
        assert!((watch.free_percent() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn free_percent_rounds_halves_to_even() {
        let machine = Machine::new(4, 4, false);
        // Two 1 s samples at the shares a 4-core machine gives: 0.5 then 0.75, so 1.25 adjusted seconds out of 2.
        let (before, half) = (
            IDLE,
            SystemTimes {
                idle: 2_000_000_000,
                kernel: 2_000_000_000,
                user: 2_000_000_000,
            },
        );
        let quarter = SystemTimes {
            idle: 5_000_000_000,
            kernel: 5_000_000_000,
            user: 3_000_000_000,
        };
        let mut watch = Watch::new(config(5000, 5), machine, before, Ticks(0), None);
        watch.sample(&sample(1000, half, false));
        watch.sample(&sample(2000, quarter, false));
        assert!(
            (watch.free_percent() - 62.0).abs() < f64::EPSILON,
            "62.5 rounds to the even 62, got {}",
            watch.free_percent()
        );

        // Two 1 s samples at 0.875 each: 1.75 adjusted seconds out of 2, so 87.5 rounds up to the even 88.
        let eighth = SystemTimes {
            idle: 3_500_000_000,
            kernel: 3_500_000_000,
            user: 500_000_000,
        };
        let two_eighths = SystemTimes {
            idle: 7_000_000_000,
            kernel: 7_000_000_000,
            user: 1_000_000_000,
        };
        let mut watch = Watch::new(config(5000, 5), machine, before, Ticks(0), None);
        watch.sample(&sample(1000, eighth, false));
        watch.sample(&sample(2000, two_eighths, false));
        assert!(
            (watch.free_percent() - 88.0).abs() < f64::EPSILON,
            "87.5 rounds to the even 88, got {}",
            watch.free_percent()
        );
    }

    #[test]
    fn paused_seconds_are_left_out_of_the_free_percent() {
        // A second of a full share and a second paused: 1 s counted, not 2.
        let machine = Machine::new(4, 4, true);
        let mut watch = Watch::new(config(5000, 5), machine, IDLE, Ticks(0), None);
        watch.sample(&sample(1000, IDLE, false));
        watch.sample(&sample(2000, IDLE, true));
        assert!((watch.adjusted_seconds() - 1.0).abs() < f64::EPSILON);
        assert!((watch.free_percent() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn last_free_percent_is_the_last_share_rounded_half_to_even() {
        let mut watch = Watch::new(config(5000, 5), Machine::new(4, 4, false), IDLE, Ticks(0), None);
        assert_eq!(watch.last_free_percent(), None, "no sample has found a share yet");
        // 5 idle of 8 ticks on a 4-core machine asking for all four: a share of 0.625.
        let five_eighths = SystemTimes {
            idle: 5_000,
            kernel: 5_000,
            user: 3_000,
        };
        watch.sample(&sample(1000, five_eighths, false));
        assert_eq!(watch.last_free_percent(), Some(62), "62.5 rounds to the even 62");
        watch.sample(&half_sample(2000));
        assert_eq!(watch.last_free_percent(), Some(50), "the last sample's share, not the average");
    }

    #[test]
    fn a_failed_sampler_leaves_no_last_free_percent() {
        let mut watch = watching(config(5000, 5));
        watch.sample(&half_sample(1000));
        assert!(!watch.sampler_has_failed());
        watch.sampler_failed("the job is gone");
        assert!(watch.sampler_has_failed());
        assert_eq!(watch.last_free_percent(), None, "a failed sampler has no share to report");
    }

    #[test]
    fn stall_fires_at_the_window() {
        let watch = watching(config(1, 5000));
        assert!(
            watch.kill_check(Duration::from_nanos(999_900)).is_none(),
            "a 100 ns tick short of the window is no stall"
        );
        assert!(
            matches!(watch.kill_check(Duration::from_millis(1)), Some(Kill::Stalled { .. })),
            "the window itself is a stall"
        );
    }

    #[test]
    fn a_late_watcher_does_not_count_its_own_delay_as_quiet() {
        // The watcher wakes 26.4 s after the sample it stamped: a baseline build server at 600 ms, then a kill check
        // read at 27 s, once the starved sample's reads had finished.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&server_tick(600, &[], &[(7, 55)], &[], true));
        assert!(
            watch.kill_check(Duration::from_secs(27)).is_none(),
            "the delay between the sample's stamp and the kill check is not the command's quiet time"
        );
        // The next sample, stamped 5 s in, finds the same server further along: that CPU is progress.
        watch.sample(&server_tick(5600, &[], &[(7, 26_440_000)], &[], true));
        assert!(
            watch.kill_check(Duration::from_millis(5600)).is_none(),
            "the server CPU that rose between the samples is progress"
        );
    }

    #[test]
    fn a_late_sample_with_no_progress_is_still_a_stall() {
        // A sample stamped 27 s in with the same server CPU as its baseline: the command really was quiet.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&server_tick(600, &[], &[(7, 55)], &[], true));
        watch.sample(&server_tick(27_000, &[], &[(7, 55)], &[], true));
        assert!(
            matches!(
                watch.kill_check(Duration::from_secs(27)),
                Some(Kill::Stalled { quiet, .. }) if quiet == Duration::from_secs(27)
            ),
            "a sample that saw no progress is a stall, quiet measured from the last progress"
        );
    }

    #[test]
    fn a_late_sample_counts_its_delay_at_the_share_it_read() {
        // A watcher starved of CPU: its second sample began 6 s in and its reads finished at 31 s, so the interval it
        // counts is the 30 s of system time the sample read, at the half share those times show.
        let mut watch = watching(config(60_000, 15));
        watch.sample(&sample(1000, IDLE, false));
        let mut late = sample(31_000, HALF_BUSY_30S, false);
        late.began = Duration::from_millis(6000);
        watch.sample(&late);
        assert!(
            (watch.adjusted_seconds() - (1.0 + 30.0 * 0.5)).abs() < 1e-9,
            "an idle second and 30 s at half a share, got {}",
            watch.adjusted_seconds()
        );
        assert!(
            matches!(watch.kill_check(Duration::from_secs(31)), Some(Kill::TimedOut { .. })),
            "16 s of load-adjusted time is past a ceiling of 15"
        );
    }

    #[test]
    fn a_late_sample_does_not_count_its_own_duration_as_quiet() {
        // One sample began 6 s in and finished its reads at 31 s with no progress: the stall is judged at the sample's
        // began stamp, so its own 25 s of reads are not the command's quiet time.
        let mut watch = watching(config(10_000, 5000));
        let mut late = sample(31_000, IDLE, false);
        late.began = Duration::from_millis(6000);
        watch.sample(&late);
        assert!(
            watch.kill_check(Duration::from_secs(31)).is_none(),
            "6 s of quiet is under the 10 s window; only the sample's own 25 s of reads would pass it"
        );
    }

    #[test]
    fn a_late_samples_progress_counts_to_its_end_stamp() {
        // A late sample that saw the log grow: the progress it found was read next to its end stamp, so the quiet the
        // next sample judges starts there. Its begin stamp would instead count the 25 s between them as quiet.
        let mut watch = watching(config(10_000, 5000));
        let mut late = tick(31_000, 0, 1, &[], false);
        late.began = Duration::from_millis(6000);
        watch.sample(&late);
        watch.sample(&tick(32_000, 0, 1, &[], false));
        assert!(
            watch.kill_check(Duration::from_secs(32)).is_none(),
            "quiet runs from the late sample's end stamp, 1 s back, under the 10 s window; from its begin it would be 26 s"
        );
    }

    #[test]
    fn a_late_sample_with_job_cpu_is_progress() {
        // The job used 3 s of CPU over the window, over the floor of 100 000 ticks a second: progress whatever the
        // late stamp.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(27_000, 3_000_000, 0, &[], false));
        assert!(
            watch.kill_check(Duration::from_secs(27)).is_none(),
            "the job's CPU over the floor is progress even at a late stamp"
        );
    }

    #[test]
    fn cpu_trickle_below_one_percent_is_still_a_stall() {
        // The floor is 1% of one core over the window: 100_000 ticks over 1 s.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 99_999, 0, &[10], false));
        assert!(
            watch.kill_check(Duration::from_millis(1500)).is_none(),
            "the first sample's new pid is progress, whatever the CPU"
        );
        watch.sample(&tick(2000, 199_998, 0, &[10], false));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(2000)), Some(Kill::Stalled { .. })),
            "a tick under the floor over the window is a stall"
        );

        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 99_999, 0, &[10], false));
        watch.sample(&tick(2000, 199_999, 0, &[10], false));
        assert!(watch.kill_check(Duration::from_millis(2000)).is_none(), "the floor itself is progress");
    }

    #[test]
    fn output_growth_is_progress() {
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 0, 1, &[], false));
        assert!(
            watch.kill_check(Duration::from_millis(1500)).is_none(),
            "the log growing holds the stall off past the window"
        );
    }

    #[test]
    fn log_shrink_is_progress() {
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 0, 10, &[], false));
        watch.sample(&tick(2000, 0, 5, &[], false));
        assert!(
            watch.kill_check(Duration::from_millis(2500)).is_none(),
            "a log that shrank is a log that changed"
        );
    }

    #[test]
    fn cpu_at_the_floor_is_progress_and_carries_forward() {
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 60_000, 0, &[], false));
        watch.sample(&tick(2000, 100_000, 0, &[], false));
        assert!(
            watch.kill_check(Duration::from_millis(2000)).is_none(),
            "two sub-floor stretches that sum to the floor are progress"
        );
    }

    #[test]
    fn a_new_pid_is_progress() {
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 0, 0, &[1], false));
        watch.sample(&tick(2000, 0, 0, &[1, 2], false));
        assert!(
            watch.kill_check(Duration::from_millis(2500)).is_none(),
            "the pid that appeared is progress"
        );
        watch.sample(&tick(3000, 0, 0, &[1, 2], false));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(3000)), Some(Kill::Stalled { .. })),
            "the same pids again are no progress"
        );
    }

    #[test]
    fn a_pid_that_leaves_and_returns_counts_as_new() {
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 0, 0, &[1, 2], false));
        watch.sample(&tick(2000, 0, 0, &[1], false));
        watch.sample(&tick(3000, 0, 0, &[1, 2], false));
        assert!(
            watch.kill_check(Duration::from_millis(3500)).is_none(),
            "a pid the last sample had dropped is a pid that appeared again"
        );
    }

    #[test]
    fn paused_is_progress_and_stops_the_clock() {
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 0, 0, &[], true));
        assert!(watch.adjusted_seconds().abs() < f64::EPSILON, "a paused second is no load-adjusted time");
        assert!((watch.free_percent() - 100.0).abs() < f64::EPSILON);
        assert!(watch.kill_check(Duration::from_millis(1500)).is_none(), "the pause is progress");
    }

    #[test]
    fn stalled_outranks_timed_out_outranks_backstop() {
        // The stall window, the ceiling and the backstop all pass on one sample.
        let mut watch = watching(config(1000, 1));
        watch.sample(&tick(6000, 0, 0, &[], false));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(6000)), Some(Kill::Stalled { .. })),
            "the stall outranks the other two"
        );
        watch.sample(&tick(7000, 0, 0, &[], false));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(7000)), Some(Kill::Stalled { .. })),
            "and still does on the next check"
        );
    }

    #[test]
    fn stalled_outranks_timed_out() {
        // The stall window and the ceiling pass; the backstop's five ceilings do not.
        let mut watch = watching(config(1000, 2));
        watch.sample(&tick(3000, 0, 0, &[], false));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(3000)), Some(Kill::Stalled { .. })),
            "the stall outranks the ceiling"
        );
    }

    #[test]
    fn stalled_outranks_backstop() {
        // An unpaused run with no progress: the stall window and the backstop's wall both pass; the ceiling does not.
        let mut watch = watching(config(1000, 1));
        watch.sample(&tick(10_000, 0, 0, &[], false));
        assert!(
            matches!(watch.kill_check(Duration::from_secs(15)), Some(Kill::Stalled { .. })),
            "the stall outranks the backstop"
        );
    }

    #[test]
    fn timed_out_outranks_backstop() {
        let mut watch = watching(config(60_000, 1));
        watch.sample(&tick(6000, 0, 0, &[], false));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(6000)), Some(Kill::TimedOut { .. })),
            "the ceiling outranks the backstop"
        );
    }

    #[test]
    fn timed_out_on_adjusted_time_only() {
        let mut watch = watching(config(60_000, 1));
        watch.sample(&half_sample(1000));
        assert!(
            watch.kill_check(Duration::from_millis(1000)).is_none(),
            "1 s of wall at half a share is half a second of the ceiling"
        );
        let mut busier = half_sample(2000);
        busier.system = SystemTimes {
            idle: 1_000_000_000,
            kernel: 1_000_000_000,
            user: 1_400_000_000,
        };
        watch.sample(&busier);
        assert!(
            watch.kill_check(Duration::from_millis(2000)).is_none(),
            "three quarters of a second of adjusted time is under the ceiling"
        );
        watch.sample(&tick(3000, 0, 0, &[], false));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(3000)), Some(Kill::TimedOut { .. })),
            "the adjusted second that reached the ceiling is the kill"
        );
    }

    #[test]
    fn backstop_on_wall_less_paused() {
        let mut watch = watching(config(60_000, 1));
        watch.sample(&tick(2000, 0, 0, &[], true));
        assert!(
            watch.kill_check(Duration::from_millis(6000)).is_none(),
            "4 s of unpaused wall is under the 5 s backstop"
        );
        assert!(
            matches!(watch.kill_check(Duration::from_millis(7000)), Some(Kill::Backstop { .. })),
            "5 s of unpaused wall is the backstop"
        );
    }

    #[test]
    fn after_sampler_failure_only_backstop() {
        let mut watch = watching(config(1000, 1));
        watch.sample(&tick(2000, 0, 0, &[], false));
        let line = watch.sampler_failed("the job is gone");
        assert_eq!(line, "gate: sampler failed: the job is gone; watching by wall time only");
        assert!(
            watch.kill_check(Duration::from_millis(4000)).is_none(),
            "the stall and the ceiling are not read once the sampler has failed"
        );
        assert!(
            matches!(
                watch.kill_check(Duration::from_millis(6000)),
                Some(Kill::Backstop { free_percent: None, .. })
            ),
            "the backstop still holds, with the machine load unknown"
        );
    }

    #[test]
    fn paused_time_after_sampler_failure_is_left_out_of_the_backstop() {
        let mut watch = watching(config(1000, 10));
        watch.sampler_failed("the job is gone");
        watch.wall_only(Duration::from_secs(50), true);
        watch.wall_only(Duration::from_secs(80), false);
        assert!(
            watch.kill_check(Duration::from_secs(80)).is_none(),
            "80 s of wall less the 50 s paused since the last sample is 30 s, under the 50 s backstop"
        );
        assert!(
            matches!(watch.kill_check(Duration::from_secs(100)), Some(Kill::Backstop { .. })),
            "the backstop first holds when the unpaused wall reaches 50 s"
        );
    }

    #[test]
    fn final_partial_interval_added_at_last_share() {
        let machine = Machine::new(16, 16, false);
        let mut watch = Watch::new(config(60_000, 5000), machine, IDLE, Ticks(0), None);
        watch.sample(&half_sample(2000));
        watch.exited(Duration::from_millis(3000));
        assert!(
            (watch.adjusted_seconds() - 1.5).abs() < f64::EPSILON,
            "the last second counts at the last share, got {}",
            watch.adjusted_seconds()
        );

        let mut watch = Watch::new(config(60_000, 5000), machine, IDLE, Ticks(0), None);
        watch.sample(&half_sample(2000));
        watch.sampler_failed("the job is gone");
        watch.exited(Duration::from_millis(3000));
        assert!(
            (watch.adjusted_seconds() - 1.0).abs() < f64::EPSILON,
            "a failed sampler leaves no share to count the stretch at"
        );
    }

    #[test]
    fn kill_lines_are_exact() {
        const SPOKEN: &str = "cargo test -p gate-watchdog";
        let stalled = Kill::Stalled {
            quiet: Duration::from_millis(2500),
            free_percent: 37.0,
        };
        assert_eq!(
            stalled.line(SPOKEN),
            "gate: STALLED: no output and no CPU for 2 s (machine free 37% on average); \
             killed cargo test -p gate-watchdog and its children"
        );

        let timed_out = Kill::TimedOut {
            timeout: 5,
            wall: Duration::from_millis(123_456),
            free_percent: 88.0,
        };
        assert_eq!(
            timed_out.line(SPOKEN),
            "gate: TIMED OUT after 5 s of load-adjusted time (wall 123 s, machine free 88% on \
             average); killed cargo test -p gate-watchdog and its children"
        );

        let backstop = Kill::Backstop {
            timeout: 5,
            wall: Duration::from_secs(30),
            adjusted: 12.0,
            free_percent: Some(50.0),
        };
        assert_eq!(
            backstop.line(SPOKEN),
            "gate: BACKSTOP: 30 s of wall time (5 x 5 s ceiling, load-adjusted 12 s, machine free \
             50% on average); killed cargo test -p gate-watchdog and its children"
        );

        let unknown = Kill::Backstop {
            timeout: 5,
            wall: Duration::from_secs(30),
            adjusted: 12.0,
            free_percent: None,
        };
        assert_eq!(
            unknown.line(SPOKEN),
            "gate: BACKSTOP: 30 s of wall time (5 x 5 s ceiling, machine load unknown: the sampler \
             failed); killed cargo test -p gate-watchdog and its children"
        );

        assert_eq!(
            sampler_failed_line("the job is gone"),
            "gate: sampler failed: the job is gone; watching by wall time only"
        );
        assert_eq!(
            wsl_unavailable_line("the counter is missing"),
            "gate: WSL VM CPU unavailable (the counter is missing); retrying each sample, and until a read succeeds \
             this wsl command counts only output and job CPU as progress"
        );
    }

    #[test]
    fn measure_cpu_reads_the_moves_powershell_reads() {
        let before = BTreeMap::from([(1, 100), (2, 50)]);
        let after = BTreeMap::from([(1, 150), (2, 50), (3, 70)]);
        assert_eq!(
            measure_cpu(&before, &after),
            CpuMove {
                gained: 120,
                continued: 50,
                rose: true,
                appeared: true,
            },
            "a continuing process adds its delta to both, an appearing one its whole CPU to gained alone"
        );
        assert_eq!(
            measure_cpu(&before, &BTreeMap::from([(1, 100), (2, 50), (3, 70)])),
            CpuMove {
                gained: 70,
                continued: 0,
                rose: false,
                appeared: true,
            },
            "no process that was there before gained anything, so nothing rose"
        );
        assert_eq!(
            measure_cpu(&BTreeMap::from([(1, 500)]), &BTreeMap::from([(1, 400)])),
            CpuMove::default(),
            "a process whose CPU reads lower than before gains nothing and does not rise"
        );
        assert_eq!(
            measure_cpu(&BTreeMap::from([(1, 500), (2, 60)]), &BTreeMap::from([(1, 500)])),
            CpuMove::default(),
            "a process that vanished is not in the sample at all, so it neither gains nor appears"
        );
    }

    #[test]
    fn a_build_server_that_only_appeared_is_not_progress() {
        let mut watch = watching(config(1000, 5000));
        watch.sample(&server_tick(1000, &[10], &[], &[], false));
        watch.sample(&server_tick(2000, &[10], &[(99, 16_000_000)], &[], true));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(2000)), Some(Kill::Stalled { .. })),
            "a server appearing is not a server working"
        );
    }

    #[test]
    fn a_server_rising_counts_only_while_the_job_holds_a_build_client() {
        // Test-SharedCompiler: the compiler server an earlier build left running works for the client in the job.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&server_tick(1000, &[10], &[], &[(77, 1_000_000)], true));
        watch.sample(&server_tick(2000, &[10], &[], &[(77, 9_000_000)], true));
        assert!(
            watch.kill_check(Duration::from_millis(2000)).is_none(),
            "the shared compiler's CPU is the quiet job's progress"
        );

        // Test-OldServer: without a client in the job, no server's CPU counts.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&server_tick(1000, &[10], &[], &[(77, 1_000_000)], false));
        watch.sample(&server_tick(2000, &[10], &[], &[(77, 9_000_000)], false));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(2000)), Some(Kill::Stalled { .. })),
            "the same server with no client in the job is no progress"
        );
    }

    #[test]
    fn a_new_server_rising_counts_only_while_the_job_holds_a_build_client() {
        // Test-UnrelatedServer: a build server another process starts while the gate runs is new to the job, not one
        // it started; its CPU counts only while the job holds a build client.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&server_tick(1000, &[10], &[(99, 1_000_000)], &[], true));
        watch.sample(&server_tick(2000, &[10], &[(99, 9_000_000)], &[], true));
        assert!(
            watch.kill_check(Duration::from_millis(2000)).is_none(),
            "the new server's CPU is the quiet job's progress"
        );

        let mut watch = watching(config(1000, 5000));
        watch.sample(&server_tick(1000, &[10], &[(99, 1_000_000)], &[], false));
        watch.sample(&server_tick(2000, &[10], &[(99, 9_000_000)], &[], false));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(2000)), Some(Kill::Stalled { .. })),
            "the same new server with no client in the job is no progress"
        );
    }

    #[test]
    fn own_cpu_counts_the_servers_only_for_a_build_client() {
        // A server gaining 1.6 core-seconds over the 1 s sample counts as the command's own CPU with a client, so the
        // share of a machine busy on every core is a tenth; without one it is the 5% floor.
        assert_eq!(server_share(&[(99, 16_000_000)], &[], true), Some(10));
        assert_eq!(server_share(&[(99, 16_000_000)], &[], false), Some(5));
    }

    #[test]
    fn a_shared_server_counts_only_its_continuing_cpu_as_own() {
        let mut watch = Watch::new(config(60_000, 5000), Machine::new(16, 16, false), IDLE, Ticks(0), None);
        let mut first = server_tick(1000, &[], &[], &[(77, 5_000_000)], true);
        first.system = FULLY_BUSY;
        watch.sample(&first);
        assert_eq!(
            watch.last_free_percent(),
            Some(5),
            "a shared server first seen adds none of its lifetime CPU to the command's own"
        );
        let mut second = server_tick(2000, &[], &[], &[(77, 21_000_000)], true);
        second.system = FULLY_BUSY_SECOND;
        watch.sample(&second);
        assert_eq!(
            watch.last_free_percent(),
            Some(10),
            "the delta of a continuing shared server is the command's own CPU"
        );
    }

    #[test]
    fn the_cpu_of_a_child_that_exited_is_progress() {
        // Test-ShortLivedChild: the command runs one busy child after another, so the job's accounting gains the
        // floor's CPU while the pids the last sample held keep leaving it.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 100_000, 0, &[10, 11], false));
        watch.sample(&tick(2000, 200_000, 0, &[10], false));
        assert!(
            watch.kill_check(Duration::from_millis(2000)).is_none(),
            "the job gained the floor's CPU while a pid left"
        );
    }

    #[test]
    fn a_grandchild_whose_parent_exited_is_progress() {
        // Test-OrphanProgress: the busy grandchild stays in the job after the child that started it exited, so only
        // the job's CPU shows it working.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(1000, 0, 0, &[10, 11], false));
        watch.sample(&tick(2000, 100_000, 0, &[10, 11], false));
        assert!(
            watch.kill_check(Duration::from_millis(2000)).is_none(),
            "the orphaned grandchild's CPU is progress"
        );
    }

    #[test]
    fn the_vm_floor_is_exactly_a_tenth_of_the_stall_window() {
        // The floor of a stall window is a tenth of it in core-seconds, 100 000 ticks each: it is reached by exactly
        // that many ticks and not by one tick less.
        let windows: [(u64, u64); 4] = [(3, 3_000_000), (5, 5_000_000), (6, 6_000_000), (7, 7_000_000)];
        for (stall_s, floor) in windows {
            let mut watch = watching(wsl_config(stall_s * 1000, 5000));
            watch.sample(&vm_sample(1000, Some(1_000_000)));
            watch.sample(&vm_sample(stall_s * 1000, Some(1_000_000 + floor - 1)));
            assert!(
                matches!(watch.kill_check(Duration::from_millis(1000 + stall_s * 1000)), Some(Kill::Stalled { .. })),
                "a tick of the VM short of the {stall_s} s window's floor is not progress"
            );

            let mut watch = watching(wsl_config(stall_s * 1000, 5000));
            watch.sample(&vm_sample(1000, Some(1_000_000)));
            watch.sample(&vm_sample(stall_s * 1000, Some(1_000_000 + floor)));
            assert!(
                watch.kill_check(Duration::from_millis(1000 + stall_s * 1000)).is_none(),
                "the {stall_s} s window's floor itself is progress"
            );
        }
    }

    #[test]
    fn the_vm_cpu_is_never_the_commands_own_cpu() {
        // 19.9 core-seconds of VM CPU over the second sample of a machine busy on every core: the VM's CPU is not the
        // command's own, so the share stays at the floor.
        let mut watch = Watch::new(wsl_config(60_000, 5000), Machine::new(16, 16, false), IDLE, Ticks(0), None);
        let mut first = vm_sample(1000, Some(1_000_000));
        first.system = FULLY_BUSY;
        watch.sample(&first);
        let mut second = vm_sample(2000, Some(200_000_000));
        second.system = FULLY_BUSY_SECOND;
        watch.sample(&second);
        assert_eq!(watch.last_free_percent(), Some(5), "the VM's CPU is not counted as the command's own");
    }

    #[test]
    fn the_vm_core_seconds_survive_a_failed_reading() {
        // PowerShell drops the counter on a failed read but keeps VmBusy: the reading after the failure is a fresh
        // baseline, and the ticks accrued before it still count.
        let mut watch = watching(wsl_config(5000, 5000));
        watch.sample(&vm_sample(1000, Some(1_000_000)));
        watch.sample(&vm_sample(2000, Some(3_000_000)));
        watch.sample(&vm_sample(3000, None));
        watch.sample(&vm_sample(4000, Some(20_000_000)));
        watch.sample(&vm_sample(5000, Some(23_000_000)));
        assert!(
            watch.kill_check(Duration::from_millis(5000)).is_none(),
            "the 0.2 core-seconds before the failed read and the 0.3 after it reach the 0.5 floor"
        );
    }

    #[test]
    fn the_vm_accrual_resets_on_any_progress() {
        // Two 0.4 core-second stretches, unbroken, would pass the 0.5 floor of a 5 s window.
        let mut watch = watching(wsl_config(5000, 5000));
        watch.sample(&vm_sample(1000, Some(1_000_000)));
        let mut grew = vm_sample(2000, Some(5_000_000));
        grew.output_size = 1;
        watch.sample(&grew);
        let mut quiet = vm_sample(7000, Some(9_000_000));
        quiet.output_size = 1;
        watch.sample(&quiet);
        assert!(
            matches!(watch.kill_check(Duration::from_millis(7000)), Some(Kill::Stalled { .. })),
            "the log growing at 2 s reset the accrual, so the sample at 7 s adds only 0.4 core-seconds"
        );
    }

    #[test]
    fn the_first_reading_after_none_is_a_fresh_baseline() {
        let mut watch = watching(wsl_config(5000, 5000));
        watch.sample(&vm_sample(1000, Some(1_000_000)));
        watch.sample(&vm_sample(2000, None));
        watch.sample(&vm_sample(5000, Some(20_000_000)));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(5000)), Some(Kill::Stalled { .. })),
            "the reading after an unreadable one is a baseline, not 1.9 core-seconds of progress"
        );
    }

    #[test]
    fn a_vm_total_that_falls_is_a_fresh_baseline() {
        // A VM that restarted between samples reads a far lower total: that reading is only a baseline, so what the
        // new VM uses after it must reach the 5 s window's floor on its own.
        let floor = 5_000_000;
        let mut watch = watching(wsl_config(5000, 5000));
        watch.sample(&vm_sample(1000, Some(900_000_000)));
        watch.sample(&vm_sample(2000, Some(1_000_000)));
        watch.sample(&vm_sample(6000, Some(1_000_000 + floor - 1)));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(6000)), Some(Kill::Stalled { .. })),
            "a tick short of the floor after the fall is not progress"
        );

        let mut watch = watching(wsl_config(5000, 5000));
        watch.sample(&vm_sample(1000, Some(900_000_000)));
        watch.sample(&vm_sample(2000, Some(1_000_000)));
        watch.sample(&vm_sample(6000, Some(1_000_000 + floor)));
        assert!(
            watch.kill_check(Duration::from_millis(6000)).is_none(),
            "the floor reached after the fall is progress"
        );
    }

    #[test]
    fn the_first_sample_after_a_baseline_shows_the_vm_percent_since_it() {
        // 2.5 core-seconds over the first 5 s since the baseline the watch began with is half a core.
        let mut watch = Watch::new(wsl_config(60_000, 5000), Machine::new(16, 16, false), IDLE, Ticks(0), Some(1_000_000));
        assert_eq!(watch.last_vm_percent(), Some(0.0), "no sample yet");
        watch.sample(&vm_sample(5000, Some(26_000_000)));
        let percent = watch.last_vm_percent().unwrap_or(f64::NAN);
        assert!((percent - 50.0).abs() < 1e-9, "the first sample counts from the baseline, read {percent}");
        watch.sample(&vm_sample(6000, Some(56_000_000)));
        let percent = watch.last_vm_percent().unwrap_or(f64::NAN);
        assert!((percent - 300.0).abs() < 1e-9, "three cores read above 100, read {percent}");
        watch.sample(&vm_sample(7000, None));
        assert_eq!(watch.last_vm_percent(), Some(0.0), "a missing reading");
        watch.sample(&vm_sample(8000, Some(60_000_000)));
        assert_eq!(watch.last_vm_percent(), Some(0.0), "the reading after a missing one is a baseline");
        assert_eq!(watching(config(5000, 5000)).last_vm_percent(), None, "not a WSL command");
    }

    #[test]
    fn a_vm_total_that_falls_shows_no_vm_percent() {
        let mut watch = watching(wsl_config(60_000, 5000));
        watch.sample(&vm_sample(1000, Some(900_000_000)));
        watch.sample(&vm_sample(2000, Some(1_000_000)));
        assert_eq!(watch.last_vm_percent(), Some(0.0), "a VM that restarted used nothing yet");
    }

    #[test]
    fn the_vm_counts_only_for_a_wsl_command() {
        let mut watch = watching(config(5000, 5000));
        watch.sample(&vm_sample(1000, Some(1_000_000)));
        watch.sample(&vm_sample(5000, Some(6_000_000)));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(5000)), Some(Kill::Stalled { .. })),
            "a command that is not a WSL one counts no VM CPU"
        );
    }

    #[test]
    fn a_vm_baseline_at_the_start_counts_the_first_sample() {
        // PowerShell reads the VM once when it creates the watch, so its first sample counts what the VM used since the
        // command started: a watch given 1 000 000 ticks then reads 6 000 000 and has its 5 s window's floor.
        let mut watch = Watch::new(wsl_config(5000, 5000), Machine::new(16, 16, false), IDLE, Ticks(0), Some(1_000_000));
        watch.sample(&vm_sample(1000, Some(6_000_000)));
        assert!(
            watch.kill_check(Duration::from_millis(5000)).is_none(),
            "the first sample counts the 0.5 core-seconds the VM used since the watch began"
        );
    }
}
