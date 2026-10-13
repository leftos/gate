//! The one word a `release` line gives for how a command ended.

/// The kill lines the watchdog writes, the word each starts with and the verdict it gives.
const KILL_WORDS: [(&str, Verdict); 3] = [
    ("STALLED", Verdict::Stalled),
    ("TIMED OUT", Verdict::TimedOut),
    ("BACKSTOP", Verdict::Backstop),
];

/// How a command ended, as its `release` line's `verdict` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// The command exited 0 and the gate's status is 0.
    Passed,
    /// The command exited non-zero, killed by nothing the gate did.
    Failed,
    /// The command exited 0, but a failure marker in its log made the gate's status non-zero.
    LogFailure,
    /// The watchdog killed the command for producing no output and using no CPU.
    Stalled,
    /// The watchdog killed the command at its ceiling.
    TimedOut,
    /// The watchdog killed the command at the backstop.
    Backstop,
    /// The gate ended before its command had a verdict.
    Unknown,
}

impl Verdict {
    /// The verdict of a command that exited `exited` under a gate whose status is `status`: `stalled`, `timed-out` or
    /// `backstop` when `kill`, the gate's kill line, starts `gate: STALLED`, `gate: TIMED OUT` or `gate: BACKSTOP` at a
    /// word boundary, in any case; else `log-failure` when the command exited 0 and the status is not 0; else `passed` for status 0
    /// and `failed` for any other. A command that exits 124 by itself, with no kill line, is `failed`.
    #[must_use]
    pub fn of(kill: Option<&str>, exited: i64, status: i64) -> Verdict {
        if let Some(verdict) = kill.and_then(kill_verdict) {
            return verdict;
        }
        if exited == 0 && status != 0 {
            return Verdict::LogFailure;
        }
        if status == 0 { Verdict::Passed } else { Verdict::Failed }
    }

    /// The word the journal writes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Passed => "passed",
            Verdict::Failed => "failed",
            Verdict::LogFailure => "log-failure",
            Verdict::Stalled => "stalled",
            Verdict::TimedOut => "timed-out",
            Verdict::Backstop => "backstop",
            Verdict::Unknown => "unknown",
        }
    }

    /// The verdict the journal writes as `name`, the inverse of [`Verdict::as_str`]; `None` for any other word.
    pub(crate) fn named(name: &str) -> Option<Verdict> {
        match name {
            "passed" => Some(Verdict::Passed),
            "failed" => Some(Verdict::Failed),
            "log-failure" => Some(Verdict::LogFailure),
            "stalled" => Some(Verdict::Stalled),
            "timed-out" => Some(Verdict::TimedOut),
            "backstop" => Some(Verdict::Backstop),
            "unknown" => Some(Verdict::Unknown),
            _ => None,
        }
    }
}

/// The kill verdict `line` gives, or `None` when it is no kill line. Case is ignored, as PowerShell's `-match` ignores it.
fn kill_verdict(line: &str) -> Option<Verdict> {
    let rest = strip_prefix_ignoring_case(line, "gate: ")?;
    KILL_WORDS.iter().find_map(|(word, verdict)| {
        let after = strip_prefix_ignoring_case(rest, word)?;
        let bounded = after.chars().next().is_none_or(|next| !(next.is_alphanumeric() || next == '_'));
        bounded.then_some(*verdict)
    })
}

/// `text` after `prefix`, the ASCII letters of either matched in any case, or `None` when `text` does not start with it.
fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}
