//! The remote client's watchdog and caps, as pure rules: when a call that runs under the run's watchdog is killed, and
//! the line it is killed under; how often the watchdog looks; the stall window a remote run is given; and how much of a
//! call's output the client keeps in memory.

use std::collections::VecDeque;
use std::time::Duration;

/// The shortest stall window a remote run is given, in seconds: the box's heartbeats come every 5 s, so a shorter one
/// could fall between two of them.
const STALL_FLOOR: u64 = 15;
/// The longest and the shortest time between two looks of the watchdog.
const LONGEST_TICK: Duration = Duration::from_millis(1000);
const SHORTEST_TICK: Duration = Duration::from_millis(10);
/// The most bytes of one line the client keeps, the most lines of one call it keeps, and the most bytes those whole
/// lines may total: the last ones.
pub(super) const LINE_CAP: usize = 64 * 1024;
const BYTES_KEPT: usize = 1024 * 1024;
const LINES_KEPT: usize = 1000;
/// The most bytes the job of the sync-back's fetch may write: 4 GiB.
pub(super) const FETCH_CAP: u64 = 4 << 30;

/// Why the watchdog killed a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kill {
    /// Nothing came from the call for the stall window.
    Stalled,
    /// The run passed its ceiling.
    TimedOut,
}

/// Why the client killed a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cut {
    /// The run's watchdog killed it.
    Watchdog(Kill),
    /// Nothing came from it for this long.
    Silent(Duration),
    /// It ran for this long.
    Late(Duration),
    /// It sent a line longer than [`LINE_CAP`].
    TooLong,
    /// It was still running at the run's ceiling.
    Ceiling,
    /// Its job wrote more than this many bytes.
    TooBig(u64),
}

impl Cut {
    /// Why the call `what` was killed, as a failure's reason.
    pub(super) fn reason(self, what: &str) -> String {
        match self {
            Cut::Watchdog(Kill::Stalled) => format!("{what} stalled"),
            Cut::Watchdog(Kill::TimedOut) => format!("{what} outlived the run's ceiling"),
            Cut::Silent(window) => format!("{what} did not answer for {}", amount(window)),
            Cut::Late(limit) => format!("{what} did not answer within {}", amount(limit)),
            Cut::TooLong => format!("{what} sent a line longer than 64 KiB"),
            Cut::Ceiling => format!("{what} did not finish within the run's ceiling"),
            Cut::TooBig(cap) => format!("{what} wrote more than {}", size(cap)),
        }
    }
}

/// `bytes` in the largest binary unit, KiB, MiB or GiB, in which it is at least 1.0 once rounded half up to one
/// decimal, so 1023.99 KiB is `1.0 MiB`; in bytes when it is not even 1.0 KiB.
pub(super) fn size(bytes: u64) -> String {
    const UNITS: [(&str, u32); 3] = [("GiB", 30), ("MiB", 20), ("KiB", 10)];
    for (unit, shift) in UNITS {
        let one = 1_u128 << shift;
        let tenths = (u128::from(bytes) * 10 + one / 2) / one;
        if tenths >= 10 {
            return format!("{}.{} {unit}", tenths / 10, tenths % 10);
        }
    }
    format!("{bytes} B")
}

/// `duration` in whole seconds when it is a whole number of them, else in whole milliseconds.
fn amount(duration: Duration) -> String {
    match duration.as_millis() % 1000 {
        0 => format!("{} s", duration.as_secs()),
        _ => format!("{} ms", duration.as_millis()),
    }
}

/// Whether a call is killed: stalled once nothing came for `stall` (`idle` since the last byte), else timed out once
/// the run's clock (`elapsed`) reached its `ceiling`; `None` while neither holds.
pub(super) fn kill(idle: Duration, elapsed: Duration, stall: Duration, ceiling: Duration) -> Option<Kill> {
    if idle >= stall {
        Some(Kill::Stalled)
    } else if elapsed >= ceiling {
        Some(Kill::TimedOut)
    } else {
        None
    }
}

/// The line a call killed for `kill` is killed under, in PowerShell's words: the stall window or the ceiling in whole
/// seconds, rounded up.
pub(super) fn kill_line(kill: Kill, stall: Duration, ceiling: Duration) -> String {
    match kill {
        Kill::Stalled => format!("gate: STALLED: no output or remote CPU for {} s", whole_seconds(stall)),
        Kill::TimedOut => format!("gate: TIMED OUT after {} s", whole_seconds(ceiling)),
    }
}

/// How long the watchdog waits between two looks at a call under the `stall` window: a quarter of it, at most 1 s and
/// at least 10 ms.
pub(super) fn tick(stall: Duration) -> Duration {
    (stall / 4).clamp(SHORTEST_TICK, LONGEST_TICK)
}

/// The stall window of a remote run given `--stall <stall>`, and the line saying it was raised: `--test-stall-ms` when
/// given, as it is; else the stall, raised to 15 s when it is shorter.
pub(super) fn stall_window(stall: u64, stall_ms: Option<u64>) -> (Duration, Option<String>) {
    match stall_ms {
        Some(millis) => (Duration::from_millis(millis), None),
        None if stall < STALL_FLOOR => (
            Duration::from_secs(STALL_FLOOR),
            Some(format!(
                "gate: --stall {stall} raised to {STALL_FLOOR} for a remote run (heartbeats come every 5 s)"
            )),
        ),
        None => (Duration::from_secs(stall), None),
    }
}

/// `duration` in whole seconds, rounded up.
pub(super) fn whole_seconds(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

/// Whether the line `bytes` holds, its line end aside, is longer than [`LINE_CAP`].
pub(super) fn too_long(bytes: &[u8]) -> bool {
    let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    line.len() > LINE_CAP
}

/// How much of `bytes` ends at a character's boundary: all of it, less an unfinished UTF-8 sequence at its end, which
/// the bytes still to come may finish.
pub(super) fn complete(bytes: &[u8]) -> usize {
    for (back, byte) in bytes.iter().rev().take(3).enumerate() {
        if byte & 0xC0 != 0x80 {
            let needs = match byte {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 1,
            };
            return if needs > back + 1 { bytes.len() - (back + 1) } else { bytes.len() };
        }
    }
    bytes.len()
}

/// `line` cut to its first [`LINE_CAP`] bytes, at a character's boundary.
pub(super) fn truncated(line: &str) -> &str {
    let mut end = line.len().min(LINE_CAP);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// The lines of one call the client keeps in memory: the last [`LINES_KEPT`], each cut to [`LINE_CAP`] bytes, and those
/// whole lines totalling at most [`BYTES_KEPT`] bytes.
#[derive(Default)]
pub(super) struct Kept {
    lines: VecDeque<String>,
    /// The bytes `lines` holds.
    bytes: usize,
}

impl Kept {
    /// Keeps `line`, cut, dropping the oldest line kept while the call keeps more lines than are kept, or its whole
    /// lines total more bytes than are kept. One line is at most [`LINE_CAP`] bytes, so the one just pushed always stays.
    pub(super) fn push(&mut self, line: &str) {
        let line = truncated(line);
        self.lines.push_back(line.to_owned());
        self.bytes += line.len();
        while self.lines.len() > LINES_KEPT || self.bytes > BYTES_KEPT {
            let Some(oldest) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= oldest.len();
        }
    }

    /// The lines kept, oldest first.
    pub(super) fn into_lines(self) -> Vec<String> {
        self.lines.into()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        BYTES_KEPT, Cut, Kept, Kill, LINE_CAP, LINES_KEPT, complete, kill, kill_line, size, stall_window, tick, too_long, truncated, whole_seconds,
    };

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn an_unfinished_character_at_the_end_waits_for_the_bytes_to_come() {
        assert_eq!(complete(b"abc"), 3, "plain ASCII");
        assert_eq!(complete("a\u{e9}".as_bytes()), 3, "a finished two-byte character");
        assert_eq!(complete(&"a\u{e9}".as_bytes()[..2]), 1, "the first byte of a two-byte character");
        assert_eq!(complete(&"a\u{20ac}".as_bytes()[..3]), 1, "two bytes of a three-byte character");
        assert_eq!(complete(&"\u{1f600}".as_bytes()[..3]), 0, "three bytes of a four-byte character");
        assert_eq!(complete("\u{1f600}".as_bytes()), 4, "a finished four-byte character");
        assert_eq!(complete(b""), 0);
    }

    #[test]
    fn a_call_cut_for_what_it_wrote_names_the_cap_in_its_largest_binary_unit() {
        assert_eq!(Cut::TooBig(4 << 30).reason("git fetch"), "git fetch wrote more than 4.0 GiB");
        assert_eq!(Cut::TooBig(262_144).reason("git fetch"), "git fetch wrote more than 256.0 KiB");
        assert_eq!(Cut::TooBig(64 << 20).reason("git fetch"), "git fetch wrote more than 64.0 MiB");
        assert_eq!(Cut::TooBig(1536 << 20).reason("git fetch"), "git fetch wrote more than 1.5 GiB");
    }

    #[test]
    fn a_size_is_rounded_before_its_unit_is_chosen() {
        for (bytes, shown) in [
            (971, "971 B"),
            (1023, "1.0 KiB"),
            (1024, "1.0 KiB"),
            (985_661, "962.6 KiB"),
            (1_048_565, "1.0 MiB"),
            (1_048_575, "1.0 MiB"),
            (1 << 20, "1.0 MiB"),
            ((1 << 30) - 1, "1.0 GiB"),
            (1 << 30, "1.0 GiB"),
        ] {
            assert_eq!(size(bytes), shown, "{bytes}");
        }
    }

    #[test]
    fn a_call_cut_at_the_run_s_ceiling_did_not_finish_within_it() {
        assert_eq!(Cut::Ceiling.reason("git fetch"), "git fetch did not finish within the run's ceiling");
    }

    #[test]
    fn a_killed_call_s_reason_names_what_cut_it() {
        assert_eq!(Cut::Silent(120 * SECOND).reason("git fetch"), "git fetch did not answer for 120 s");
        assert_eq!(
            Cut::Late(Duration::from_millis(1500)).reason("git ls-remote"),
            "git ls-remote did not answer within 1500 ms"
        );
        assert_eq!(Cut::TooLong.reason("git ls-remote"), "git ls-remote sent a line longer than 64 KiB");
        assert_eq!(Cut::Watchdog(Kill::Stalled).reason("git push"), "git push stalled");
        assert_eq!(Cut::Watchdog(Kill::TimedOut).reason("git push"), "git push outlived the run's ceiling");
    }

    #[test]
    fn a_call_is_stalled_once_idle_for_the_window_else_timed_out_at_the_ceiling() {
        let (stall, ceiling) = (15 * SECOND, 60 * SECOND);
        assert_eq!(kill(14 * SECOND, 59 * SECOND, stall, ceiling), None, "neither yet");
        assert_eq!(
            kill(15 * SECOND, SECOND, stall, ceiling),
            Some(Kill::Stalled),
            "idle for exactly the window"
        );
        assert_eq!(kill(SECOND, 60 * SECOND, stall, ceiling), Some(Kill::TimedOut), "exactly at the ceiling");
        assert_eq!(
            kill(20 * SECOND, 70 * SECOND, stall, ceiling),
            Some(Kill::Stalled),
            "a stall wins over the ceiling"
        );
        assert_eq!(kill(Duration::ZERO, Duration::ZERO, stall, ceiling), None, "a call that just started");
    }

    #[test]
    fn the_kill_lines_keep_powershell_s_words_in_whole_seconds_rounded_up() {
        assert_eq!(
            kill_line(Kill::Stalled, 15 * SECOND, 60 * SECOND),
            "gate: STALLED: no output or remote CPU for 15 s"
        );
        assert_eq!(kill_line(Kill::TimedOut, 15 * SECOND, 60 * SECOND), "gate: TIMED OUT after 60 s");
        assert_eq!(
            kill_line(Kill::Stalled, Duration::from_millis(300), SECOND),
            "gate: STALLED: no output or remote CPU for 1 s",
            "a seam's window"
        );
    }

    #[test]
    fn the_watchdog_looks_four_times_a_window_at_most_each_second_and_at_least_each_10_ms() {
        assert_eq!(tick(120 * SECOND), SECOND, "at most each second");
        assert_eq!(tick(4 * SECOND), SECOND);
        assert_eq!(tick(Duration::from_millis(300)), Duration::from_millis(75), "a quarter of the window");
        assert_eq!(tick(Duration::from_millis(20)), Duration::from_millis(10), "at least 10 ms");
    }

    #[test]
    fn a_stall_under_15_s_is_raised_to_15_unless_the_seam_gives_one() {
        assert_eq!(
            stall_window(3, None),
            (
                15 * SECOND,
                Some("gate: --stall 3 raised to 15 for a remote run (heartbeats come every 5 s)".to_owned())
            )
        );
        assert_eq!(stall_window(14, None).0, 15 * SECOND);
        assert_eq!(stall_window(15, None), (15 * SECOND, None), "exactly the floor");
        assert_eq!(stall_window(120, None), (120 * SECOND, None));
        assert_eq!(stall_window(3, Some(300)), (Duration::from_millis(300), None), "the seam, with no floor");
    }

    #[test]
    fn whole_seconds_round_up() {
        assert_eq!(whole_seconds(30 * SECOND), 30);
        assert_eq!(whole_seconds(Duration::from_millis(200)), 1);
        assert_eq!(whole_seconds(Duration::from_millis(30_001)), 31);
        assert_eq!(whole_seconds(Duration::ZERO), 0);
    }

    #[test]
    fn a_line_is_too_long_past_64_kib_its_line_end_aside() {
        let at_cap = vec![b'x'; LINE_CAP];
        assert!(!too_long(&at_cap), "exactly the cap");
        assert!(!too_long(&[at_cap.as_slice(), b"\r\n"].concat()), "the cap and a line end");
        assert!(
            !too_long(&[at_cap.as_slice(), b"\r"].concat()),
            "a carriage return that may start a line end"
        );
        assert!(too_long(&vec![b'x'; LINE_CAP + 1]), "one byte past the cap");
        assert!(too_long(&[vec![b'x'; LINE_CAP + 1].as_slice(), b"\n"].concat()));
    }

    #[test]
    fn a_kept_line_is_cut_to_64_kib_at_a_character_s_boundary() {
        let long = "x".repeat(LINE_CAP + 10);
        assert_eq!(truncated(&long).len(), LINE_CAP);
        let wide = format!("{}é", "x".repeat(LINE_CAP - 1));
        assert_eq!(truncated(&wide), "x".repeat(LINE_CAP - 1), "the two-byte character does not fit");
        assert_eq!(truncated("short"), "short");
    }

    #[test]
    fn only_the_last_1000_lines_are_kept() {
        let mut kept = Kept::default();
        for number in 0..LINES_KEPT + 5 {
            kept.push(&number.to_string());
        }
        let lines = kept.into_lines();
        assert_eq!(lines.len(), LINES_KEPT);
        assert_eq!(lines.first().map(String::as_str), Some("5"), "the oldest are dropped");
        assert_eq!(lines.last().map(String::as_str), Some("1004"));
    }

    /// `n` lines of [`LINE_CAP`] bytes each, the first three bytes of each naming it, so a kept line is recognisable.
    fn lines_of_the_cap(n: usize) -> Vec<String> {
        (0..n).map(|number| format!("{number:0>3}{}", "x".repeat(LINE_CAP - 3))).collect()
    }

    #[test]
    fn a_calls_kept_lines_stay_within_1_mib_dropping_the_oldest() {
        let mut kept = Kept::default();
        for line in lines_of_the_cap(100) {
            kept.push(&line);
        }
        let lines = kept.into_lines();
        let bytes: usize = lines.iter().map(String::len).sum();
        assert!(bytes <= BYTES_KEPT, "the bytes kept: {bytes}");
        assert!(lines.last().is_some_and(|line| line.starts_with("099")), "the newest line is kept");
        assert!(!lines.iter().any(|line| line.starts_with("000")), "the oldest line is dropped");
    }

    #[test]
    fn one_line_over_the_budget_is_still_kept() {
        let mut kept = Kept::default();
        for line in lines_of_the_cap(16) {
            kept.push(&line);
        }
        let last = format!("999{}", "x".repeat(LINE_CAP - 3));
        kept.push(&last);
        let lines = kept.into_lines();
        let bytes: usize = lines.iter().map(String::len).sum();
        assert!(bytes <= BYTES_KEPT, "the bytes kept: {bytes}");
        assert_eq!(lines.last().map(String::as_str), Some(last.as_str()), "the newest line is kept");
    }

    #[test]
    fn the_line_limit_and_the_byte_budget_drop_together() {
        let mut kept = Kept::default();
        for number in 0..LINES_KEPT {
            kept.push(&number.to_string());
        }
        let big = lines_of_the_cap(16);
        for line in &big {
            kept.push(line);
        }
        let lines = kept.into_lines();
        let bytes: usize = lines.iter().map(String::len).sum();
        assert!(bytes <= BYTES_KEPT, "the bytes kept: {bytes}");
        assert_eq!(lines, big, "the short lines are all dropped, and every line of the cap is kept");
    }
}
