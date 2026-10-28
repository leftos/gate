//! `gate box drain`: the builder's drain barrier, and the one admission function the routed verbs pass before they
//! touch the work root. Everything lives in `{root}\drain\`, a protected folder only the elevated box writes:
//!
//! - `lock`, held around each read-modify of the files below and never across a wait or a child process, but for the
//!   end stamp of a call's session, which takes it when it is free and writes without it otherwise;
//! - `barrier` (`{"Raised","Lapses","Generation"}`), which stands until it is lifted or lapses, then `lapsed`
//!   (`{"At"}`) once it lapsed and `lifted` (`{"At"}`) once it was lifted;
//! - `tickets\{seq:08}-{session}` (`{"Session","Polled"}`), one per run waiting in line;
//! - `sessions\{session}` (`{"Pc","Repo","Command","Job","Called"}`), one per admitted run, and
//!   `sessions\{session}.{pid}`, the holder each admitted box process keeps open, sharing nothing, for its whole life.
//!
//! The drain is "drained" only from one hold of the lock that finds the barrier standing, no live session, and no
//! process of the root running ([`gate_wire::drain::counted_processes`], [`command_jobs`]); that hold removes every session it found dead.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Instant, SystemTime};

use gate_args::BoxSeams;
use gate_win32::{Descriptor, Make, Share};
use gate_wire::drain::{
    BARRIER, Barrier, Clock, DRAIN, Kind, LAPSED, LIFTED, LOCK, LOCK_RETRY, LOCK_STEP, LiveSession, MOST_COMMAND_CHARS, MOST_NAME_CHARS, Record,
    Refused, SESSIONS, TICKETS, Ticket, at_of, barrier_line, barrier_of, counted_processes, decided, older_live, refused_lines, session_line,
    state_line, ticket_live, ticket_name, ticket_of_name, ticket_seq, tickets_line, unix_ms, unix_s,
};
use gate_wire::{DrainAction, PathForm, Payload, escaped, is_recorded_job_name, is_session_id};
use proc_table::{ProcessTable, Win32ProcessTable};
use serde_json::{Value, json};
use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES};

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::inputs::{Leftover, protected_folder, protected_security};
use crate::boxside::run::{Interrupted, LOCK_VIOLATION, SHARING_VIOLATION, read_command_record};
use crate::boxside::{preflight, stamping};
use crate::console::Console;

/// Answers `gate box drain`: raises, reads or lifts the barrier under the payload's `Root` as its `Action` says, then
/// says how the drain stands, one line each ([`State::status`]). The exit status: 0 once it answered,
/// [`FAILED_STATUS`] after `gate: drain failed: {why}`, or [`USAGE_STATUS`] after the usage line for a payload without
/// a `Root` path or an `Action`.
pub(crate) fn drain(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u32 {
    let read = payload.path("Root").and_then(|root| Ok((root, payload.drain_action()?)));
    let (root, action) = match read {
        Ok(read) => read,
        Err(line) => {
            stamping::err(console, &line);
            return u32::from(USAGE_STATUS);
        }
    };
    match answered(Path::new(root), action, seams, console) {
        Ok(lines) => {
            for line in &lines {
                stamping::out(console, line);
            }
            0
        }
        Err(why) => {
            stamping::err(console, &format!("gate: drain failed: {why}"));
            u32::from(FAILED_STATUS)
        }
    }
}

/// The lines answering `action` on the root `root`. A raise makes the root, judged and held as the preflight holds it
/// ([`gate_win32::check_work_root`]), and `drain\` when they are missing; a status or a lift of a root or `drain\` that
/// is not there reads as no barrier and changes nothing.
fn answered(root: &Path, action: DrainAction, seams: &BoxSeams, console: &mut Console) -> Result<Vec<String>, String> {
    if action != DrainAction::Raise && !preflight::is_real_folder(root) {
        return Ok(open_lines());
    }
    // The check's handle may hold DELETE on a root it made; the root is held again as a status holds it.
    drop(gate_win32::check_work_root(root, seams.root_walk_stop.as_deref()).map_err(|why| why.to_string())?);
    let held_root = preflight::held_folder_at(root)
        .map_err(|error| cannot("open", root, &error))?
        .ok_or_else(|| format!("{} is a link, a junction or not a folder", escaped(&root.to_string_lossy())))?;
    if action != DrainAction::Raise
        && existing(&held_root, DRAIN)
            .map_err(|error| cannot("read", &root.join(DRAIN), &error))?
            .is_none()
    {
        return Ok(open_lines());
    }
    let security = protected_security()?;
    let drain_path = root.join(DRAIN);
    let drain = ready_drain(&held_root, &drain_path, &security, console)?;
    let _lock = take_drain_lock(&drain_path.join(LOCK))?;
    let state = State {
        drain: &drain,
        security: &security,
        clock: Clock::of(seams),
        readable_only: seams.drain_readable_only == gate_args::ProcessScan::ReadableOnly,
        root,
        scanned_root: root_text(&held_root)?,
    };
    match action {
        DrainAction::Raise => state.raise()?,
        DrainAction::Lift => state.lift()?,
        DrainAction::Status => {}
    }
    state.status()
}

/// `drain\` beneath the held root `root`, at `path`, readied as [`protected_folder`] readies it. Anything at its name
/// that is not the box's protected folder (a link, a junction, a file, or a folder of any other security) is first
/// renamed aside as itself, never followed, to `drain.gate-moved-{n}` ([`preflight::moved_aside`]), and said on
/// `console`. Else why not, a rename that failed included.
fn ready_drain(root: &File, path: &Path, security: &Descriptor, console: &mut Console) -> Result<File, String> {
    if foreign_drain(root, path, FILE_GENERIC_READ)?.is_some()
        && let Some(found) = foreign_drain(root, path, FILE_GENERIC_READ | DELETE)?
    {
        let moved = preflight::moved_aside(root, DRAIN, &found).map_err(|unmoved| unmoved.why(&path.to_string_lossy()))?;
        drop(found);
        let line = format!(
            "gate: drain moved aside {} to {}: it was not the box's protected folder",
            escaped(&path.to_string_lossy()),
            escaped(&moved)
        );
        stamping::err(console, &line);
    }
    protected_folder(root, path, FILE_GENERIC_READ, security, Leftover::MoveAside(console))
}

/// What has the name `drain` beneath `root`, opened as itself with `access` and sharing everything, when it is not the
/// box's protected folder; `None` when it is, or when nothing has the name. The first look asks no `DELETE`, so it
/// never meets the sharing of another call that holds the box's own `drain\`.
fn foreign_drain(root: &File, path: &Path, access: u32) -> Result<Option<File>, String> {
    let found = match gate_win32::open_beneath_as(root, DRAIN, access, Share::All, Make::Open) {
        Ok(found) => found,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(cannot("open", path, &error)),
    };
    let protected = preflight::is_folder_itself(&found)
        .and_then(|itself| Ok(itself && gate_win32::is_protected_folder(&found)?))
        .map_err(|error| cannot("read", path, &error))?;
    Ok((!protected).then_some(found))
}

/// The held root's final path in its usual form ([`gate_win32::usual_final_path`]), which the process scan matches,
/// so the payload's spelling of the root (`D:\gate-remote\.`, `d:/gate-remote`) does not matter.
fn root_text(held: &File) -> Result<String, String> {
    gate_win32::usual_final_path(held)
        .map(|full| full.to_string_lossy().into_owned())
        .map_err(|error| format!("cannot read the root's final path: {}", escaped(&error.to_string())))
}

/// The lines of a root with no `drain\`: no barrier, no ticket, open.
fn open_lines() -> Vec<String> {
    vec![barrier_line(Barrier::None), tickets_line(0), state_line(Barrier::None, false)]
}

/// The drain lock `path`: opened for reading and writing, made when missing, shared with no one, never through a link,
/// and only as a plain disk file, as the work folder's lock is opened. An open another handle refuses is tried again
/// every [`LOCK_STEP`] until [`LOCK_RETRY`] has passed; else why not.
pub(super) fn take_drain_lock(path: &Path) -> Result<File, String> {
    let begun = Instant::now();
    loop {
        match lock_opened(path) {
            Ok(file) => return Ok(file),
            Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => {
                if begun.elapsed() >= LOCK_RETRY {
                    return Err(format!("the drain lock {} is held by another process", escaped(&path.to_string_lossy())));
                }
                thread::sleep(LOCK_STEP);
            }
            Err(error) => return Err(cannot("take", path, &error)),
        }
    }
}

/// One attempt at the drain lock `path`: the file opened for reading and writing, made when missing, shared with no
/// one, never through a link, and only as a plain disk file. Else the system's error, a sharing or lock violation
/// while another process holds it.
fn lock_opened(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    gate_win32::check_plain_file(&file)?;
    Ok(file)
}

/// Why the box could not `what` the file or folder `path`.
pub(super) fn cannot(what: &str, path: &Path, error: &io::Error) -> String {
    format!("cannot {what} {}: {}", escaped(&path.to_string_lossy()), escaped(&error.to_string()))
}

/// Whether a barrier stands under the root `root` at the time `seams` give ([`Clock::of`]), read only: no lock is
/// taken and nothing is written, so a process that may not write `drain\`, the prune, can ask. A root that is not a
/// folder of its own, or has no `drain\` folder of its own or no `barrier` in it, has none standing; a barrier that
/// does not read as one stands until its last write and a lapse ([`barrier_of`]). Else why it could not be read.
pub(crate) fn barrier_stands(root: &Path, seams: &BoxSeams) -> Result<bool, String> {
    if !preflight::is_real_folder(root) {
        return Ok(false);
    }
    let Some(held_root) = preflight::held_folder_at(root).map_err(|error| cannot("open", root, &error))? else {
        return Ok(false);
    };
    let drain_path = root.join(DRAIN);
    let Some(drain) = existing(&held_root, DRAIN).map_err(|error| cannot("read", &drain_path, &error))? else {
        return Ok(false);
    };
    if !preflight::is_folder_itself(&drain).map_err(|error| cannot("read", &drain_path, &error))? {
        return Ok(false);
    }
    let read = read_text(&drain, BARRIER).map_err(|error| cannot("read", &drain_path.join(BARRIER), &error))?;
    Ok(read.is_some_and(|(text, written)| matches!(barrier_of(&text, written, Clock::of(seams)), Barrier::Standing(_))))
}

/// The ids of the processes the job `job` holds; none when no job has the name or it holds none. Else the system's
/// error, so a job the box may not list holds the drain open.
fn job_processes(job: &str) -> io::Result<Vec<u32>> {
    if job.is_empty() {
        return Ok(Vec::new());
    }
    match gate_win32::OpenedJob::open_to_list(job)? {
        None => Ok(Vec::new()),
        Some(listed) => listed.process_ids(),
    }
}

/// What the `{root}\work\{key}.command` records say: the processes of the jobs they name, and the keys whose record
/// could not be read.
#[derive(Default)]
struct CommandJobs {
    pids: Vec<u32>,
    unreadable: Vec<String>,
}

/// The processes of each job a `{root}\work\{key}.command` record names ([`Interrupted::read`]), at `now`, each record
/// read through the run's own reader ([`read_command_record`]), and the keys of the records that reader could not read.
/// A work folder that is not there has none.
fn command_jobs(root: &Path, now: SystemTime) -> Result<CommandJobs, String> {
    let work = root.join("work");
    let entries = match fs::read_dir(&work) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(CommandJobs::default()),
        Err(error) => return Err(cannot("list", &work, &error)),
    };
    let mut found = CommandJobs::default();
    for entry in entries {
        let path = entry.map_err(|error| cannot("list", &work, &error))?.path();
        let Some(key) = path
            .extension()
            .filter(|extension| extension.eq_ignore_ascii_case("command"))
            .and(path.file_stem())
            .and_then(|stem| stem.to_str())
        else {
            continue;
        };
        let text = match read_command_record(&path.to_string_lossy()) {
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(_) => {
                found.unreadable.push(key.to_owned());
                continue;
            }
        };
        if let Some(job) = Interrupted::read(&text, key, now).job {
            found
                .pids
                .extend(job_processes(&job).map_err(|error| format!("cannot list the job {}: {error}", escaped(&job)))?);
        }
    }
    Ok(found)
}

/// The entry `name` beneath `folder`, opened for reading, sharing everything; `None` when nothing has the name.
pub(super) fn existing(folder: &File, name: &str) -> io::Result<Option<File>> {
    match gate_win32::open_beneath(folder, name, Share::All) {
        Ok(found) => Ok(Some(found)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The text of the file `name` beneath `folder` and the Unix second of its last write; `None` when nothing has the
/// name.
pub(super) fn read_text(folder: &File, name: &str) -> io::Result<Option<(String, u64)>> {
    let Some(mut file) = existing(folder, name)? else {
        return Ok(None);
    };
    let written = unix_s(file.metadata()?.modified()?);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some((String::from_utf8_lossy(&bytes).into_owned(), written)))
}

/// Writes `text` as the whole of the file `name` beneath `folder`: into a new file of `security`, flushed to the disk,
/// then renamed over `name`, so a reader sees the old text or the new and never part of one.
pub(super) fn write_text(folder: &File, name: &str, text: &str, security: &Descriptor) -> io::Result<()> {
    let temporary = format!("{name}.new-{}", std::process::id());
    removed(folder, &temporary)?;
    let access = FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE;
    let mut file = gate_win32::make_file_beneath(folder, &temporary, access, Share::None, security)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    gate_win32::rename_beneath(&file, folder, name)
}

/// Whether the file `name` beneath `folder` is held open, shared with no one, by a live process: one that cannot be
/// deleted for sharing. A dead one is removed.
pub(super) fn held_open(folder: &File, name: &str) -> io::Result<bool> {
    match removed(folder, name) {
        Ok(_) => Ok(false),
        Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => Ok(true),
        Err(error) => Err(error),
    }
}

/// The file `name` beneath `folder`, made with `security` or opened when it exists, for reading and deletion and
/// shared with no one.
pub(super) fn held_share_none(folder: &File, name: &str, security: &Descriptor) -> io::Result<File> {
    let access = FILE_GENERIC_READ | DELETE;
    match gate_win32::make_file_beneath(folder, name, access, Share::None, security) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => gate_win32::open_beneath_as(folder, name, access, Share::None, Make::Open),
        made => made,
    }
}

/// Removes the entry `name` beneath `folder` as itself; whether there was one. A file another process holds open
/// without delete sharing is the system's sharing violation.
pub(super) fn removed(folder: &File, name: &str) -> io::Result<bool> {
    match gate_win32::open_beneath_as(folder, name, DELETE | FILE_READ_ATTRIBUTES, Share::All, Make::Open) {
        Ok(found) => gate_win32::delete_opened(&found).map(|()| true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The drain's files under one hold of its lock.
struct State<'a> {
    drain: &'a File,
    security: &'a Descriptor,
    clock: Clock,
    /// Whether the process scan counts only processes whose command line it reads (`--test-box-drain-readable-only`).
    readable_only: bool,
    root: &'a Path,
    /// The held root's final path, which the process scan matches ([`root_text`]).
    scanned_root: String,
}

impl State<'_> {
    /// How the barrier stands; one found lapsed is replaced by its `lapsed` record.
    fn barrier(&self) -> Result<Barrier, String> {
        if let Some((text, written)) = read_text(self.drain, BARRIER).map_err(|error| self.cannot("read", BARRIER, &error))? {
            match barrier_of(&text, written, self.clock) {
                Barrier::Lapsed(at) => {
                    self.write(LAPSED, &json!({ "At": at }))?;
                    removed(self.drain, BARRIER).map_err(|error| self.cannot("remove", BARRIER, &error))?;
                }
                standing => return Ok(standing),
            }
        }
        match read_text(self.drain, LAPSED).map_err(|error| self.cannot("read", LAPSED, &error))? {
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
            removed(self.drain, name).map_err(|error| self.cannot("remove", name, &error))?;
        }
        self.write(LIFTED, &json!({ "At": self.clock.now_s() }))
    }

    /// The Unix second of the last lift or lapse, 0 for none.
    fn lift_time(&self) -> Result<u64, String> {
        let mut latest = 0;
        for name in [LAPSED, LIFTED] {
            if let Some((text, written)) = read_text(self.drain, name).map_err(|error| self.cannot("read", name, &error))? {
                latest = latest.max(at_of(&text, written));
            }
        }
        Ok(latest)
    }

    /// The tickets in `tickets`, in no order; a ticket that does not read polled at its last write.
    fn tickets(&self, tickets: &File) -> Result<Vec<Ticket>, String> {
        let names = gate_win32::names_beneath(tickets).map_err(|error| self.cannot("list", TICKETS, &error))?;
        let mut found = Vec::new();
        for name in names.iter().filter_map(|name| name.to_str()) {
            let Some((seq, session)) = ticket_of_name(name) else {
                continue;
            };
            let Some((text, written)) = read_text(tickets, name).map_err(|error| self.cannot("read", name, &error))? else {
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

    /// The live tickets in `folder`, in no order; each dead one ([`ticket_live`]) removed in this same hold, so a session
    /// whose ticket died queues again at the back.
    fn live_tickets_in(&self, folder: &File) -> Result<Vec<Ticket>, String> {
        let (boot, lift) = (boot_s()?, self.lift_time()?);
        let mut live = Vec::new();
        for ticket in self.tickets(folder)? {
            if ticket_live(ticket.polled, boot, lift, self.clock) {
                live.push(ticket);
            } else {
                let name = ticket_name(ticket.seq, &ticket.session);
                removed(folder, &name).map_err(|error| self.cannot("remove", &name, &error))?;
            }
        }
        Ok(live)
    }

    /// How many live tickets wait, each dead one removed.
    fn live_tickets(&self) -> Result<usize, String> {
        let Some(folder) = existing(self.drain, TICKETS).map_err(|error| self.cannot("read", TICKETS, &error))? else {
            return Ok(0);
        };
        Ok(self.live_tickets_in(&folder)?.len())
    }

    /// Whether the session `id` is live for a call of its own: a holder of it held, or its record called within the
    /// lease. Each dead holder of it met is removed.
    fn own_session_live(&self, id: &str) -> Result<bool, String> {
        let Some(folder) = existing(self.drain, SESSIONS).map_err(|error| self.cannot("read", SESSIONS, &error))? else {
            return Ok(false);
        };
        let names = gate_win32::names_beneath(&folder).map_err(|error| self.cannot("list", SESSIONS, &error))?;
        for name in names.iter().filter_map(|name| name.to_str()) {
            if holder_of(name) == Some(id) && self.holder_live(&folder, name)? {
                return Ok(true);
            }
        }
        let record = read_text(&folder, id).map_err(|error| self.cannot("read", id, &error))?;
        Ok(record.is_some_and(|(text, written)| self.clock.within_lease(Record::of(&text, written).called)))
    }

    /// The live sessions, each dead one's record and every dead holder removed. A session whose job is one of a key in
    /// `unreadable`, whose `.command` record could not be read, is live.
    fn sessions(&self, unreadable: &[String]) -> Result<Vec<LiveSession>, String> {
        let Some(folder) = existing(self.drain, SESSIONS).map_err(|error| self.cannot("read", SESSIONS, &error))? else {
            return Ok(Vec::new());
        };
        let names = gate_win32::names_beneath(&folder).map_err(|error| self.cannot("list", SESSIONS, &error))?;
        let mut ids = BTreeSet::new();
        let mut held = BTreeSet::new();
        for name in names.iter().filter_map(|name| name.to_str()) {
            if is_session_id(name) {
                ids.insert(name.to_owned());
            } else if let Some(id) = holder_of(name) {
                ids.insert(id.to_owned());
                if self.holder_live(&folder, name)? {
                    held.insert(id.to_owned());
                }
            }
        }
        let mut live = Vec::new();
        for id in &ids {
            if let Some(session) = self.session(&folder, id, held.contains(id), unreadable)? {
                live.push(session);
            }
        }
        Ok(live)
    }

    /// Whether the holder `name` is held by a live process: one that cannot be deleted for sharing. A dead one is
    /// removed.
    fn holder_live(&self, folder: &File, name: &str) -> Result<bool, String> {
        held_open(folder, name).map_err(|error| self.cannot("remove", name, &error))
    }

    /// The session `id` when it is live, judged in this order and the job listed only when nothing before made it live:
    /// `held`, called within the lease, its job one of a key in `unreadable` ([`is_recorded_job_name`]), or its job
    /// holding a process or one that cannot be listed. A dead one's record is removed.
    fn session(&self, folder: &File, id: &str, held: bool, unreadable: &[String]) -> Result<Option<LiveSession>, String> {
        let record = read_text(folder, id)
            .map_err(|error| self.cannot("read", id, &error))?
            .map(|(text, written)| Record::of(&text, written));
        let live = held
            || record.as_ref().is_some_and(|record| self.clock.within_lease(record.called))
            || record
                .as_ref()
                .and_then(|record| record.job.as_deref())
                .is_some_and(|job| job_live(job, unreadable));
        if live {
            let now = self.clock.now_s();
            return Ok(Some(match record {
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
            }));
        }
        removed(folder, id).map_err(|error| self.cannot("remove", id, &error))?;
        Ok(None)
    }

    /// The status lines, in their order: the barrier, each live session, each counted process, the live tickets, and
    /// the state.
    ///
    /// A key whose `.command` record could not be read and whose job no live session names gets its own line, and keeps
    /// the drain waiting.
    fn status(&self) -> Result<Vec<String>, String> {
        let barrier = self.barrier()?;
        let commands = command_jobs(self.root, self.clock.system_time())?;
        let sessions = self.sessions(&commands.unreadable)?;
        let unclaimed: Vec<&String> = commands
            .unreadable
            .iter()
            .filter(|key| {
                !sessions.iter().any(|session| {
                    session
                        .job
                        .as_deref()
                        .is_some_and(|job| is_recorded_job_name(job, key, PathForm::Windows))
                })
            })
            .collect();
        let mut processes: BTreeMap<u32, Kind> = commands.pids.into_iter().map(|pid| (pid, Kind::Job)).collect();
        for (pid, kind) in counted_processes(
            Win32ProcessTable.snapshot(),
            &self.scanned_root,
            std::process::id(),
            self.readable_only,
            PathForm::Windows,
        ) {
            processes.entry(pid).and_modify(|known| *known = (*known).min(kind)).or_insert(kind);
        }
        let mut lines = vec![barrier_line(barrier)];
        lines.extend(sessions.iter().map(session_line));
        lines.extend(processes.iter().map(|(pid, kind)| format!("gate: drain process {pid} {}", kind.word())));
        lines.extend(unclaimed.iter().map(|key| format!("gate: drain command {} unreadable", escaped(key))));
        lines.push(tickets_line(self.live_tickets()?));
        lines.push(state_line(barrier, sessions.is_empty() && processes.is_empty() && unclaimed.is_empty()));
        Ok(lines)
    }

    /// Writes `value` as the drain's file `name`.
    fn write(&self, name: &str, value: &Value) -> Result<(), String> {
        write_text(self.drain, name, &value.to_string(), self.security).map_err(|error| self.cannot("write", name, &error))
    }

    /// Why the drain could not `what` its file `name`.
    fn cannot(&self, what: &str, name: &str, error: &io::Error) -> String {
        cannot(what, &self.root.join(DRAIN).join(name), error)
    }
}

/// The box's last boot, in Unix seconds.
fn boot_s() -> Result<u64, String> {
    gate_win32::boot_time()
        .map(unix_s)
        .map_err(|error| format!("cannot read the boot time: {error}"))
}

/// The session a holder's file name `{session}.{pid}` names; `None` for any other name.
fn holder_of(name: &str) -> Option<&str> {
    let (id, pid) = name.split_once('.')?;
    (is_session_id(id) && !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit())).then_some(id)
}

/// Whether the session job `job` keeps its session live: it is one of a key in `unreadable`, whose `.command` record
/// could not be read, or it holds a process, or it cannot be listed, so the drained test fails closed.
fn job_live(job: &str, unreadable: &[String]) -> bool {
    unreadable.iter().any(|key| is_recorded_job_name(job, key, PathForm::Windows)) || !job_processes(job).is_ok_and(|pids| pids.is_empty())
}

/// A routed run's drain session, as its payload names it.
pub(crate) struct Session<'p> {
    pub(crate) id: &'p str,
    pub(crate) pc: &'p str,
    pub(crate) repo: &'p str,
    pub(crate) command: &'p str,
    pub(crate) job: &'p str,
}

/// The work root a verb holds as the preflight holds it, and its path.
pub(crate) struct HeldRoot<'a> {
    pub(crate) held: &'a File,
    pub(crate) path: &'a Path,
}

/// An admitted call's hold of its session ([`Holder`]); none for a call with no session, or under a root with no
/// drain. Dropping it stamps the session's end on its record, then closes and deletes the holder.
pub(crate) struct Admitted {
    holder: Option<Holder>,
}

/// The holder `sessions\{session}.{pid}`, open and shared with no one, which marks the session live while the process
/// lives, and what the end of the call needs to rewrite the session's record: the sessions folder, the session, the
/// security the record is written with, and the `--test-box-now` second the end is stamped at, else the clock's.
struct Holder {
    file: File,
    folder: File,
    id: String,
    security: Descriptor,
    test_now: Option<u64>,
    /// The drain's `lock`, which the end's rewrite takes when it is free.
    lock: PathBuf,
}

impl Holder {
    /// Rewrites the session's record with `Called` the end's second, every other field kept ([`record_fields`]), under
    /// the drain lock when one attempt takes it, else without it.
    fn stamp_end(&self) -> io::Result<()> {
        // One attempt, never a wait, since a drop must not wait. Held, no call of the same session can write the record
        // between this read and this write; without it, such a write can be put back to the fields read here.
        let _lock = lock_opened(&self.lock).ok();
        let now_s = self.test_now.unwrap_or_else(|| unix_ms(SystemTime::now()) / 1000);
        let mut record = record_fields(&self.folder, &self.id)?;
        record.insert("Called".to_owned(), json!(now_s));
        write_text(&self.folder, &self.id, &Value::Object(record).to_string(), &self.security)
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        if let Some(holder) = self.holder.take() {
            // The session's lease runs from the end of its last call, so a call longer than the lease leaves it live.
            // The rewrite takes the drain lock only when it is free and writes without it otherwise: while the holder
            // is held the session is live, so no drained test removes its record before the holder's deletion, and the
            // lock only keeps another call of the same session from writing between the read and the write. A rewrite
            // that fails is not said, a drop having no console: the holder is deleted all the same, and the session
            // lapses at its lease counted from its admission.
            drop(holder.stamp_end());
            // A holder left behind is dead once this process ends, and the next drained test removes it.
            drop(gate_win32::delete_opened(&holder.file));
        }
    }
}

/// The fields of the session `id`'s record in `folder`; none for a record that is missing or is not a JSON object.
fn record_fields(folder: &File, id: &str) -> io::Result<serde_json::Map<String, Value>> {
    Ok(read_text(folder, id)?
        .and_then(|(text, _)| serde_json::from_str(&text).ok())
        .unwrap_or_default())
}

/// Admits a call of `session` under the root `root` or refuses it, in one hold of the drain lock: a call is refused
/// while a barrier stands or an older live ticket waits, and a call with no session under a standing barrier
/// ([`decided`]). A refused call of a session takes or renews its ticket unless `no_ticket`; an admitted one writes its
/// session record and holds its holder before the lock is let go, and gives up its ticket. Else why the drain could not
/// be read or written.
pub(crate) fn admit(
    root: &HeldRoot<'_>,
    session: Option<&Session<'_>>,
    no_ticket: bool,
    seams: &BoxSeams,
    console: &mut Console,
) -> Result<Result<Admitted, Refused>, String> {
    let drain_path = root.path.join(DRAIN);
    // With no `drain\`, no barrier stands and a call with no session takes no ticket: nothing to make.
    if session.is_none() && existing(root.held, DRAIN).map_err(|error| cannot("read", &drain_path, &error))?.is_none() {
        return Ok(Ok(Admitted { holder: None }));
    }
    locked(root, seams, console, |state| {
        let standing = match state.barrier()? {
            Barrier::Standing(raised) => Some(raised),
            Barrier::None | Barrier::Lapsed(_) => None,
        };
        let tickets_folder = protected_folder(
            state.drain,
            &drain_path.join(TICKETS),
            FILE_GENERIC_READ,
            state.security,
            Leftover::Refuse,
        )?;
        let tickets = state.live_tickets_in(&tickets_folder)?;
        let older = older_live(&tickets, session.map(|session| session.id), |_| true);
        let live = match session {
            Some(session) => state.own_session_live(session.id)?,
            None => false,
        };
        match decided(standing, older, session.map(|session| session.id), live) {
            Ok(()) => registered(state, &tickets_folder, &tickets, session, seams.now).map(Ok),
            Err(refused) => {
                if let Some(session) = session
                    && !no_ticket
                {
                    let name = ticket_name(ticket_seq(&tickets, session.id), session.id);
                    let ticket = json!({ "Session": session.id, "Polled": state.clock.now_s() });
                    write_text(&tickets_folder, &name, &ticket.to_string(), state.security).map_err(|error| state.cannot("write", &name, &error))?;
                }
                Ok(Err(refused))
            }
        }
    })
}

/// Runs `under` with the drain's files under the root `root` ([`State`]), `drain\` readied ([`ready_drain`]) and its
/// lock held for the whole of `under`. Else why the drain could not be readied or locked, or `under`'s own error.
fn locked<T>(root: &HeldRoot<'_>, seams: &BoxSeams, console: &mut Console, under: impl FnOnce(&State<'_>) -> Result<T, String>) -> Result<T, String> {
    let drain_path = root.path.join(DRAIN);
    let security = protected_security()?;
    let drain = ready_drain(root.held, &drain_path, &security, console)?;
    let _lock = take_drain_lock(&drain_path.join(LOCK))?;
    let state = State {
        drain: &drain,
        security: &security,
        clock: Clock::of(seams),
        readable_only: seams.drain_readable_only == gate_args::ProcessScan::ReadableOnly,
        root: root.path,
        scanned_root: root_text(root.held)?,
    };
    under(&state)
}

/// Registers the admitted call of `session`: its record, its holder, held from now on, its end stamped at
/// `--test-box-now`'s second `test_now` when given, and its ticket given up.
fn registered(
    state: &State<'_>,
    tickets_folder: &File,
    tickets: &[Ticket],
    session: Option<&Session<'_>>,
    test_now: Option<u64>,
) -> Result<Admitted, String> {
    let Some(session) = session else {
        return Ok(Admitted { holder: None });
    };
    let folder = sessions_folder(state)?;
    let kept = record_fields(&folder, session.id).map_err(|error| state.cannot("read", session.id, &error))?;
    // A call that names no command or job (a preflight, an `inputs` or `lfs` call) keeps the ones its run recorded.
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
        "Called": state.clock.now_s(),
    });
    write_text(&folder, session.id, &record.to_string(), state.security).map_err(|error| state.cannot("write", session.id, &error))?;
    let holder = holding(state, folder, session.id, test_now)?;
    for ticket in tickets.iter().filter(|ticket| ticket.session == session.id) {
        let name = ticket_name(ticket.seq, &ticket.session);
        removed(tickets_folder, &name).map_err(|error| state.cannot("remove", &name, &error))?;
    }
    Ok(Admitted { holder: Some(holder) })
}

/// The hold of the session `id` in its sessions folder `folder` ([`Holder`]), its holder held from now on and its end
/// stamped at `test_now` when given.
fn holding(state: &State<'_>, folder: File, id: &str, test_now: Option<u64>) -> Result<Holder, String> {
    Ok(Holder {
        file: held_holder(state, &folder, id)?,
        folder,
        id: id.to_owned(),
        // The same security the state holds, made again: a descriptor is the system's allocation and is not copied.
        security: protected_security()?,
        test_now,
        lock: state.root.join(DRAIN).join(LOCK),
    })
}

/// The first `most` characters of `text`.
fn capped(text: &str, most: usize) -> String {
    text.chars().take(most).collect()
}

/// `drain\sessions\`, made as a protected folder when it is missing.
fn sessions_folder(state: &State<'_>) -> Result<File, String> {
    protected_folder(
        state.drain,
        &state.root.join(DRAIN).join(SESSIONS),
        FILE_GENERIC_READ,
        state.security,
        Leftover::Refuse,
    )
}

/// This process's holder of the session `id` in its sessions folder `folder`, made or opened, shared with no one.
fn held_holder(state: &State<'_>, folder: &File, id: &str) -> Result<File, String> {
    let holder_name = format!("{id}.{}", std::process::id());
    held_share_none(folder, &holder_name, state.security).map_err(|error| state.cannot("hold", &holder_name, &error))
}

/// What an `outputs` call may do under the drain: the hold of its session while it sends, when the session is live, and
/// whether it may remove the archive it sent.
pub(crate) struct Renewal {
    pub(crate) held: Option<Admitted>,
    pub(crate) may_delete: bool,
}

/// Renews the session `session` for an `outputs` call under the root `root`, in one hold of the drain lock: a live one
/// ([`State::own_session_live`]) gets its record's `Called` set to now, every other field kept, and its holder held
/// until the [`Renewal`] drops, and may delete; a dead or missing one is held by nothing and may delete only while no
/// barrier stands. A root with no `drain\` has no barrier, and the call makes none. Else why the drain could not be
/// read or written.
pub(crate) fn renewed(root: &HeldRoot<'_>, session: Option<&str>, seams: &BoxSeams, console: &mut Console) -> Result<Renewal, String> {
    let drain_path = root.path.join(DRAIN);
    if existing(root.held, DRAIN).map_err(|error| cannot("read", &drain_path, &error))?.is_none() {
        return Ok(Renewal {
            held: None,
            may_delete: true,
        });
    }
    locked(root, seams, console, |state| {
        let standing = matches!(state.barrier()?, Barrier::Standing(_));
        match session {
            Some(id) if state.own_session_live(id)? => Ok(Renewal {
                held: Some(renew(state, id, seams.now)?),
                may_delete: true,
            }),
            _ => Ok(Renewal {
                held: None,
                may_delete: !standing,
            }),
        }
    })
}

/// Sets the live session `id`'s record's `Called` to now, keeping its other fields (none for a record that does not
/// read), and holds its holder, its end stamped at `--test-box-now`'s second `test_now` when given.
fn renew(state: &State<'_>, id: &str, test_now: Option<u64>) -> Result<Admitted, String> {
    let folder = sessions_folder(state)?;
    let mut record = record_fields(&folder, id).map_err(|error| state.cannot("read", id, &error))?;
    record.insert("Called".to_owned(), json!(state.clock.now_s()));
    write_text(&folder, id, &Value::Object(record).to_string(), state.security).map_err(|error| state.cannot("write", id, &error))?;
    Ok(Admitted {
        holder: Some(holding(state, folder, id, test_now)?),
    })
}

/// Runs `under` in one hold of the drain lock under the root `root`, `drain\` readied as an admission readies it, giving
/// it the judge of whether a session is live for a call of its own ([`State::own_session_live`]): the one lock every
/// take and judge of the builder's own slots runs under. Else why the drain could not be read or written, or `under`'s
/// own error.
pub(super) fn under_drain_lock<T>(
    root: &HeldRoot<'_>,
    seams: &BoxSeams,
    console: &mut Console,
    under: impl FnOnce(&dyn Fn(&str) -> Result<bool, String>) -> Result<T, String>,
) -> Result<T, String> {
    locked(root, seams, console, |state| under(&|id| state.own_session_live(id)))
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

/// The root of the work folder `work` (`<root>\work\<key>`) and its folder, held as the preflight holds it: read
/// access, no deletion shared. `None` when nothing is at the root's path, where no barrier can stand. Else why not: a
/// link, a junction or anything but a folder there, or a work folder with no root above it.
pub(super) fn held_root_of(work: &str) -> Result<Option<(std::path::PathBuf, File)>, String> {
    let root = gate_wire::work_root(work, PathForm::Windows).ok_or_else(|| format!(r"the work folder {} is not <root>\work\<key>", escaped(work)))?;
    let root = std::path::PathBuf::from(root);
    Ok(held_root_at(&root)?.map(|held| (root, held)))
}

/// The root `root`, held as the preflight holds it ([`held_root_of`]); `None` when nothing is at its path. Else why
/// not: a link, a junction or anything but a folder there.
pub(super) fn held_root_at(root: &Path) -> Result<Option<File>, String> {
    if fs::symlink_metadata(root).is_err_and(|error| error.kind() == io::ErrorKind::NotFound) {
        return Ok(None);
    }
    let held = preflight::held_folder_at(root)
        .map_err(|error| cannot("open", root, &error))?
        .ok_or_else(|| format!("{} is a link, a junction or not a folder", escaped(&root.to_string_lossy())))?;
    Ok(Some(held))
}

/// Passes a call of `run`, `inputs` or `lfs` through the drain under the work folder `work`'s root as the session
/// `payload` names ([`session_of`]), with a run's command text and job `run` (both empty for `inputs` and `lfs`),
/// taking no ticket: the guard, held until the verb returns, or the status to exit with, its lines said
/// ([`admission_said`]), the usage line and [`USAGE_STATUS`] for a session the payload gets wrong. A root not there yet
/// admits the call, holding nothing.
pub(crate) fn passed(work: &str, payload: &Payload, run: Option<(&str, &str)>, seams: &BoxSeams, console: &mut Console) -> Result<Admitted, u32> {
    let (command, job) = run.unwrap_or(("", ""));
    let session = match session_of(payload, command, job) {
        Ok(session) => session,
        Err(line) => {
            stamping::err(console, &line);
            return Err(u32::from(USAGE_STATUS));
        }
    };
    let (path, held) = match held_root_of(work) {
        Ok(Some(held)) => held,
        Ok(None) => return Ok(Admitted { holder: None }),
        Err(why) => return Err(drain_failed(&why, console)),
    };
    let root = HeldRoot { held: &held, path: &path };
    admission_said(admit(&root, session.as_ref(), true, seams, console), payload, None, console)
}

/// The guard of `admission`, or the status to exit with, its lines said: for a refusal, `first` when given on stdout,
/// then on stderr, unstamped as every refusal tag is, the `drain` refusal tag of `payload`'s nonce (left out for a
/// payload without one, as a preflight's root refusal reads none) and the [`refused_lines`], and status 75; for a
/// drain that could not be read or written, `first` when given on stdout, then `gate: drain failed: {why}` on stderr
/// and [`FAILED_STATUS`].
pub(crate) fn admission_said(
    admission: Result<Result<Admitted, Refused>, String>,
    payload: &Payload,
    first: Option<&str>,
    console: &mut Console,
) -> Result<Admitted, u32> {
    let not_admitted = match admission {
        Ok(Ok(admitted)) => return Ok(admitted),
        Ok(Err(refused)) => Ok(refused),
        Err(why) => Err(why),
    };
    if let Some(first) = first {
        stamping::out(console, first);
    }
    let refused = match not_admitted {
        Ok(refused) => refused,
        Err(why) => return Err(drain_failed(&why, console)),
    };
    if let Ok(nonce) = payload.nonce() {
        stamping::protocol(console, &gate_wire::refused_line(nonce, gate_wire::Refusal::Drain));
    }
    for line in refused_lines(refused) {
        stamping::protocol(console, &line);
    }
    Err(crate::boxside::run::REFUSED_STATUS)
}

/// Says `gate: drain failed: {why}`; [`FAILED_STATUS`].
fn drain_failed(why: &str, console: &mut Console) -> u32 {
    stamping::err(console, &format!("gate: drain failed: {why}"));
    u32::from(FAILED_STATUS)
}

/// [`renewed`] for an `outputs` call of `session` under the work folder `work`'s root; a root not there yet has no
/// barrier, holds nothing and may delete. Else why the drain could not be read.
pub(crate) fn renewed_at(work: &str, session: Option<&str>, seams: &BoxSeams, console: &mut Console) -> Result<Renewal, String> {
    match held_root_of(work)? {
        Some((path, held)) => renewed(&HeldRoot { held: &held, path: &path }, session, seams, console),
        None => Ok(Renewal {
            held: None,
            may_delete: true,
        }),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make or read its drain must fail loudly")]
mod tests {
    use std::fs;

    use gate_args::BoxSeams;
    use jiff::Timestamp;
    use proc_table::ProcessInfo;
    use serde_json::{Value, json};

    use gate_wire::PathForm;
    use gate_wire::drain::{LAPSE_MS, Refused, counted_processes};

    use super::{HeldRoot, Session, admit, renewed};
    use crate::boxside::preflight;
    use crate::console::Console;

    use super::{barrier_stands, root_text};

    /// A scratch root `gate-drain-{name}-{pid}` under the temp folder, made empty, and held as the preflight holds one.
    fn scratch_root(name: &str) -> (std::path::PathBuf, std::fs::File) {
        let root = crate::test_support::temp_dir().join(format!("gate-drain-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("an earlier scratch root can be removed");
        }
        fs::create_dir_all(&root).expect("the scratch root can be made");
        let held = preflight::held_folder_at(&root)
            .expect("the scratch root can be opened")
            .expect("the scratch root is a folder");
        (root, held)
    }

    #[test]
    fn a_dead_ticket_is_removed_and_its_session_returns_at_the_back() {
        const T: u64 = 1_800_000_000;
        let (root, held) = scratch_root("tickets");
        let held_root = HeldRoot { held: &held, path: &root };
        let mut console = Console::new();
        assert!(
            admit(&held_root, None, true, &BoxSeams::default(), &mut console)
                .expect("the root can be read")
                .is_ok(),
            "a call with no session under a root with no drain is admitted"
        );
        assert!(!root.join("drain").exists(), "and makes no drain folder");
        let other = Session {
            id: "z",
            pc: "",
            repo: "",
            command: "",
            job: "",
        };
        let first = admit(&held_root, Some(&other), true, &BoxSeams::default(), &mut console).expect("the drain can be read");
        drop(first);
        fs::write(
            root.join("drain").join("barrier"),
            json!({ "Raised": T, "Lapses": T + 1800, "Generation": 1 }).to_string(),
        )
        .expect("a barrier can be written");
        let session = |id: &'static str| Session {
            id,
            pc: "pc1",
            repo: "repo1",
            command: "gate run x",
            job: "",
        };
        let mut call = |id: &'static str, now: u64| {
            let seams = BoxSeams {
                now: Some(now),
                ..BoxSeams::default()
            };
            admit(&held_root, Some(&session(id)), false, &seams, &mut console)
                .expect("the drain can be read")
                .err()
        };

        call("a", T);
        call("b", T + 200);
        call("c", T + 200);
        let back = call("a", T + 200);

        let mut names: Vec<String> = fs::read_dir(root.join("drain").join("tickets"))
            .expect("the tickets can be listed")
            .map(|entry| entry.expect("a ticket can be read").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["00000001-b", "00000002-c", "00000003-a"], "a's dead ticket was removed");
        assert_eq!(back.and_then(|refused| refused.place), Some(3), "a returns behind c");
    }

    #[test]
    fn the_root_is_matched_by_the_held_folder_s_final_path_not_the_payload_s_spelling() {
        let (root, _made) = scratch_root("spelling");
        let held = preflight::held_folder_at(&root.join("."))
            .expect("the root spelled with a trailing dot can be opened")
            .expect("it is a folder");
        let named = format!("git-receive-pack '{}/k.git'", root.to_string_lossy().replace('\\', "/"));
        let text = root_text(&held).expect("the held root's final path can be read");
        let counted = counted_processes(
            vec![process(10, 1, "git-receive-pack.exe", Some(&named))],
            &text,
            0,
            true,
            PathForm::Windows,
        );
        assert_eq!(counted.into_keys().collect::<Vec<_>>(), [10], "{text} {named}");
    }

    #[test]
    fn an_admission_registers_and_holds_its_session_and_a_later_one_under_a_barrier_takes_ticket_1() {
        const T: u64 = 1_800_000_000;
        let root = crate::test_support::temp_dir().join(format!("gate-drain-admit-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("an earlier scratch root can be removed");
        }
        fs::create_dir_all(&root).expect("the scratch root can be made");
        let held = preflight::held_folder_at(&root)
            .expect("the scratch root can be opened")
            .expect("the scratch root is a folder");
        let held_root = HeldRoot { held: &held, path: &root };
        let seams = BoxSeams {
            now: Some(T),
            ..BoxSeams::default()
        };
        let mut console = Console::new();
        let session = |id: &'static str| Session {
            id,
            pc: "pc1",
            repo: "repo1",
            command: "gate run cargo test",
            job: "",
        };
        let read = |path: &std::path::Path| -> Value {
            serde_json::from_str(&fs::read_to_string(path).expect("the record can be read")).expect("the record is JSON")
        };

        let first = admit(&held_root, Some(&session("s1")), false, &seams, &mut console).expect("the drain can be read");
        let sessions = root.join("drain").join("sessions");
        let holder = sessions.join(format!("s1.{}", std::process::id()));
        assert!(first.is_ok(), "with no barrier and no ticket the call is admitted");
        assert_eq!(
            read(&sessions.join("s1")),
            json!({ "Pc": "pc1", "Repo": "repo1", "Command": "gate run cargo test", "Job": "", "Called": T })
        );
        assert!(holder.exists(), "the admitted call holds its holder");
        drop(first);
        assert!(!holder.exists(), "dropping the guard deletes the holder");

        fs::write(
            root.join("drain").join("barrier"),
            json!({ "Raised": T, "Lapses": T + 1800, "Generation": 1 }).to_string(),
        )
        .expect("a barrier can be written");
        let second = admit(&held_root, Some(&session("s2")), false, &seams, &mut console).expect("the drain can be read");

        assert_eq!(
            second.as_ref().err(),
            Some(&Refused {
                place: Some(1),
                raised: Some(T),
                lapses: Some(T + 1800)
            }),
            "a standing barrier refuses a new session, first in line"
        );
        let ticket = root.join("drain").join("tickets").join("00000001-s2");
        assert_eq!(
            read(&ticket),
            json!({ "Session": "s2", "Polled": T }),
            "the refused session takes ticket 1"
        );

        let live = admit(&held_root, Some(&session("s1")), false, &seams, &mut console).expect("the drain can be read");
        assert!(live.is_ok(), "s1, called within the lease, is admitted under the barrier");
        drop(live);
        let called_long_ago = json!({ "Pc": "pc1", "Repo": "repo1", "Command": "x", "Job": "", "Called": T - 1000 });
        fs::write(sessions.join("s3"), called_long_ago.to_string()).expect("a dead session's record can be written");
        let dead = admit(&held_root, Some(&session("s3")), false, &seams, &mut console).expect("the drain can be read");
        assert_eq!(dead.err().and_then(|refused| refused.place), Some(2), "a dead session waits behind s2");
        assert_eq!(
            read(&root.join("drain").join("tickets").join("00000002-s3")),
            json!({ "Session": "s3", "Polled": T }),
            "the dead session takes a ticket"
        );
    }

    /// Writes a barrier raised at `raised` under the scratch root `root`'s `drain\`.
    fn raise_at(root: &std::path::Path, raised: u64) {
        fs::write(
            root.join("drain").join("barrier"),
            json!({ "Raised": raised, "Lapses": raised + 1800, "Generation": 1 }).to_string(),
        )
        .expect("a barrier can be written");
    }

    #[test]
    fn an_outputs_call_of_a_live_session_renews_and_holds_it() {
        const T: u64 = 1_800_000_000;
        let (root, held) = scratch_root("renew-live");
        let held_root = HeldRoot { held: &held, path: &root };
        let at = |now: u64| BoxSeams {
            now: Some(now),
            ..BoxSeams::default()
        };
        let mut console = Console::new();
        let session = Session {
            id: "s1",
            pc: "pc1",
            repo: "repo1",
            command: "cargo test",
            job: "job1",
        };
        drop(admit(&held_root, Some(&session), false, &at(T), &mut console).expect("the drain can be read"));
        raise_at(&root, T + 10);

        let renewal = renewed(&held_root, Some("s1"), &at(T + 60), &mut console).expect("the drain can be read");

        let sessions = root.join("drain").join("sessions");
        let holder = sessions.join(format!("s1.{}", std::process::id()));
        assert!(renewal.may_delete, "a live session may delete its archive under a barrier");
        assert!(renewal.held.is_some() && holder.exists(), "the renewal holds the session's holder");
        let record: Value = serde_json::from_str(&fs::read_to_string(sessions.join("s1")).expect("the record can be read")).expect("JSON");
        assert_eq!(
            record,
            json!({ "Pc": "pc1", "Repo": "repo1", "Command": "cargo test", "Job": "job1", "Called": T + 60 }),
            "only Called is renewed"
        );
        drop(renewal);
        assert!(!holder.exists(), "dropping the renewal lets the holder go");
    }

    #[test]
    fn an_outputs_call_of_a_dead_session_may_delete_only_with_no_barrier_standing() {
        const T: u64 = 1_800_000_000;
        let (root, held) = scratch_root("renew-dead");
        let held_root = HeldRoot { held: &held, path: &root };
        let seams = BoxSeams {
            now: Some(T),
            ..BoxSeams::default()
        };
        let mut console = Console::new();
        let other = Session {
            id: "z",
            pc: "",
            repo: "",
            command: "",
            job: "",
        };
        drop(admit(&held_root, Some(&other), true, &seams, &mut console).expect("the drain can be read"));
        let sessions = root.join("drain").join("sessions");
        fs::create_dir_all(&sessions).expect("the sessions folder can be made");
        let called_long_ago = json!({ "Pc": "pc1", "Repo": "repo1", "Command": "x", "Job": "", "Called": T - 1000 });
        fs::write(sessions.join("s3"), called_long_ago.to_string()).expect("a dead session's record can be written");

        let open = renewed(&held_root, Some("s3"), &seams, &mut console).expect("the drain can be read");
        assert!(open.held.is_none() && open.may_delete, "with no barrier a dead session may delete");
        raise_at(&root, T);
        let drained = renewed(&held_root, Some("s3"), &seams, &mut console).expect("the drain can be read");
        assert!(drained.held.is_none(), "a dead session is not held");
        assert!(!drained.may_delete, "under a barrier a dead session may not delete");
        let missing = renewed(&held_root, None, &seams, &mut console).expect("the drain can be read");
        assert!(missing.held.is_none() && !missing.may_delete, "nor may a call with no session");
        let record: Value = serde_json::from_str(&fs::read_to_string(sessions.join("s3")).expect("the record can be read")).expect("JSON");
        assert_eq!(record, called_long_ago, "a dead session's record is not renewed");
    }

    #[test]
    fn an_outputs_call_under_a_root_with_no_drain_makes_none_and_may_delete() {
        let (root, held) = scratch_root("renew-none");
        let held_root = HeldRoot { held: &held, path: &root };
        let renewal = renewed(&held_root, Some("s1"), &BoxSeams::default(), &mut Console::new()).expect("the root can be read");
        assert!(renewal.held.is_none() && renewal.may_delete);
        assert!(!root.join("drain").exists(), "outputs makes no drain folder");
    }

    /// The JSON of the session `id`'s record under the scratch root `root`.
    fn session_record(root: &std::path::Path, id: &str) -> Value {
        let text = fs::read_to_string(root.join("drain").join("sessions").join(id)).expect("the record can be read");
        serde_json::from_str(&text).expect("the record is JSON")
    }

    /// `BoxSeams` whose clock reads the Unix second `now`.
    fn at(now: u64) -> BoxSeams {
        BoxSeams {
            now: Some(now),
            ..BoxSeams::default()
        }
    }

    #[test]
    fn a_session_s_lease_runs_from_the_end_of_its_last_call() {
        const T: u64 = 1_800_000_000;
        let (root, held) = scratch_root("end-stamp");
        let held_root = HeldRoot { held: &held, path: &root };
        let mut console = Console::new();
        let session = Session {
            id: "s1",
            pc: "pc1",
            repo: "repo1",
            command: "cargo test",
            job: "job1",
        };
        let mut admitted = admit(&held_root, Some(&session), false, &at(T), &mut console)
            .expect("the drain can be read")
            .expect("with no barrier the call is admitted");
        // The call ends 200 s after its admission, past the lease counted from the admission.
        admitted.holder.as_mut().expect("an admitted session is held").test_now = Some(T + 200);

        drop(admitted);

        assert_eq!(
            session_record(&root, "s1"),
            json!({ "Pc": "pc1", "Repo": "repo1", "Command": "cargo test", "Job": "job1", "Called": T + 200 }),
            "the end of the call is stamped, every other field kept"
        );
        raise_at(&root, T + 210);
        let later = admit(&held_root, Some(&session), true, &at(T + 250), &mut console).expect("the drain can be read");
        assert!(
            later.is_ok(),
            "the session is live within its lease from its call's end: {:?}",
            later.err()
        );
    }

    #[test]
    fn a_call_naming_no_command_or_job_keeps_the_ones_its_run_recorded() {
        const T: u64 = 1_800_000_000;
        let (root, held) = scratch_root("keep-run");
        let held_root = HeldRoot { held: &held, path: &root };
        let mut console = Console::new();
        let run = Session {
            id: "s1",
            pc: "pc1",
            repo: "repo1",
            command: "cargo test",
            job: "job1",
        };
        let preflight = Session { command: "", job: "", ..run };
        drop(admit(&held_root, Some(&run), false, &at(T), &mut console).expect("the drain can be read"));

        drop(admit(&held_root, Some(&preflight), false, &at(T + 5), &mut console).expect("the drain can be read"));

        assert_eq!(
            session_record(&root, "s1"),
            json!({ "Pc": "pc1", "Repo": "repo1", "Command": "cargo test", "Job": "job1", "Called": T + 5 }),
            "the run's command and job are kept"
        );
    }

    #[test]
    fn a_session_record_caps_its_pc_and_repo_at_100_characters_and_its_command_at_300() {
        let (root, held) = scratch_root("caps");
        let held_root = HeldRoot { held: &held, path: &root };
        let (pc, repo, command) = ("p".repeat(150), "r".repeat(150), "c".repeat(400));
        let session = Session {
            id: "s1",
            pc: &pc,
            repo: &repo,
            command: &command,
            job: "job1",
        };

        drop(admit(&held_root, Some(&session), false, &at(1_800_000_000), &mut Console::new()).expect("the drain can be read"));

        let record = session_record(&root, "s1");
        assert_eq!(record["Pc"], json!("p".repeat(100)));
        assert_eq!(record["Repo"], json!("r".repeat(100)));
        assert_eq!(record["Command"], json!("c".repeat(300)));
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

    /// The seams of a box at the Unix second `now_s`, with the default lapse.
    fn seams_at(now_s: u64) -> BoxSeams {
        BoxSeams {
            now: Some(now_s),
            ..BoxSeams::default()
        }
    }

    /// A scratch root `gate-drain-stands-{name}-{pid}` under the temp folder, made empty.
    fn stands_root(name: &str) -> std::path::PathBuf {
        let root = crate::test_support::temp_dir().join(format!("gate-drain-stands-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("an earlier scratch root can be removed");
        }
        fs::create_dir_all(&root).expect("the scratch root can be made");
        root
    }

    #[test]
    fn no_barrier_stands_under_a_missing_root_a_root_without_drain_or_a_drain_without_a_barrier() {
        let root = stands_root("none");
        let missing = root.join("missing");
        let without_drain = barrier_stands(&root, &seams_at(1000));
        fs::create_dir(root.join("drain")).expect("the drain folder can be made");
        let without_barrier = barrier_stands(&root, &seams_at(1000));
        let at_missing = barrier_stands(&missing, &seams_at(1000));
        fs::remove_dir_all(&root).expect("the scratch root can be removed");

        assert_eq!(at_missing, Ok(false), "a root that is not there");
        assert_eq!(without_drain, Ok(false), "a root without drain");
        assert_eq!(without_barrier, Ok(false), "a drain without a barrier");
    }

    #[test]
    fn a_drain_that_is_a_junction_or_a_file_holds_no_barrier() {
        let root = stands_root("not-a-folder");
        let elsewhere = stands_root("not-a-folder-target");
        fs::write(elsewhere.join("barrier"), r#"{"Raised":1000,"Lapses":2800,"Generation":1}"#).expect("the barrier can be written");
        crate::test_support::junction(&root.join("drain"), &elsewhere);
        let through_junction = barrier_stands(&root, &seams_at(2000));
        fs::remove_dir(root.join("drain")).expect("the junction can be removed");
        fs::write(root.join("drain"), r#"{"Raised":1000,"Lapses":2800,"Generation":1}"#).expect("a drain file can be written");
        let as_file = barrier_stands(&root, &seams_at(2000));
        fs::remove_dir_all(&root).expect("the scratch root can be removed");
        fs::remove_dir_all(&elsewhere).expect("the junction's target can be removed");

        assert_eq!(through_junction, Ok(false), "a junction at drain is never followed");
        assert_eq!(as_file, Ok(false), "a file at drain holds no barrier");
    }

    #[test]
    fn a_barrier_stands_until_its_lapse_read_only() {
        let root = stands_root("standing");
        fs::create_dir(root.join("drain")).expect("the drain folder can be made");
        let barrier = root.join(r"drain\barrier");
        let text = r#"{"Raised":1000,"Lapses":2800,"Generation":1}"#;
        fs::write(&barrier, text).expect("the barrier can be written");

        let standing = barrier_stands(&root, &seams_at(2799));
        let lapsed = barrier_stands(&root, &seams_at(2800));
        let left = fs::read_to_string(&barrier);
        let lapsed_record = root.join(r"drain\lapsed").exists();
        fs::remove_dir_all(&root).expect("the scratch root can be removed");

        assert_eq!(standing, Ok(true), "it stands before its lapse");
        assert_eq!(lapsed, Ok(false), "it no longer stands at its lapse");
        assert_eq!(left.ok().as_deref(), Some(text), "the read changes nothing");
        assert!(!lapsed_record, "the read writes no lapsed record");
    }

    #[test]
    fn an_unreadable_barrier_file_stands_for_the_reader_until_its_last_write_plus_the_lapse() {
        let root = stands_root("unreadable");
        fs::create_dir(root.join("drain")).expect("the drain folder can be made");
        let barrier = root.join(r"drain\barrier");
        fs::write(&barrier, "{not json").expect("the barrier can be written");
        let written = fs::metadata(&barrier)
            .and_then(|metadata| metadata.modified())
            .expect("the barrier's last write can be read");
        let written_s = written.duration_since(std::time::UNIX_EPOCH).expect("after 1970").as_secs();

        let within = barrier_stands(&root, &seams_at(written_s + LAPSE_MS / 1000 - 1));
        let past = barrier_stands(&root, &seams_at(written_s + LAPSE_MS / 1000));
        fs::remove_dir_all(&root).expect("the scratch root can be removed");

        assert_eq!(within, Ok(true), "it stands within a lapse of its last write");
        assert_eq!(past, Ok(false), "it no longer stands a lapse after its last write");
    }
}
