//! The builder's own slot pool, as both boxes judge it: the count file `{root}/slots.json`, the claim each held slot
//! `{root}/slots/{n}.json` carries, the choice of a slot given how each one is judged, the holder text a hold may
//! carry, and the lines a full builder and a hold say. Nothing here touches the operating system; each box reads and
//! writes its own `slots` folder and judges each claim's life (a session claim by the drain's session rule, a hold
//! claim by its `{n}.lock` held open), then calls these.

use std::time::Duration;

use serde_json::{Value, json};

use crate::drain::{MOST_COMMAND_CHARS, MOST_NAME_CHARS};
use crate::{escaped, is_session_id};

/// The count file beneath the work root.
pub const COUNT_FILE: &str = "slots.json";
/// The folder of claims and hold locks beneath the work root.
pub const SLOTS: &str = "slots";
/// The most slots a count file may give.
pub const MOST_SLOTS: u32 = 64;
/// How long a hold with no free slot waits between its looks.
pub const HOLD_POLL: Duration = Duration::from_secs(5);
/// How many of a hold's looks pass between its waiting lines: one line each 30 s at [`HOLD_POLL`].
pub const HOLD_LINE_POLLS: u64 = 6;
/// The line a hold says when the builder has no count file.
pub const NO_COUNT_LINE: &str = "gate box: this builder has no slot count; run gate push";

/// The file name of slot `n`'s claim.
#[must_use]
pub fn claim_name(n: usize) -> String {
    format!("{n}.json")
}

/// The file name of slot `n`'s hold lock, which a hold keeps open, shared with no one, while it lives.
#[must_use]
pub fn lock_name(n: usize) -> String {
    format!("{n}.lock")
}

/// The slot count the count file's text `text` gives (`{"v":1,"slots":N}`, `1 <= N <= MOST_SLOTS`).
///
/// # Errors
///
/// Why the text is no count file.
pub fn count_of(text: &str) -> Result<u32, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| format!("it is not JSON: {error}"))?;
    let object = value.as_object().ok_or("it is not a JSON object")?;
    if object.get("v").and_then(Value::as_u64) != Some(1) {
        return Err("its v is not 1".to_owned());
    }
    match object.get("slots").and_then(Value::as_u64) {
        Some(slots) if (1..=u64::from(MOST_SLOTS)).contains(&slots) => Ok(u32::try_from(slots).unwrap_or(MOST_SLOTS)),
        _ => Err(format!("its slots is not a whole number from 1 to {MOST_SLOTS}")),
    }
}

/// The line a box says of a count file that cannot be read or is malformed, which then counts as no file.
#[must_use]
pub fn unread_count_line(why: &str) -> String {
    format!("gate box: the builder's slot count cannot be read, so it takes no slot: {}", escaped(why))
}

/// A slot held by a routed run's drain session, live while that session is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionClaim {
    /// The drain session.
    pub session: String,
    /// The PC's host name, at most [`MOST_NAME_CHARS`] characters.
    pub pc: String,
    /// The run's repository, at most [`MOST_NAME_CHARS`] characters.
    pub repo: String,
    /// The run's command, at most [`MOST_COMMAND_CHARS`] characters.
    pub command: String,
    /// When it was taken, in UTC.
    pub taken: String,
}

impl SessionClaim {
    /// The claim of `session`, its texts capped as the drain caps a session record's.
    #[must_use]
    pub fn capped(session: &str, pc: &str, repo: &str, command: &str, taken: &str) -> SessionClaim {
        SessionClaim {
            session: session.to_owned(),
            pc: capped(pc, MOST_NAME_CHARS),
            repo: capped(repo, MOST_NAME_CHARS),
            command: capped(command, MOST_COMMAND_CHARS),
            taken: taken.to_owned(),
        }
    }
}

/// A slot held by `gate box hold`, live while its hold keeps the slot's lock open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldClaim {
    /// The holder's text ([`holder_refusal`]).
    pub holder: String,
    /// The hold's ceiling, in seconds above 0.
    pub ceiling: u64,
    /// The hold's process id, above 0.
    pub pid: u32,
    /// When it started, in UTC.
    pub started: String,
}

/// What a slot's claim says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// A routed run's session holds it.
    Session(SessionClaim),
    /// A hold holds it.
    Hold(HoldClaim),
}

impl Claim {
    /// The claim the text `text` gives.
    ///
    /// # Errors
    ///
    /// Why the text is no claim: not a JSON object, a `v` that is not 1, a `kind` that is neither `session` nor
    /// `hold`, or a field of its kind missing or of the wrong form.
    pub fn of(text: &str) -> Result<Claim, String> {
        let value: Value = serde_json::from_str(text).map_err(|error| format!("it is not JSON: {error}"))?;
        if !value.is_object() {
            return Err("it is not a JSON object".to_owned());
        }
        if value.get("v").and_then(Value::as_u64) != Some(1) {
            return Err("its v is not 1".to_owned());
        }
        let text_of = |key: &str| -> Result<String, String> {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("its {key} is not text"))
        };
        match value.get("kind").and_then(Value::as_str) {
            Some("session") => {
                let session = text_of("session")?;
                if !is_session_id(&session) {
                    return Err("its session is not a session id".to_owned());
                }
                Ok(Claim::Session(SessionClaim {
                    session,
                    pc: text_of("pc")?,
                    repo: text_of("repo")?,
                    command: text_of("command")?,
                    taken: text_of("taken")?,
                }))
            }
            Some("hold") => {
                let holder = text_of("holder")?;
                if let Some(why) = holder_refusal(&holder) {
                    return Err(format!("its holder {why}"));
                }
                let ceiling = value
                    .get("ceiling")
                    .and_then(Value::as_u64)
                    .filter(|ceiling| *ceiling > 0)
                    .ok_or("its ceiling is not a whole number of seconds above 0")?;
                let pid = value
                    .get("pid")
                    .and_then(Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok())
                    .filter(|pid| *pid > 0)
                    .ok_or("its pid is not a process id above 0")?;
                Ok(Claim::Hold(HoldClaim {
                    holder,
                    ceiling,
                    pid,
                    started: text_of("started")?,
                }))
            }
            _ => Err("its kind is not session or hold".to_owned()),
        }
    }

    /// The claim as its file holds it, one compact JSON object.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Claim::Session(claim) => json!({
                "v": 1,
                "kind": "session",
                "session": claim.session,
                "pc": claim.pc,
                "repo": claim.repo,
                "command": claim.command,
                "taken": claim.taken,
            }),
            Claim::Hold(claim) => json!({
                "v": 1,
                "kind": "hold",
                "holder": claim.holder,
                "ceiling": claim.ceiling,
                "pid": claim.pid,
                "started": claim.started,
            }),
        }
        .to_string()
    }

    /// The session it names, for a session claim.
    #[must_use]
    pub fn session(&self) -> Option<&str> {
        match self {
            Claim::Session(claim) => Some(&claim.session),
            Claim::Hold(_) => None,
        }
    }
}

/// The first `most` characters of `text`.
fn capped(text: &str, most: usize) -> String {
    text.chars().take(most).collect()
}

/// Why the holder text `holder` cannot be a hold's: empty, holding a control character, or longer than
/// [`MOST_COMMAND_CHARS`] characters; `None` when it can.
#[must_use]
pub fn holder_refusal(holder: &str) -> Option<String> {
    if holder.is_empty() {
        Some("is empty".to_owned())
    } else if holder.chars().any(char::is_control) {
        Some("holds a control character".to_owned())
    } else if holder.chars().count() > MOST_COMMAND_CHARS {
        Some(format!("is longer than {MOST_COMMAND_CHARS} characters"))
    } else {
        None
    }
}

/// The usage line of a holder text `holder` refused for `why` ([`holder_refusal`]), the text escaped.
#[must_use]
pub fn holder_line(holder: &str, why: &str) -> String {
    format!("gate box: the holder '{}' {why}", escaped(holder))
}

/// One slot as the box judged it under its lock, from a claim or lock file of its index in the `slots` folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged {
    /// The slot's index, which may be past a count lowered since its claim was taken.
    pub n: usize,
    /// Whether its claim is live; a slot with no claim, or a dead one, is not.
    pub live: bool,
    /// The session its live claim names, for a session claim.
    pub session: Option<String>,
}

/// What a taker gets from the slots as judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    /// The session already holds this slot, and keeps it.
    Kept(usize),
    /// This slot is free, the lowest one that is.
    Free(usize),
    /// Every slot is held, `held` of them.
    Full {
        /// How many are held.
        held: usize,
    },
}

/// The slot a taker of `session` (none for a hold) gets of `count` from `slots`, every slot the box judged whatever its
/// index: a slot the session's live claim already holds, at any index; else, while fewer than `count` claims are live
/// (each counts, a claim past a lowered count too), the lowest index below `count` with no live claim; else none.
#[must_use]
pub fn chosen(slots: &[Judged], count: u32, session: Option<&str>) -> Take {
    if let Some(session) = session
        && let Some(own) = slots.iter().find(|slot| slot.live && slot.session.as_deref() == Some(session))
    {
        return Take::Kept(own.n);
    }
    let held = slots.iter().filter(|slot| slot.live).count();
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    if held >= count {
        return Take::Full { held };
    }
    // Fewer live claims than the count leave an index below it with none.
    match (0..count).find(|n| !slots.iter().any(|slot| slot.live && slot.n == *n)) {
        Some(free) => Take::Free(free),
        None => Take::Full { held },
    }
}

/// The line after a `full` refusal tag: `held` of `slots` slots are held.
#[must_use]
pub fn full_line(held: usize, slots: u32) -> String {
    format!("gate: remote builder is full: {held} of {slots} slots held")
}

/// The line a waiting hold says: `held` of `slots` slots are held.
#[must_use]
pub fn waiting_line(held: usize, slots: u32) -> String {
    format!("gate: waiting for a builder slot: {held} of {slots} held")
}

/// The line a hold says when its ceiling of `seconds` ended it.
#[must_use]
pub fn ceiling_line(seconds: u64) -> String {
    format!("gate: hold ended at its ceiling of {seconds} s")
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a claim the test wrote that does not read back must fail the test loudly")]
mod tests {
    use serde_json::{Value, json};

    use super::{
        Claim, HoldClaim, Judged, MOST_SLOTS, SessionClaim, Take, chosen, claim_name, count_of, full_line, holder_line, holder_refusal, lock_name,
        waiting_line,
    };
    use crate::drain::{MOST_COMMAND_CHARS, MOST_NAME_CHARS};

    fn session_claim() -> Value {
        json!({ "v": 1, "kind": "session", "session": "s1", "pc": "pc1", "repo": "repo1", "command": "cargo test", "taken": "2026-10-27T10:00:00Z" })
    }

    fn hold_claim() -> Value {
        json!({
            "v": 1,
            "kind": "hold",
            "holder": "github-actions repo ci/test run 7",
            "ceiling": 3600,
            "pid": 42,
            "started": "2026-10-27T10:00:00Z"
        })
    }

    /// `claim` with `key` set to `value`, or removed when `value` is `None`.
    fn with(claim: &Value, key: &str, value: Option<Value>) -> String {
        let mut changed = claim.clone();
        let object = changed.as_object_mut().expect("a claim is an object");
        match value {
            Some(value) => object.insert(key.to_owned(), value),
            None => object.remove(key),
        };
        changed.to_string()
    }

    #[test]
    fn a_count_file_gives_1_to_64_slots() {
        assert_eq!(count_of(r#"{"v":1,"slots":1}"#), Ok(1));
        assert_eq!(count_of(r#"{"v":1,"slots":64}"#), Ok(MOST_SLOTS));
        assert_eq!(count_of(r#"{"slots":3,"v":1,"note":"kept"}"#), Ok(3), "another key is ignored");
    }

    #[test]
    fn a_count_file_out_of_its_range_or_malformed_is_refused() {
        let range = "its slots is not a whole number from 1 to 64";
        for (text, why) in [
            (r#"{"v":1,"slots":0}"#, range),
            (r#"{"v":1,"slots":65}"#, range),
            (r#"{"v":1,"slots":-1}"#, range),
            (r#"{"v":1,"slots":1.5}"#, range),
            (r#"{"v":1,"slots":"2"}"#, range),
            (r#"{"v":1}"#, range),
            (r#"{"v":2,"slots":2}"#, "its v is not 1"),
            (r#"{"slots":2}"#, "its v is not 1"),
            ("[1]", "it is not a JSON object"),
        ] {
            assert_eq!(count_of(text), Err(why.to_owned()), "{text}");
        }
        assert!(count_of("").is_err_and(|why| why.starts_with("it is not JSON")));
        assert!(count_of("{\"v\":1,").is_err_and(|why| why.starts_with("it is not JSON")));
    }

    #[test]
    fn a_claim_reads_back_as_it_was_written() {
        let session = Claim::Session(SessionClaim::capped("s1", "pc1", "repo1", "cargo test", "2026-10-27T10:00:00Z"));
        let hold = Claim::Hold(HoldClaim {
            holder: "github-actions repo ci/test run 7".to_owned(),
            ceiling: 3600,
            pid: 42,
            started: "2026-10-27T10:00:00Z".to_owned(),
        });
        for claim in [&session, &hold] {
            assert_eq!(Claim::of(&claim.text()).as_ref(), Ok(claim));
        }
        assert_eq!(
            serde_json::from_str::<Value>(&session.text()).expect("JSON"),
            session_claim(),
            "the session claim's fields"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&hold.text()).expect("JSON"),
            hold_claim(),
            "the hold claim's fields"
        );
        assert_eq!(session.session(), Some("s1"));
        assert_eq!(hold.session(), None);
    }

    #[test]
    fn a_session_claim_caps_its_texts_as_the_drain_does() {
        let long = "x".repeat(MOST_COMMAND_CHARS + 5);
        let claim = SessionClaim::capped("s1", &long, &long, &long, "t");
        assert_eq!(claim.pc.chars().count(), MOST_NAME_CHARS);
        assert_eq!(claim.repo.chars().count(), MOST_NAME_CHARS);
        assert_eq!(claim.command.chars().count(), MOST_COMMAND_CHARS);
    }

    #[test]
    fn a_session_claim_with_a_field_missing_or_malformed_is_refused() {
        let claim = session_claim();
        for (key, value, why) in [
            ("v", Some(json!(2)), "its v is not 1"),
            ("v", None, "its v is not 1"),
            ("kind", Some(json!("other")), "its kind is not session or hold"),
            ("kind", None, "its kind is not session or hold"),
            ("session", Some(json!("a.b")), "its session is not a session id"),
            ("session", Some(json!("")), "its session is not a session id"),
            ("session", None, "its session is not text"),
            ("pc", Some(json!(3)), "its pc is not text"),
            ("repo", None, "its repo is not text"),
            ("command", Some(json!(null)), "its command is not text"),
            ("taken", Some(json!(17)), "its taken is not text"),
        ] {
            assert_eq!(Claim::of(&with(&claim, key, value.clone())), Err(why.to_owned()), "{key} {value:?}");
        }
        assert_eq!(Claim::of("[]"), Err("it is not a JSON object".to_owned()));
        assert!(Claim::of("{").is_err_and(|why| why.starts_with("it is not JSON")));
    }

    #[test]
    fn a_hold_claim_with_a_field_missing_or_malformed_is_refused() {
        let claim = hold_claim();
        let ceiling = "its ceiling is not a whole number of seconds above 0";
        let pid = "its pid is not a process id above 0";
        for (key, value, why) in [
            ("holder", None, "its holder is not text".to_owned()),
            ("holder", Some(json!("a\nb")), "its holder holds a control character".to_owned()),
            ("holder", Some(json!("")), "its holder is empty".to_owned()),
            ("ceiling", Some(json!(0)), ceiling.to_owned()),
            ("ceiling", Some(json!(-5)), ceiling.to_owned()),
            ("ceiling", Some(json!("60")), ceiling.to_owned()),
            ("ceiling", None, ceiling.to_owned()),
            ("pid", Some(json!(0)), pid.to_owned()),
            ("pid", Some(json!(u64::from(u32::MAX) + 1)), pid.to_owned()),
            ("pid", None, pid.to_owned()),
            ("started", Some(json!(false)), "its started is not text".to_owned()),
        ] {
            assert_eq!(Claim::of(&with(&claim, key, value.clone())), Err(why), "{key} {value:?}");
        }
        assert!(
            Claim::of(&with(&claim, "pid", Some(json!(u32::MAX)))).is_ok(),
            "the highest pid a u32 holds"
        );
    }

    #[test]
    fn a_holder_text_is_refused_empty_with_a_control_character_or_past_the_command_cap() {
        assert_eq!(holder_refusal("github-actions repo ci/test run 7"), None);
        assert_eq!(holder_refusal(&"é".repeat(MOST_COMMAND_CHARS)), None, "the cap counts characters");
        assert_eq!(holder_refusal(""), Some("is empty".to_owned()));
        for control in ["a\nb", "a\rb", "a\u{1b}[2Jb", "a\u{0}b", "a\u{85}b", "tab\there"] {
            assert_eq!(holder_refusal(control), Some("holds a control character".to_owned()), "{control:?}");
        }
        assert_eq!(
            holder_refusal(&"x".repeat(MOST_COMMAND_CHARS + 1)),
            Some(format!("is longer than {MOST_COMMAND_CHARS} characters"))
        );
        assert_eq!(
            holder_line("a\nb", "holds a control character"),
            r"gate box: the holder 'a\nb' holds a control character",
            "the line shows the text escaped"
        );
    }

    fn judged(n: usize, live: bool, session: Option<&str>) -> Judged {
        Judged {
            n,
            live,
            session: session.map(str::to_owned),
        }
    }

    #[test]
    fn a_taker_gets_its_own_live_slot_else_the_lowest_free_else_none() {
        let slots = [
            judged(0, true, None),
            judged(1, false, Some("old")),
            judged(2, true, Some("s1")),
            judged(3, false, None),
        ];
        assert_eq!(chosen(&slots, 4, Some("s1")), Take::Kept(2), "a session keeps the slot it holds");
        assert_eq!(chosen(&slots, 4, Some("s2")), Take::Free(1), "the lowest free slot");
        assert_eq!(chosen(&slots, 4, None), Take::Free(1), "a hold takes the lowest free slot");
        assert_eq!(
            chosen(&slots, 4, Some("old")),
            Take::Free(1),
            "a dead claim of the session is no slot of its own"
        );
        let full = [judged(0, true, Some("a")), judged(1, true, None)];
        assert_eq!(chosen(&full, 2, Some("b")), Take::Full { held: 2 });
        assert_eq!(chosen(&full, 2, Some("a")), Take::Kept(0), "a full builder keeps a session its slot");
        assert_eq!(chosen(&[], 1, None), Take::Free(0), "a slot with no claim is free");
        assert_eq!(chosen(&[judged(4, true, None)], 3, None), Take::Free(0), "a slot not judged is free");
    }

    #[test]
    fn a_live_claim_past_a_lowered_count_counts_against_it() {
        let slots = [judged(0, false, None), judged(2, true, Some("s1")), judged(5, true, None)];
        assert_eq!(chosen(&slots, 2, Some("s2")), Take::Full { held: 2 }, "two live claims fill a count of 2");
        assert_eq!(chosen(&slots, 1, None), Take::Full { held: 2 }, "and overfill a count of 1");
        assert_eq!(chosen(&slots, 2, Some("s1")), Take::Kept(2), "a session keeps its claim past the count");
        assert_eq!(chosen(&slots, 3, Some("s2")), Take::Free(0), "a free slot below the count");
        let high = [judged(0, true, None), judged(1, true, None), judged(4, true, None)];
        assert_eq!(chosen(&high, 4, None), Take::Free(2), "only an index below the count is taken");
    }

    #[test]
    fn the_names_and_lines() {
        assert_eq!(claim_name(3), "3.json");
        assert_eq!(lock_name(3), "3.lock");
        assert_eq!(full_line(2, 2), "gate: remote builder is full: 2 of 2 slots held");
        assert_eq!(waiting_line(1, 1), "gate: waiting for a builder slot: 1 of 1 held");
        assert_eq!(super::ceiling_line(5), "gate: hold ended at its ceiling of 5 s");
    }
}
