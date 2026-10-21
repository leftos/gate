//! Where a run's time went: of the processes the command's job remembers ([`gate_win32::Job::traced`]), the few worth
//! naming in the `release` line's `children`.

use gate_state::TracedChild;
use gate_win32::{TracedMember, worth_naming};

/// The most processes a release names.
pub(crate) const CHILDREN_CAP: usize = 8;
/// The 100 ns ticks of a second.
const TICKS_PER_SECOND: f64 = 10_000_000.0;

/// The `members` worth naming at `now`: those that used at least 1 CPU-second or lived at least 10 s, the
/// [`CHILDREN_CAP`] with the most CPU, most first, their times in seconds after `since`, the command's start, all three
/// `FILETIME` ticks.
pub(crate) fn children(members: Vec<TracedMember>, since: u64, now: u64) -> Vec<TracedChild> {
    let mut named: Vec<TracedMember> = members.into_iter().filter(|member| worth_naming(&member.span, now)).collect();
    named.sort_by_key(|member| std::cmp::Reverse(member.span.cpu));
    named
        .into_iter()
        .take(CHILDREN_CAP)
        .map(|member| TracedChild {
            cmd: member.cmd,
            image: member.image,
            start: seconds_after(member.span.created, since),
            end: member.span.exited.map(|exited| seconds_after(exited, since)),
            cpu: ticks_as_seconds(member.span.cpu),
        })
        .collect()
}

/// The seconds from `since` to `at`, both `FILETIME` ticks; 0 for a moment before it.
fn seconds_after(at: u64, since: u64) -> f64 {
    ticks_as_seconds(at.saturating_sub(since))
}

/// `ticks` of 100 ns in seconds.
#[expect(
    clippy::cast_precision_loss,
    reason = "ticks under 2^53, over 28 years, are exact; a release keeps a tenth of a second"
)]
pub(crate) fn ticks_as_seconds(ticks: u64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND
}

#[cfg(test)]
mod tests {
    use gate_win32::{ProcessSpan, TracedMember};

    use super::{CHILDREN_CAP, children};

    /// The ticks of `seconds`.
    fn ticks(seconds: f64) -> u64 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a test's seconds are small and positive"
        )]
        let ticks = (seconds * 10_000_000.0).round() as u64;
        ticks
    }

    /// The command's start, as ticks: a whole number of seconds, so a child's times read back exactly.
    const SINCE: u64 = 1_000 * 10_000_000;

    /// A member named `cmd` that started `start` s after the command, ended at `end` (or still runs) and used `cpu`
    /// CPU-seconds.
    fn member(cmd: &str, start: f64, end: Option<f64>, cpu: f64) -> TracedMember {
        TracedMember {
            cmd: cmd.to_owned(),
            image: Some(format!(r"C:\tools\{cmd}.exe")),
            span: ProcessSpan {
                created: SINCE + ticks(start),
                exited: end.map(|end| SINCE + ticks(end)),
                cpu: ticks(cpu),
            },
        }
    }

    #[test]
    fn only_a_process_that_used_a_cpu_second_or_lived_ten_seconds_is_named() {
        let members = vec![
            member("one", 0.0, Some(2.0), 1.0),
            member("two", 0.0, Some(9.9), 0.9),
            member("three", 5.0, Some(15.0), 0.0),
            member("four", 1.0, None, 0.0),
        ];

        let named: Vec<String> = children(members, SINCE, SINCE + ticks(10.5)).into_iter().map(|child| child.cmd).collect();

        assert_eq!(named, ["one", "three"], "the quiet one still running at 10.5 s has lived 9.5 s");
    }

    #[test]
    fn the_eight_with_the_most_cpu_are_named_most_first_with_their_image_and_times_after_the_start() {
        let members = (1..=10)
            .map(|n| member(&format!("tool{n}"), f64::from(n), Some(f64::from(n) + 20.0), f64::from(n)))
            .collect();

        let named = children(members, SINCE, SINCE + ticks(60.0));

        assert_eq!(named.len(), CHILDREN_CAP);
        let cpus: Vec<f64> = named.iter().map(|child| child.cpu).collect();
        assert_eq!(cpus, [10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0]);
        let first = &named[0];
        assert_eq!(
            (first.cmd.as_str(), first.image.as_deref(), first.start, first.end),
            ("tool10", Some(r"C:\tools\tool10.exe"), 10.0, Some(30.0))
        );
    }

    #[test]
    fn a_process_still_running_has_no_end_and_one_started_before_the_command_starts_at_zero() {
        let mut early = member("early", 0.0, None, 3.0);
        early.span.created = SINCE - ticks(2.0);

        let named = children(vec![early], SINCE, SINCE + ticks(5.0));

        assert_eq!(named.len(), 1);
        assert_eq!((named[0].start, named[0].end, named[0].cpu), (0.0, None, 3.0));
    }
}
