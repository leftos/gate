//! Nesting under a parent gate's slot: the marker `GATE_SLOT_HELD=<kind>:<pid>:<start>[:<h>]` a gate hands its command,
//! read once a run, and what a gate started under it does: runs under the parent's slot when that slot covers its kind,
//! is refused when it is critical and the slot does not cover it, and otherwise takes a slot of its own.

use std::ffi::OsStr;

use gate_args::{Slot, Spelling};
use gate_state::StateRoot;

use crate::resolve::Environment;
use crate::slots::{Liveness, gate_liveness};

/// The most digits a marker's pid may have.
const PID_DIGITS: usize = 9;
/// The most digits a marker's start may have.
const START_DIGITS: usize = 18;
/// The most characters of a malformed marker its line shows.
const MARKER_SHOWN: usize = 64;

/// A live parent gate named by the marker: the slot kind it holds, its pid, and its marker as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Parent {
    pub held: Slot,
    pub pid: u32,
    pub marker: String,
}

/// What a gate does under the marker it was started with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Nesting {
    /// No marker, or one of another state root: the gate takes its own slot and says nothing of it.
    Own,
    /// A marker, as written, that is not `<kind>:<pid>:<start>[:<h>]`: the gate takes its own slot.
    Malformed(String),
    /// A marker naming a gate that no longer runs with its start: the gate takes its own slot.
    Ended { marker: String, pid: u32 },
    /// A parent whose slot covers this gate's kind: the gate takes no slot and runs under the parent's.
    Covered(Parent),
    /// A parent whose slot does not cover this critical gate, which would wait for that slot for good: it does not run.
    Refused(Parent),
    /// A parent whose slot does not cover this gate's kind: the gate takes its own slot.
    Under(Parent),
}

/// A marker as read: the kind of slot its gate holds, the gate's pid and start, and the root hash after them.
struct Marker<'m> {
    held: Slot,
    pid: u32,
    start: u64,
    hash: Option<&'m str>,
}

impl Nesting {
    /// The nesting of a gate of `kind` under the state root `root`, from the marker in `environment`, the marker's gate
    /// judged by the start the system gives its pid.
    pub(crate) fn read(environment: &Environment, kind: Slot, root: &StateRoot) -> Nesting {
        Nesting::decide(environment.slot_held.as_deref(), kind, root.hash(), gate_liveness)
    }

    /// The nesting of a gate of `kind` under the root hashed `root_hash` (`None` for the default root) for `marker`,
    /// unset or empty when there is none; `liveness_of` judges the gate of a pid and its start in Unix epoch
    /// milliseconds. Only a gate judged gone ends the marker: one whose start cannot be read counts as running, so a
    /// child never queues behind the slot its own parent holds.
    pub(crate) fn decide(marker: Option<&OsStr>, kind: Slot, root_hash: Option<&str>, liveness_of: impl Fn(u32, u64) -> Liveness) -> Nesting {
        let Some(marker) = marker.filter(|marker| !marker.is_empty()) else {
            return Nesting::Own;
        };
        let text = marker.to_string_lossy();
        let Some(Marker { held, pid, start, hash }) = parse(&text) else {
            return Nesting::Malformed(text.into_owned());
        };
        if hash != root_hash {
            return Nesting::Own;
        }
        if matches!(liveness_of(pid, start), Liveness::Gone) {
            return Nesting::Ended {
                marker: text.into_owned(),
                pid,
            };
        }
        let parent = Parent {
            held,
            pid,
            marker: text.into_owned(),
        };
        if covers(held, kind) {
            Nesting::Covered(parent)
        } else if kind == Slot::Critical {
            Nesting::Refused(parent)
        } else {
            Nesting::Under(parent)
        }
    }

    /// The parent whose slot this gate runs under, when it covers the gate.
    pub(crate) fn covering(&self) -> Option<&Parent> {
        match self {
            Nesting::Covered(parent) => Some(parent),
            _ => None,
        }
    }

    /// The slot kind a gate of `kind` runs its command as: critical under a critical parent's slot, which it follows,
    /// else its own.
    pub(crate) fn runs_as(&self, kind: Slot) -> Slot {
        match self.covering() {
            Some(parent) if parent.held == Slot::Critical => Slot::Critical,
            _ => kind,
        }
    }

    /// The line a gate of `kind`, asked for in `spelling`, says of this nesting; none for [`Nesting::Own`].
    pub(crate) fn line(&self, kind: Slot, spelling: Spelling) -> Option<String> {
        let name = kind.as_str();
        let line = match self {
            Nesting::Own => return None,
            Nesting::Malformed(marker) => format!(
                "gate: ignored GATE_SLOT_HELD='{}', which is not <kind>:<pid>:<start>; taking a {name} slot",
                shown_marker(marker)
            ),
            Nesting::Ended { marker, pid } => {
                format!("gate: ignored GATE_SLOT_HELD='{marker}': gate {pid} is no longer running; taking a {name} slot")
            }
            Nesting::Covered(parent) => format!("gate: running under the {} slot of gate {}", parent.held.as_str(), parent.pid),
            Nesting::Refused(parent) => format!(
                "gate: {} critical cannot run under the {} slot of gate {}: it would wait for that slot for good; run the critical \
                 command outside that gate",
                slot_flag(spelling),
                parent.held.as_str(),
                parent.pid
            ),
            Nesting::Under(parent) => parent.uncovered_line(kind),
        };
        Some(line)
    }
}

impl Parent {
    /// The line a gate of `kind` this parent's slot does not cover says as it goes on to take a local slot of its own.
    pub(crate) fn uncovered_line(&self, kind: Slot) -> String {
        let kind = kind.as_str();
        format!(
            "gate: the {} slot of gate {} does not cover a {kind} gate; taking a {kind} slot",
            self.held.as_str(),
            self.pid
        )
    }
}

/// A malformed marker as its line shows it: every character but an ASCII letter, a digit or `:` as `?`, so it cannot
/// forge a line or a failure marker in the log, and at most [`MARKER_SHOWN`] characters, `...` after a longer one.
fn shown_marker(marker: &str) -> String {
    let mut shown: String = marker
        .chars()
        .take(MARKER_SHOWN)
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == ':' {
                character
            } else {
                '?'
            }
        })
        .collect();
    if marker.chars().count() > MARKER_SHOWN {
        shown.push_str("...");
    }
    shown
}

/// Whether a parent holding a `held` slot covers a gate of `kind`: heavy covers heavy and light, light covers light, and
/// critical covers all three.
fn covers(held: Slot, kind: Slot) -> bool {
    match held {
        Slot::Heavy => kind != Slot::Critical,
        Slot::Light => kind == Slot::Light,
        Slot::Critical => true,
    }
}

/// The slot flag as the caller spelled the command line.
fn slot_flag(spelling: Spelling) -> &'static str {
    match spelling {
        Spelling::Cli => "--slot",
        Spelling::Compat => "-Slot",
    }
}

/// `text` read as `<kind>:<pid>:<start>[:<h>]`: a lowercase kind, a pid of 1 to 9 digits, a start of 1 to 18 digits,
/// and an optional root hash that is not empty; `None` when it is not.
fn parse(text: &str) -> Option<Marker<'_>> {
    let mut parts = text.split(':');
    let held = match parts.next()? {
        "heavy" => Slot::Heavy,
        "light" => Slot::Light,
        "critical" => Slot::Critical,
        _ => return None,
    };
    let pid = digits(parts.next()?, PID_DIGITS)?;
    let start = digits(parts.next()?, START_DIGITS)?;
    let hash = parts.next();
    if hash.is_some_and(str::is_empty) || parts.next().is_some() {
        return None;
    }
    Some(Marker {
        held,
        pid: u32::try_from(pid).ok()?,
        start,
        hash,
    })
}

/// `text` as a whole number of 1 to `most` ASCII digits, no sign.
fn digits(text: &str, most: usize) -> Option<u64> {
    let fits = (1..=most).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit());
    fits.then(|| text.parse().ok()).flatten()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a decision that has no line must fail the test loudly")]
mod tests {
    use std::ffi::OsStr;
    use std::io;

    use gate_args::{Slot, Spelling};

    use super::{Nesting, Parent};
    use crate::slots::Liveness;

    /// The hash of the state root every decision here is made under.
    const HASH: &str = "0a1b2c3d";
    /// The one gate that runs, and since when, in Unix epoch milliseconds.
    const PID: u32 = 4242;
    const START: u64 = 1_759_439_760_571;
    /// A pid whose start cannot be read.
    const UNREADABLE: u32 = 5151;

    /// How the gate `pid` started at `start` is judged: live only as [`PID`] since [`START`], unknown as
    /// [`UNREADABLE`], else gone.
    fn judged(pid: u32, start: u64) -> Liveness {
        match pid {
            PID if start == START => Liveness::Live,
            UNREADABLE => Liveness::Unknown(io::Error::other("the system failed")),
            _ => Liveness::Gone,
        }
    }

    /// The decision of a gate of `kind` under the root hashed [`HASH`] for `marker`.
    fn decided(marker: &str, kind: Slot) -> Nesting {
        Nesting::decide(Some(OsStr::new(marker)), kind, Some(HASH), judged)
    }

    #[test]
    fn a_parent_whose_start_cannot_be_read_counts_as_running() {
        let marker = |held: &str| format!("{held}:{UNREADABLE}:{START}:{HASH}");
        let parent = |held: Slot| Parent {
            held,
            pid: UNREADABLE,
            marker: marker(held.as_str()),
        };
        assert_eq!(decided(&marker("heavy"), Slot::Light), Nesting::Covered(parent(Slot::Heavy)));
        assert_eq!(decided(&marker("heavy"), Slot::Critical), Nesting::Refused(parent(Slot::Heavy)));
        assert_eq!(decided(&marker("light"), Slot::Heavy), Nesting::Under(parent(Slot::Light)));
    }

    #[test]
    fn a_malformed_marker_is_shown_with_only_letters_digits_and_colons_and_at_most_64_characters() {
        let shown = |marker: &str| {
            Nesting::Malformed(marker.to_owned())
                .line(Slot::Light, Spelling::Cli)
                .expect("a malformed marker has a line")
        };
        let line = |echo: &str| format!("gate: ignored GATE_SLOT_HELD='{echo}', which is not <kind>:<pid>:<start>; taking a light slot");
        assert_eq!(shown("x\ngate: forged"), line("x?gate:?forged"), "a newline");
        assert_eq!(shown("x\u{1b}[31my"), line("x??31my"), "an escape character");
        assert_eq!(shown("x: error y"), line("x:?error?y"), "a failure marker");
        assert_eq!(shown(&"a".repeat(64)), line(&"a".repeat(64)), "64 characters are shown whole");
        assert_eq!(shown(&"é".repeat(65)), line(&format!("{}...", "?".repeat(64))), "a longer one is cut");
    }

    /// The marker of the running gate [`PID`] holding a `held` slot under the root hashed [`HASH`].
    fn marker(held: Slot) -> String {
        format!("{}:{PID}:{START}:{HASH}", held.as_str())
    }

    fn parent(held: Slot) -> Parent {
        Parent {
            held,
            pid: PID,
            marker: marker(held),
        }
    }

    #[test]
    fn an_unset_or_empty_marker_is_no_nesting() {
        assert_eq!(Nesting::decide(None, Slot::Heavy, Some(HASH), judged), Nesting::Own);
        assert_eq!(Nesting::decide(Some(OsStr::new("")), Slot::Heavy, Some(HASH), judged), Nesting::Own);
    }

    #[test]
    fn a_marker_reads_each_kind_and_caps_its_digits() {
        for held in [Slot::Heavy, Slot::Light, Slot::Critical] {
            assert_eq!(decided(&marker(held), Slot::Light).covering(), Some(&parent(held)), "{held:?}");
        }
        let longest = format!("heavy:123456789:123456789012345678:{HASH}");
        assert_eq!(
            decided(&longest, Slot::Heavy),
            Nesting::Ended {
                marker: longest.clone(),
                pid: 123_456_789
            },
            "a 9-digit pid and an 18-digit start are read"
        );
        let malformed = [
            format!("medium:{PID}:{START}:{HASH}"),
            format!("Heavy:{PID}:{START}:{HASH}"),
            format!("heavy:{PID}"),
            format!("heavy:{PID}:{START}:"),
            format!("heavy:{PID}:{START}:{HASH}:extra"),
            format!("heavy:1234567890:{START}:{HASH}"),
            format!("heavy:{PID}:1234567890123456789:{HASH}"),
            format!("heavy::{START}:{HASH}"),
            format!("heavy:+{PID}:{START}:{HASH}"),
            format!("heavy:{PID}:-{START}:{HASH}"),
            format!(" heavy:{PID}:{START}:{HASH}"),
        ];
        for text in malformed {
            assert_eq!(decided(&text, Slot::Heavy), Nesting::Malformed(text.clone()), "{text}");
        }
    }

    #[test]
    fn a_marker_of_another_state_root_is_ignored_silently() {
        let other = format!("heavy:{PID}:{START}:ffffffff");
        let hashless = format!("heavy:{PID}:{START}");
        assert_eq!(decided(&other, Slot::Heavy), Nesting::Own, "another root's hash");
        assert_eq!(decided(&hashless, Slot::Heavy), Nesting::Own, "no hash under a root that has one");
        let under_default = |text: &str| Nesting::decide(Some(OsStr::new(text)), Slot::Heavy, None, judged);
        assert_eq!(under_default(&marker(Slot::Heavy)), Nesting::Own, "a hash under the default root");
        assert_eq!(
            under_default(&hashless).covering(),
            Some(&Parent {
                held: Slot::Heavy,
                pid: PID,
                marker: hashless.clone()
            }),
            "no hash under the default root"
        );
    }

    #[test]
    fn a_marker_of_a_gate_no_longer_running_is_ended() {
        let restarted = format!("heavy:{PID}:{}:{HASH}", START + 1);
        let gone = format!("heavy:{}:{START}:{HASH}", PID + 1);
        assert_eq!(
            decided(&restarted, Slot::Heavy),
            Nesting::Ended {
                marker: restarted.clone(),
                pid: PID
            },
            "a pid running since another start"
        );
        assert_eq!(
            decided(&gone, Slot::Heavy),
            Nesting::Ended {
                marker: gone.clone(),
                pid: PID + 1
            },
            "a pid that does not run"
        );
    }

    #[test]
    fn heavy_covers_heavy_and_light_light_covers_light_and_critical_covers_all() {
        use Slot::{Critical, Heavy, Light};

        let covered = |held| Nesting::Covered(parent(held));
        let table = [
            (Heavy, Heavy, covered(Heavy)),
            (Heavy, Light, covered(Heavy)),
            (Heavy, Critical, Nesting::Refused(parent(Heavy))),
            (Light, Heavy, Nesting::Under(parent(Light))),
            (Light, Light, covered(Light)),
            (Light, Critical, Nesting::Refused(parent(Light))),
            (Critical, Heavy, covered(Critical)),
            (Critical, Light, covered(Critical)),
            (Critical, Critical, covered(Critical)),
        ];
        for (held, kind, expected) in table {
            assert_eq!(decided(&marker(held), kind), expected, "a {kind:?} gate under a {held:?} marker");
        }
    }

    #[test]
    fn each_decision_says_its_line() {
        let ended = Nesting::Ended {
            marker: "light:7:8".to_owned(),
            pid: 7,
        };
        let cases = [
            (Nesting::Own, Slot::Heavy, Spelling::Cli, None),
            (
                Nesting::Malformed("x:y".to_owned()),
                Slot::Light,
                Spelling::Cli,
                Some("gate: ignored GATE_SLOT_HELD='x:y', which is not <kind>:<pid>:<start>; taking a light slot"),
            ),
            (
                ended,
                Slot::Heavy,
                Spelling::Cli,
                Some("gate: ignored GATE_SLOT_HELD='light:7:8': gate 7 is no longer running; taking a heavy slot"),
            ),
            (
                Nesting::Covered(parent(Slot::Critical)),
                Slot::Light,
                Spelling::Cli,
                Some("gate: running under the critical slot of gate 4242"),
            ),
            (
                Nesting::Refused(parent(Slot::Heavy)),
                Slot::Critical,
                Spelling::Cli,
                Some(
                    "gate: --slot critical cannot run under the heavy slot of gate 4242: it would wait for that slot for good; run the \
                     critical command outside that gate",
                ),
            ),
            (
                Nesting::Refused(parent(Slot::Light)),
                Slot::Critical,
                Spelling::Compat,
                Some(
                    "gate: -Slot critical cannot run under the light slot of gate 4242: it would wait for that slot for good; run the \
                     critical command outside that gate",
                ),
            ),
            (
                Nesting::Under(parent(Slot::Light)),
                Slot::Heavy,
                Spelling::Cli,
                Some("gate: the light slot of gate 4242 does not cover a heavy gate; taking a heavy slot"),
            ),
        ];
        for (nesting, kind, spelling, line) in cases {
            assert_eq!(nesting.line(kind, spelling).as_deref(), line, "{nesting:?}");
        }
    }

    #[test]
    fn only_a_gate_covered_by_a_critical_parent_runs_as_critical() {
        assert_eq!(Nesting::Covered(parent(Slot::Critical)).runs_as(Slot::Light), Slot::Critical);
        assert_eq!(Nesting::Covered(parent(Slot::Heavy)).runs_as(Slot::Light), Slot::Light);
        assert_eq!(Nesting::Under(parent(Slot::Light)).runs_as(Slot::Heavy), Slot::Heavy);
        assert_eq!(Nesting::Own.runs_as(Slot::Critical), Slot::Critical);
        assert_eq!(
            Nesting::Refused(parent(Slot::Heavy)).covering(),
            None,
            "a refused gate runs under nothing"
        );
        assert_eq!(
            Nesting::Under(parent(Slot::Light)).covering(),
            None,
            "an uncovered gate runs under nothing"
        );
    }
}
