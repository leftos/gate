//! The builder drain's rules both boxes decide by: the clock and its lease and lapse, how a barrier file stands, the
//! line of tickets, the admission decision, the lines `gate box drain` and a refused call say, and which processes the
//! drained test counts for a root. Nothing here touches the operating system but [`Clock::of`], which reads the clock;
//! each box keeps its own reading and writing of `{root}/drain/` and calls these.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate_args::BoxSeams;
/// A process as the drained test sees it, which each box's process scan gives [`counted_processes`].
pub use proc_table::ProcessInfo;
use proc_table::ProcessTree;
use serde_json::{Value, json};

use crate::{PathForm, is_session_id};

/// How long a ticket and a session stay live after their last poll or call.
pub const LEASE_MS: u64 = 120_000;
/// How long a barrier stands after its last raise.
pub const LAPSE_MS: u64 = 30 * 60 * 1000;
/// How long a call waits for the drain lock another call holds.
pub const LOCK_RETRY: Duration = Duration::from_secs(10);
/// How long a call waits between its tries of the drain lock.
pub const LOCK_STEP: Duration = Duration::from_millis(20);
/// The most characters of a run's command a session record keeps.
pub const MOST_COMMAND_CHARS: usize = 300;
/// The most characters of a PC's host name and of a repository's name a session record keeps.
pub const MOST_NAME_CHARS: usize = 100;

/// The drain's folder beneath the work root.
pub const DRAIN: &str = "drain";
/// The drain lock, in the drain's folder.
pub const LOCK: &str = "lock";
/// The barrier record, in the drain's folder.
pub const BARRIER: &str = "barrier";
/// The record of the last lapse, in the drain's folder.
pub const LAPSED: &str = "lapsed";
/// The record of the last lift, in the drain's folder.
pub const LIFTED: &str = "lifted";
/// The folder of tickets, in the drain's folder.
pub const TICKETS: &str = "tickets";
/// The folder of session records and holders, in the drain's folder.
pub const SESSIONS: &str = "sessions";

/// The time the drain judges at, and its lease and lapse, all in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    /// Now, in milliseconds since the Unix epoch.
    pub now: u64,
    /// How long a ticket and a session stay live.
    pub lease: u64,
    /// How long a barrier stands after its last raise.
    pub lapse: u64,
}

impl Clock {
    /// The clock's time, or `--test-box-now`'s, with the lease and the lapse the seams give, else their defaults.
    #[must_use]
    pub fn of(seams: &BoxSeams) -> Clock {
        let now = seams
            .now
            .map_or_else(|| unix_ms(SystemTime::now()), |seconds| seconds.saturating_mul(1000));
        Clock {
            now,
            lease: seams.drain_lease_ms.unwrap_or(LEASE_MS),
            lapse: seams.drain_lapse_ms.unwrap_or(LAPSE_MS),
        }
    }

    /// Now in whole seconds since the Unix epoch.
    #[must_use]
    pub fn now_s(self) -> u64 {
        self.now / 1000
    }

    /// Whether a lease counted from `since_s` (Unix seconds) still holds now.
    #[must_use]
    pub fn within_lease(self, since_s: u64) -> bool {
        self.now < since_s.saturating_mul(1000).saturating_add(self.lease)
    }

    /// The Unix second a barrier raised at `raised_s` lapses at.
    #[must_use]
    pub fn lapses_at(self, raised_s: u64) -> u64 {
        raised_s.saturating_mul(1000).saturating_add(self.lapse) / 1000
    }

    /// Now as a system time.
    #[must_use]
    pub fn system_time(self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.now)
    }
}

/// `time` in whole milliseconds since the Unix epoch; 0 for a time before it, which no clock of a builder reads.
#[must_use]
pub fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// `time` in whole seconds since the Unix epoch.
#[must_use]
pub fn unix_s(time: SystemTime) -> u64 {
    unix_ms(time) / 1000
}

/// A raised barrier: when it was last raised, when it lapses, and how many raises in a row it has had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Raising {
    /// The Unix second of its last raise.
    pub raised: u64,
    /// The Unix second it lapses at.
    pub lapses: u64,
    /// How many raises in a row it has had.
    pub generation: u64,
}

/// How the barrier stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Barrier {
    /// None was raised, or the last was lifted.
    None,
    /// It stands.
    Standing(Raising),
    /// The last one lapsed at this Unix second without a lift.
    Lapsed(u64),
}

/// How the barrier file `text`, last written at `written_s`, stands at `clock`'s time: standing until its `Lapses`,
/// then lapsed at it. A file that does not read as a barrier stands as one raised at its last write, so a damaged
/// barrier holds the drain closed for one lapse and no longer.
#[must_use]
pub fn barrier_of(text: &str, written_s: u64, clock: Clock) -> Barrier {
    let read = serde_json::from_str::<Value>(text).ok().and_then(|value| {
        Some(Raising {
            raised: value.get("Raised")?.as_u64()?,
            lapses: value.get("Lapses")?.as_u64()?,
            generation: value.get("Generation")?.as_u64()?,
        })
    });
    let raised = read.unwrap_or(Raising {
        raised: written_s,
        lapses: clock.lapses_at(written_s),
        generation: 0,
    });
    if clock.now < raised.lapses.saturating_mul(1000) {
        Barrier::Standing(raised)
    } else {
        Barrier::Lapsed(raised.lapses)
    }
}

/// The `At` of a `lapsed` or `lifted` record `text`; `written_s`, its last write, when it does not read as one.
#[must_use]
pub fn at_of(text: &str, written_s: u64) -> u64 {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("At")?.as_u64())
        .unwrap_or(written_s)
}

/// One run's place in the drain's line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    /// Its number in line.
    pub seq: u64,
    /// The session it holds the place of.
    pub session: String,
    /// The Unix second of its last poll; its file's last write when it does not read.
    pub polled: u64,
}

/// The file name of the ticket `seq` of `session`.
#[must_use]
pub fn ticket_name(seq: u64, session: &str) -> String {
    format!("{seq:08}-{session}")
}

/// The number and session a ticket's file name `name` gives; `None` for any other name.
#[must_use]
pub fn ticket_of_name(name: &str) -> Option<(u64, &str)> {
    let (seq, session) = name.split_once('-')?;
    let numbered = seq.len() >= 8 && seq.bytes().all(|byte| byte.is_ascii_digit());
    (numbered && is_session_id(session)).then_some(())?;
    Some((seq.parse().ok()?, session))
}

/// The number `session`'s ticket has among `tickets`: its own, so a renewal never renumbers it, else one more than the
/// highest.
#[must_use]
pub fn ticket_seq(tickets: &[Ticket], session: &str) -> u64 {
    tickets.iter().find(|ticket| ticket.session == session).map_or_else(
        || tickets.iter().map(|ticket| ticket.seq).max().unwrap_or(0).saturating_add(1),
        |own| own.seq,
    )
}

/// Whether a ticket last polled at `polled` is live at `clock`'s time: within the lease of the latest of its poll,
/// the box's boot `boot`, and the last lift or lapse `lift`, so no waiter dies of a reboot or of the wait the barrier
/// itself made.
#[must_use]
pub fn ticket_live(polled: u64, boot: u64, lift: u64, clock: Clock) -> bool {
    clock.within_lease(polled.max(boot).max(lift))
}

/// How many live tickets of other sessions wait ahead of `session`: older than its own ticket, or every one when it has
/// none.
pub fn older_live(tickets: &[Ticket], session: Option<&str>, live: impl Fn(&Ticket) -> bool) -> usize {
    let own = session
        .and_then(|session| tickets.iter().find(|ticket| ticket.session == session))
        .map(|ticket| ticket.seq);
    tickets
        .iter()
        .filter(|ticket| Some(ticket.session.as_str()) != session && own.is_none_or(|seq| ticket.seq < seq) && live(ticket))
        .count()
}

/// Why the drain refused a call: its place in line (`None` for a call that cannot queue), and the standing barrier's
/// raise and lapse times, `None` when none stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    /// Its place in line; `None` for a call that cannot queue.
    pub place: Option<u32>,
    /// The Unix second the standing barrier was raised at.
    pub raised: Option<u64>,
    /// The Unix second the standing barrier lapses at.
    pub lapses: Option<u64>,
}

/// The drain's decision for a call of `session` with `older` live tickets ahead of it while `standing` stands: a call of
/// a session `live` already (its holder held, or called within the lease) is admitted whatever stands or waits; a call
/// with no session is refused under a standing barrier and admitted otherwise, as it cannot queue; any other call of a
/// session is refused while a barrier stands or an older live ticket waits.
///
/// # Errors
///
/// The call is refused: its place in line and the standing barrier.
pub fn decided(standing: Option<Raising>, older: usize, session: Option<&str>, live: bool) -> Result<(), Refused> {
    let refused = |place| Refused {
        place,
        raised: standing.map(|raised| raised.raised),
        lapses: standing.map(|raised| raised.lapses),
    };
    let place = Some(u32::try_from(older.saturating_add(1)).unwrap_or(u32::MAX));
    match (standing, session) {
        (_, Some(_)) if live => Ok(()),
        (Some(_), None) => Err(refused(None)),
        (Some(_), Some(_)) => Err(refused(place)),
        (None, Some(_)) if older > 0 => Err(refused(place)),
        (None, _) => Ok(()),
    }
}

/// A session found live by the drained test, as its status line shows it.
#[derive(Debug, PartialEq, Eq)]
pub struct LiveSession {
    /// Seconds since its last call.
    pub age: u64,
    /// The PC's host name.
    pub pc: Option<String>,
    /// The run's repository.
    pub repo: Option<String>,
    /// The run's command.
    pub command: Option<String>,
    /// The run's job.
    pub job: Option<String>,
}

/// What a session record says; its last write stands in for `Called` when it does not read.
pub struct Record {
    /// The Unix second of its last call.
    pub called: u64,
    /// The PC's host name.
    pub pc: Option<String>,
    /// The run's repository.
    pub repo: Option<String>,
    /// The run's command.
    pub command: Option<String>,
    /// The run's job.
    pub job: Option<String>,
}

impl Record {
    /// The session record `text`, last written at `written_s`; an empty text field reads as none.
    #[must_use]
    pub fn of(text: &str, written_s: u64) -> Record {
        let value = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
        let text_of = |key: &str| value.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_owned);
        Record {
            called: value.get("Called").and_then(Value::as_u64).unwrap_or(written_s),
            pc: text_of("Pc"),
            repo: text_of("Repo"),
            command: text_of("Command"),
            job: text_of("Job"),
        }
    }
}

/// The barrier's status line.
#[must_use]
pub fn barrier_line(barrier: Barrier) -> String {
    match barrier {
        Barrier::None => "gate: drain barrier none".to_owned(),
        Barrier::Standing(raised) => format!("gate: drain barrier {} {}", raised.raised, raised.lapses),
        Barrier::Lapsed(at) => format!("gate: drain barrier lapsed {at}"),
    }
}

/// A live session's status line: its age, then one compact JSON object `{"Pc","Repo","Command"}`, `null` for a field it
/// lacks.
#[must_use]
pub fn session_line(session: &LiveSession) -> String {
    let fields = json!({ "Pc": session.pc, "Repo": session.repo, "Command": session.command });
    format!("gate: drain session {} {fields}", session.age)
}

/// The live tickets' status line.
#[must_use]
pub fn tickets_line(count: usize) -> String {
    format!("gate: drain tickets {count}")
}

/// The state's status line: drained under a standing barrier with nothing `idle` says is going, waiting under one
/// with something going, open with none.
#[must_use]
pub fn state_line(barrier: Barrier, idle: bool) -> String {
    let state = match barrier {
        Barrier::Standing(_) if idle => "drained",
        Barrier::Standing(_) => "waiting",
        Barrier::None | Barrier::Lapsed(_) => "open",
    };
    format!("gate: drain state {state}")
}

/// The two lines a refused call says after its `drain` tag: its place in line (`gate: drain place {n}`, or `none` for a
/// call that cannot queue), then the standing barrier's raise and lapse times as [`barrier_line`] says them, or
/// `gate: drain barrier none` when no barrier stands.
#[must_use]
pub fn refused_lines(refused: Refused) -> [String; 2] {
    let place = refused.place.map_or_else(|| "none".to_owned(), |place| place.to_string());
    let barrier = match (refused.raised, refused.lapses) {
        (Some(raised), Some(lapses)) => Barrier::Standing(Raising {
            raised,
            lapses,
            generation: 0,
        }),
        _ => Barrier::None,
    };
    [format!("gate: drain place {place}"), barrier_line(barrier)]
}

/// Whether `character` belongs to a file name, so a root followed or preceded by one is not that root.
fn is_name_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '-' | '_' | '.')
}

/// `text` with its ASCII letters lowercased and, under [`PathForm::Windows`], every `/` made `\`, so two spellings of
/// one path compare equal. A Unix path keeps its `\`, which is a file name's character there.
fn folded(text: &str, form: PathForm) -> String {
    text.chars()
        .map(|character| match form {
            PathForm::Windows if character == '/' => '\\',
            PathForm::Windows | PathForm::Unix => character.to_ascii_lowercase(),
        })
        .collect()
}

/// The root `root` folded ([`folded`]), its trailing separators dropped: `\` and `/` under [`PathForm::Windows`], `/`
/// under [`PathForm::Unix`].
fn folded_root(root: &str, form: PathForm) -> String {
    let trimmed = match form {
        PathForm::Windows => root.trim_end_matches(['\\', '/']),
        PathForm::Unix => root.trim_end_matches('/'),
    };
    folded(trimmed, form)
}

/// Whether the command line `line` names the root `root` on a path boundary, case and separators folded under `form`:
/// so `D:/gate-remote/x.git` names `D:\gate-remote`, and `D:\gate-remote-old` does not.
#[must_use]
pub fn names_root(line: &str, root: &str, form: PathForm) -> bool {
    let root = folded_root(root, form);
    let line = folded(line, form);
    !root.is_empty()
        && line.match_indices(&root).any(|(at, _)| {
            let before = line.get(..at).and_then(|start| start.chars().next_back());
            let after = line.get(at + root.len()..).and_then(|rest| rest.chars().next());
            !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char)
        })
}

/// Whether the command line `line` is a `gate box prune --root {root}` (`gate-box box prune --root {root}` on a Unix
/// box), case and separators folded under `form`.
#[must_use]
pub fn names_prune(line: &str, root: &str, form: PathForm) -> bool {
    const PRUNE: &str = " prune --root ";
    let root = folded_root(root, form);
    let line = folded(line, form);
    line.match_indices(PRUNE).any(|(at, _)| {
        let rest = line.get(at + PRUNE.len()..).unwrap_or_default().trim_start_matches(['"', '\'']);
        line.get(..at).is_some_and(|start| names_box(start, form))
            && rest.strip_prefix(root.as_str()).is_some_and(|after| !after.starts_with(is_name_char))
    })
}

/// Whether the folded command line `start`, the part before a prune's verb, calls the box: under [`PathForm::Windows`]
/// any text holding `box`; under [`PathForm::Unix`] the word `box` itself, quotes trimmed, since the program
/// `gate-box` holds it.
fn names_box(start: &str, form: PathForm) -> bool {
    match form {
        PathForm::Windows => start.contains("box"),
        PathForm::Unix => start.split_whitespace().any(|word| word.trim_matches(['"', '\'']) == "box"),
    }
}

/// Whether `process`, whose command line could not be read, counts as the root's, so the drained test fails closed on
/// it. Its image name, case ignored: under [`PathForm::Windows`], a stem (the name without `.exe`) that is `git`,
/// starts with `git-`, or is `gate` or `pwsh`; under [`PathForm::Unix`], a name that is `git`, starts with `git-`, or
/// is `gate-box`.
#[must_use]
pub fn unreadable_counts(process: &ProcessInfo, form: PathForm) -> bool {
    let name = process.name.to_ascii_lowercase();
    let counted = match form {
        PathForm::Windows => {
            let stem = name.strip_suffix(".exe").unwrap_or(&name);
            matches!(stem, "git" | "gate" | "pwsh") || stem.starts_with("git-")
        }
        PathForm::Unix => matches!(name.as_str(), "git" | "gate-box") || name.starts_with("git-"),
    };
    process.command_line.is_none() && counted
}

/// What a counted process is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// A `gate box prune` of the root, or a process under one.
    Prune,
    /// A process naming the root, one whose command line cannot be read, or a process under one.
    Git,
    /// A process of a job a `{Work}.command` record names.
    Job,
}

impl Kind {
    /// The word its status line says.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Kind::Prune => "prune",
            Kind::Git => "git",
            Kind::Job => "job",
        }
    }
}

/// The processes of `processes` the drained test counts for the root `root` under `form`, by pid, but `own`, the box
/// itself: a prune of the root, a process whose command line names it ([`names_root`]) or cannot be read
/// ([`unreadable_counts`]), and every process under one of them ([`under`]).
///
/// With `readable_only`, a process whose command line cannot be read never counts.
#[must_use]
pub fn counted_processes(processes: Vec<ProcessInfo>, root: &str, own: u32, readable_only: bool, form: PathForm) -> BTreeMap<u32, Kind> {
    let tree = ProcessTree::new(processes);
    let mut by_parent: HashMap<u32, Vec<&ProcessInfo>> = HashMap::new();
    for process in tree.iter().filter(|process| process.pid != process.parent_pid) {
        by_parent.entry(process.parent_pid).or_default().push(process);
    }
    let mut found: BTreeMap<u32, Kind> = BTreeMap::new();
    for process in tree.iter() {
        let kind = match process.command_line.as_deref() {
            Some(line) if names_prune(line, root, form) => Kind::Prune,
            Some(line) if names_root(line, root, form) => Kind::Git,
            None if !readable_only && unreadable_counts(process, form) => Kind::Git,
            _ => continue,
        };
        for pid in std::iter::once(process.pid).chain(under(&by_parent, process)) {
            if pid != own {
                found.entry(pid).and_modify(|known| *known = (*known).min(kind)).or_insert(kind);
            }
        }
    }
    found
}

/// The pids under `top`, breadth first, each once, its children found in `by_parent`. A child counts unless both its
/// creation time and its parent's are read and it started before its parent, so it holds a pid the parent's real child
/// left; a time that cannot be read never hides a process, so the drained test fails closed.
fn under<'a>(by_parent: &HashMap<u32, Vec<&'a ProcessInfo>>, top: &'a ProcessInfo) -> Vec<u32> {
    let mut visited = HashSet::from([top.pid]);
    let mut found = Vec::new();
    let mut queue = VecDeque::from([top]);
    while let Some(parent) = queue.pop_front() {
        for &child in by_parent.get(&parent.pid).into_iter().flatten() {
            let reused = matches!((parent.created, child.created), (Some(parent_created), Some(created)) if created < parent_created);
            if !reused && visited.insert(child.pid) {
                found.push(child.pid);
                queue.push_back(child);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use proc_table::ProcessInfo;

    use super::{
        Barrier, Clock, Kind, LAPSE_MS, LEASE_MS, Raising, Refused, Ticket, barrier_of, counted_processes, decided, names_prune, names_root,
        older_live, refused_lines, ticket_live, ticket_name, ticket_of_name, ticket_seq, unreadable_counts,
    };
    use crate::PathForm;

    fn clock(now_s: u64) -> Clock {
        Clock {
            now: now_s * 1000,
            lease: LEASE_MS,
            lapse: LAPSE_MS,
        }
    }

    fn ticket(seq: u64, session: &str, polled: u64) -> Ticket {
        Ticket {
            seq,
            session: session.to_owned(),
            polled,
        }
    }

    #[test]
    fn a_new_ticket_is_one_past_the_highest_and_a_renewal_keeps_its_number() {
        let tickets = [ticket(3, "a", 0), ticket(7, "b", 0), ticket(5, "c", 0)];
        assert_eq!(ticket_seq(&[], "a"), 1, "the first ticket is number 1");
        assert_eq!(ticket_seq(&tickets, "new"), 8, "one more than the highest");
        assert_eq!(ticket_seq(&tickets, "c"), 5, "a renewal is never renumbered");
        assert_eq!(ticket_name(8, "new"), "00000008-new");
        assert_eq!(ticket_of_name("00000008-new-run"), Some((8, "new-run")));
        for other in ["8-new", "0000000x-new", "00000008-", "00000008-a.new-12", "lock"] {
            assert_eq!(ticket_of_name(other), None, "{other}");
        }
    }

    #[test]
    fn only_older_live_tickets_of_other_sessions_wait_ahead() {
        let tickets = [ticket(1, "a", 0), ticket(2, "dead", 0), ticket(3, "b", 0), ticket(4, "c", 0)];
        let live = |ticket: &Ticket| ticket.session != "dead";
        assert_eq!(older_live(&tickets, Some("b"), live), 1, "a waits ahead of b; dead does not");
        assert_eq!(older_live(&tickets, Some("a"), live), 0);
        assert_eq!(
            older_live(&tickets, Some("new"), live),
            3,
            "a session with no ticket waits behind every live one"
        );
    }

    #[test]
    fn a_ticket_dies_a_lease_after_the_latest_of_its_poll_the_boot_and_the_lift() {
        let now = 10_000;
        assert!(ticket_live(now - 119, 0, 0, clock(now)), "polled within the lease");
        assert!(!ticket_live(now - 120, 0, 0, clock(now)), "dead a whole lease after its poll");
        assert!(ticket_live(now - 500, now - 60, 0, clock(now)), "a boot since restarts its lease");
        assert!(ticket_live(now - 500, 0, now - 60, clock(now)), "a lift since restarts its lease");
        assert!(
            !ticket_live(now - 500, now - 300, now - 200, clock(now)),
            "dead a lease after the latest of all three"
        );
    }

    #[test]
    fn a_barrier_stands_until_its_lapse_then_lapses_at_it() {
        let text = r#"{"Raised":1000,"Lapses":2800,"Generation":3}"#;
        let raised = Raising {
            raised: 1000,
            lapses: 2800,
            generation: 3,
        };
        assert_eq!(barrier_of(text, 0, clock(2799)), Barrier::Standing(raised));
        assert_eq!(barrier_of(text, 0, clock(2800)), Barrier::Lapsed(2800));
    }

    #[test]
    fn an_unreadable_barrier_stands_until_its_last_write_plus_the_lapse() {
        let written = 5000;
        let standing = barrier_of("{not json", written, clock(written + 1799));
        assert_eq!(
            standing,
            Barrier::Standing(Raising {
                raised: written,
                lapses: written + 1800,
                generation: 0
            })
        );
        assert_eq!(barrier_of("", written, clock(written + 1800)), Barrier::Lapsed(written + 1800));
    }

    #[test]
    fn a_call_without_a_session_is_refused_under_a_standing_barrier_only() {
        let raised = Raising {
            raised: 10,
            lapses: 1810,
            generation: 1,
        };
        let refused = |place| Refused {
            place,
            raised: Some(10),
            lapses: Some(1810),
        };
        assert_eq!(decided(Some(raised), 0, None, false), Err(refused(None)), "no session under a barrier");
        assert_eq!(
            decided(Some(raised), 2, Some("s"), false),
            Err(refused(Some(3))),
            "a session waits its place"
        );
        assert_eq!(decided(None, 0, None, false), Ok(()), "no barrier and no session is admitted");
        assert_eq!(decided(None, 3, None, false), Ok(()), "a call with no session cannot queue");
        assert_eq!(
            decided(None, 1, Some("s"), false),
            Err(Refused {
                place: Some(2),
                raised: None,
                lapses: None
            }),
            "an older live ticket waits even with no barrier"
        );
        assert_eq!(decided(None, 0, Some("s"), false), Ok(()));
    }

    #[test]
    fn a_live_session_is_admitted_past_a_barrier_and_older_tickets_and_a_dead_one_is_not() {
        let raised = Raising {
            raised: 10,
            lapses: 1810,
            generation: 1,
        };
        assert_eq!(
            decided(Some(raised), 0, Some("s"), true),
            Ok(()),
            "a live session under a standing barrier"
        );
        assert_eq!(decided(None, 2, Some("s"), true), Ok(()), "a live session with older tickets waiting");
        assert_eq!(
            decided(Some(raised), 0, Some("s"), false),
            Err(Refused {
                place: Some(1),
                raised: Some(10),
                lapses: Some(1810)
            }),
            "a dead session under a standing barrier"
        );
    }

    #[test]
    fn the_root_is_named_only_on_a_path_boundary_with_separators_and_case_folded() {
        let root = r"D:\gate-remote";
        let windows = PathForm::Windows;
        assert!(names_root("git-receive-pack 'D:/gate-remote/x.git'", root, windows));
        assert!(names_root(r#"git.exe --git-dir="d:\GATE-REMOTE\work\k\.git" fetch"#, root, windows));
        assert!(names_root(r"tool D:\gate-remote", r"D:\gate-remote\", windows));
        assert!(!names_root(r"git-upload-pack D:\gate-remote-old\x.git", root, windows));
        assert!(!names_root(r"git-upload-pack XD:\gate-remote\x.git", root, windows));
        assert!(!names_root(r"git status", root, windows));
    }

    #[test]
    fn a_unix_prune_counts_only_as_a_box_prune() {
        let root = "/Users/builder/gate-remote";
        let unix = PathForm::Unix;
        assert!(names_prune(
            "/usr/local/bin/gate-box box prune --root /Users/builder/gate-remote",
            root,
            unix
        ));
        assert!(names_prune(
            "'/usr/local/bin/gate-box' BOX prune --root '/users/builder/gate-remote' --keys k",
            root,
            unix
        ));
        assert!(names_prune(
            "gate-box box --test-box-now 5 prune --root /Users/builder/gate-remote",
            root,
            unix
        ));
        assert!(!names_prune("gate-box box prune --root /Users/builder/gate-remote-old", root, unix));
        assert!(
            !names_prune("/usr/local/bin/gate-box prune --root /Users/builder/gate-remote", root, unix),
            "only a box prune, not any word holding box"
        );
        assert!(!names_prune("sandbox prune --root /Users/builder/gate-remote", root, unix));
    }

    #[test]
    fn a_prune_of_the_root_is_matched_with_separators_and_case_folded() {
        let root = r"D:\gate-remote";
        let windows = PathForm::Windows;
        assert!(names_prune(r"C:\gate\gate.exe box prune --root D:/Gate-Remote", root, windows));
        assert!(names_prune(
            r#""C:\gate\gate.exe" BOX prune --root "D:\gate-remote" --keys k"#,
            root,
            windows
        ));
        assert!(!names_prune(r"C:\gate\gate.exe box prune --root D:\gate-remote-old", root, windows));
        assert!(
            !names_prune(r"C:\gate\gate.exe prune --root D:\gate-remote", root, windows),
            "only a box prune"
        );
    }

    #[test]
    fn the_root_is_named_on_a_unix_path_boundary_only() {
        let root = "/Users/builder/gate-remote";
        let unix = PathForm::Unix;
        assert!(names_root("git-receive-pack '/Users/builder/gate-remote/x.git'", root, unix));
        assert!(names_root("tool /Users/builder/gate-remote", "/Users/builder/gate-remote/", unix));
        assert!(!names_root("git-upload-pack /Users/builder/gate-remote-old/x.git", root, unix));
        assert!(!names_root("git-upload-pack /x/Users/builder/gate-remote/x.git", root, unix));
        assert!(
            !names_root(r"git -C \Users\builder\gate-remote status", root, unix),
            "a \\ is no separator on a Unix box"
        );
        assert!(names_prune(
            "/usr/local/bin/gate-box box prune --root /Users/builder/gate-remote --keys k",
            root,
            unix
        ));
        assert!(!names_prune(
            "/usr/local/bin/gate-box box prune --root /Users/builder/gate-remote-old",
            root,
            unix
        ));
    }

    #[test]
    fn a_unix_root_is_matched_with_ascii_case_folded() {
        let root = "/Users/Builder/Gate-Remote";
        let unix = PathForm::Unix;
        assert!(names_root("git --git-dir=/users/builder/GATE-REMOTE/work/k/.git fetch", root, unix));
        assert!(names_prune("gate-box BOX prune --root '/USERS/builder/gate-remote'", root, unix));
    }

    fn process(pid: u32, parent: u32, name: &str, line: Option<&str>) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: parent,
            name: name.to_owned(),
            command_line: line.map(str::to_owned),
            created: Some(Timestamp::from_second(i64::from(pid)).unwrap_or(Timestamp::UNIX_EPOCH)),
        }
    }

    /// A process whose creation time cannot be read.
    fn untimed(pid: u32, parent: u32, name: &str, line: Option<&str>) -> ProcessInfo {
        ProcessInfo {
            created: None,
            ..process(pid, parent, name, line)
        }
    }

    #[test]
    fn an_unreadable_unix_command_counts_for_git_and_gate_box_only() {
        let counts = |name: &str| unreadable_counts(&process(7, 1, name, None), PathForm::Unix);
        for counted in ["git", "Git", "git-receive-pack", "git-upload-pack", "gate-box"] {
            assert!(counts(counted), "{counted}");
        }
        for left_out in ["gate", "pwsh", "git.exe", "gitk", "gate-box.exe", "sh"] {
            assert!(!counts(left_out), "{left_out}");
        }
        assert!(
            !unreadable_counts(&process(7, 1, "git", Some("git status")), PathForm::Unix),
            "a command line read is matched, not counted as unreadable"
        );
    }

    #[test]
    fn the_root_s_processes_their_children_and_unreadable_git_gate_or_pwsh_count() {
        let root = r"D:\gate-remote";
        let processes = vec![
            process(1, 0, "sshd.exe", Some("sshd")),
            process(10, 1, "git-receive-pack.exe", Some("git-receive-pack 'D:/gate-remote/k.git'")),
            process(11, 10, "git.exe", Some("git index-pack --stdin")),
            untimed(12, 10, "conhost.exe", Some("conhost")),
            process(5, 10, "old.exe", Some("started before its parent")),
            untimed(60, 1, "git.exe", Some("git -C D:/gate-remote/k.git gc")),
            process(61, 60, "git.exe", Some("git repack")),
            untimed(33, 1, "Git-Upload-Pack.EXE", None),
            untimed(34, 1, "gitk.exe", None),
            untimed(35, 1, "gate.exe", None),
            process(20, 1, "gate.exe", Some(r"gate.exe box prune --root D:\gate-remote")),
            process(21, 20, "git.exe", Some("git update-ref")),
            process(30, 1, "pwsh.exe", None),
            process(31, 1, "GIT.EXE", None),
            process(32, 1, "notepad.exe", None),
            process(40, 1, "git.exe", Some(r"git -C D:\gate-remote-old status")),
            process(50, 1, "gate.exe", Some(r"gate.exe box drain --stdin")),
        ];
        let readable: Vec<u32> = counted_processes(processes.clone(), root, 50, true, PathForm::Windows)
            .into_keys()
            .collect();
        assert_eq!(
            readable,
            [10, 11, 12, 20, 21, 60, 61],
            "the readable-only seam leaves out what cannot be read; a child counts whatever times are unknown"
        );
        let counted: Vec<(u32, Kind)> = counted_processes(processes, root, 50, false, PathForm::Windows).into_iter().collect();
        assert_eq!(
            counted,
            [
                (10, Kind::Git),
                (11, Kind::Git),
                (12, Kind::Git),
                (20, Kind::Prune),
                (21, Kind::Prune),
                (30, Kind::Git),
                (31, Kind::Git),
                (33, Kind::Git),
                (35, Kind::Git),
                (60, Kind::Git),
                (61, Kind::Git)
            ]
        );
    }

    #[test]
    fn a_unix_process_naming_the_root_or_a_prune_of_it_counts() {
        let root = "/Users/builder/gate-remote";
        let processes = vec![
            process(1, 0, "launchd", Some("/sbin/launchd")),
            process(10, 1, "sshd", Some("sshd: builder")),
            process(11, 10, "git-receive-pack", Some("git-receive-pack '/Users/builder/gate-remote/k.git'")),
            process(12, 11, "git", Some("git index-pack --stdin")),
            process(
                20,
                1,
                "gate-box",
                Some("/usr/local/bin/gate-box box prune --root /Users/builder/gate-remote"),
            ),
            process(21, 20, "git", Some("git update-ref -d refs/gate/k")),
            process(30, 1, "sh", Some("sh -c 'read line' sh /USERS/builder/Gate-Remote/work")),
            process(40, 1, "git", Some("git -C /Users/builder/gate-remote-old status")),
            process(41, 1, "git", Some(r"git -C C:\Users\builder\gate-remote status")),
            process(50, 1, "gate-box", Some("/usr/local/bin/gate-box box drain --stdin")),
        ];
        let counted: Vec<(u32, Kind)> = counted_processes(processes, root, 50, false, PathForm::Unix).into_iter().collect();
        assert_eq!(
            counted,
            [(11, Kind::Git), (12, Kind::Git), (20, Kind::Prune), (21, Kind::Prune), (30, Kind::Git)],
            "the root's own processes, their children, and a prune of it count; another root's and the box itself do not"
        );
    }

    #[test]
    fn an_unreadable_git_or_gate_box_process_counts_on_unix() {
        let root = "/Users/builder/gate-remote";
        let processes = vec![
            process(1, 0, "launchd", Some("/sbin/launchd")),
            process(10, 1, "git", None),
            process(11, 10, "cat", Some("cat")),
            process(12, 1, "git-upload-pack", None),
            process(13, 1, "gate-box", None),
            process(14, 1, "sh", None),
            process(15, 1, "gate", None),
            process(16, 1, "git.exe", None),
        ];
        let counted: Vec<u32> = counted_processes(processes.clone(), root, 99, false, PathForm::Unix)
            .into_keys()
            .collect();
        assert_eq!(
            counted,
            [10, 11, 12, 13],
            "an unreadable git, git-* or gate-box counts, with its children"
        );
        let readable: Vec<u32> = counted_processes(processes, root, 99, true, PathForm::Unix).into_keys().collect();
        assert!(readable.is_empty(), "the readable-only seam leaves them out: {readable:?}");
    }

    #[test]
    fn a_refusal_says_its_place_and_the_standing_barrier_or_none() {
        let refused = |place, barrier: Option<(u64, u64)>| Refused {
            place,
            raised: barrier.map(|(raised, _)| raised),
            lapses: barrier.map(|(_, lapses)| lapses),
        };
        assert_eq!(
            refused_lines(refused(Some(2), Some((100, 1900)))),
            ["gate: drain place 2", "gate: drain barrier 100 1900"]
        );
        assert_eq!(
            refused_lines(refused(None, Some((100, 1900)))),
            ["gate: drain place none", "gate: drain barrier 100 1900"]
        );
        assert_eq!(refused_lines(refused(Some(1), None)), ["gate: drain place 1", "gate: drain barrier none"]);
    }
}
