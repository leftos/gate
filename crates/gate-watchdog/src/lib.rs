//! The watchdog's arithmetic, as pure rules over injected facts: the share of the machine a command could have had,
//! the load-adjusted clock that share drives, the stall, ceiling and backstop kills with the lines they write, the
//! diagnostics block a kill writes before it terminates the job, and the box heartbeat's beat rule and the pump that
//! writes it. Nothing here reads a clock, a process or a file: the time, the samples and the CPU are given to it, so
//! the crate builds and tests on Linux.

mod diagnostics;
mod heartbeat;
mod share;
mod watch;

pub use diagnostics::{ProcessFacts, Report, SampleFacts, SampleHistory, ServerFacts, WslVm, failed_line, lines};
pub use heartbeat::{BeatClock, CpuSource, Pump, Ticker};
pub use share::{Machine, SystemTimes, Ticks, free_share};
pub use watch::{Config, Kill, Sample, Watch, sampler_failed_line};
