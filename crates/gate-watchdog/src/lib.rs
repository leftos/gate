//! The watchdog's arithmetic, as pure rules over injected facts: the share of the machine a command could have had,
//! the load-adjusted clock that share drives, the stall, ceiling and backstop kills with the lines they write, and the
//! box heartbeat's beat rule and the pump that writes it. Nothing here reads a clock, a process or a file: the time,
//! the samples and the CPU are given to it, so the crate builds and tests on Linux.

mod heartbeat;
mod share;
mod watch;

pub use heartbeat::{BeatClock, CpuSource, Pump, Ticker};
pub use share::{Machine, SystemTimes, Ticks, free_share};
pub use watch::{Config, Kill, Sample, Watch, sampler_failed_line};
