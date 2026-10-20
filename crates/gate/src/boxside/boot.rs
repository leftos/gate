//! The builder's last boot: the `gate: remote boot <time>` line. The client compares two such lines from one builder
//! with a slack, so the jitter of reading the boot time as now less the time since the start is harmless.

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::console::Console;

/// Says the boot line on stdout through `console` and returns whether the boot time was read; a boot time that cannot be
/// read is said on stderr instead. Only the `Boot` verb turns the failure into a status; the preflight fails nothing.
pub(crate) fn say(console: &mut Console) -> bool {
    say_with(read, console)
}

/// Says what `reader` gave: the round-trip line on stdout when it read the time, else the failure on stderr. Returns
/// whether the boot time was read.
pub(crate) fn say_with(reader: impl FnOnce() -> Result<Timestamp, String>, console: &mut Console) -> bool {
    match reader() {
        Ok(time) => {
            console.out(&boot_line(time));
            true
        }
        Err(message) => {
            console.err(&boot_failure(&message));
            false
        }
    }
}

/// The `gate: remote boot <time>` line for `time`.
fn boot_line(time: Timestamp) -> String {
    format!("gate: remote boot {}", round_trip(time))
}

/// The `gate: remote could not read the boot time: <message>` line for `message`.
fn boot_failure(message: &str) -> String {
    format!("gate: remote could not read the boot time: {message}")
}

fn read() -> Result<Timestamp, String> {
    let time = gate_win32::boot_time().map_err(|error| error.to_string())?;
    Timestamp::try_from(time).map_err(|error| error.to_string())
}

/// `time` in .NET's round-trip `o` format of a UTC time: seven fractional digits (100 ns ticks, truncated), then `Z`.
pub(super) fn round_trip(time: Timestamp) -> String {
    let utc = time.to_zoned(TimeZone::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}Z",
        utc.year(),
        utc.month(),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.subsec_nanosecond() / 100
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose fixed boot time does not parse must fail loudly")]
mod tests {
    use jiff::Timestamp;

    use crate::console::Console;

    use super::{boot_line, round_trip, say_with};

    fn formatted(text: &str) -> Option<String> {
        text.parse::<Timestamp>().ok().map(round_trip)
    }

    #[test]
    fn the_boot_time_has_seven_fractional_digits_and_a_z() {
        assert_eq!(formatted("2026-10-09T21:09:31.121Z").as_deref(), Some("2026-10-09T21:09:31.1210000Z"));
        assert_eq!(formatted("2026-01-02T03:04:05Z").as_deref(), Some("2026-01-02T03:04:05.0000000Z"));
        assert_eq!(
            formatted("2026-10-09T23:59:59.123456789+02:00").as_deref(),
            Some("2026-10-09T21:59:59.1234567Z"),
            "UTC, the ticks truncated"
        );
    }

    #[test]
    fn a_read_boot_time_is_said_as_the_round_trip_line_and_reports_success() {
        let time = "2026-10-09T21:09:31.121Z".parse::<Timestamp>().expect("the boot time parses");
        assert_eq!(boot_line(time), "gate: remote boot 2026-10-09T21:09:31.1210000Z");

        let mut console = Console::new();
        assert!(say_with(|| Ok(time), &mut console), "a read boot time is a success");
        assert!(console.take_notes().is_empty(), "the boot line is said on stdout, not gathered as a note");
    }

    #[test]
    fn a_boot_time_that_cannot_be_read_is_said_on_stderr_and_reports_failure() {
        let mut console = Console::new();
        assert!(
            !say_with(|| Err("the boot fell over".to_owned()), &mut console),
            "an unread boot time is a failure"
        );
        assert_eq!(console.take_notes(), ["gate: remote could not read the boot time: the boot fell over"]);
    }
}
