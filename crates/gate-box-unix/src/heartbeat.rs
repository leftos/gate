//! The Unix box's heartbeat: gate-watchdog's [`Pump`] saying `gate-remote-heartbeat: <cpu ms>` on stderr through the
//! box's [`Forwarding`] output, from before the checkout to the run's end, every [`Periods::heartbeat`]. Each beat is
//! said under the output lock and dropped once the client is gone; each idle tick probes the client without the lock.
//! The pump never signals and never decides an ending: the run's main thread does both.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use gate_watchdog::{Pump, Ticker};

use crate::console::{Forwarding, Stamp, Stream};
use crate::forward::DRAIN;

/// How often the heartbeat looks, and the box's ceiling samples, unless a test says otherwise.
pub const PERIOD: Duration = Duration::from_secs(5);

/// The nanoseconds in one 100 ns tick.
const NANOS_PER_TICK: u128 = 100;

/// How often the run's watches look: the heartbeat's period and the box ceiling's sample period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Periods {
    /// How often the heartbeat looks, and so the most often it says a line.
    pub heartbeat: Duration,
    /// How often the box's ceiling samples the command.
    pub sample: Duration,
}

impl Default for Periods {
    /// [`PERIOD`] for both.
    fn default() -> Periods {
        Periods {
            heartbeat: PERIOD,
            sample: PERIOD,
        }
    }
}

/// Starts the heartbeat, saying its lines on stderr through `output`, looking every `period`.
///
/// # Errors
///
/// Why the pump's thread could not be started.
pub fn start(output: Arc<dyn Forwarding>, period: Duration) -> io::Result<Pump> {
    let writer = Beats {
        output,
        never: AtomicBool::new(false),
    };
    Pump::start(writer, Arc::new(BoxClock { started: Instant::now() }), period)
}

/// Stops `pump` at the run's end, waiting at most [`DRAIN`] for its thread, so a client that stopped reading cannot
/// keep the work lock held. Nothing is said for a pump left stuck or one that panicked: the stuck stream is the one it
/// would be said on, and a panicking pump has said so itself whenever it still could.
pub fn stop(pump: Pump) {
    // The run's status stands whichever way the pump ended.
    let _ = pump.stop_within(DRAIN);
}

/// The box's monotonic clock in 100 ns ticks since the heartbeat started.
struct BoxClock {
    started: Instant,
}

impl Ticker for BoxClock {
    fn now_ticks(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos() / NANOS_PER_TICK).unwrap_or(u64::MAX)
    }
}

/// The pump's writer: each line the pump writes is said on stderr through the output, under its lock, unless the
/// client is gone ([`Forwarding::say_unless`] with a stop flag never set); a flush probes the client
/// ([`Forwarding::client_gone_probe`]). A client gone fails the write or the flush, which ends the heartbeats.
struct Beats {
    output: Arc<dyn Forwarding>,
    never: AtomicBool,
}

impl Write for Beats {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let line = text.strip_suffix('\n').unwrap_or(&text);
        self.output.say_unless(&self.never, Stream::Err, Stamp::Bare, line)?;
        if self.output.client_gone() {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.output.client_gone_probe() {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::Beats;
    use crate::console::{Forwarding, HungUpSink, Output, RecordingSink, Stream};

    #[test]
    fn the_writer_says_each_beat_on_stderr_and_a_gone_client_ends_the_flush() {
        let output = Arc::new(Output::new(RecordingSink::default()));
        let mut beats = Beats {
            output: Arc::clone(&output) as Arc<dyn Forwarding>,
            never: AtomicBool::new(false),
        };
        assert!(beats.write_all(b"gate-remote-heartbeat: 5\n").is_ok());
        assert!(beats.flush().is_ok(), "a client still there passes the flush");
        assert!(beats.write_all(b"gate-remote-heartbeat: 7\n").is_ok());
        assert_eq!(
            output.sink().lines(),
            [
                (Stream::Err, "gate-remote-heartbeat: 5".to_owned()),
                (Stream::Err, "gate-remote-heartbeat: 7".to_owned())
            ],
            "each beat one line on stderr"
        );
        output.mark_gone();
        assert!(beats.write_all(b"gate-remote-heartbeat: 9\n").is_err(), "a gone client fails the write");
        assert_eq!(output.sink().lines().len(), 2, "and nothing is said");

        let hung_up = Arc::new(Output::new(HungUpSink));
        let mut probed = Beats {
            output: Arc::clone(&hung_up) as Arc<dyn Forwarding>,
            never: AtomicBool::new(false),
        };
        assert!(probed.flush().is_err(), "a client the probe sees gone ends the flush");
        assert!(hung_up.client_gone());
    }
}
