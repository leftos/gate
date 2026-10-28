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

/// The slot whose claim is named `name`: `n` only for a name equal to [`claim_name`] of `n`, case-sensitive, so a name
/// no box writes (`00.json`, `0.JSON`) is no claim. The Windows box, whose judge reads a claim's name in any case, lowers
/// the name's ASCII case first.
#[must_use]
pub fn claim_index(name: &str) -> Option<usize> {
    let n: usize = name.strip_suffix(".json")?.parse().ok()?;
    (claim_name(n) == name).then_some(n)
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

/// The most bytes a box slots line holds, its line end aside: the PC client's cap on any one line of a call, past which
/// it cuts the call as too long.
pub const MOST_LINE_BYTES: usize = 64 * 1024;

/// What every preflight's line of the builder's own slots starts with, its JSON body after it ([`BoxSlots::line`]).
pub const BOX_SLOTS_LINE: &str = "gate: remote box slots ";

/// One claim in the builder's `slots` folder, as a look found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    /// The slot's index, from its claim's file name.
    pub slot: usize,
    /// What its claim says.
    pub claim: Claim,
}

/// The builder's own slots as a preflight looks at them, without the drain lock and judging no claim: a dead claim is
/// still there until a taker removes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxSlots {
    /// The box's clock when it looked, in UTC.
    pub now: String,
    /// The count file's slot count; `None` with no count file or one that does not read.
    pub count: Option<u32>,
    /// Why the count file did not read, for one that is there.
    pub count_problem: Option<String>,
    /// How many claim files did not read or are no claim.
    pub unreadable: usize,
    /// How many claims that read are left out of `holders`: past the first [`MOST_SLOTS`], or past what fits the line.
    pub more: usize,
    /// The claims that read, by slot, at most [`MOST_SLOTS`] of them and no more than fit in [`MOST_LINE_BYTES`].
    pub holders: Vec<Holder>,
}

impl BoxSlots {
    /// The look at `now` of the count `count` read ([`count_of`], `Err` with why the file is there and did not read) and
    /// the `claims` read, each its slot and its file's text, `None` for one that could not be read: a claim that does
    /// not read is counted unreadable, the rest are its holders, by slot, the first [`MOST_SLOTS`] of them kept, fewer
    /// when their line would pass [`MOST_LINE_BYTES`], and the rest counted as `more`.
    #[must_use]
    pub fn looked(now: &str, count: Result<Option<u32>, String>, claims: Vec<(usize, Option<String>)>) -> BoxSlots {
        let (count, count_problem) = match count {
            Ok(count) => (count, None),
            Err(why) => (None, Some(why)),
        };
        let mut holders = Vec::new();
        let mut unreadable = 0;
        for (slot, text) in claims {
            match text.and_then(|text| Claim::of(&text).ok()) {
                Some(claim) => holders.push(Holder { slot, claim }),
                None => unreadable += 1,
            }
        }
        holders.sort_by_key(|holder| holder.slot);
        let most = usize::try_from(MOST_SLOTS).unwrap_or(usize::MAX);
        let more = holders.len().saturating_sub(most);
        holders.truncate(most);
        BoxSlots {
            now: now.to_owned(),
            count,
            count_problem,
            unreadable,
            more,
            holders,
        }
        .fitted()
    }

    /// The look with only as many of its holders, from the first, as keep its line within [`MOST_LINE_BYTES`], the
    /// rest counted into `more`.
    fn fitted(mut self) -> BoxSlots {
        let holders = std::mem::take(&mut self.holders);
        let total = self.more.saturating_add(holders.len());
        // The line with no holder and the longest `more` it can say, then each holder's JSON and its comma.
        let bare = BoxSlots { more: total, ..self.clone() };
        let mut length = bare.line().len();
        for holder in holders {
            let added = holder_value(&holder).to_string().len() + usize::from(!self.holders.is_empty());
            if length.saturating_add(added) > MOST_LINE_BYTES {
                break;
            }
            length += added;
            self.holders.push(holder);
        }
        self.more = total - self.holders.len();
        self
    }

    /// The line a preflight says of the look: [`BOX_SLOTS_LINE`], then one compact JSON object,
    /// `{"v":1,"now":..,"count":N|null,"countProblem":..,"unreadable":N,"more":N,"holders":[..]}`, `countProblem` only
    /// for a count file that did not read, `unreadable` and `more` only when not 0; each holder its claim's fields after
    /// its `slot`, in the look's order, a hold's `pid` as `builderPid`, each `pc` and `repo` cut to [`MOST_NAME_CHARS`]
    /// characters and each `command` and `holder` to [`MOST_COMMAND_CHARS`].
    #[must_use]
    pub fn line(&self) -> String {
        let mut body = serde_json::Map::new();
        body.insert("v".to_owned(), json!(1));
        body.insert("now".to_owned(), json!(self.now));
        body.insert("count".to_owned(), json!(self.count));
        if let Some(problem) = &self.count_problem {
            body.insert("countProblem".to_owned(), json!(problem));
        }
        if self.unreadable > 0 {
            body.insert("unreadable".to_owned(), json!(self.unreadable));
        }
        if self.more > 0 {
            body.insert("more".to_owned(), json!(self.more));
        }
        body.insert("holders".to_owned(), self.holders.iter().map(holder_value).collect());
        format!("{BOX_SLOTS_LINE}{}", Value::Object(body))
    }

    /// The look the line `line` says ([`BoxSlots::line`]).
    ///
    /// # Errors
    ///
    /// Why the line is no box slots line: another start, a body that is not a JSON object, a `v` that is not 1, or a
    /// field missing or of the wrong form, a holder's named by its place in the list.
    pub fn of_line(line: &str) -> Result<BoxSlots, String> {
        let text = line
            .strip_prefix(BOX_SLOTS_LINE)
            .ok_or_else(|| format!("it does not start with '{BOX_SLOTS_LINE}'"))?;
        let value: Value = serde_json::from_str(text).map_err(|error| format!("it is not JSON: {error}"))?;
        let body = value.as_object().ok_or("it is not a JSON object")?;
        if body.get("v").and_then(Value::as_u64) != Some(1) {
            return Err("its v is not 1".to_owned());
        }
        let now = body.get("now").and_then(Value::as_str).ok_or("its now is not text")?.to_owned();
        let count = match body.get("count") {
            Some(Value::Null) => None,
            Some(count) => Some(count_value(count)?),
            None => return Err(count_refusal()),
        };
        let count_problem = match body.get("countProblem") {
            None => None,
            Some(problem) => Some(problem.as_str().ok_or("its countProblem is not text")?.to_owned()),
        };
        let unreadable = whole_or_none(body, "unreadable")?;
        let more = whole_or_none(body, "more")?;
        let listed = body.get("holders").and_then(Value::as_array).ok_or("its holders is not a list")?;
        let holders = listed
            .iter()
            .enumerate()
            .map(|(at, holder)| holder_of(at, holder))
            .collect::<Result<_, _>>()?;
        Ok(BoxSlots {
            now,
            count,
            count_problem,
            unreadable,
            more,
            holders,
        })
    }
}

/// The whole number `body` gives at `key`, 0 when it has none. Else why not.
fn whole_or_none(body: &serde_json::Map<String, Value>, key: &str) -> Result<usize, String> {
    match body.get(key) {
        None => Ok(0),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| format!("its {key} is not a whole number")),
    }
}

/// Why a box slots line's `count` is refused.
fn count_refusal() -> String {
    format!("its count is not null or a whole number from 1 to {MOST_SLOTS}")
}

/// The slot count `value` gives, `1 <= N <= MOST_SLOTS`; else [`count_refusal`].
fn count_value(value: &Value) -> Result<u32, String> {
    value
        .as_u64()
        .filter(|count| (1..=u64::from(MOST_SLOTS)).contains(count))
        .and_then(|count| u32::try_from(count).ok())
        .ok_or_else(count_refusal)
}

/// The JSON of `holder` in a box slots line: its `slot`, then its claim's fields but `v`, a hold's `pid` as
/// `builderPid`, its `pc` and `repo` cut to [`MOST_NAME_CHARS`] characters and its `command` or `holder` to
/// [`MOST_COMMAND_CHARS`], as a claim's writer cuts them.
fn holder_value(holder: &Holder) -> Value {
    match &holder.claim {
        Claim::Session(claim) => json!({
            "slot": holder.slot,
            "kind": "session",
            "session": claim.session,
            "pc": capped(&claim.pc, MOST_NAME_CHARS),
            "repo": capped(&claim.repo, MOST_NAME_CHARS),
            "command": capped(&claim.command, MOST_COMMAND_CHARS),
            "taken": claim.taken,
        }),
        Claim::Hold(claim) => json!({
            "slot": holder.slot,
            "kind": "hold",
            "holder": capped(&claim.holder, MOST_COMMAND_CHARS),
            "ceiling": claim.ceiling,
            "builderPid": claim.pid,
            "started": claim.started,
        }),
    }
}

/// The holder at `at` in a box slots line's list, `value`, read as its claim would be ([`Claim::of`]) once its `slot`
/// is taken out and a hold's `builderPid` is read as its `pid`. Else why not, naming its place.
fn holder_of(at: usize, value: &Value) -> Result<Holder, String> {
    let mut fields = value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("its holder {at} is not a JSON object"))?;
    let slot = fields
        .remove("slot")
        .and_then(|slot| slot.as_u64())
        .and_then(|slot| usize::try_from(slot).ok())
        .ok_or_else(|| format!("its holder {at} has no slot number"))?;
    if fields.get("kind").and_then(Value::as_str) == Some("hold") {
        if fields.contains_key("pid") {
            return Err(format!("its holder {at} names a pid, not a builderPid"));
        }
        let pid = fields
            .remove("builderPid")
            .filter(|pid| pid.as_u64().and_then(|pid| u32::try_from(pid).ok()).is_some_and(|pid| pid > 0))
            .ok_or_else(|| format!("its holder {at}: its builderPid is not a process id above 0"))?;
        fields.insert("pid".to_owned(), pid);
    }
    fields.insert("v".to_owned(), json!(1));
    let claim = Claim::of(&Value::Object(fields).to_string()).map_err(|why| format!("its holder {at}: {why}"))?;
    Ok(Holder { slot, claim })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a claim the test wrote that does not read back must fail the test loudly")]
mod tests {
    use serde_json::{Value, json};

    use super::{
        BOX_SLOTS_LINE, BoxSlots, Claim, HoldClaim, Holder, Judged, MOST_LINE_BYTES, MOST_SLOTS, SessionClaim, Take, chosen, claim_index, claim_name,
        count_of, full_line, holder_line, holder_refusal, lock_name, waiting_line,
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

    const NOW: &str = "2026-10-27T10:05:00Z";

    fn hold(holder: &str) -> Claim {
        Claim::Hold(HoldClaim {
            holder: holder.to_owned(),
            ceiling: 3600,
            pid: 42,
            started: "2026-10-27T10:00:00Z".to_owned(),
        })
    }

    fn session(command: &str) -> Claim {
        Claim::Session(SessionClaim {
            session: "s1".to_owned(),
            pc: "pc1".to_owned(),
            repo: "repo1".to_owned(),
            command: command.to_owned(),
            taken: "2026-10-27T10:01:00Z".to_owned(),
        })
    }

    /// The JSON body of a box slots line.
    fn body(line: &str) -> Value {
        let body = line.strip_prefix(BOX_SLOTS_LINE).expect("the line starts with its prefix");
        serde_json::from_str(body).expect("the body is JSON")
    }

    #[test]
    fn the_box_slots_line_reads_back_with_a_session_and_a_hold() {
        let slots = BoxSlots {
            now: NOW.to_owned(),
            count: Some(2),
            count_problem: None,
            unreadable: 0,
            more: 0,
            holders: vec![
                Holder {
                    slot: 0,
                    claim: session("cargo test"),
                },
                Holder {
                    slot: 1,
                    claim: hold("github-actions repo ci/test run 7"),
                },
            ],
        };
        let line = slots.line();
        assert!(!line.contains('\n'), "{line}");
        assert!(
            line.starts_with(r#"gate: remote box slots {"v":1,"now":"2026-10-27T10:05:00Z","count":2,"holders":["#),
            "{line}"
        );
        assert_eq!(
            body(&line)["holders"],
            json!([
                {
                    "slot": 0,
                    "kind": "session",
                    "session": "s1",
                    "pc": "pc1",
                    "repo": "repo1",
                    "command": "cargo test",
                    "taken": "2026-10-27T10:01:00Z"
                },
                {
                    "slot": 1,
                    "kind": "hold",
                    "holder": "github-actions repo ci/test run 7",
                    "ceiling": 3600,
                    "builderPid": 42,
                    "started": "2026-10-27T10:00:00Z"
                },
            ]),
            "{line}"
        );
        assert_eq!(BoxSlots::of_line(&line), Ok(slots));
    }

    #[test]
    fn a_box_slots_line_with_no_count_says_null_and_why_the_count_file_did_not_read() {
        let none = BoxSlots::looked(NOW, Ok(None), Vec::new());
        assert_eq!(
            none.line(),
            r#"gate: remote box slots {"v":1,"now":"2026-10-27T10:05:00Z","count":null,"holders":[]}"#
        );
        assert_eq!(BoxSlots::of_line(&none.line()), Ok(none));
        let unread = BoxSlots::looked(NOW, Err("its v is not 1".to_owned()), Vec::new());
        assert_eq!(
            unread.line(),
            r#"gate: remote box slots {"v":1,"now":"2026-10-27T10:05:00Z","count":null,"countProblem":"its v is not 1","holders":[]}"#
        );
        assert_eq!(BoxSlots::of_line(&unread.line()), Ok(unread));
    }

    #[test]
    fn a_claim_that_does_not_read_is_counted_unreadable_and_said_only_when_there_is_one() {
        let read = BoxSlots::looked(NOW, Ok(Some(3)), vec![(2, Some(hold("h").text())), (0, Some(session("c").text()))]);
        assert_eq!(read.unreadable, 0);
        assert_eq!(read.holders.iter().map(|holder| holder.slot).collect::<Vec<_>>(), [0, 2]);
        assert!(body(&read.line()).get("unreadable").is_none(), "{}", read.line());
        let unread = BoxSlots::looked(NOW, Ok(Some(3)), vec![(0, None), (1, Some("{".to_owned())), (2, Some(hold("h").text()))]);
        assert_eq!(unread.unreadable, 2);
        assert_eq!(body(&unread.line())["unreadable"], json!(2));
        assert_eq!(BoxSlots::of_line(&unread.line()), Ok(unread));
    }

    #[test]
    fn a_long_command_or_holder_is_cut_to_300_characters_in_the_line() {
        let long = "é".repeat(400);
        let mut held = BoxSlots::looked(NOW, Ok(Some(2)), vec![(0, Some(session(&long).text()))]);
        held.holders.push(Holder { slot: 1, claim: hold(&long) });
        let read = BoxSlots::of_line(&held.line()).expect("the line reads back");
        let texts: Vec<usize> = read
            .holders
            .iter()
            .map(|holder| match &holder.claim {
                Claim::Session(claim) => claim.command.chars().count(),
                Claim::Hold(claim) => claim.holder.chars().count(),
            })
            .collect();
        assert_eq!(texts, [MOST_COMMAND_CHARS, MOST_COMMAND_CHARS]);
    }

    #[test]
    fn a_malformed_box_slots_line_is_refused() {
        let line = |body: &str| format!("{BOX_SLOTS_LINE}{body}");
        let holder = r#"{"slot":0,"kind":"hold","holder":"h","ceiling":5,"builderPid":7,"started":"t"}"#;
        assert!(BoxSlots::of_line(&line(&format!(r#"{{"v":1,"now":"t","count":1,"holders":[{holder}]}}"#))).is_ok());
        for (text, why) in [
            ("gate: remote box slot {}".to_owned(), "it does not start with 'gate: remote box slots '"),
            (line("[]"), "it is not a JSON object"),
            (line(r#"{"v":2,"now":"t","count":1,"holders":[]}"#), "its v is not 1"),
            (line(r#"{"v":1,"count":1,"holders":[]}"#), "its now is not text"),
            (
                line(r#"{"v":1,"now":"t","count":0,"holders":[]}"#),
                "its count is not null or a whole number from 1 to 64",
            ),
            (
                line(r#"{"v":1,"now":"t","holders":[]}"#),
                "its count is not null or a whole number from 1 to 64",
            ),
            (
                line(r#"{"v":1,"now":"t","count":null,"countProblem":3,"holders":[]}"#),
                "its countProblem is not text",
            ),
            (
                line(r#"{"v":1,"now":"t","count":null,"unreadable":-1,"holders":[]}"#),
                "its unreadable is not a whole number",
            ),
            (line(r#"{"v":1,"now":"t","count":1,"holders":{}}"#), "its holders is not a list"),
            (
                line(r#"{"v":1,"now":"t","count":1,"holders":[{"kind":"hold"}]}"#),
                "its holder 0 has no slot number",
            ),
            (
                line(r#"{"v":1,"now":"t","count":1,"holders":[{"slot":0,"kind":"hold","holder":"h","ceiling":5,"pid":7,"started":"t"}]}"#),
                "its holder 0 names a pid, not a builderPid",
            ),
            (
                line(r#"{"v":1,"now":"t","count":1,"holders":[{"slot":0,"kind":"hold","holder":"h","ceiling":5,"started":"t"}]}"#),
                "its holder 0: its builderPid is not a process id above 0",
            ),
            (
                line(r#"{"v":1,"now":"t","count":1,"more":"2","holders":[]}"#),
                "its more is not a whole number",
            ),
            (
                line(r#"{"v":1,"now":"t","count":1,"holders":[{"slot":0,"kind":"other"}]}"#),
                "its holder 0: its kind is not session or hold",
            ),
        ] {
            assert_eq!(BoxSlots::of_line(&text), Err(why.to_owned()), "{text}");
        }
        assert!(BoxSlots::of_line(&line("{")).is_err_and(|why| why.starts_with("it is not JSON")));
    }

    #[test]
    fn a_claim_index_is_read_only_from_the_name_its_claim_is_written_under() {
        assert_eq!(claim_index("0.json"), Some(0));
        assert_eq!(claim_index("12.json"), Some(12));
        for other in [
            "00.json",
            "007.json",
            "0.JSON",
            "0.Json",
            "0.lock",
            ".json",
            "a.json",
            "-1.json",
            "+1.json",
            "0.json.new-7",
            "１.json",
        ] {
            assert_eq!(claim_index(other), None, "{other:?}");
        }
        assert_eq!(claim_index("99999999999999999999999.json"), None, "past a usize");
    }

    #[test]
    fn a_holder_s_pc_and_repo_are_cut_as_a_claim_s_writer_cuts_them() {
        let long = "p".repeat(MOST_NAME_CHARS + 50);
        let claim = Claim::Session(SessionClaim {
            session: "s1".to_owned(),
            pc: long.clone(),
            repo: long,
            command: "c".to_owned(),
            taken: "t".to_owned(),
        });
        let slots = BoxSlots::looked(NOW, Ok(Some(1)), vec![(0, Some(claim.text()))]);
        let read = BoxSlots::of_line(&slots.line()).expect("the line reads back");
        let cut: Vec<(usize, usize)> = read
            .holders
            .iter()
            .filter_map(|holder| match &holder.claim {
                Claim::Session(said) => Some((said.pc.chars().count(), said.repo.chars().count())),
                Claim::Hold(_) => None,
            })
            .collect();
        assert_eq!(cut, [(MOST_NAME_CHARS, MOST_NAME_CHARS)]);
    }

    #[test]
    fn a_look_whose_holders_would_pass_the_line_cap_keeps_those_that_fit_and_counts_the_rest_as_more() {
        let heavy = |chars: usize| "\"𝄞".repeat(chars / 2);
        let claims = (0..64)
            .map(|n| {
                let claim = Claim::Session(SessionClaim {
                    session: "s1".to_owned(),
                    pc: heavy(MOST_NAME_CHARS),
                    repo: heavy(MOST_NAME_CHARS),
                    command: heavy(MOST_COMMAND_CHARS),
                    taken: "t".to_owned(),
                });
                (n, Some(claim.text()))
            })
            .collect();
        let slots = BoxSlots::looked(NOW, Ok(Some(MOST_SLOTS)), claims);
        let line = slots.line();
        assert!(line.len() <= MOST_LINE_BYTES, "{} bytes", line.len());
        assert!(slots.more > 0, "some holders are left out");
        assert_eq!(slots.holders.len() + slots.more, 64);
        assert_eq!(body(&line)["more"], json!(slots.more));
        assert_eq!(BoxSlots::of_line(&line), Ok(slots));
    }

    #[test]
    fn a_look_keeps_the_first_64_holders_by_slot_and_counts_the_rest_as_more() {
        let claims = (0..70).rev().map(|n| (n, Some(hold("h").text()))).collect();
        let slots = BoxSlots::looked(NOW, Ok(Some(MOST_SLOTS)), claims);
        assert_eq!(slots.holders.len(), 64);
        assert_eq!(slots.holders.first().map(|holder| holder.slot), Some(0));
        assert_eq!(slots.holders.last().map(|holder| holder.slot), Some(63));
        assert_eq!(slots.more, 6);
        assert_eq!(body(&slots.line())["more"], json!(6));
        assert_eq!(BoxSlots::of_line(&slots.line()), Ok(slots));
        let few = BoxSlots::looked(NOW, Ok(Some(1)), vec![(0, Some(hold("h").text()))]);
        assert!(body(&few.line()).get("more").is_none(), "{}", few.line());
    }
}
