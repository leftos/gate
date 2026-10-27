//! The two rules a call of the client is retried on. Any call that failed before the ssh banner, so the builder never
//! started anything and the call is safe to make again, is made again up to [`PRE_BANNER_RETRIES`] times, each after a
//! pause, for as long as every call fails the same way. A transfer after the command, the sync-back's `ls-remote` and
//! `fetch` and the outputs call, that failed as a connection fails ([`transfer_failed`]) is made again up to
//! [`TRANSFER_RETRIES`] times, each after its own pause, the first rule nested inside each attempt.

use std::time::Duration;

use gate_args::TestSeams;

use super::watchdog::Cut;

/// The status ssh exits with when it fails itself, and the one git exits with when the ssh it ran failed.
pub(crate) const SSH_FAILED: i32 = 255;
pub(crate) const GIT_SSH_FAILED: i32 = 128;
/// The pause before the retry, when `--test-remote-retry-pause-ms` gives none.
const PAUSE: Duration = Duration::from_secs(2);
/// How many times a call that failed before the banner is made again, each after [`pause`]: a builder whose ssh server
/// stops starting sessions for tens of seconds at a time is passed by the calls this many pauses apart.
pub(crate) const PRE_BANNER_RETRIES: usize = 3;
/// The least of a bounded call's bound that must be left after the pause for its retry to be made.
const FLOOR: Duration = Duration::from_secs(5);
/// The bound of a bounded call, the `stop` or the `boot`, when `--test-remote-bound-ms` gives none.
const BOUND: Duration = Duration::from_secs(30);
/// ssh's own words, at the start of its first standard error line, that it reached the builder but no session began:
/// the banner did not come within the connect timeout, or the builder closed the connection before it.
const PRE_BANNER: [&str; 3] = [
    "Connection timed out during banner exchange",
    "kex_exchange_identification: ",
    "banner exchange: ",
];

/// How one call ended, as the rules read it: its exit status, the status its program exits with when ssh failed,
/// whether it wrote anything on standard output, its first standard error line as it came, whether the box said the
/// run's start marker, why the client cut it, if it did, and every line it said that it kept.
pub(crate) struct Ended<'a> {
    pub status: i32,
    pub fail_status: i32,
    pub output_seen: bool,
    pub first_error: Option<&'a str>,
    pub started: bool,
    pub cut: Option<Cut>,
    pub said: Vec<&'a str>,
}

/// Whether the call failed before the banner: no start marker from the box, ssh's own failure status, nothing on
/// standard output, and a first standard error line that starts with one of ssh's pre-banner words, compared
/// case-sensitively. A call whose box said the start marker reached the builder, whatever its first line.
pub(crate) fn before_the_banner(ended: &Ended<'_>) -> bool {
    !ended.started
        && ended.status == ended.fail_status
        && !ended.output_seen
        && ended
            .first_error
            .is_some_and(|line| PRE_BANNER.iter().any(|words| line.starts_with(words)))
}

/// The line said before the retry `n` of the `total` a call to `host` gets, counted from 1.
pub(crate) fn retry_line(host: &str, n: usize, total: usize) -> String {
    format!("gate: {host} did not answer the ssh banner; trying again ({n} of {total})")
}

/// The pause before a retry: `--test-remote-retry-pause-ms` when given, else 2 s.
pub(crate) fn pause(seams: &TestSeams) -> Duration {
    seams.remote_retry_pause_ms.map_or(PAUSE, Duration::from_millis)
}

/// How many times a transfer after the command that failed as a connection fails is made again.
pub(crate) const TRANSFER_RETRIES: usize = 2;
/// The pauses before a transfer's first and second retries, when `--test-remote-retry-pause-ms` gives none.
const TRANSFER_PAUSES: [Duration; TRANSFER_RETRIES] = [Duration::from_secs(5), Duration::from_secs(15)];

/// The words, compared without regard to case, that git or its ssh says when the connection to the builder failed: a
/// git call's [`GIT_SSH_FAILED`] counts as a connection failure only beside one of them or of [`PRE_BANNER`]'s.
const CONNECTION_WORDS: [&str; 8] = [
    "Could not read from remote repository",
    "the remote end hung up unexpectedly",
    "early EOF",
    "Connection reset",
    "Connection closed",
    "Connection timed out",
    "Broken pipe",
    "kex_exchange_identification",
];

/// Whether a transfer after the command failed as a connection fails: cut at its deadline, or uncut with its program's
/// own status for a failed ssh. ssh's own [`SSH_FAILED`] counts as it is; git's [`GIT_SSH_FAILED`], which git also
/// exits with for its own errors, only when a line it said holds one of [`CONNECTION_WORDS`] or [`PRE_BANNER`]'s. Any
/// other cut, a stall or the transfer ceiling among them, and any other status are not.
pub(crate) fn transfer_failed(ended: &Ended<'_>) -> bool {
    match ended.cut {
        Some(Cut::Late(_)) => true,
        Some(_) => false,
        None if ended.status != ended.fail_status => false,
        None => ended.fail_status == SSH_FAILED || ended.said.iter().any(|line| connection_lost(line)),
    }
}

/// Whether `line` holds one of [`CONNECTION_WORDS`] or [`PRE_BANNER`]'s, in any case.
fn connection_lost(line: &str) -> bool {
    let line = line.to_lowercase();
    CONNECTION_WORDS
        .iter()
        .chain(PRE_BANNER.iter())
        .any(|words| line.contains(&words.to_lowercase()))
}

/// The line said before the retry `n`, counted from 1, of a transfer from `host` that failed for `reason`.
pub(crate) fn transfer_retry_line(host: &str, reason: &str, n: usize) -> String {
    format!("gate: bringing the results back from {host} failed ({reason}); trying again ({n} of {TRANSFER_RETRIES})")
}

/// The pauses before a transfer's retries: each `--test-remote-retry-pause-ms` when given, else 5 s and then 15 s.
pub(crate) fn transfer_pauses(seams: &TestSeams) -> [Duration; TRANSFER_RETRIES] {
    seams
        .remote_retry_pause_ms
        .map_or(TRANSFER_PAUSES, |ms| [Duration::from_millis(ms); TRANSFER_RETRIES])
}

/// The bound of a bounded call, the `stop` or the `boot`: `--test-remote-bound-ms` when given, else 30 s.
pub(crate) fn bound(seams: &TestSeams) -> Duration {
    seams.remote_bound_ms.map_or(BOUND, Duration::from_millis)
}

/// Whether a bounded call that failed before the banner `elapsed` after it started is made once more after `pause`:
/// only when at least 5 s of its `bound` is left after the pause, as the two attempts share it.
pub(crate) fn retry_allowed(bound: Duration, elapsed: Duration, pause: Duration) -> bool {
    bound.saturating_sub(elapsed).saturating_sub(pause) >= FLOOR
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Ended, GIT_SSH_FAILED, PRE_BANNER_RETRIES, SSH_FAILED, before_the_banner, retry_allowed, retry_line};

    #[test]
    fn a_bounded_call_is_retried_only_with_5_s_of_its_bound_left_after_the_pause() {
        let (bound, pause) = (Duration::from_secs(30), Duration::from_secs(2));
        assert!(retry_allowed(bound, Duration::from_secs(1), pause), "plenty left");
        assert!(retry_allowed(bound, Duration::from_secs(23), pause), "exactly 5 s left");
        assert!(!retry_allowed(bound, Duration::from_millis(23_001), pause), "just under 5 s left");
        assert!(!retry_allowed(bound, Duration::from_secs(40), pause), "past the bound");
        assert!(
            !retry_allowed(Duration::from_millis(200), Duration::ZERO, pause),
            "a bound under the floor"
        );
    }

    fn ssh(status: i32, output_seen: bool, first_error: Option<&str>) -> bool {
        before_the_banner(&Ended {
            status,
            fail_status: SSH_FAILED,
            output_seen,
            first_error,
            started: false,
            cut: None,
            said: Vec::new(),
        })
    }

    #[test]
    fn a_call_whose_box_said_its_start_marker_never_failed_before_the_banner() {
        for line in [
            "Connection timed out during banner exchange",
            "kex_exchange_identification: read: Connection reset by peer",
            "banner exchange: Connection to 192.0.2.1 port 22: invalid format",
        ] {
            let started = before_the_banner(&Ended {
                status: SSH_FAILED,
                fail_status: SSH_FAILED,
                output_seen: false,
                first_error: Some(line),
                started: true,
                cut: None,
                said: Vec::new(),
            });
            assert!(!started, "{line}");
        }
    }

    #[test]
    fn each_of_ssh_s_pre_banner_words_is_a_pre_banner_failure() {
        for line in [
            "Connection timed out during banner exchange",
            "kex_exchange_identification: read: Connection reset by peer",
            "banner exchange: Connection to 192.0.2.1 port 22: invalid format",
        ] {
            assert!(ssh(255, false, Some(line)), "{line}");
        }
    }

    #[test]
    fn git_s_own_status_for_a_failed_ssh_is_its_pre_banner_status() {
        let git = |status| {
            before_the_banner(&Ended {
                status,
                fail_status: GIT_SSH_FAILED,
                output_seen: false,
                first_error: Some("kex_exchange_identification: Connection closed by remote host"),
                started: false,
                cut: None,
                said: Vec::new(),
            })
        };
        assert!(git(128));
        assert!(!git(255), "git exits 128, not ssh's 255");
    }

    #[test]
    fn output_another_first_line_or_another_status_is_no_pre_banner_failure() {
        let banner = Some("Connection timed out during banner exchange");
        assert!(!ssh(255, true, banner), "a call that wrote on stdout reached the builder");
        assert!(!ssh(1, false, banner), "another status");
        assert!(!ssh(255, false, None), "no standard error at all");
        assert!(
            !ssh(255, false, Some("ssh: connect to host builder port 22: Connection refused")),
            "another first line"
        );
        assert!(
            !ssh(255, false, Some("connection timed out during banner exchange")),
            "the words are compared by case"
        );
        assert!(
            !ssh(255, false, Some(" Connection timed out during banner exchange")),
            "at the start of the line only"
        );
    }

    #[test]
    fn the_retry_line_names_the_host_and_the_retry() {
        assert_eq!(
            retry_line("builder", 1, 1),
            "gate: builder did not answer the ssh banner; trying again (1 of 1)"
        );
        assert_eq!(
            retry_line("builder", 1, PRE_BANNER_RETRIES),
            "gate: builder did not answer the ssh banner; trying again (1 of 3)"
        );
        assert_eq!(
            retry_line("builder", 3, PRE_BANNER_RETRIES),
            "gate: builder did not answer the ssh banner; trying again (3 of 3)"
        );
    }
}
