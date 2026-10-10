//! The builder's last boot: the `gate: remote boot <time>` line. The client compares two such lines from one builder
//! with a slack, so the jitter of reading the boot time as now less the time since the start is harmless.

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::console::Console;

/// Says the boot line on stdout; a boot time that cannot be read is said on stderr instead, and fails nothing.
pub(crate) fn say(console: &mut Console) {
    match read() {
        Ok(time) => console.out(&format!("gate: remote boot {}", round_trip(time))),
        Err(message) => console.err(&format!("gate: remote could not read the boot time: {message}")),
    }
}

fn read() -> Result<Timestamp, String> {
    let time = gate_win32::boot_time().map_err(|error| error.to_string())?;
    Timestamp::try_from(time).map_err(|error| error.to_string())
}

/// `time` in .NET's round-trip `o` format of a UTC time: seven fractional digits (100 ns ticks, truncated), then `Z`.
fn round_trip(time: Timestamp) -> String {
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
mod tests {
    use jiff::Timestamp;

    use super::round_trip;

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
}
