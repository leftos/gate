//! The box's own ceiling on a routed run's command: the local gate's load-adjusted clock, counted on the builder's
//! machine from the command's start, with the local backstop of five ceilings of wall time behind it. The stall is the
//! client's, which watches the heartbeat, so the box never kills a command as stalled.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use gate_args::{BoxSeams, Slot};
use gate_watchdog::{Config, Kill, Sample, SystemTimes, Ticks, Watch};
use gate_win32::Job;

use crate::boxside::forward::OutputLock;
use crate::boxside::stop::Ending;
use crate::console::Console;
use crate::remote::{BoxKill, adjusted_line, killed_line};

/// How often the box samples its machine unless `--test-box-sample-ms` says otherwise: the local gate's period.
const SAMPLE_PERIOD: Duration = Duration::from_secs(5);
/// The status a run whose command the box killed ends with, as a local kill's.
pub(super) const KILLED: u32 = 124;
/// The sampler seam's failure.
const SAMPLER_FAIL: &str = "--test-box-sampler-fail makes every sample fail (test seam only)";

/// What the box's ceiling needs of the run: its ceiling in whole seconds, `None` for none; the PC's name for the
/// builder, empty when the payload gave none, and the command's words, both escaped for a line; and its nonce.
pub(super) struct Spec<'a> {
    pub(super) timeout: Option<u64>,
    pub(super) host: String,
    pub(super) spoken: String,
    pub(super) nonce: &'a str,
}

/// The watch of one command under the box's ceiling: the pure watch, the job it reads, the lock its lines take, and
/// when it samples next.
pub(super) struct Ceiling<'a> {
    watch: Watch,
    job: &'a Job,
    lock: &'a OutputLock,
    started: Instant,
    period: Duration,
    next: Duration,
    sampler: Sampler,
    host: String,
    spoken: String,
    nonce: &'a str,
}

/// Where a sample's system times come from.
enum Sampler {
    /// The machine's own times.
    Machine,
    /// None: every sample fails (`--test-box-sampler-fail`).
    Failing,
    /// Times made up so the machine is free by the seam's share (`--test-box-free-percent`).
    Synthetic(Synthetic),
}

/// System times made up sample by sample so others hold all of a machine of `cores` but `percent` of it, the command's
/// own CPU counted on top of theirs: the times last made, and the job's CPU and the watch's time they were made at.
struct Synthetic {
    percent: u64,
    cores: u64,
    times: SystemTimes,
    job_cpu: Ticks,
    at: Duration,
}

impl Synthetic {
    /// The times at `at`, the command's job at `job_cpu`.
    fn next(&mut self, at: Duration, job_cpu: Ticks) -> SystemTimes {
        let interval = u64::try_from(at.saturating_sub(self.at).as_nanos() / 100).unwrap_or(u64::MAX);
        let capacity = interval.saturating_mul(self.cores);
        let others = u64::try_from(u128::from(capacity) * u128::from(100 - self.percent) / 100).unwrap_or(u64::MAX);
        let busy = others.saturating_add(job_cpu.saturating_sub(self.job_cpu).0).min(capacity);
        let idle = capacity - busy;
        self.times = SystemTimes {
            idle: self.times.idle.saturating_add(idle),
            kernel: self.times.kernel.saturating_add(idle),
            user: self.times.user.saturating_add(busy),
        };
        self.job_cpu = job_cpu;
        self.at = at;
        self.times
    }
}

impl<'a> Ceiling<'a> {
    /// The watch of the command just started in `job`, beginning now, or `None` for a run with no ceiling. Its machine
    /// is every core of the builder at normal priority, as a heavy local gate's. A first read that fails is the
    /// sampler's failure, said under `lock`: from then on only the backstop is read.
    pub(super) fn begin(spec: Spec<'a>, job: &'a Job, lock: &'a OutputLock, seams: &BoxSeams, console: &mut Console) -> Option<Ceiling<'a>> {
        let timeout = spec.timeout?;
        let processors = {
            let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
            crate::wait::processors(console)
        };
        let machine = crate::watch::machine(Slot::Heavy, processors);
        let config = Config {
            timeout,
            stall: Duration::MAX,
            wsl: false,
        };
        let sampler = if seams.sampler_fail {
            Sampler::Failing
        } else if let Some(percent) = seams.free_percent {
            Sampler::Synthetic(Synthetic {
                percent: u64::from(percent.min(100)),
                cores: u64::from(machine.cores),
                times: SystemTimes { idle: 0, kernel: 0, user: 0 },
                job_cpu: Ticks(0),
                at: Duration::ZERO,
            })
        } else {
            Sampler::Machine
        };
        let started = Instant::now();
        let first = first_read(&sampler, job);
        let (system, job_cpu) = first
            .as_ref()
            .map_or((SystemTimes { idle: 0, kernel: 0, user: 0 }, Ticks(0)), |read| *read);
        let mut ceiling = Ceiling {
            watch: Watch::new(config, machine, system, job_cpu, None),
            job,
            lock,
            started,
            period: seams.sample_ms.map_or(SAMPLE_PERIOD, Duration::from_millis),
            next: Duration::ZERO,
            sampler,
            host: spec.host,
            spoken: spec.spoken,
            nonce: spec.nonce,
        };
        if let Sampler::Synthetic(synthetic) = &mut ceiling.sampler {
            synthetic.job_cpu = job_cpu;
        }
        ceiling.next = ceiling.period;
        if let Err(error) = first {
            let line = ceiling.watch.sampler_failed(&error.to_string());
            ceiling.say(console, &[line]);
        }
        Some(ceiling)
    }

    /// One look while the command runs: a sample once its period has passed, then the kill that holds now, if any.
    pub(super) fn look(&mut self, console: &mut Console) -> Option<Kill> {
        let began = self.started.elapsed();
        if began >= self.next {
            self.next = began + self.period;
            self.take_sample(began, console);
        }
        self.watch.kill_check(self.started.elapsed())
    }

    /// The watch is over, the command having exited or been found past a limit: the stretch since the last sample is
    /// counted, so the load-adjusted seconds stop here, whenever they are said.
    pub(super) fn closed(mut self) -> Self {
        self.watch.exited(self.started.elapsed());
        self
    }

    /// The command was killed for `kill`, its job ended as `ending` says and its readers drained: says, in one hold of
    /// the box's output lock, a note naming what was left when the job still holds processes or could not be ended,
    /// then the kill line and the kill tag, as the run is a kill however its ending went, then the load-adjusted
    /// seconds.
    pub(super) fn killed(&self, kill: Kill, ending: &io::Result<Ending>, console: &mut Console) {
        let name = self.job.name();
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (tag, what) = match kill {
            Kill::Backstop { .. } => (BoxKill::Backstop, "backstop"),
            // The box's watch has no stall window, so a ceiling is the only other kill it can find.
            Kill::TimedOut { .. } | Kill::Stalled { .. } => (BoxKill::TimedOut, "ceiling"),
        };
        match ending {
            Ok(ending) if ending.left == 0 => ending.say_late(name, console),
            Ok(ending) => {
                ending.say_late(name, console);
                console.err(&format!(
                    "gate: note: the box's {what} kill left {} process(es) running in job {name}",
                    ending.left
                ));
            }
            Err(error) => console.err(&format!("gate: note: the box's {what} kill could not end job {name}: {error}")),
        }
        console.err(&self.kill_line(kill));
        console.err(&killed_line(self.nonce, tag));
        if let Some(line) = self.adjusted() {
            console.err(&line);
        }
    }

    /// The command exited and its readers drained: says the load-adjusted seconds under the box's output lock, unless
    /// the sampler failed.
    pub(super) fn ended(&self, console: &mut Console) {
        let lines: Vec<String> = self.adjusted().into_iter().collect();
        self.say(console, &lines);
    }

    /// The line for `kill` of the command, naming the PC's name for the builder when the payload gave one.
    fn kill_line(&self, kill: Kill) -> String {
        if self.host.is_empty() {
            kill.line(&self.spoken)
        } else {
            kill.line_on(&self.spoken, &self.host)
        }
    }

    /// The line saying the load-adjusted seconds of the closed watch, `None` once the sampler failed.
    fn adjusted(&self) -> Option<String> {
        (!self.watch.sampler_has_failed()).then(|| adjusted_line(self.nonce, self.watch.adjusted_seconds()))
    }

    /// Takes the sample whose reads begin at `began`, or, once the sampler has failed, brings the watch's wall time up
    /// to it; a sample that fails is the sampler's failure, said.
    fn take_sample(&mut self, began: Duration, console: &mut Console) {
        if self.watch.sampler_has_failed() {
            self.watch.wall_only(began, false);
            return;
        }
        match self.read(began) {
            Ok(sample) => self.watch.sample(&sample),
            Err(error) => {
                let line = self.watch.sampler_failed(&error.to_string());
                self.say(console, &[line]);
                self.watch.wall_only(began, false);
            }
        }
    }

    /// The sample whose reads begin at `began`: the job's CPU, then the system's times, stamped with the watch's time.
    fn read(&mut self, began: Duration) -> io::Result<Sample> {
        if matches!(self.sampler, Sampler::Failing) {
            return Err(io::Error::other(SAMPLER_FAIL));
        }
        let job_cpu = Ticks(self.job.accounting()?.cpu);
        let at = self.started.elapsed();
        let system = match &mut self.sampler {
            Sampler::Synthetic(synthetic) => synthetic.next(at, job_cpu),
            Sampler::Machine | Sampler::Failing => crate::watch::system_times()?,
        };
        Ok(Sample {
            began,
            at,
            system,
            job_cpu,
            output_size: 0,
            pids: BTreeSet::new(),
            paused: false,
            escaped: BTreeMap::new(),
            vm_cpu: None,
        })
    }

    /// Says `lines` on stderr in one hold of the box's output lock, so no other line lands between them.
    fn say(&self, console: &mut Console, lines: &[String]) {
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        for line in lines {
            console.err(line);
        }
    }
}

/// The system's times and the job's CPU at the command's start, as `sampler` reads them.
fn first_read(sampler: &Sampler, job: &Job) -> io::Result<(SystemTimes, Ticks)> {
    match sampler {
        Sampler::Failing => Err(io::Error::other(SAMPLER_FAIL)),
        Sampler::Synthetic(synthetic) => Ok((synthetic.times, Ticks(job.accounting()?.cpu))),
        Sampler::Machine => {
            let job_cpu = Ticks(job.accounting()?.cpu);
            Ok((crate::watch::system_times()?, job_cpu))
        }
    }
}
