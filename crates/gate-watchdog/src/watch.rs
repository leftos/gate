//! The state of one watched command: the load-adjusted clock the samples advance, and the share of the machine it
//! counts at.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::share::{Machine, SystemTimes, Ticks, free_share};

/// The shortest interval a sample may measure, so two samples at the same instant still count a millisecond.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

/// The wall-time nanoseconds one tick of the CPU floor stands for: the 1% of a core that a 100 ns tick of CPU counts,
/// so 10 000.
const FLOOR_NANOS_PER_TICK: u128 = 10_000;

/// What kills a watched command, and when.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The ceiling on load-adjusted time, in whole seconds as the PowerShell gate's `[int]` is, so the line a kill
    /// writes can never print a different number than the one that killed.
    pub timeout: u64,
    /// The window without progress after which the command is killed as `STALLED`.
    pub stall: Duration,
}

/// One sample of a watched command: the raw facts the Win32 side reads, with the time the watch had run.
#[derive(Clone, Debug)]
pub struct Sample {
    /// How long the watch had run at this sample.
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
}

/// The state of one watched command: the load-adjusted seconds it has accrued, the seconds it spent paused, where the
/// last progress was, whether its sampler still works, and the facts the last sample left to measure the next one
/// against.
#[derive(Clone, Debug)]
pub struct Watch {
    config: Config,
    machine: Machine,
    last_at: Duration,
    last_progress: Duration,
    adjusted: f64,
    paused: Duration,
    last_free: f64,
    system: SystemTimes,
    job_cpu: Ticks,
    progress_cpu: Ticks,
    output_size: u64,
    pids: BTreeSet<u32>,
    sampled: bool,
    sampler_failed: bool,
}

impl Watch {
    /// A watch beginning now, with `system` and `job_cpu` the facts at the start of the command: the clock starts at
    /// zero, the progress clock with it, and nothing has been counted or read yet.
    #[must_use]
    pub fn new(config: Config, machine: Machine, system: SystemTimes, job_cpu: Ticks) -> Self {
        Self {
            config,
            machine,
            last_at: Duration::ZERO,
            last_progress: Duration::ZERO,
            adjusted: 0.0,
            paused: Duration::ZERO,
            last_free: 1.0,
            system,
            job_cpu,
            progress_cpu: job_cpu,
            output_size: 0,
            pids: BTreeSet::new(),
            sampled: false,
            sampler_failed: false,
        }
    }

    /// Takes one sample: the interval since the last one advances the clock at the share of the machine the command
    /// could have had, or counts as paused time while a gate under this one waits behind a critical request. The
    /// sample is progress when the log's size changed either way, the job's CPU since the last progress reached 1% of
    /// one core over the stall window, a pid appeared that the last sample did not have, or the run is paused.
    pub fn sample(&mut self, sample: &Sample) {
        let interval = sample.at.saturating_sub(self.last_at).max(MIN_INTERVAL);
        let own_gained = sample.job_cpu.saturating_sub(self.job_cpu);
        let floor = self.config.stall.as_nanos() / FLOOR_NANOS_PER_TICK;
        let grew = sample.output_size != self.output_size;
        let working = u128::from(sample.job_cpu.saturating_sub(self.progress_cpu).0) >= floor;
        let appeared = !sample.pids.is_subset(&self.pids);
        if grew || working || appeared || sample.paused {
            self.last_progress = sample.at;
            self.progress_cpu = sample.job_cpu;
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
        self.system = sample.system;
        self.job_cpu = sample.job_cpu;
        self.output_size = sample.output_size;
        self.pids.clone_from(&sample.pids);
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
    /// `paused` and is left out of the backstop, and either way the clock is brought up to `now`.
    pub fn wall_only(&mut self, now: Duration, paused: bool) {
        let stretch = now.saturating_sub(self.last_at);
        if paused {
            self.paused += stretch;
        }
        self.last_at = now;
    }

    /// The command exited at `now`: the stretch since the last sample is counted at the share the last sample found.
    /// A sampler that failed leaves no share to count it at, so nothing is added after one has.
    pub fn exited(&mut self, now: Duration) {
        if self.sampler_failed {
            return;
        }
        self.adjusted += now.saturating_sub(self.last_at).as_secs_f64() * self.last_free;
        self.last_at = now;
    }

    /// The load-adjusted seconds the clock has counted, unrounded.
    #[must_use]
    pub fn adjusted_seconds(&self) -> f64 {
        self.adjusted
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

    /// Whether a sample has failed, so the watch goes by wall time alone.
    #[must_use]
    pub fn sampler_has_failed(&self) -> bool {
        self.sampler_failed
    }

    /// The kill for the first of the three reasons that holds at `now`, or `None` while none does: the stall, then the
    /// ceiling, then the backstop. Once the sampler has failed only the backstop is read, since the stall and the
    /// load-adjusted clock no longer move.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "the ceiling is read as the double the clock counts in, as the PowerShell gate reads it"
    )]
    pub fn kill_check(&self, now: Duration) -> Option<Kill> {
        let free_percent = self.free_percent();
        if !self.sampler_failed {
            let quiet = now.saturating_sub(self.last_progress);
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

    /// The stall window and ceiling under test.
    fn config(stall_ms: u64, timeout_s: u64) -> Config {
        Config {
            stall: Duration::from_millis(stall_ms),
            timeout: timeout_s,
        }
    }

    /// A watch of a 16-core machine asking a command for every core of it: a sample that reads [`IDLE`] again sees a
    /// full share.
    fn watching(config: Config) -> Watch {
        Watch::new(config, Machine::new(16, 16, false), IDLE, Ticks(0))
    }

    /// A sample at `ms` of an idle machine: `job_cpu` used by the job, `output` bytes in the log, `pids` in the job.
    fn tick(ms: u64, job_cpu: u64, output: u64, pids: &[u32], paused: bool) -> Sample {
        Sample {
            at: Duration::from_millis(ms),
            system: IDLE,
            job_cpu: Ticks(job_cpu),
            output_size: output,
            pids: pids.iter().copied().collect(),
            paused,
        }
    }

    /// A sample at `ms` reading `system` as the machine's times since the watch began.
    fn sample(ms: u64, system: SystemTimes, paused: bool) -> Sample {
        Sample {
            at: Duration::from_millis(ms),
            system,
            job_cpu: Ticks(0),
            output_size: 0,
            pids: BTreeSet::new(),
            paused,
        }
    }

    /// A sample at `ms` of a half-busy machine, so its share over that stretch is 0.5.
    fn half_sample(ms: u64) -> Sample {
        sample(ms, HALF_BUSY, false)
    }

    #[test]
    fn free_percent_counts_nothing_as_100() {
        let watch = Watch::new(config(5000, 5), Machine::new(4, 4, false), IDLE, Ticks(0));
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
        let mut watch = Watch::new(config(5000, 5), machine, before, Ticks(0));
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
        let mut watch = Watch::new(config(5000, 5), machine, before, Ticks(0));
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
        let mut watch = Watch::new(config(5000, 5), machine, IDLE, Ticks(0));
        watch.sample(&sample(1000, IDLE, false));
        watch.sample(&sample(2000, IDLE, true));
        assert!((watch.adjusted_seconds() - 1.0).abs() < f64::EPSILON);
        assert!((watch.free_percent() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn last_free_percent_is_the_last_share_rounded_half_to_even() {
        let mut watch = Watch::new(config(5000, 5), Machine::new(4, 4, false), IDLE, Ticks(0));
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
        // A paused run: the stall window and the backstop's unpaused wall both pass; the ceiling does not.
        let mut watch = watching(config(1000, 1));
        watch.sample(&tick(10_000, 0, 0, &[], true));
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
        let mut watch = Watch::new(config(60_000, 5000), machine, IDLE, Ticks(0));
        watch.sample(&half_sample(2000));
        watch.exited(Duration::from_millis(3000));
        assert!(
            (watch.adjusted_seconds() - 1.5).abs() < f64::EPSILON,
            "the last second counts at the last share, got {}",
            watch.adjusted_seconds()
        );

        let mut watch = Watch::new(config(60_000, 5000), machine, IDLE, Ticks(0));
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
    }
}
