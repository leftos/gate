//! The watchdog's arithmetic, as pure rules over injected facts: the share of the machine a command could have had,
//! the load-adjusted clock that share drives, the progress its samples carry (the log, the job's CPU, the processes that
//! left the job and the WSL VM), the stall, ceiling and backstop kills with the lines they write, the diagnostics block
//! a kill writes before it terminates the job, and the box heartbeat's beat rule and the pump that writes it. Nothing
//! here reads a clock, a process or a file: the time, the samples and the CPU are given to it, so the crate builds and
//! tests on Linux.

mod cores;
mod diagnostics;
mod heartbeat;
mod share;
mod watch;

pub use cores::{Cores, Hint, SlotKind, hint};
pub use diagnostics::{JOB_LINES, OUTSIDE_LINES, ProcessFacts, Report, SampleFacts, SampleHistory, WslVm, failed_block, failed_line, lines};
pub use heartbeat::{BeatClock, CpuSource, Pump, Ticker};
pub use share::{Machine, SystemTimes, Ticks};
pub use watch::{Config, CpuMove, Kill, Sample, Watch, is_wsl_command, measure_cpu, sampler_failed_line, wsl_unavailable_line};
