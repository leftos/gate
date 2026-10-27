//! When a wait's next still-waiting line is due: every period, counted from the start of the wait, skipping any period
//! a slow poll missed. `gate cutover` and `gate drain` say theirs on it.

use std::time::Duration;

/// When the next still-waiting line is due, counted from the start of the wait.
#[derive(Debug)]
pub(crate) struct Heartbeat {
    period: Duration,
    next: Duration,
}

impl Heartbeat {
    pub(crate) fn new(period: Duration) -> Heartbeat {
        Heartbeat { period, next: period }
    }

    /// `elapsed`, the time since the start of the wait, when a still-waiting line is due then, which moves the next one
    /// to the first period boundary after it, skipping any missed; else `None`.
    pub(crate) fn due(&mut self, elapsed: Duration) -> Option<Duration> {
        if elapsed < self.next {
            return None;
        }
        while self.next <= elapsed {
            self.next += self.period;
        }
        Some(elapsed)
    }

    /// How long after `elapsed` the next still-waiting line is due.
    pub(crate) fn until_due(&self, elapsed: Duration) -> Duration {
        self.next.saturating_sub(elapsed)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Heartbeat;

    fn secs(seconds: f64) -> Duration {
        Duration::from_secs_f64(seconds)
    }

    #[test]
    fn the_heartbeat_keeps_its_cadence_from_the_start_and_skips_missed_periods() {
        let mut heartbeat = Heartbeat::new(Duration::from_secs(30));
        assert_eq!(heartbeat.due(secs(29.9)), None);
        assert_eq!(heartbeat.until_due(secs(29.9)), secs(30.0).saturating_sub(secs(29.9)));
        assert_eq!(heartbeat.due(secs(30.0)), Some(secs(30.0)));
        assert_eq!(heartbeat.until_due(secs(30.0)), secs(30.0), "the next is at 60 s");
        assert_eq!(heartbeat.due(secs(61.0)), Some(secs(61.0)));
        assert_eq!(heartbeat.until_due(secs(61.0)), secs(29.0), "the next is at 90 s, not 91 s");
        assert_eq!(heartbeat.due(secs(95.0)), Some(secs(95.0)));
        assert_eq!(heartbeat.until_due(secs(95.0)), secs(25.0), "the next is at 120 s");
        assert_eq!(heartbeat.due(secs(119.0)), None);
    }
}
