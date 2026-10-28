//! `gate-box box drain`: the builder's drain barrier on a Unix box, the twin of the Windows box's `gate box drain`,
//! deciding by the same rules ([`gate_wire::drain`]). Everything lives in `{root}/drain/`, the box user's own folder at
//! mode 0700, which it readies with its `tickets/` and `sessions/` folders, a planted `drain` moved aside:
//!
//! - `lock`, an `flock` held around each read-modify of the files below and never across a wait or a child process;
//! - `barrier` (`{"Raised","Lapses","Generation"}`), which stands until it is lifted or lapses, then `lapsed` (`{"At"}`)
//!   once it lapsed and `lifted` (`{"At"}`) once it was lifted;
//! - `tickets/{seq:08}-{session}` (`{"Session","Polled"}`), a refused session's place in line;
//! - `sessions/{session}` (`{"Pc","Repo","Command","Job","Called"}`), an admitted session's record, and
//!   `sessions/{session}.{pid}`, its holder, a file the admitted call holds an `flock` on until it ends.
//!
//! The preflight, `run` and `inputs` are admitted ([`admit`]) or refused, and `outputs` renews its session
//! ([`renewed_at`]); a status removes the dead tickets, holders and session records it finds. It reads "drained" only
//! with the barrier standing, no live session (a session naming a job is live while the work folder's records hold
//! that job live, [`job_live`]) and no process of the box's user counted for the root ([`counted_here`]); the prune reads
//! the barrier alone ([`barrier_stands`]).
//!
//! The routed command runs as the box's own user, so no mode keeps it from these files, as none keeps it from the rest
//! of the work root.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use gate_args::BoxSeams;
use gate_wire::drain::{
    BARRIER, Barrier, Clock, DRAIN, Kind, LAPSED, LIFTED, LOCK, LOCK_RETRY, LOCK_STEP, LiveSession, MOST_COMMAND_CHARS, MOST_NAME_CHARS, ProcessInfo,
    Raising, Record, Refused, SESSIONS, TICKETS, Ticket, at_of, barrier_line, barrier_of, counted_processes, decided, names_prune, older_live,
    refused_lines, session_line, state_line, ticket_live, ticket_name, ticket_of_name, ticket_seq, tickets_line, unix_s,
};
use gate_wire::{DrainAction, PathForm, Payload, escaped, is_session_id, refused_line};
use serde_json::{Map, Value, json};

use crate::console::Console;
use crate::files::{self, HeldLock, Locked, RECORD_LIMIT};
use crate::lines::{FAILED_STATUS, JobRecord, USAGE_STATUS, job_record, recorded_group};
use crate::preflight::{self, Refusal};
use crate::process_scan;
use crate::prune::group_current;
use crate::run::REFUSED_STATUS;
use crate::stop::records_folder;

/// The most names `drain.gate-moved-{n}` a planted `drain` is tried at.
const MOVED_ASIDE_LIMIT: u32 = 100;

/// Answers `gate-box box drain`: raises, reads or lifts the barrier under the payload's `Root` as its `Action` says,
/// then says how the drain stands, one line each on stdout ([`State::status`]). The status: 0 once it answered,
/// [`FAILED_STATUS`] after `gate: drain failed: {why}`, or [`USAGE_STATUS`] after the usage line for a payload without
/// a `Root` path or an `Action`.
pub(crate) fn drain(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u8 {
    let read = payload.path("Root").and_then(|root| Ok((root, payload.drain_action()?)));
    let (root, action) = match read {
        Ok(read) => read,
        Err(line) => {
            console.err(&line);
            return USAGE_STATUS;
        }
    };
    match answered(root, action, seams, console) {
        Ok(lines) => {
            for line in &lines {
                console.out(line);
            }
            0
        }
        Err(why) => {
            console.err(&format!("gate: drain failed: {why}"));
            FAILED_STATUS
        }
    }
}

/// The lines answering `action` on the root `root`. A raise makes the root as the preflight makes it
/// ([`preflight::trusted_work_root`]) and `drain/` when they are missing; a status or a lift of a root or `drain/` that
/// is not there reads as no barrier and changes nothing. A root that is there is judged as the preflight judges it.
fn answered(root: &str, action: DrainAction, seams: &BoxSeams, console: &mut Console) -> Result<Vec<String>, String> {
    let root_path = Path::new(root);
    if action != DrainAction::Raise && !files::plain_folder(root_path) {
        return Ok(open_lines());
    }
    preflight::trusted_work_root(root).map_err(|refusal| match refusal {
        // The refusal is a whole line of its own; the drain says it as its reason, without its `gate: `.
        Refusal::Folder(line) | Refusal::Untrusted(line) => line.strip_prefix("gate: ").map_or_else(|| line.clone(), str::to_owned),
    })?;
    let drain = root_path.join(DRAIN);
    if action != DrainAction::Raise && !files::exists(&drain) {
        return Ok(open_lines());
    }
    ready_drain(&drain, console)?;
    let _lock = take_drain_lock(&drain.join(LOCK), LOCK_RETRY)?;
    let state = State {
        drain: &drain,
        clock: Clock::of(seams),
    };
    match action {
        DrainAction::Raise => state.raise()?,
        DrainAction::Lift => state.lift()?,
        DrainAction::Status => {}
    }
    state.status(root_path, seams.drain_readable_only == gate_args::ProcessScan::ReadableOnly)
}

/// The lines of a root with no `drain/`: no barrier, no ticket, open.
fn open_lines() -> Vec<String> {
    vec![barrier_line(Barrier::None), tickets_line(0), state_line(Barrier::None, false)]
}

/// A file or folder as itself: its device and inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
}

impl Identity {
    fn of(metadata: &fs::Metadata) -> Identity {
        Identity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

/// What is at the name of one of the drain's folders, looked at as itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    /// Nothing.
    Missing,
    /// The box user's own folder at mode 0700.
    Own,
    /// Anything else, this one: a link, a file, another user's folder, or a folder of another mode.
    Planted(Identity),
}

/// What is at `path` ([`Found`]), judged by [`files::unusable_folder`] and its mode, a link never followed; else why it
/// could not be looked at.
fn found(path: &Path) -> Result<Found, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Found::Missing),
        Err(error) => Err(cannot("read", path, &error)),
        Ok(metadata) if files::unusable_folder(path, true).is_some() || metadata.mode() & 0o7777 != 0o700 => {
            Ok(Found::Planted(Identity::of(&metadata)))
        }
        Ok(_) => Ok(Found::Own),
    }
}

/// How many more times a planted `drain` that changed before its move is judged again.
const JUDGE_RETRIES: u32 = 3;

/// The step of [`ready_drain_seen`] its hook is called before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The move of a planted `drain` aside, once it was judged planted.
    Move,
    /// The making of a folder found missing.
    Make,
}

/// `drain/` at `drain` and its `tickets/` and `sessions/`, readied as the box user's own folders at mode 0700, each made
/// when missing ([`readied_folder`]). Anything planted at `drain` is first renamed aside as itself, never followed, to
/// `drain.gate-moved-{n}` ([`planted_set_aside`]), and said on `console`; anything planted at a folder within it is
/// refused. Else why not.
fn ready_drain(drain: &Path, console: &mut Console) -> Result<(), String> {
    ready_drain_seen(drain, console, &mut |_, _| {})
}

/// [`ready_drain`], calling `between` with the step and its path before each move aside and each making.
fn ready_drain_seen(drain: &Path, console: &mut Console, between: &mut dyn FnMut(Step, &Path)) -> Result<(), String> {
    planted_set_aside(drain, console, between)?;
    for folder in [drain.to_path_buf(), drain.join(TICKETS), drain.join(SESSIONS)] {
        readied_folder(&folder, between)?;
    }
    Ok(())
}

/// Moves a planted `drain` aside ([`moved_aside`]) and says so on `console`. One that changed between its judgement
/// and its move, moved or replaced by another call, is judged again, up to [`JUDGE_RETRIES`] more times, and never
/// moved as it was judged. Else why not.
fn planted_set_aside(drain: &Path, console: &mut Console, between: &mut dyn FnMut(Step, &Path)) -> Result<(), String> {
    for _ in 0..=JUDGE_RETRIES {
        let Found::Planted(planted) = found(drain)? else {
            return Ok(());
        };
        between(Step::Move, drain);
        if let Some(moved) = moved_aside(drain, planted)? {
            console.err(&format!(
                "gate: drain moved aside {} to {}: it was not the box's protected folder",
                escaped(&drain.to_string_lossy()),
                escaped(&moved)
            ));
            return Ok(());
        }
    }
    Err(format!(
        "{} changed each of the {} times it was judged",
        escaped(&drain.to_string_lossy()),
        JUDGE_RETRIES + 1
    ))
}

/// The folder `folder`, made at mode 0700 when missing. A folder another call made meanwhile is judged again and
/// adopted when it is the box user's own at mode 0700, as one found there is; anything else there is refused.
fn readied_folder(folder: &Path, between: &mut dyn FnMut(Step, &Path)) -> Result<(), String> {
    let not_own = || format!("{} is not the box user's own folder at mode 0700", escaped(&folder.to_string_lossy()));
    match found(folder)? {
        Found::Own => Ok(()),
        Found::Planted(_) => Err(not_own()),
        Found::Missing => {
            between(Step::Make, folder);
            match DirBuilder::new().mode(0o700).create(folder) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match found(folder)? {
                    Found::Own => Ok(()),
                    Found::Planted(_) => Err(not_own()),
                    Found::Missing => Err(cannot("make", folder, &error)),
                },
                Err(error) => Err(cannot("make", folder, &error)),
            }
        }
    }
}

/// Moves `planted`, judged at `drain`, aside beside it, as itself, to `drain.gate-moved-{n}` for the first `n` up to
/// [`MOVED_ASIDE_LIMIT`] nothing has; its new name. `None` when `drain` no longer holds `planted`, looked at just before
/// the rename, or holds nothing at the rename: another call moved it. Else why not.
fn moved_aside(drain: &Path, planted: Identity) -> Result<Option<String>, String> {
    let shown = escaped(&drain.to_string_lossy());
    for n in 1..=MOVED_ASIDE_LIMIT {
        let moved = format!("{DRAIN}.gate-moved-{n}");
        let target = drain.with_file_name(&moved);
        if files::exists(&target) {
            continue;
        }
        match fs::symlink_metadata(drain) {
            Ok(metadata) if Identity::of(&metadata) == planted => {}
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(cannot("read", drain, &error)),
            _ => return Ok(None),
        }
        match fs::rename(drain, &target) {
            Ok(()) => return Ok(Some(moved)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if matches!(error.raw_os_error(), Some(libc::EEXIST | libc::ENOTEMPTY | libc::ENOTDIR)) => {}
            Err(error) => return Err(format!("cannot move aside {shown}: {}", escaped(&error.to_string()))),
        }
    }
    Err(format!(
        "cannot move aside {shown}: {DRAIN}.gate-moved-1 to {DRAIN}.gate-moved-{MOVED_ASIDE_LIMIT} are all taken"
    ))
}

/// The drain lock `path`, taken as the work lock is ([`files::take_lock`]); a lock another open holds is tried again
/// every [`LOCK_STEP`] until `retry` has passed, [`LOCK_RETRY`] for every call but a test's. Else why not.
pub(crate) fn take_drain_lock(path: &Path, retry: Duration) -> Result<HeldLock, String> {
    let begun = Instant::now();
    loop {
        match files::take_lock(path) {
            Ok(Locked::Held(held)) => return Ok(held),
            Ok(Locked::Busy) => {
                if begun.elapsed() >= retry {
                    return Err(format!("the drain lock {} is held by another process", escaped(&path.to_string_lossy())));
                }
                thread::sleep(LOCK_STEP);
            }
            Err(error) => return Err(cannot("take", path, &error)),
        }
    }
}

/// Why the box could not `what` the file or folder `path`.
pub(crate) fn cannot(what: &str, path: &Path, error: &io::Error) -> String {
    format!("cannot {what} {}: {}", escaped(&path.to_string_lossy()), escaped(&error.to_string()))
}

/// The drain's files under one hold of its lock.
struct State<'a> {
    drain: &'a Path,
    clock: Clock,
}

impl State<'_> {
    /// How the barrier stands; one found lapsed is replaced by its `lapsed` record.
    fn barrier(&self) -> Result<Barrier, String> {
        if let Some((text, written)) = read_text(&self.drain.join(BARRIER))? {
            match barrier_of(&text, written, self.clock) {
                Barrier::Lapsed(at) => {
                    self.write(LAPSED, &json!({ "At": at }))?;
                    removed(&self.drain.join(BARRIER))?;
                }
                standing => return Ok(standing),
            }
        }
        match read_text(&self.drain.join(LAPSED))? {
            Some((text, written)) => Ok(Barrier::Lapsed(at_of(&text, written))),
            None => Ok(Barrier::None),
        }
    }

    /// Raises the barrier now, lapsing a lapse from now, one generation on from one that stood.
    fn raise(&self) -> Result<(), String> {
        let generation = match self.barrier()? {
            Barrier::Standing(raised) => raised.generation.saturating_add(1),
            Barrier::None | Barrier::Lapsed(_) => 1,
        };
        let now = self.clock.now_s();
        let lapses = (self.clock.now.saturating_add(self.clock.lapse)) / 1000;
        self.write(BARRIER, &json!({ "Raised": now, "Lapses": lapses, "Generation": generation }))
    }

    /// Lifts the barrier, recording the lift's time.
    fn lift(&self) -> Result<(), String> {
        for name in [BARRIER, LAPSED] {
            removed(&self.drain.join(name))?;
        }
        self.write(LIFTED, &json!({ "At": self.clock.now_s() }))
    }

    /// The Unix second of the last lift or lapse, 0 for none.
    fn lift_time(&self) -> Result<u64, String> {
        let mut latest = 0;
        for name in [LAPSED, LIFTED] {
            if let Some((text, written)) = read_text(&self.drain.join(name))? {
                latest = latest.max(at_of(&text, written));
            }
        }
        Ok(latest)
    }

    /// The barrier's raising while one stands ([`State::barrier`]); `None` while none does.
    fn standing(&self) -> Result<Option<Raising>, String> {
        Ok(match self.barrier()? {
            Barrier::Standing(raised) => Some(raised),
            Barrier::None | Barrier::Lapsed(_) => None,
        })
    }

    /// The tickets, in no order; a ticket that does not read was polled at its last write.
    fn tickets(&self) -> Result<Vec<Ticket>, String> {
        let folder = self.drain.join(TICKETS);
        let mut found = Vec::new();
        for name in names_in(&folder)? {
            let Some((seq, session)) = ticket_of_name(&name) else {
                continue;
            };
            let Some((text, written)) = read_text(&folder.join(&name))? else {
                continue;
            };
            let polled = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value.get("Polled")?.as_u64())
                .unwrap_or(written);
            found.push(Ticket {
                seq,
                session: session.to_owned(),
                polled,
            });
        }
        Ok(found)
    }

    /// The live tickets, in no order; each dead one ([`ticket_live`]) is removed in this same hold, so a session whose
    /// ticket died queues again at the back.
    fn live_tickets_in(&self) -> Result<Vec<Ticket>, String> {
        let tickets = self.tickets()?;
        // Read only once a ticket is found, so a drain with none never needs the boot time.
        if tickets.is_empty() {
            return Ok(tickets);
        }
        let (boot, lift) = (boot_s()?, self.lift_time()?);
        let folder = self.drain.join(TICKETS);
        let mut live = Vec::new();
        for ticket in tickets {
            if ticket_live(ticket.polled, boot, lift, self.clock) {
                live.push(ticket);
            } else {
                removed(&folder.join(ticket_name(ticket.seq, &ticket.session)))?;
            }
        }
        Ok(live)
    }

    /// How many live tickets wait, each dead one removed ([`State::live_tickets_in`]).
    fn live_tickets(&self) -> Result<usize, String> {
        Ok(self.live_tickets_in()?.len())
    }

    /// Whether the session `id` is live for a call of its own: a holder of it held, or its record called within the
    /// lease. Each dead holder of it met is removed.
    fn own_session_live(&self, id: &str) -> Result<bool, String> {
        let folder = self.drain.join(SESSIONS);
        for name in names_in(&folder)? {
            if holder_of(&name) == Some(id) && holder_live(&folder.join(&name))? {
                return Ok(true);
            }
        }
        let record = read_text(&folder.join(id))?;
        Ok(record.is_some_and(|(text, written)| self.clock.within_lease(Record::of(&text, written).called)))
    }

    /// The live sessions, in the order of their ids ([`State::session`]), their jobs judged by the records in the work
    /// folder `work`, each dead one's record and every dead holder removed.
    fn sessions(&self, work: &Path) -> Result<Vec<LiveSession>, String> {
        let folder = self.drain.join(SESSIONS);
        let mut ids = BTreeSet::new();
        let mut held = BTreeSet::new();
        for name in names_in(&folder)? {
            if is_session_id(&name) {
                ids.insert(name);
            } else if let Some(id) = holder_of(&name) {
                ids.insert(id.to_owned());
                if holder_live(&folder.join(&name))? {
                    held.insert(id.to_owned());
                }
            }
        }
        let mut live = Vec::new();
        for id in &ids {
            if let Some(session) = self.session(&folder, id, held.contains(id), work)? {
                live.push(session);
            }
        }
        Ok(live)
    }

    /// The session `id` in the sessions folder `folder` when it is live, judged in this order and the job looked at only
    /// when nothing before made it live: `held`, its record called within the lease, or its record naming a job the
    /// work folder `work` holds live ([`job_live`]). A dead one's record is removed.
    fn session(&self, folder: &Path, id: &str, held: bool, work: &Path) -> Result<Option<LiveSession>, String> {
        let path = folder.join(id);
        let record = read_text(&path)?.map(|(text, written)| Record::of(&text, written));
        let live = held
            || record.as_ref().is_some_and(|record| self.clock.within_lease(record.called))
            || match record.as_ref().and_then(|record| record.job.as_deref()) {
                Some(job) => job_live(work, job)?,
                None => false,
            };
        if !live {
            removed(&path)?;
            return Ok(None);
        }
        let now = self.clock.now_s();
        Ok(Some(match record {
            Some(record) => LiveSession {
                age: now.saturating_sub(record.called),
                pc: record.pc,
                repo: record.repo,
                command: record.command,
                job: record.job,
            },
            None => LiveSession {
                age: 0,
                pc: None,
                repo: None,
                command: None,
                job: None,
            },
        }))
    }

    /// Registers the admitted call of `session`: its record written, the command and job a call naming none kept from
    /// the record, its holder held from now on, its end stamped at `--test-box-now`'s second `test_now` when given, and
    /// its tickets among `tickets` given up.
    fn registered(&self, tickets: &[Ticket], session: &Session<'_>, test_now: Option<u64>) -> Result<Admitted, String> {
        let path = self.drain.join(SESSIONS).join(session.id);
        let kept = record_fields(&path)?;
        // A call that names no command or job (a preflight, an `inputs` call) keeps the ones its run recorded.
        let given_or_kept = |given: &str, key: &str| -> String {
            if given.is_empty() {
                kept.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
            } else {
                given.to_owned()
            }
        };
        let record = json!({
            "Pc": capped(session.pc, MOST_NAME_CHARS),
            "Repo": capped(session.repo, MOST_NAME_CHARS),
            "Command": capped(&given_or_kept(session.command, "Command"), MOST_COMMAND_CHARS),
            "Job": given_or_kept(session.job, "Job"),
            "Called": self.clock.now_s(),
        });
        written(&path, &record)?;
        let holder = self.holding(session.id, test_now)?;
        let folder = self.drain.join(TICKETS);
        for ticket in tickets.iter().filter(|ticket| ticket.session == session.id) {
            removed(&folder.join(ticket_name(ticket.seq, &ticket.session)))?;
        }
        Ok(Admitted { holder: Some(holder) })
    }

    /// Sets the live session `id`'s record's `Called` to now, keeping its other fields (none for a record that does not
    /// read), and holds its holder, its end stamped at `test_now` when given.
    fn renew(&self, id: &str, test_now: Option<u64>) -> Result<Admitted, String> {
        let path = self.drain.join(SESSIONS).join(id);
        let mut record = record_fields(&path)?;
        record.insert("Called".to_owned(), json!(self.clock.now_s()));
        written(&path, &Value::Object(record))?;
        Ok(Admitted {
            holder: Some(self.holding(id, test_now)?),
        })
    }

    /// The hold of the session `id`: this process's holder `sessions/{id}.{pid}` taken as a lock is
    /// ([`files::take_lock`]), so no command the box starts inherits it, and its end stamped at `test_now` when given.
    fn holding(&self, id: &str, test_now: Option<u64>) -> Result<Holder, String> {
        let sessions = self.drain.join(SESSIONS);
        let path = sessions.join(format!("{id}.{}", std::process::id()));
        let held = match files::take_lock(&path) {
            Ok(Locked::Held(held)) => held,
            Ok(Locked::Busy) => return Err(format!("cannot hold {}: another open holds it", escaped(&path.to_string_lossy()))),
            Err(error) => return Err(cannot("hold", &path, &error)),
        };
        Ok(Holder {
            held,
            record: sessions.join(id),
            lock: self.drain.join(LOCK),
            test_now,
        })
    }

    /// The status lines of the work root `root`, in their order: the barrier, each live session, each counted process
    /// ([`counted_here`], only those whose command line was read when `readable_only`) and each group a work folder's
    /// record holds running ([`running_groups`], `job`), each group record that cannot be read, the live tickets, and
    /// the state.
    ///
    /// Processes that could not be listed get a line of their own, `gate: drain processes unknown: {why}`, a group
    /// record that cannot be read or holds no group `gate: drain record {name} unreadable`, and either keeps the drain
    /// waiting.
    fn status(&self, root: &Path, readable_only: bool) -> Result<Vec<String>, String> {
        let barrier = self.barrier()?;
        let work = root.join(WORK);
        let sessions = self.sessions(&work)?;
        let (groups, unreadable) = running_groups(&work)?;
        let mut lines = vec![barrier_line(barrier)];
        lines.extend(sessions.iter().map(session_line));
        let mut processes: BTreeMap<u32, Kind> = groups.into_iter().map(|group| (group, Kind::Job)).collect();
        let scanned = match counted_here(root, readable_only) {
            Ok(counted) => {
                for (pid, kind) in counted {
                    processes.entry(pid).and_modify(|known| *known = (*known).min(kind)).or_insert(kind);
                }
                true
            }
            Err(why) => {
                lines.push(format!("gate: drain processes unknown: {}", escaped(&why)));
                false
            }
        };
        lines.extend(processes.iter().map(|(pid, kind)| format!("gate: drain process {pid} {}", kind.word())));
        lines.extend(unreadable.iter().map(|name| format!("gate: drain record {} unreadable", escaped(name))));
        lines.push(tickets_line(self.live_tickets()?));
        let quiet = scanned && processes.is_empty() && unreadable.is_empty();
        lines.push(state_line(barrier, sessions.is_empty() && quiet));
        Ok(lines)
    }

    /// Writes `value` as the drain's file `name`, whole ([`written`]).
    fn write(&self, name: &str, value: &Value) -> Result<(), String> {
        written(&self.drain.join(name), value)
    }
}

/// Writes `value` as the drain's file `path`, whole ([`files::replace_plain`]). Else why not.
fn written(path: &Path, value: &Value) -> Result<(), String> {
    files::replace_plain(path, value.to_string().as_bytes())
        .map_err(|failure| format!("cannot write {}: {}", escaped(&path.to_string_lossy()), escaped(&failure.to_string())))
}

/// The fields of the session record `path`; none for a record that is missing or is not a JSON object.
fn record_fields(path: &Path) -> Result<Map<String, Value>, String> {
    Ok(read_text(path)?
        .and_then(|(text, _)| serde_json::from_str(&text).ok())
        .unwrap_or_default())
}

/// The first `most` characters of `text`.
fn capped(text: &str, most: usize) -> String {
    text.chars().take(most).collect()
}

/// The work folders' folder beneath the work root.
const WORK: &str = "work";

/// The extensions of a run's group records, longest first, since `.tools.pgid` also ends in `.pgid`, each with what
/// its record's job adds to the run's job: its git step's `{key}.pgid` the job itself, its tool restore's
/// `{key}.tools.pgid` `{job}-tools`, and its package restore's `{key}.packages.pgid` `{job}-packages`.
const GROUP_JOBS: [(&str, &str); 3] = [(".packages.pgid", "-packages"), (".tools.pgid", "-tools"), (".pgid", "")];

/// What the drained test reads of one group record in the work folder.
struct GroupRecord {
    /// Its file name.
    name: String,
    /// What its job adds to the run's job ([`GROUP_JOBS`]).
    suffix: &'static str,
    /// Its bytes and its last write; `None` when it could not be read, a link or anything but a plain file at its name
    /// included.
    read: Option<(Vec<u8>, SystemTime)>,
}

/// The group records in the work folder `work` ([`GROUP_JOBS`]), in no order; none when it is not there. Else why it
/// could not be listed.
fn group_records(work: &Path) -> Result<Vec<GroupRecord>, String> {
    if !files::exists(work) {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for name in names_in(work)? {
        let Some(suffix) = GROUP_JOBS
            .iter()
            .find(|(extension, _)| name.ends_with(extension))
            .map(|&(_, suffix)| suffix)
        else {
            continue;
        };
        let read = match files::read_plain_dated(&work.join(&name), RECORD_LIMIT) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => None,
            Ok(read) => Some(read),
        };
        found.push(GroupRecord { name, suffix, read });
    }
    Ok(found)
}

/// Whether the session job `job` keeps its session live by the group records in the work folder `work`
/// ([`group_records`]): one naming its job, `job` and the extension's suffix, and a group that is still its run's
/// ([`group_current`]). A group record that cannot be read, and one of its job that is malformed, count as live, so the
/// drained test fails closed. The interrupted-command record is not read: it outlives every run that did not end clean,
/// so it says nothing of what runs. Else why the work folder could not be listed.
fn job_live(work: &Path, job: &str) -> Result<bool, String> {
    for record in group_records(work)? {
        let Some((bytes, written)) = record.read else {
            return Ok(true);
        };
        let live = match job_record(&bytes, &format!("{job}{}", record.suffix)) {
            JobRecord::Other => false,
            JobRecord::Malformed => true,
            JobRecord::Group(recorded) => group_current(recorded, written),
        };
        if live {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What the group records in the work folder `work` hold running, whatever their job, so a run that named no session
/// holds the drain too: the groups still their runs' ([`group_current`]), and the names of the records that cannot be
/// read or hold no group. Else why the work folder could not be listed.
fn running_groups(work: &Path) -> Result<(Vec<u32>, Vec<String>), String> {
    let (mut groups, mut unreadable) = (Vec::new(), Vec::new());
    for record in group_records(work)? {
        match record.read.and_then(|(bytes, written)| Some((recorded_group(&bytes)?, written))) {
            None => unreadable.push(record.name),
            Some((recorded, written)) => {
                if group_current(recorded, written) {
                    groups.push(recorded.group.unsigned_abs());
                }
            }
        }
    }
    Ok((groups, unreadable))
}

/// The processes of the box's user the drained test counts for the work root `root` ([`counted_processes`]), this
/// process left out, its command line matched against the root as given and, when it differs, as its canonical path,
/// so a process naming the root through a link counts too. Else why the processes could not be listed.
fn counted_here(root: &Path, readable_only: bool) -> Result<BTreeMap<u32, Kind>, String> {
    let mut spellings = vec![root.to_string_lossy().into_owned()];
    // A root that cannot be resolved is still matched as given.
    if let Ok(real) = fs::canonicalize(root) {
        let real = real.to_string_lossy().into_owned();
        if !spellings.contains(&real) {
            spellings.push(real);
        }
    }
    let own = std::process::id();
    let processes = without_box_calls(process_scan::own_processes()?, &spellings, own);
    let mut counted: BTreeMap<u32, Kind> = BTreeMap::new();
    for spelling in &spellings {
        for (pid, kind) in counted_processes(processes.clone(), spelling, own, readable_only, PathForm::Unix) {
            counted.entry(pid).and_modify(|known| *known = (*known).min(kind)).or_insert(kind);
        }
    }
    Ok(counted)
}

/// `processes` without the box's own calls, which never hold the drain: each process whose command line runs the box
/// at `{root}/bin/gate-box`, for a spelling of the root in `roots`, unless it is a prune of the root ([`names_prune`]),
/// and each ancestor of `own`, the drain call itself, found by parent pids. The walk stops at a parent that started
/// after its child, which holds a pid the real parent left. A process whose command line cannot be read is kept. A
/// process left out leaves its children in: [`counted_processes`] still counts each one whose own command line names
/// the root.
fn without_box_calls(processes: Vec<ProcessInfo>, roots: &[String], own: u32) -> Vec<ProcessInfo> {
    let by_pid: HashMap<u32, &ProcessInfo> = processes.iter().map(|process| (process.pid, process)).collect();
    let mut ancestors = HashSet::new();
    let mut at = own;
    while let Some(child) = by_pid.get(&at) {
        let Some(parent) = by_pid.get(&child.parent_pid) else {
            break;
        };
        let reused = matches!((parent.created, child.created), (Some(parent_start), Some(child_start)) if parent_start > child_start);
        if parent.pid == at || reused || !ancestors.insert(parent.pid) {
            break;
        }
        at = parent.pid;
    }
    processes
        .into_iter()
        .filter(|process| !ancestors.contains(&process.pid) && !process.command_line.as_deref().is_some_and(|line| box_call(line, roots)))
        .collect()
}

/// Whether the command line `line` runs the box at `{root}/bin/gate-box` for a root of `roots`, ASCII case folded, and
/// is a prune of none of them, so a prune whose program and `--root` spell the root differently is still no box call.
fn box_call(line: &str, roots: &[String]) -> bool {
    let folded = line.to_ascii_lowercase();
    let runs_box = roots.iter().any(|root| {
        let program = format!("{}/bin/gate-box", root.trim_end_matches('/')).to_ascii_lowercase();
        folded.strip_prefix(&program).is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    });
    runs_box && !roots.iter().any(|root| names_prune(line, root, PathForm::Unix))
}

/// Whether a drain barrier stands on the work root `root` at `clock`, read only and with no lock, as the prune reads
/// it. None stands under a root that is not a plain folder, with no `drain/`, or with anything at `drain` but the box
/// user's own folder at mode 0700 ([`found`]), which no call of the box made; a lapsed one stands no more. Else why the
/// barrier could not be read.
pub(crate) fn barrier_stands(root: &Path, clock: Clock) -> Result<bool, String> {
    let drain = root.join(DRAIN);
    if !files::plain_folder(root) || found(&drain)? != Found::Own {
        return Ok(false);
    }
    let read = read_text(&drain.join(BARRIER))?;
    Ok(read.is_some_and(|(text, written)| matches!(barrier_of(&text, written, clock), Barrier::Standing(_))))
}

/// The session a holder's file name `{session}.{pid}` names; `None` for any other name.
fn holder_of(name: &str) -> Option<&str> {
    let (id, pid) = name.split_once('.')?;
    (is_session_id(id) && !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit())).then_some(id)
}

/// Whether the holder `path` is held by a live process: another open holds its `flock`. One whose `flock` this probe
/// takes is dead and is removed. Else why it could not be looked at.
pub(crate) fn holder_live(path: &Path) -> Result<bool, String> {
    match files::take_lock(path) {
        Ok(Locked::Busy) => Ok(true),
        Ok(Locked::Held(probe)) => {
            removed(path)?;
            drop(probe);
            Ok(false)
        }
        Err(error) => Err(cannot("read", path, &error)),
    }
}

/// A routed call's drain session, as its payload names it.
pub(crate) struct Session<'p> {
    pub(crate) id: &'p str,
    pub(crate) pc: &'p str,
    pub(crate) repo: &'p str,
    pub(crate) command: &'p str,
    pub(crate) job: &'p str,
}

/// Runs `under` in one hold of the drain lock under the work root `root`, `drain/` readied as an admission readies it,
/// giving it the judge of whether a session is live for a call of its own ([`State::own_session_live`]): the one lock
/// every take and judge of the builder's own slots runs under. Else why the drain could not be read or written, or
/// `under`'s own error.
pub(crate) fn under_drain_lock<T>(
    root: &Path,
    seams: &BoxSeams,
    console: &mut Console,
    under: impl FnOnce(&dyn Fn(&str) -> Result<bool, String>) -> Result<T, String>,
) -> Result<T, String> {
    let drain = root.join(DRAIN);
    ready_drain(&drain, console)?;
    let _lock = take_drain_lock(&drain.join(LOCK), LOCK_RETRY)?;
    let state = State {
        drain: &drain,
        clock: Clock::of(seams),
    };
    under(&|id| state.own_session_live(id))
}

/// The folder `folder`, readied as one of the drain's own: made at mode 0700 when missing, and refused unless it is the
/// box user's own folder at mode 0700, a link never followed ([`readied_folder`]). Else why not.
pub(crate) fn private_folder(folder: &Path) -> Result<(), String> {
    readied_folder(folder, &mut |_, _| {})
}

/// The drain session `payload` names (`Session`, `Pc`, `Repo`, the last two empty when absent) for a call of `command`
/// in the job `job`; `None` when it names none. Else the usage line.
pub(crate) fn session_of<'p>(payload: &'p Payload, command: &'p str, job: &'p str) -> Result<Option<Session<'p>>, String> {
    let Some(id) = payload.session()? else {
        return Ok(None);
    };
    Ok(Some(Session {
        id,
        pc: payload.pc()?.unwrap_or(""),
        repo: payload.repo()?.unwrap_or(""),
        command,
        job,
    }))
}

/// An admitted call's hold of its session ([`Holder`]); none for a call with no session, or under a root with no
/// drain. Dropping it stamps the session's end on its record, then removes and lets go of the holder.
pub(crate) struct Admitted {
    holder: Option<Holder>,
}

impl Admitted {
    /// The guard of a call admitted holding nothing.
    fn none() -> Admitted {
        Admitted { holder: None }
    }
}

/// The holder `sessions/{session}.{pid}`, its `flock` held, which marks the session live while the process lives, and
/// what the end of the call needs to rewrite the session's record: the record, the drain's lock, and the
/// `--test-box-now` second the end is stamped at, else the clock's.
struct Holder {
    held: HeldLock,
    record: PathBuf,
    lock: PathBuf,
    test_now: Option<u64>,
}

impl Holder {
    /// Rewrites the session's record with `Called` the end's second, every other field kept ([`record_fields`]), under
    /// the drain lock when one attempt takes it, else without it.
    fn stamp_end(&self) -> Result<(), String> {
        // One attempt, never a wait, since a drop must not wait. Held, no call of the same session can write the record
        // between this read and this write; without it, such a write can be put back to the fields read here.
        let _lock = files::take_lock(&self.lock).ok().and_then(|locked| match locked {
            Locked::Held(held) => Some(held),
            Locked::Busy => None,
        });
        let now_s = self.test_now.unwrap_or_else(|| unix_s(SystemTime::now()));
        let mut record = record_fields(&self.record)?;
        record.insert("Called".to_owned(), json!(now_s));
        written(&self.record, &Value::Object(record))
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        if let Some(holder) = self.holder.take() {
            // The session's lease runs from the end of its last call, so a call longer than the lease leaves it live.
            // The rewrite takes the drain lock only when it is free and writes without it otherwise: while the holder
            // is held the session is live, so no drained test removes its record before the holder's removal, and the
            // lock only keeps another call of the same session from writing between the read and the write. A rewrite
            // that fails is not said, a drop having no console: the holder is removed all the same, and the session
            // lapses at its lease counted from its admission.
            drop(holder.stamp_end());
            // A holder left behind is dead once this process ends, and the next drained test removes it.
            drop(files::remove_if_there(holder.held.path()));
        }
    }
}

/// Admits a call of `session` under the work root `root` or refuses it, in one hold of the drain lock: a call is
/// refused while a barrier stands or an older live ticket waits, and a call with no session under a standing barrier
/// ([`decided`]). A refused call of a session takes or renews its ticket unless `no_ticket`; an admitted one writes its
/// session record and holds its holder before the lock is let go, and gives up its ticket. Else why the drain could not
/// be read or written.
pub(crate) fn admit(
    root: &Path,
    session: Option<&Session<'_>>,
    no_ticket: bool,
    seams: &BoxSeams,
    console: &mut Console,
) -> Result<Result<Admitted, Refused>, String> {
    let drain = root.join(DRAIN);
    // With no `drain/`, no barrier stands and a call with no session takes no ticket: nothing to make.
    if session.is_none() && !files::exists(&drain) {
        return Ok(Ok(Admitted::none()));
    }
    ready_drain(&drain, console)?;
    let _lock = take_drain_lock(&drain.join(LOCK), LOCK_RETRY)?;
    let state = State {
        drain: &drain,
        clock: Clock::of(seams),
    };
    let standing = state.standing()?;
    let tickets = state.live_tickets_in()?;
    let id = session.map(|session| session.id);
    let older = older_live(&tickets, id, |_| true);
    let live = match id {
        Some(id) => state.own_session_live(id)?,
        None => false,
    };
    match (decided(standing, older, id, live), session) {
        (Ok(()), Some(session)) => state.registered(&tickets, session, seams.now).map(Ok),
        (Ok(()), None) => Ok(Ok(Admitted::none())),
        (Err(refused), session) => {
            if let Some(session) = session
                && !no_ticket
            {
                let name = ticket_name(ticket_seq(&tickets, session.id), session.id);
                let ticket = json!({ "Session": session.id, "Polled": state.clock.now_s() });
                written(&drain.join(TICKETS).join(name), &ticket)?;
            }
            Ok(Err(refused))
        }
    }
}

/// Why a call was not passed through the drain.
pub(crate) enum Unpassed {
    /// The payload names its session wrong: the usage line.
    Usage(String),
    /// The drain refused it.
    Refused(Refused),
    /// The drain could not be read or written: why.
    Failed(String),
}

impl Unpassed {
    /// Says why on `console`'s stderr; the status. A refusal says the `drain` refusal tag of `payload`'s nonce (left out
    /// for a payload without one) and the [`refused_lines`], status 75; a drain that failed says [`failed_line`],
    /// [`FAILED_STATUS`]; a usage error its line, [`USAGE_STATUS`].
    pub(crate) fn said(self, payload: &Payload, console: &mut Console) -> u8 {
        match self {
            Unpassed::Usage(line) => {
                console.err(&line);
                USAGE_STATUS
            }
            Unpassed::Refused(refused) => {
                if let Ok(nonce) = payload.nonce() {
                    console.err(&refused_line(nonce, gate_wire::Refusal::Drain));
                }
                for line in refused_lines(refused) {
                    console.err(&line);
                }
                REFUSED_STATUS
            }
            Unpassed::Failed(why) => {
                console.err(&failed_line(&why));
                FAILED_STATUS
            }
        }
    }
}

/// The line of a drain that could not be read or written for `why`.
pub(crate) fn failed_line(why: &str) -> String {
    format!("gate: drain failed: {why}")
}

/// The guard of `admission` ([`admit`]), or why it was not passed.
///
/// # Errors
///
/// The call was refused, or the drain could not be read or written.
pub(crate) fn admission_of(admission: Result<Result<Admitted, Refused>, String>) -> Result<Admitted, Unpassed> {
    match admission {
        Ok(Ok(admitted)) => Ok(admitted),
        Ok(Err(refused)) => Err(Unpassed::Refused(refused)),
        Err(why) => Err(Unpassed::Failed(why)),
    }
}

/// Whether the work root `root` is there to hold a drain: false when nothing is at its name. Else why not: a link or
/// anything but a folder there.
pub(crate) fn root_there(root: &Path) -> Result<bool, String> {
    if !files::exists(root) {
        return Ok(false);
    }
    if files::plain_folder(root) {
        Ok(true)
    } else {
        Err(format!("{} is a link or not a folder", escaped(&root.to_string_lossy())))
    }
}

/// Passes a call of `run` or `inputs` through the drain under the work root `root` as the session `payload` names
/// ([`session_of`]), with a run's command text and job `run` (both empty for `inputs`), taking no ticket: the guard,
/// held until the verb returns. A root not there yet admits the call, holding nothing.
///
/// # Errors
///
/// The payload names its session wrong, the call is refused, or the drain could not be read or written.
pub(crate) fn passed(
    root: &str,
    payload: &Payload,
    run: Option<(&str, &str)>,
    seams: &BoxSeams,
    console: &mut Console,
) -> Result<Admitted, Unpassed> {
    let (command, job) = run.unwrap_or(("", ""));
    let session = session_of(payload, command, job).map_err(Unpassed::Usage)?;
    let root = Path::new(root);
    if !root_there(root).map_err(Unpassed::Failed)? {
        return Ok(Admitted::none());
    }
    admission_of(admit(root, session.as_ref(), true, seams, console))
}

/// What an `outputs` call may do under the drain: the hold of its session while it sends, when the session is live, and
/// whether it may remove the archive it sent.
pub(crate) struct Renewal {
    pub(crate) held: Option<Admitted>,
    pub(crate) may_delete: bool,
}

/// Renews the session `session` for an `outputs` call under the work root `root`, in one hold of the drain lock: a
/// live one ([`State::own_session_live`]) gets its record's `Called` set to now, every other field kept, and its holder
/// held until the [`Renewal`] drops, and may delete; a dead or missing one is held by nothing and may delete only while
/// no barrier stands. A root with no `drain/` has no barrier, and the call makes none. Else why the drain could not be
/// read or written.
fn renewed(root: &Path, session: Option<&str>, seams: &BoxSeams, console: &mut Console) -> Result<Renewal, String> {
    let drain = root.join(DRAIN);
    if !files::exists(&drain) {
        return Ok(Renewal {
            held: None,
            may_delete: true,
        });
    }
    ready_drain(&drain, console)?;
    let _lock = take_drain_lock(&drain.join(LOCK), LOCK_RETRY)?;
    let state = State {
        drain: &drain,
        clock: Clock::of(seams),
    };
    let standing = state.standing()?.is_some();
    match session {
        Some(id) if state.own_session_live(id)? => Ok(Renewal {
            held: Some(state.renew(id, seams.now)?),
            may_delete: true,
        }),
        _ => Ok(Renewal {
            held: None,
            may_delete: !standing,
        }),
    }
}

/// [`renewed`] for an `outputs` call of `session` under the work folder `work`'s root, the root judged first as its own
/// records folder, making nothing ([`records_folder`]); a root not there yet has no barrier, holds nothing and may
/// delete. Else why the drain could not be read, or the root's refusal, its `gate: ` left out, with nothing touched
/// beneath it.
pub(crate) fn renewed_at(work: &str, session: Option<&str>, seams: &BoxSeams, console: &mut Console) -> Result<Renewal, String> {
    let root = gate_wire::work_root(work, PathForm::Unix).ok_or_else(|| format!("the work folder {} is not <root>/work/<key>", escaped(work)))?;
    match records_folder(&root, Path::new(&root)) {
        Ok(true) => {}
        Ok(false) => {
            return Ok(Renewal {
                held: None,
                may_delete: true,
            });
        }
        Err((line, _)) => return Err(line.strip_prefix("gate: ").map_or_else(|| line.clone(), str::to_owned)),
    }
    renewed(Path::new(&root), session, seams, console)
}

/// The text of the plain file `path` and the Unix second of its last write ([`files::read_plain_dated`]); `None` when
/// nothing has the name. Else why it could not be read, a link or anything but a plain file included.
pub(crate) fn read_text(path: &Path) -> Result<Option<(String, u64)>, String> {
    match files::read_plain_dated(path, RECORD_LIMIT) {
        Ok((bytes, written)) => Ok(Some((String::from_utf8_lossy(&bytes).into_owned(), unix_s(written)))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(cannot("read", path, &error)),
    }
}

/// Removes the file `path`; a missing one is fine. Else why not.
pub(crate) fn removed(path: &Path) -> Result<(), String> {
    files::remove_if_there(path).map_err(|error| cannot("remove", path, &error))
}

/// The names in the folder `folder` that are UTF-8, in no order. Else why it could not be listed.
pub(crate) fn names_in(folder: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(folder).map_err(|error| cannot("list", folder, &error))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| cannot("list", folder, &error))?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// The box's last boot, in Unix seconds ([`crate::boot::boot_time`]).
fn boot_s() -> Result<u64, String> {
    let (seconds, _) = crate::boot::boot_time().map_err(|why| format!("cannot read the boot time: {why}"))?;
    u64::try_from(seconds).map_err(|_| format!("cannot read the boot time: {seconds} is before 1970"))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a case whose folder or lock cannot be made must fail loudly")]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use gate_wire::escaped;

    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::Path;
    use std::time::Duration;

    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    use gate_wire::PathForm;
    use gate_wire::drain::{Clock, LAPSE_MS, LEASE_MS, ProcessInfo, counted_processes};
    use serde_json::json;

    use super::{State, Step, holder_live, ready_drain_seen, take_drain_lock, without_box_calls};
    use crate::console::Console;
    use crate::files;
    use crate::lines::job_record_text;
    use crate::pid_info::start_of;

    /// The Unix second the job cases judge at.
    const NOW: u64 = 1_800_000_000;
    /// The job the job cases' session names.
    const JOB: &str = "gate-remote-k-0123456789ab";

    /// The root `{case}/root` with its `drain/`, `tickets/`, `sessions/` and `work/` made at mode 0700; the root, its
    /// drain and its work folder.
    fn drain_of(case: &Case) -> (PathBuf, PathBuf, PathBuf) {
        let root = case.base.join("root");
        let (drain, work) = (root.join("drain"), root.join("work"));
        for folder in [root.clone(), drain.clone(), drain.join("tickets"), drain.join("sessions"), work.clone()] {
            folder_at(&folder, 0o700);
        }
        (root, drain, work)
    }

    /// The drain `drain` judged at [`NOW`].
    fn state_of(drain: &Path) -> State<'_> {
        State {
            drain,
            clock: Clock {
                now: NOW * 1000,
                lease: LEASE_MS,
                lapse: LAPSE_MS,
            },
        }
    }

    /// Writes the record of the session `id` in `drain`, naming the job `job`, called long before its lease ended.
    fn session_naming(drain: &Path, id: &str, job: &str) {
        let record = json!({ "Pc": "pc1", "Repo": "repo", "Command": "x", "Job": job, "Called": NOW - 1000 });
        fs::write(drain.join("sessions").join(id), record.to_string()).expect("the session's record is written");
    }

    /// A process sleeping as the leader of a process group of its own, as a run's command is; killed and reaped when
    /// dropped.
    struct Leader(Child);

    impl Leader {
        #[expect(
            clippy::disallowed_methods,
            reason = "the spawn lock is the Windows gate's; this stands in for a run's command"
        )]
        fn new() -> Leader {
            Leader(Command::new("/bin/sleep").arg("600").process_group(0).spawn().expect("sleep starts"))
        }

        fn pid(&self) -> i32 {
            i32::try_from(self.0.id()).expect("a pid is a pid_t")
        }

        /// The text of a group record of `job` naming this leader's group, its start `started`.
        fn record(&self, job: &str, started: (u64, u32)) -> String {
            // SAFETY: `getsid` only reads the process table.
            let session = unsafe { libc::getsid(self.pid()) };
            job_record_text(job, self.pid(), session, started)
        }
    }

    impl Drop for Leader {
        fn drop(&mut self) {
            // Best effort: a sleep left behind ends on its own.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// The jobs of the live sessions of `drain`, judged by the records in `work`.
    fn live_jobs(drain: &Path, work: &Path) -> Vec<Option<String>> {
        let sessions = state_of(drain).sessions(work).expect("the sessions are read");
        sessions.into_iter().map(|session| session.job).collect()
    }

    #[test]
    fn a_malformed_restore_record_keeps_its_session_live() {
        let case = Case::new("restore-malformed");
        let (_, drain, work) = drain_of(&case);
        // A package restore's record names its own job, `{job}-packages`; its group line is no group.
        fs::write(work.join("k.packages.pgid"), format!("{JOB}-packages\nnot a group\n")).expect("the malformed record is written");
        session_naming(&drain, "s1", JOB);

        assert_eq!(
            live_jobs(&drain, &work),
            [Some(JOB.to_owned())],
            "a malformed record of the job fails closed"
        );
    }

    #[test]
    fn a_prune_spelling_the_root_two_ways_is_no_box_call() {
        let (given, real) = ("/Users/builder/gate-remote", "/Volumes/Data/Users/builder/gate-remote");
        let process = |pid: u32, line: &str| ProcessInfo {
            pid,
            parent_pid: 1,
            name: "gate-box".to_owned(),
            command_line: Some(line.to_owned()),
            created: None,
        };
        let processes = vec![
            process(20, &format!("{given}/bin/gate-box box prune --root {real}")),
            process(21, &format!("{given}/bin/gate-box box preflight --stdin")),
        ];

        let kept = without_box_calls(processes, &[given.to_owned(), real.to_owned()], 99);

        let pids: Vec<u32> = kept.iter().map(|process| process.pid).collect();
        assert_eq!(pids, [20], "the prune is kept whichever spelling its --root uses");
    }

    #[test]
    fn the_ancestor_walk_stops_at_a_parent_started_after_its_child() {
        let process = |pid: u32, parent: u32, started: i64| ProcessInfo {
            pid,
            parent_pid: parent,
            name: "sh".to_owned(),
            command_line: Some("sh".to_owned()),
            created: jiff::Timestamp::from_second(started).ok(),
        };
        // The drain's parent 30 ended; its pid now belongs to 30, started after the drain, whose parent is 10.
        let processes = vec![process(10, 1, 100), process(30, 10, 500), process(40, 30, 300)];

        let kept = without_box_calls(processes, &[], 40);

        let pids: Vec<u32> = kept.iter().map(|process| process.pid).collect();
        assert_eq!(pids, [10, 30, 40], "a pid reused since is no ancestor, and the walk stops there");
    }

    // `start_of` reads a process's start only on macOS.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_session_whose_group_record_names_a_live_group_stays_live() {
        let case = Case::new("group-live");
        let (_, drain, work) = drain_of(&case);
        let leader = Leader::new();
        let started = start_of(leader.pid()).expect("the leader's start is read");
        // A tool restore's record names its own job, `{job}-tools`.
        let job = format!("{JOB}-tools");
        fs::write(work.join("k.tools.pgid"), leader.record(&job, started)).expect("the group record is written");
        session_naming(&drain, "s1", JOB);
        session_naming(&drain, "s2", "gate-remote-other-0123456789ab");

        assert_eq!(live_jobs(&drain, &work), [Some(JOB.to_owned())]);
        assert!(
            !drain.join("sessions/s2").exists(),
            "a session whose job no record names is dead and removed"
        );
    }

    // `start_of` reads a process's start only on macOS.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_session_whose_group_record_is_stale_is_dead() {
        let case = Case::new("group-stale");
        let (_, drain, work) = drain_of(&case);
        let leader = Leader::new();
        let started = start_of(leader.pid()).expect("the leader's start is read");
        // The group's leader started at another time: a pid reused since the record was written.
        fs::write(work.join("k.pgid"), leader.record(JOB, (1, 0))).expect("the stale record is written");
        // Another job's live group and another job's command record say nothing of this one.
        fs::write(work.join("j.pgid"), leader.record("gate-remote-j-0123456789ab", started)).expect("another job's record is written");
        fs::write(work.join("j.command"), "2026-10-27T12:00:00.0000000Z gate-remote-j-0123456789ab").expect("another command record is written");
        session_naming(&drain, "s1", JOB);

        assert!(live_jobs(&drain, &work).is_empty(), "the session is dead");
        assert!(!drain.join("sessions/s1").exists(), "and its record is removed");
    }

    #[test]
    fn the_box_s_own_calls_and_the_drain_s_ancestors_are_left_out() {
        let root = "/Users/builder/gate-remote";
        let process = |pid: u32, parent: u32, line: Option<&str>| ProcessInfo {
            pid,
            parent_pid: parent,
            name: "x".to_owned(),
            command_line: line.map(str::to_owned),
            created: None,
        };
        let processes = vec![
            process(1, 0, Some("/sbin/launchd")),
            process(10, 1, Some("sshd: builder")),
            process(11, 10, Some("sh -c /Users/builder/gate-remote/bin/gate-box box drain --stdin")),
            process(12, 11, Some("/Users/builder/gate-remote/bin/gate-box box drain --stdin")),
            process(20, 10, Some("/USERS/builder/Gate-Remote/bin/gate-box box preflight --stdin")),
            process(21, 20, Some("git -C /Users/builder/gate-remote/work/k status")),
            process(22, 20, Some("git index-pack --stdin")),
            process(
                30,
                10,
                Some("/Users/builder/gate-remote/bin/gate-box box prune --root /Users/builder/gate-remote"),
            ),
            process(40, 10, Some("/Users/builder/gate-remote/bin/gate-box-old box run --stdin")),
            process(50, 10, None),
        ];

        let kept = without_box_calls(processes, &[root.to_owned()], 12);

        let pids: Vec<u32> = kept.iter().map(|process| process.pid).collect();
        assert_eq!(
            pids,
            [21, 22, 30, 40, 50],
            "the drain's ancestors and the box's calls, its own included, go; a prune, another program and an unreadable line stay"
        );
        let counted: Vec<u32> = counted_processes(kept, root, 12, false, PathForm::Unix).into_keys().collect();
        assert_eq!(
            counted,
            [21, 30, 40],
            "a box call's child naming the root counts on its own; one that does not is no longer counted"
        );
    }

    // Off macOS the process scan is not built, so the status cannot read drained.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_session_whose_command_record_outlived_its_run_is_dead() {
        let case = Case::new("command-outlived");
        let (root, drain, work) = drain_of(&case);
        // A run cut short keeps its interrupted-command record, naming the job, with no group of it left.
        fs::write(work.join("k.command"), format!("2026-10-27T12:00:00.0000000Z {JOB}")).expect("the command record is written");
        session_naming(&drain, "s1", JOB);
        let state = state_of(&drain);
        state.raise().expect("the barrier is raised");

        let status = state.status(&root, true).expect("the status is read");

        assert_eq!(status.last().map(String::as_str), Some("gate: drain state drained"), "{status:?}");
        assert!(!drain.join("sessions/s1").exists(), "the dead session's record is removed");
    }

    // Off macOS the process scan is not built, so the status cannot read drained.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unreadable_group_record_keeps_the_drain_waiting() {
        let case = Case::new("group-unreadable");
        let (root, drain, work) = drain_of(&case);
        let record = work.join("k.packages.pgid");
        // A folder at the record's name cannot be read as a record.
        folder_at(&record, 0o700);
        session_naming(&drain, "s1", JOB);
        let state = state_of(&drain);
        state.raise().expect("the barrier is raised");

        let waiting = state.status(&root, true).expect("the status is read");
        fs::remove_dir(&record).expect("the folder is removed");
        let after = state.status(&root, true).expect("the status is read again");

        assert!(
            waiting.iter().any(|line| line.starts_with("gate: drain session 1000 ")),
            "the session is live: {waiting:?}"
        );
        assert_eq!(waiting.last().map(String::as_str), Some("gate: drain state waiting"), "{waiting:?}");
        assert_eq!(after.last().map(String::as_str), Some("gate: drain state drained"), "{after:?}");
    }

    #[test]
    fn a_live_holder_is_busy_to_flock_and_a_dead_one_is_removed() {
        let case = Case::new("holder");
        let live = case.base.join("s1.1");
        let held = files::held_for_test(&live);

        assert_eq!(holder_live(&live), Ok(true), "a holder another open holds is live");
        assert!(live.exists(), "a live holder is left in place");
        drop(held);
        assert_eq!(holder_live(&live), Ok(false), "a holder let go is dead");
        assert!(!live.exists(), "a dead holder is removed");

        let never_held = case.base.join("s2.2");
        fs::write(&never_held, "").expect("a holder no process holds is planted");
        assert_eq!(holder_live(&never_held), Ok(false), "a holder no one holds is dead");
        assert!(!never_held.exists(), "and is removed");
    }

    /// A temp folder of the case `name`, private to this process, removed when the case ends.
    struct Case {
        base: PathBuf,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-drain-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            fs::create_dir_all(&base).expect("the case folder is made");
            Case { base }
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_drain_lock_held_by_another_open_is_retried_then_fails() {
        let case = Case::new("lock");
        let path = case.base.join("lock");
        let held = files::held_for_test(&path);

        let refused = take_drain_lock(&path, Duration::from_millis(50)).expect_err("a lock held for the whole wait is not taken");
        assert_eq!(
            refused,
            format!("the drain lock {} is held by another process", escaped(&path.to_string_lossy()))
        );
        drop(held);
        assert!(take_drain_lock(&path, Duration::from_millis(50)).is_ok(), "a lock let go is taken");
    }

    /// Makes the folder `path` at `mode`, whatever the umask.
    fn folder_at(path: &Path, mode: u32) {
        fs::DirBuilder::new().mode(0o700).create(path).expect("the folder is made");
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("the folder's mode is set");
    }

    /// The permission bits of `path`, looked at as itself.
    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).expect("the path is looked at").mode() & 0o7777
    }

    #[test]
    fn a_drain_moved_aside_by_another_call_is_judged_again_not_moved_twice() {
        let case = Case::new("moved-meanwhile");
        let drain = case.base.join("drain");
        let first = case.base.join("drain.gate-moved-1");
        let second = case.base.join("drain.gate-moved-2");
        folder_at(&drain, 0o755);
        let mut console = Console::captured();
        let mut swapped = false;
        // Another call moves the planted folder aside and readies its own drain, barrier included, between this call's
        // judgement and its move.
        let readied = ready_drain_seen(&drain, &mut console, &mut |step, path| {
            if step == Step::Move && !swapped {
                swapped = true;
                fs::rename(path, path.with_file_name("drain.gate-moved-1")).expect("the other call moves it aside");
                folder_at(path, 0o700);
                fs::write(path.join("barrier"), "kept").expect("the other call raises its barrier");
            }
        });
        readied.expect("the drain is readied");
        assert_eq!(fs::read_to_string(drain.join("barrier")).expect("the barrier is read"), "kept");
        assert!(first.is_dir() && !second.exists(), "the good drain is not moved aside");
        assert!(console.err.is_empty(), "{:?}", console.err);

        fs::remove_dir_all(&drain).expect("the drain is removed");
        fs::remove_dir_all(&first).expect("the moved folder is removed");
        folder_at(&drain, 0o755);
        let mut vanished = false;
        // Another call moves it aside and has not made its own yet.
        let readied = ready_drain_seen(&drain, &mut console, &mut |step, path| {
            if step == Step::Move && !vanished {
                vanished = true;
                fs::rename(path, path.with_file_name("drain.gate-moved-1")).expect("the other call moves it aside");
            }
        });
        readied.expect("a drain moved away meanwhile is not a failure");
        assert_eq!(mode_of(&drain), 0o700, "the drain is made");
        assert!(first.is_dir() && !second.exists(), "nothing is moved twice");
        assert!(console.err.is_empty(), "{:?}", console.err);
    }

    #[test]
    fn a_drain_folder_made_by_another_call_meanwhile_is_adopted() {
        let case = Case::new("made-meanwhile");
        let drain = case.base.join("drain");
        let mut console = Console::captured();
        let mut made = false;
        let readied = ready_drain_seen(&drain, &mut console, &mut |step, path| {
            if step == Step::Make && path == drain && !made {
                made = true;
                folder_at(path, 0o700);
                fs::write(path.join("barrier"), "kept").expect("the other call raises its barrier");
            }
        });
        readied.expect("a folder made meanwhile is adopted");
        assert_eq!(fs::read_to_string(drain.join("barrier")).expect("the barrier is read"), "kept");
        for name in ["tickets", "sessions"] {
            assert_eq!(mode_of(&drain.join(name)), 0o700, "{name}");
        }

        let other = case.base.join("other");
        folder_at(&other, 0o700);
        let open = other.join("drain");
        let refused = ready_drain_seen(&open, &mut console, &mut |step, path| {
            if step == Step::Make && path == open.as_path() {
                folder_at(path, 0o755);
            }
        });
        assert_eq!(
            refused,
            Err(format!(
                "{} is not the box user's own folder at mode 0700",
                escaped(&open.to_string_lossy())
            ))
        );
        assert!(console.err.is_empty(), "{:?}", console.err);
    }
}
