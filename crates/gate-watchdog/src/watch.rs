//! The state of one watched command: the load-adjusted clock the samples advance, and the share of the machine it
//! counts at.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::cores::{Cores, tenths};
use crate::share::{Machine, SystemTimes, Ticks, free_cores, share_of};

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
    /// Whether the stall window runs only from the command's first output byte, so a quiet start is no stall: the test
    /// seam `--test-stall-from-output`, which keeps a starved command's own start-up out of the window.
    pub stall_from_output: bool,
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
    /// Whether the job holds a live gate other than the watching one: a nested gate, whose own watchdog judges its
    /// command, though it may write nothing and use next to no CPU here, as a routed one does.
    pub nested_gate: bool,
    /// The processes outside the job under the processes it holds, by pid and start (in 100 ns ticks since 1601), with
    /// the CPU time each has used in 100 ns ticks: a packaged app a process of the job starts runs in a job of its own,
    /// and its CPU is the command's own.
    pub escaped: BTreeMap<(u32, u64), u64>,
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
    escaped: BTreeMap<(u32, u64), u64>,
    /// The CPU the processes outside the job gained, sample by sample, since the last progress: one that exits takes
    /// none of it with it.
    escaped_busy: u64,
    vm_cpu: Option<u64>,
    vm_busy: u64,
    vm_percent: f64,
    sampled: bool,
    sampler_failed: bool,
    tally: Tally,
}

/// The command's busy and free cores counted so far, which [`Watch::cores`] reads.
#[derive(Clone, Debug, Default)]
struct Tally {
    /// The command's own CPU in 100 ns ticks: every unpaused sample's, and the stretch after the last sample, which
    /// the exit's last read measures.
    own_ticks: u64,
    /// The highest own CPU over one unpaused sample's interval, in cores.
    peak_cores: f64,
    /// The free cores of every unpaused sample, summed.
    free_cores_sum: f64,
    /// The unpaused samples counted in `free_cores_sum`.
    unpaused_samples: u32,
    /// Whether the command's CPU could not be read at its end, so its cores are unknown.
    unread: bool,
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
            escaped: BTreeMap::new(),
            escaped_busy: 0,
            vm_cpu,
            vm_busy: 0,
            vm_percent: 0.0,
            sampled: false,
            sampler_failed: false,
            tally: Tally::default(),
        }
    }

    /// Measures the command on `machine` from the next sample on: a command that ran on a critical parent's slot runs on
    /// a slot of its own kind once its gate took one.
    pub fn run_on(&mut self, machine: Machine) {
        self.machine = machine;
    }

    /// Takes one sample: the interval since the last one advances the clock at the share of the machine the command
    /// could have had, or counts as paused time while a gate under this one waits behind a critical request. The
    /// interval runs between the samples' end stamps, the moment their system times and job accounting were read, and
    /// a progress sample marks its own end stamp as the last progress, since the CPU and the job's figures that show
    /// it were read next to that stamp; the stall is judged at the latest sample's begin stamp, so a watcher starved
    /// between its two stamps does not count its own read time as the command's quiet time. The sample is progress
    /// when the log's size changed either way, the CPU of the job and of the processes outside it since the last
    /// progress reached 1% of one core over the stall window, a pid appeared in the job that the last sample did not
    /// have or a process outside it the last sample did not have, the WSL VM passed its floor, the job holds a nested
    /// gate, or the run is paused. Only the pause stops the clock; a nested gate's time is counted as any other.
    pub fn sample(&mut self, sample: &Sample) {
        let interval = sample.at.saturating_sub(self.last_at).max(MIN_INTERVAL);
        let escaped = measure_cpu(&self.escaped, &sample.escaped);
        let own_gained = own_gained(sample.job_cpu.saturating_sub(self.job_cpu), escaped);
        let vm_working = self.vm_working(sample.vm_cpu, interval);
        self.escaped_busy = self.escaped_busy.saturating_add(escaped.gained);
        if self.progressed(sample, escaped, vm_working) {
            self.last_progress = sample.at;
            self.progress_cpu = sample.job_cpu;
            self.escaped_busy = 0;
            self.vm_busy = 0;
        }
        let free_cores = free_cores(self.machine, self.system, sample.system, own_gained, interval.as_secs_f64());
        let free = share_of(self.machine, free_cores);
        if sample.paused {
            self.paused += interval;
        } else {
            self.adjusted += interval.as_secs_f64() * free;
            self.count_cores(own_gained, interval, free_cores);
        }
        self.last_free = free;
        self.sampled = true;
        self.last_at = sample.at;
        self.last_began = sample.began;
        self.system = sample.system;
        self.job_cpu = sample.job_cpu;
        self.output_size = sample.output_size;
        self.pids.clone_from(&sample.pids);
        self.escaped.clone_from(&sample.escaped);
    }

    /// Whether `sample` is progress: the log changed either way, the job's CPU since the last progress and the CPU the
    /// processes outside it gained sample by sample since then reached 1% of one core over the stall window together,
    /// a pid appeared in the job or a process outside it that the last sample did not have, the WSL VM worked, the job
    /// holds a live nested gate, or a gate under this one waits.
    fn progressed(&self, sample: &Sample, escaped: CpuMove, vm_working: bool) -> bool {
        let floor = self.config.stall.as_nanos() / FLOOR_NANOS_PER_TICK;
        let grew = sample.output_size != self.output_size;
        let job_busy = u128::from(sample.job_cpu.saturating_sub(self.progress_cpu).0);
        let working = job_busy + u128::from(self.escaped_busy) >= floor;
        let appeared = !sample.pids.is_subset(&self.pids) || escaped.appeared;
        grew || working || appeared || sample.paused || sample.nested_gate || vm_working
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

    /// Counts one unpaused sample's own CPU, `own_gained` over `interval`, toward the average and the peak, and the
    /// `free_cores` it found toward their mean.
    fn count_cores(&mut self, own_gained: Ticks, interval: Duration, free_cores: f64) {
        let tally = &mut self.tally;
        tally.own_ticks = tally.own_ticks.saturating_add(own_gained.0);
        let own_cores = ticks_seconds(own_gained) / interval.as_secs_f64();
        tally.peak_cores = tally.peak_cores.max(own_cores);
        tally.free_cores_sum += free_cores;
        tally.unpaused_samples += 1;
    }

    /// The command's CPU read once more as it exited: the job's total CPU `job_cpu`, and that of the processes outside
    /// the job the last sample counted that still run, `escaped` as a sample carries them. The stretch since the last
    /// sample counts toward the average on the same basis as a sample's. Nothing is counted once the sampler has
    /// failed.
    pub fn final_cpu(&mut self, job_cpu: Ticks, escaped: &BTreeMap<(u32, u64), u64>) {
        if self.sampler_failed {
            return;
        }
        let gained = own_gained(job_cpu.saturating_sub(self.job_cpu), measure_cpu(&self.escaped, escaped));
        self.tally.own_ticks = self.tally.own_ticks.saturating_add(gained.0);
        self.job_cpu = job_cpu;
        self.escaped.clone_from(escaped);
    }

    /// Records that the command's CPU could not be read at its end, so its cores are unknown; the load-adjusted clock
    /// is unaffected.
    pub fn cores_unread(&mut self) {
        self.tally.unread = true;
    }

    /// The command's average, peak and free cores: the own CPU counted over the wall time the watch counted, the
    /// paused stretches left out of both, and the mean of the free cores the unpaused samples found, each kept to one
    /// decimal; and the busiest unpaused sample interval, unrounded. `None` before an unpaused sample, once the sampler
    /// has failed, and when the CPU could not be read at the command's end.
    #[must_use]
    pub fn cores(&self) -> Option<Cores> {
        let counted = self.last_at.saturating_sub(self.paused);
        let tally = &self.tally;
        if tally.unpaused_samples == 0 || self.sampler_failed || tally.unread || counted.is_zero() {
            return None;
        }
        Some(Cores {
            avg: tenths(ticks_seconds(Ticks(tally.own_ticks)) / counted.as_secs_f64()),
            peak: tally.peak_cores,
            free: tenths(tally.free_cores_sum / f64::from(tally.unpaused_samples)),
        })
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
    /// clock no longer move. With [`Config::stall_from_output`] the stall waits for the command's first output byte, so
    /// a quiet start is no stall; the ceiling and the backstop are unaffected.
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
            // The seam's quiet start: while the command has written no byte, its window has not begun.
            let quiet_start = self.config.stall_from_output && self.output_size == 0;
            if !quiet_start && quiet >= self.config.stall {
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

/// How far one set of processes moved between two samples: the CPU gained, and whether a process appeared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuMove {
    /// Every process's CPU gained, an appearing process's whole CPU with it.
    pub gained: u64,
    /// Whether a process appeared that the earlier sample did not have.
    pub appeared: bool,
}

/// The CPU the processes of `after` gained on `before`, each known by its key (a pid, or a pid and a start), as the
/// PowerShell gate's `Measure-Cpu` measures it: an appearing process adds its whole CPU, and a process seen before what
/// it gained since.
#[must_use]
pub fn measure_cpu<K: Ord>(before: &BTreeMap<K, u64>, after: &BTreeMap<K, u64>) -> CpuMove {
    let mut moved = CpuMove::default();
    for (key, &cpu) in after {
        match before.get(key) {
            None => {
                moved.appeared = true;
                moved.gained += cpu;
            }
            Some(&was) => {
                moved.gained += cpu.saturating_sub(was);
            }
        }
    }
    moved
}

/// `ticks` of CPU in seconds.
fn ticks_seconds(ticks: Ticks) -> f64 {
    Duration::from_nanos(ticks.0.saturating_mul(100)).as_secs_f64()
}

/// The command's own CPU over the interval, in ticks: its job's gain and the gain of the processes that left it.
fn own_gained(job: Ticks, escaped: CpuMove) -> Ticks {
    Ticks(job.0 + escaped.gained)
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
        self.said(spoken, "")
    }

    /// The line a builder says for this kill of the command `spoken` it ran for a PC that calls it `host`: the gate's
    /// own line with ` on {host}` after its verdict words, `TIMED OUT on {host} after …`.
    #[must_use]
    pub fn line_on(&self, spoken: &str, host: &str) -> String {
        self.said(spoken, &format!(" on {host}"))
    }

    /// The line for this kill, `place` right after its verdict words.
    fn said(&self, spoken: &str, place: &str) -> String {
        match *self {
            Kill::Stalled { quiet, free_percent } => format!(
                "gate: STALLED{place}: no output and no CPU for {} s (machine free {free_percent}% on average); \
                 killed {spoken} and its children",
                quiet.as_secs_f64().round_ties_even()
            ),
            Kill::TimedOut { timeout, wall, free_percent } => format!(
                "gate: TIMED OUT{place} after {timeout} s of load-adjusted time (wall {} s, machine free \
                 {free_percent}% on average); killed {spoken} and its children",
                wall.as_secs_f64().round_ties_even()
            ),
            Kill::Backstop {
                timeout,
                wall,
                adjusted,
                free_percent,
            } => format!(
                "gate: BACKSTOP{place}: {} s of wall time (5 x {timeout} s ceiling, {}); killed {spoken} and its children",
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

/// Whether the command's first word `first_word` names `wsl`: its file name with a trailing `.exe` stripped is `wsl`,
/// ignoring case. A path is allowed, and a file name starts after a drive's `:`, as `Path.GetFileName` reads it.
#[must_use]
pub fn is_wsl_command(first_word: &str) -> bool {
    let file = first_word.rsplit(['\\', '/', ':']).next().unwrap_or_default();
    strip_exe(file).eq_ignore_ascii_case("wsl")
}

/// `file` without a trailing `.exe`, ignoring case.
fn strip_exe(file: &str) -> &str {
    if file.len() >= 4 && file.as_bytes()[file.len() - 4..].eq_ignore_ascii_case(b".exe") {
        return &file[..file.len() - 4];
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command's first word names `wsl` by its file name with a trailing `.exe` stripped, a path allowed.
    #[test]
    fn wsl_commands_are_read_from_the_file_name() {
        let rows: [(&str, bool); 9] = [
            ("wsl", true),
            ("wsl.exe", true),
            ("WSL.EXE", true),
            (r"C:\Windows\System32\wsl.exe", true),
            ("C:wsl.exe", true),
            ("/usr/bin/wsl", true),
            ("wslconfig", false),
            ("bash.exe", false),
            ("", false),
        ];
        for (word, want) in rows {
            assert_eq!(is_wsl_command(word), want, "{word}");
        }
    }

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
            stall_from_output: false,
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
            nested_gate: false,
            escaped: BTreeMap::new(),
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
            nested_gate: false,
            escaped: BTreeMap::new(),
            vm_cpu: None,
        }
    }

    /// A sample at `ms` of a half-busy machine, so its share over that stretch is 0.5.
    fn half_sample(ms: u64) -> Sample {
        sample(ms, HALF_BUSY, false)
    }

    /// A sample at `ms` of an idle machine whose job holds one process and has used `job_cpu`, with `escaped` the
    /// processes outside the job by pid and start, each with its CPU.
    fn away_tick(ms: u64, job_cpu: u64, escaped: &[((u32, u64), u64)]) -> Sample {
        let mut ticked = tick(ms, job_cpu, 0, &[10], false);
        ticked.escaped = escaped.iter().copied().collect();
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
    fn a_quiet_start_is_no_stall_until_the_first_output_when_the_window_starts_at_output() {
        // Twice the window with nothing written and no CPU used: the quiet start is no stall while the command has not
        // printed a byte.
        let mut watch = watching(Config {
            stall_from_output: true,
            ..config(1000, 5000)
        });
        watch.sample(&tick(600, 0, 0, &[], false));
        watch.sample(&tick(2600, 0, 0, &[], false));
        assert!(watch.kill_check(Duration::from_millis(2600)).is_none(), "a quiet start is no stall");
        // The first byte at 3600 starts the window, and the quiet window after it is a stall.
        watch.sample(&tick(3600, 0, 1, &[], false));
        assert!(
            watch.kill_check(Duration::from_millis(3600)).is_none(),
            "the first output starts the window"
        );
        watch.sample(&tick(4600, 0, 1, &[], false));
        assert!(
            matches!(
                watch.kill_check(Duration::from_millis(4600)),
                Some(Kill::Stalled { quiet, .. }) if quiet == Duration::from_millis(1000)
            ),
            "a quiet window after the first output is a stall"
        );
    }

    #[test]
    fn a_quiet_start_is_a_stall_without_the_setting() {
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(600, 0, 0, &[], false));
        watch.sample(&tick(2600, 0, 0, &[], false));
        assert!(
            matches!(
                watch.kill_check(Duration::from_millis(2600)),
                Some(Kill::Stalled { quiet, .. }) if quiet == Duration::from_millis(2600)
            ),
            "without the setting the quiet start is a stall, quiet measured from the start"
        );
    }

    #[test]
    fn a_late_watcher_does_not_count_its_own_delay_as_quiet() {
        // The watcher wakes 26.4 s after the sample it stamped: a baseline sample at 600 ms, then a kill check read at
        // 27 s, once the starved sample's reads had finished.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(600, 0, 0, &[], false));
        assert!(
            watch.kill_check(Duration::from_secs(27)).is_none(),
            "the delay between the sample's stamp and the kill check is not the command's quiet time"
        );
        // The next sample, stamped 5 s in, finds the job's CPU further along: that CPU is progress.
        watch.sample(&tick(5600, CORE_SECOND, 0, &[], false));
        assert!(
            watch.kill_check(Duration::from_millis(5600)).is_none(),
            "the job CPU that rose between the samples is progress"
        );
    }

    #[test]
    fn a_late_sample_with_no_progress_is_still_a_stall() {
        // A sample stamped 27 s in with the same facts as its baseline: the command really was quiet.
        let mut watch = watching(config(1000, 5000));
        watch.sample(&tick(600, 0, 0, &[], false));
        watch.sample(&tick(27_000, 0, 0, &[], false));
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
    fn a_sample_with_a_live_nested_gate_is_progress_but_keeps_the_clock_running() {
        // A 1 s stall window and a 2 s ceiling; the job's CPU, its log and its pids never move.
        let mut watch = watching(config(1000, 2));
        let nested = |ms: u64| Sample {
            nested_gate: true,
            ..tick(ms, 0, 0, &[], false)
        };
        watch.sample(&nested(900));
        watch.sample(&nested(1800));
        assert!(
            watch.kill_check(Duration::from_millis(1800)).is_none(),
            "1.8 s without other progress: the nested gate is progress"
        );
        assert!((watch.adjusted_seconds() - 1.8).abs() < 1e-9, "its seconds are load-adjusted time");
        assert_eq!(watch.paused_time(), Duration::ZERO, "and no paused time, so the backstop counts them");

        watch.sample(&nested(2700));
        assert!(
            matches!(watch.kill_check(Duration::from_millis(2700)), Some(Kill::TimedOut { .. })),
            "the ceiling still ends a run whose nested gate lives"
        );
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
            CpuMove { gained: 120, appeared: true },
            "a continuing process adds its delta, an appearing one its whole CPU"
        );
        assert_eq!(
            measure_cpu(&before, &BTreeMap::from([(1, 100), (2, 50), (3, 70)])),
            CpuMove { gained: 70, appeared: true },
            "no process that was there before gained anything, so only the appearing one counts"
        );
        assert_eq!(
            measure_cpu(&BTreeMap::from([(1, 500)]), &BTreeMap::from([(1, 400)])),
            CpuMove::default(),
            "a process whose CPU reads lower than before gains nothing"
        );
        assert_eq!(
            measure_cpu(&BTreeMap::from([(1, 500), (2, 60)]), &BTreeMap::from([(1, 500)])),
            CpuMove::default(),
            "a process that vanished is not in the sample at all, so it neither gains nor appears"
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

    /// 100 ns ticks in one core-second.
    const CORE_SECOND: u64 = 10_000_000;

    /// The average, peak and free cores `watch` found, which must be known.
    #[expect(clippy::expect_used, reason = "a case whose cores are unknown must fail loudly")]
    fn known_cores(watch: &Watch) -> Cores {
        watch.cores().expect("a watch with an unpaused sample knows its cores")
    }

    fn assert_near(got: f64, want: f64, what: &str) {
        assert!((got - want).abs() < 1e-9, "{what}: got {got}, expected {want}");
    }

    #[test]
    fn cores_average_the_own_cpu_over_the_wall_time_with_the_final_stretch() {
        let mut watch = watching(config(60_000, 5000));
        // 20 core-seconds over the first 5 s, 10 over the next 5, then 4 over the last 2 s that only the exit's read sees.
        watch.sample(&tick(5000, 20 * CORE_SECOND, 0, &[], false));
        watch.sample(&tick(10_000, 30 * CORE_SECOND, 0, &[], false));
        watch.final_cpu(Ticks(34 * CORE_SECOND), &BTreeMap::new());
        watch.exited(Duration::from_secs(12));
        let cores = known_cores(&watch);
        assert_near(cores.avg, 2.8, "34 core-seconds over 12 s is 2.83, kept to one decimal");
        assert_near(cores.peak, 4.0, "the busiest interval kept 4 cores busy");
        assert_near(cores.free, 16.0, "an idle 16-core machine leaves every core free");
    }

    #[test]
    fn the_final_stretch_counts_toward_the_average_only_through_the_exit_read() {
        let mut watch = watching(config(60_000, 5000));
        watch.sample(&tick(5000, 0, 0, &[], false));
        watch.exited(Duration::from_millis(10_000));
        assert_near(known_cores(&watch).avg, 0.0, "without the exit's read the last 5 s count nothing");

        let mut watch = watching(config(60_000, 5000));
        watch.sample(&tick(5000, 0, 0, &[], false));
        watch.final_cpu(Ticks(10 * CORE_SECOND), &BTreeMap::new());
        watch.exited(Duration::from_millis(10_000));
        assert_near(known_cores(&watch).avg, 1.0, "10 core-seconds over 10 s");
        assert_near(known_cores(&watch).peak, 0.0, "the final stretch is no sampler interval");
    }

    #[test]
    fn cores_are_unknown_when_the_end_could_not_be_read() {
        let mut watch = watching(config(60_000, 5000));
        watch.sample(&tick(5000, CORE_SECOND, 0, &[], false));
        watch.cores_unread();
        watch.exited(Duration::from_millis(6000));
        assert_eq!(watch.cores(), None, "the end's read failed");
        assert!(
            (watch.adjusted_seconds() - 6.0).abs() < 1e-9,
            "the load-adjusted clock still counts the last stretch, got {}",
            watch.adjusted_seconds()
        );
    }

    #[test]
    fn paused_samples_leave_the_average_peak_free_and_wall_alone() {
        let mut watch = watching(config(60_000, 5000));
        // 25 core-seconds while paused, then 1 over the next 5 s on a half-busy machine.
        watch.sample(&tick(5000, 25 * CORE_SECOND, 0, &[], true));
        let mut busy = tick(10_000, 26 * CORE_SECOND, 0, &[], false);
        busy.system = HALF_BUSY;
        watch.sample(&busy);
        watch.exited(Duration::from_millis(10_000));
        let cores = known_cores(&watch);
        assert_near(cores.peak, 0.2, "the paused interval's 5 cores are not the peak");
        assert_near(cores.avg, 0.2, "the 1 core-second of the 5 s that were not paused");
        // Of 16 cores, 8 are busy and 0.2 of those are the command's own: 7.8 held by others, 8.2 free.
        assert_near(cores.free, 8.2, "only the unpaused sample's free cores");
    }

    #[test]
    fn free_cores_are_the_mean_over_the_samples() {
        let mut watch = watching(config(60_000, 5000));
        watch.sample(&tick(10_000, 0, 0, &[], false));
        watch.sample(&sample(20_000, HALF_BUSY_30S, false));
        // An idle machine leaves 16 free, the half-busy one 8: the mean is 12, whatever the intervals' lengths.
        assert_near(known_cores(&watch).free, 12.0, "the mean of 16 and 8");
    }

    #[test]
    fn cores_are_unknown_without_an_unpaused_sample_or_once_the_sampler_failed() {
        let mut watch = watching(config(60_000, 5000));
        watch.final_cpu(Ticks(10 * CORE_SECOND), &BTreeMap::new());
        watch.exited(Duration::from_millis(3000));
        assert_eq!(watch.cores(), None, "no sample");

        let mut watch = watching(config(60_000, 5000));
        watch.sample(&tick(5000, 0, 0, &[], true));
        watch.exited(Duration::from_millis(6000));
        assert_eq!(watch.cores(), None, "only a paused sample");

        let mut watch = watching(config(60_000, 5000));
        watch.sample(&tick(5000, 0, 0, &[], false));
        watch.sampler_failed("the job is gone");
        watch.exited(Duration::from_millis(6000));
        assert_eq!(watch.cores(), None, "the sampler failed");
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

    #[test]
    fn escaped_cpu_is_own_cpu_in_the_cores_and_no_other_load() {
        // 1.6 core-seconds over the first 1 s sample of a 16-core machine busy on every core: read as the job's CPU, as
        // a process outside the job's, and not at all.
        let watched = |job_cpu: u64, escaped: &[((u32, u64), u64)]| {
            let mut watch = watching(config(60_000, 5000));
            let mut first = away_tick(1000, job_cpu, escaped);
            first.system = FULLY_BUSY;
            watch.sample(&first);
            (watch.last_free_percent(), known_cores(&watch))
        };
        let (job_share, job) = watched(16 * CORE_SECOND / 10, &[]);
        let (away_share, away) = watched(0, &[((20, 1), 16 * CORE_SECOND / 10)]);
        let (none_share, none) = watched(0, &[]);

        assert_eq!((away_share, job_share, none_share), (Some(10), Some(10), Some(5)), "the clock's share");
        assert_near(away.avg, job.avg, "the average counts it as the job's own");
        assert_near(away.avg, 1.6, "1.6 core-seconds over 1 s");
        assert_near(away.peak, 1.6, "the busiest interval's cores");
        assert_near(away.free, none.free + 1.6, "the cores others held are fewer by the escaped cores");
    }

    #[test]
    fn an_escapee_that_exits_does_not_pull_the_stall_accrual_down() {
        // A 2 s window's floor is 200 000 ticks. Two escaped processes appear at 1 s; by 2 s one has used 120 000
        // ticks; by 3 s it has exited and the other has used `last`: the floor when `last` is 80 000, though the CPU of
        // the processes still running fell.
        let checked = |last: u64| {
            let mut watch = watching(config(2000, 5000));
            watch.sample(&away_tick(1000, 0, &[((20, 1), 0), ((21, 1), 0)]));
            watch.sample(&away_tick(2000, 0, &[((20, 1), 120_000), ((21, 1), 0)]));
            watch.sample(&away_tick(3000, 0, &[((21, 1), last)]));
            watch.kill_check(Duration::from_millis(3000))
        };
        assert!(checked(80_000).is_none(), "the CPU each gained between samples reaches the floor");
        assert!(
            matches!(checked(79_999), Some(Kill::Stalled { .. })),
            "a tick short of the floor is no progress"
        );
    }

    #[test]
    fn a_new_escaped_process_is_progress_and_a_pid_with_a_new_start_is_a_new_one() {
        for (next, what) in [
            (((21, 1), 0), "a pid the last sample did not have"),
            (((20, 2), 0), "the same pid, started later"),
        ] {
            let mut watch = watching(config(1000, 5000));
            watch.sample(&away_tick(1000, 0, &[((20, 1), 0)]));
            watch.sample(&away_tick(2000, 0, &[next]));
            assert!(watch.kill_check(Duration::from_millis(2000)).is_none(), "{what} is progress");
            watch.sample(&away_tick(3000, 0, &[next]));
            assert!(
                matches!(watch.kill_check(Duration::from_millis(3000)), Some(Kill::Stalled { .. })),
                "{what}, seen again, is no progress"
            );
        }
    }

    #[test]
    fn escaped_cpu_at_the_floor_is_progress_while_the_job_is_flat() {
        // The floor of a 1 s window is 100 000 ticks; the job's own CPU does not move after the first sample.
        for (cpu, progress) in [(100_000, true), (99_999, false)] {
            let mut watch = watching(config(1000, 5000));
            watch.sample(&away_tick(1000, 50, &[((20, 1), 0)]));
            watch.sample(&away_tick(2000, 50, &[((20, 1), cpu)]));
            assert_eq!(
                watch.kill_check(Duration::from_millis(2000)).is_none(),
                progress,
                "{cpu} ticks outside the job against the floor of 100 000"
            );
        }
    }

    #[test]
    fn job_and_escaped_cpu_each_under_the_floor_are_progress_together() {
        // The floor of a 1 s window is 100 000 ticks; the job and the process outside it each gain under it.
        for (job, away, progress) in [(50_000, 50_000, true), (50_000, 49_999, false)] {
            let mut watch = watching(config(1000, 5000));
            watch.sample(&away_tick(1000, 0, &[((20, 1), 0)]));
            watch.sample(&away_tick(2000, job, &[((20, 1), away)]));
            assert_eq!(
                watch.kill_check(Duration::from_millis(2000)).is_none(),
                progress,
                "{job} ticks in the job and {away} outside it against the floor of 100 000"
            );
        }
    }
}
