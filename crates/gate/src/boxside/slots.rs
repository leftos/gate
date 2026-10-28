//! The builder's own slot pool on the Windows box: the count `{root}\slots.json`, and the claims `slots\{n}.json` in
//! `{root}\slots\`, a protected folder made as `drain\` is. A routed run's preflight takes a slot for its drain session
//! ([`taken`]), its run holds it, taking a free one again for a session whose claim lapsed and was taken
//! ([`run_taken`]), and the run's end lets it go ([`Release`]); `gate box hold` takes one for a command it runs
//! ([`hold`]), keeping `slots\{n}.lock` open, shared with no one, for its whole life. Every take and judge runs under the
//! drain's lock ([`drain::under_drain_lock`]), so two takers never get one slot: a session claim is live while its
//! session is live for a call of its own, a hold claim while its lock is held, and a dead claim is removed by the taker
//! that finds it. Every claim in `slots\` counts against the count, one past a lowered count too. With no count file,
//! nothing here takes a slot.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use gate_args::{BoxRequest, BoxSeams, HoldRequest};
use gate_win32::{Descriptor, Job, Launch, LaunchOutput, Running};
use gate_wire::drain::{DRAIN, LOCK};
use gate_wire::slots::{
    COUNT_FILE, Claim, HOLD_LINE_POLLS, HOLD_POLL, HoldClaim, Judged, NO_COUNT_LINE, SLOTS, SessionClaim, Take, ceiling_line, chosen, claim_name,
    count_of, full_line, holder_line, holder_refusal, lock_name, unread_count_line, waiting_line,
};
use gate_wire::{Payload, Refusal, escaped, refused_line};
use jiff::Timestamp;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::drain::{self, HeldRoot, Session, cannot};
use crate::boxside::inputs::{Leftover, protected_folder, protected_security};
use crate::boxside::run::REFUSED_STATUS;
use crate::boxside::stamping;
use crate::console::Console;
use crate::environment::Variables;
use crate::run::{Written, find_program, written};

/// The nonce a hold's `full` refusal tag carries, a hold having no payload to take one from.
const HOLD_NONCE: &str = "hold";
/// A hold's status when its ceiling ended its command.
const CEILING_STATUS: u32 = 124;
/// A hold's status when its command could not be started.
const NOT_STARTED_STATUS: u32 = 127;

/// The slot count of the held root `held` at `root`: `None` with no count file. Else why the file cannot be read or is
/// malformed, which counts as no file.
fn count_at(held: &File, root: &Path) -> Result<Option<u32>, String> {
    let path = root.join(COUNT_FILE);
    let Some(mut file) = drain::existing(held, COUNT_FILE).map_err(|error| cannot("open", &path, &error))? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| cannot("read", &path, &error))?;
    count_of(&String::from_utf8_lossy(&bytes))
        .map(Some)
        .map_err(|why| format!("{}: {why}", escaped(&path.to_string_lossy())))
}

/// `slots\` beneath the held root `held` at `root`, made as a protected folder when missing and refused as another
/// account's or a link, with the security its files are written with.
fn slots_folder(held: &File, root: &Path) -> Result<(File, Descriptor), String> {
    let security = protected_security()?;
    let folder = protected_folder(held, &root.join(SLOTS), FILE_GENERIC_READ, &security, Leftover::Refuse)?;
    Ok((folder, security))
}

/// The index of a slot's claim `{n}.json` or lock `{n}.lock` named `name`; `None` for any other name.
fn slot_index(name: &str) -> Option<usize> {
    let (stem, extension) = name.rsplit_once('.')?;
    let known = extension.eq_ignore_ascii_case("json") || extension.eq_ignore_ascii_case("lock");
    if !known || stem.is_empty() || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// Every slot with a claim or a lock in `folder` at `path`, whatever its index, judged under the drain lock
/// ([`judged_slot`]).
fn judged(folder: &File, path: &Path, session_live: &dyn Fn(&str) -> Result<bool, String>) -> Result<Vec<Judged>, String> {
    let names = gate_win32::names_beneath(folder).map_err(|error| cannot("list", path, &error))?;
    let indexes: BTreeSet<usize> = names.iter().filter_map(|name| name.to_str()).filter_map(slot_index).collect();
    indexes.into_iter().map(|n| judged_slot(folder, path, n, session_live)).collect()
}

/// Slot `n` in `folder` at `path`, judged under the drain lock: live while its lock is held ([`drain::held_open`]) or
/// its claim's session is (`session_live`); a dead claim is removed.
fn judged_slot(folder: &File, path: &Path, n: usize, session_live: &dyn Fn(&str) -> Result<bool, String>) -> Result<Judged, String> {
    let (lock, name) = (lock_name(n), claim_name(n));
    let held = drain::held_open(folder, &lock).map_err(|error| cannot("remove", &path.join(&lock), &error))?;
    let read = drain::read_text(folder, &name).map_err(|error| cannot("read", &path.join(&name), &error))?;
    // A claim that does not read names no session, and is live only while its lock is held.
    let session = read
        .as_ref()
        .and_then(|(text, _)| Claim::of(text).ok())
        .and_then(|claim| claim.session().map(str::to_owned));
    let live = held
        || match session.as_deref() {
            Some(id) => session_live(id)?,
            None => false,
        };
    if !live && read.is_some() {
        drain::removed(folder, &name).map_err(|error| cannot("remove", &path.join(&name), &error))?;
    }
    Ok(Judged {
        n,
        live,
        session: session.filter(|_| live),
    })
}

/// Now in UTC, or `--test-box-now`'s second.
fn now_text(seams: &BoxSeams) -> String {
    seams
        .now
        .and_then(|second| i64::try_from(second).ok())
        .and_then(|second| Timestamp::from_second(second).ok())
        .unwrap_or_else(Timestamp::now)
        .to_string()
}

/// What a session's look for its slot found.
enum SessionTake {
    /// The session holds a slot, kept or taken now.
    Held,
    /// The root has no count, so the session needs no slot and holds none.
    Unneeded,
    /// Every slot is held: `held` of `count`.
    Full { held: usize, count: u32 },
}

/// The slot count of the root `root`; `None` with no count file, and for one that cannot be read, said on `console`.
fn count_said(root: &HeldRoot<'_>, console: &mut Console) -> Option<u32> {
    match count_at(root.held, root.path) {
        Ok(count) => count,
        Err(why) => {
            stamping::err(console, &unread_count_line(&why));
            None
        }
    }
}

/// Takes a slot for `session` under the root `root` when it has a count file ([`count_said`]): a session that holds a
/// slot keeps it, else it takes the lowest free one with its session claim. Else why the slot pool could not be read or
/// written.
fn session_took(root: &HeldRoot<'_>, session: &Session<'_>, seams: &BoxSeams, console: &mut Console) -> Result<SessionTake, String> {
    let Some(count) = count_said(root, console) else {
        return Ok(SessionTake::Unneeded);
    };
    let claim = Claim::Session(SessionClaim::capped(
        session.id,
        session.pc,
        session.repo,
        session.command,
        &now_text(seams),
    ));
    drain::under_drain_lock(root, seams, console, |session_live| {
        let (folder, security) = slots_folder(root.held, root.path)?;
        let path = root.path.join(SLOTS);
        match chosen(&judged(&folder, &path, session_live)?, count, Some(session.id)) {
            Take::Kept(_) => Ok(SessionTake::Held),
            Take::Free(n) => {
                let name = claim_name(n);
                drain::write_text(&folder, &name, &claim.text(), &security).map_err(|error| cannot("write", &path.join(&name), &error))?;
                Ok(SessionTake::Held)
            }
            Take::Full { held } => Ok(SessionTake::Full { held, count }),
        }
    })
}

/// Says the `full` refusal tag of `payload`'s nonce, unstamped as every refusal tag is, and the full line of `held` of
/// `count`; [`REFUSED_STATUS`].
fn full_said(payload: &Payload, held: usize, count: u32, console: &mut Console) -> u32 {
    if let Ok(nonce) = payload.nonce() {
        stamping::protocol(console, &refused_line(nonce, Refusal::Full));
    }
    stamping::protocol(console, &full_line(held, count));
    REFUSED_STATUS
}

/// Says `gate: slots failed: {why}`; [`FAILED_STATUS`].
fn slots_failed(why: &str, console: &mut Console) -> u32 {
    stamping::err(console, &format!("gate: slots failed: {why}"));
    u32::from(FAILED_STATUS)
}

/// Takes a slot for the admitted preflight of `session` under the root `root` when it has a count file
/// ([`session_took`]): whether the session now holds a claim, which is false with no session or no count file. With
/// every slot held: `version` on stdout, then the `full` refusal tag and the full line ([`full_said`]), and
/// [`REFUSED_STATUS`]. A slot pool that cannot be read or written fails the preflight: `version`, its line, and
/// [`FAILED_STATUS`].
pub(super) fn taken(
    root: &HeldRoot<'_>,
    session: Option<&Session<'_>>,
    payload: &Payload,
    version: &str,
    seams: &BoxSeams,
    console: &mut Console,
) -> Result<bool, u32> {
    let Some(session) = session else {
        return Ok(false);
    };
    let took = session_took(root, session, seams, console);
    if !matches!(took, Ok(SessionTake::Held | SessionTake::Unneeded)) {
        stamping::out(console, version);
    }
    claim_held(took, payload, console)
}

/// Whether the look `took` left the session holding a claim; with every slot held, the `full` refusal ([`full_said`]);
/// a pool that could not be read or written, `gate: slots failed: {why}` and [`FAILED_STATUS`].
fn claim_held(took: Result<SessionTake, String>, payload: &Payload, console: &mut Console) -> Result<bool, u32> {
    match took {
        Ok(SessionTake::Held) => Ok(true),
        Ok(SessionTake::Unneeded) => Ok(false),
        Ok(SessionTake::Full { held, count }) => Err(full_said(payload, held, count, console)),
        Err(why) => Err(slots_failed(&why, console)),
    }
}

/// Holds a slot for the admitted run of the session `payload` names, its command text `command` and job `job`, under
/// the work folder `work`'s root: the slot its preflight took, or a free one when its claim lapsed and was taken
/// ([`session_took`]): whether the session now holds a claim. With every slot held: the `full` refusal tag and the full
/// line ([`full_said`]), and [`REFUSED_STATUS`]. A run with no session, or under a root not there or with no count
/// file, takes none. Else `gate: slots failed: {why}` and [`FAILED_STATUS`].
pub(crate) fn run_taken(work: &str, payload: &Payload, run: (&str, &str), seams: &BoxSeams, console: &mut Console) -> Result<bool, u32> {
    let session = drain::session_of(payload, run.0, run.1).map_err(|why| slots_failed(&why, console))?;
    let Some(session) = session else {
        return Ok(false);
    };
    let (path, held) = match drain::held_root_of(work) {
        Ok(Some(held)) => held,
        Ok(None) => return Ok(false),
        Err(why) => return Err(slots_failed(&why, console)),
    };
    let root = HeldRoot { held: &held, path: &path };
    claim_held(session_took(&root, &session, seams, console), payload, console)
}

/// The end of a run of a session, or of its preflight that failed: dropped, it removes the session's claim under the
/// drain lock, so the slot is free for the next taker, and says `gate: slot release failed: {why}` on the box's stderr
/// when it could not. Made after the call's admission and so dropped before it, while the session's holder is still
/// held.
pub(crate) struct Release {
    work: String,
    session: Option<String>,
}

impl Release {
    /// The end of the run in the work folder `work` of the session `payload` names; one with no session lets nothing
    /// go.
    pub(crate) fn at_end(work: &str, payload: &Payload) -> Release {
        Release {
            work: work.to_owned(),
            session: payload.session().ok().flatten().map(str::to_owned),
        }
    }

    /// Keeps the session's slot: dropped now, it lets nothing go, as a preflight that passed needs.
    pub(crate) fn kept(mut self) {
        self.session = None;
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        if let Some(session) = &self.session
            && let Err(why) = released(&self.work, session)
        {
            // A drop is given no console; the box's own streams are the run's. A claim left behind is judged by its
            // session, which lapses at its lease.
            stamping::err(&mut Console::new(), &format!("gate: slot release failed: {why}"));
        }
    }
}

/// Removes every claim of `session` in the `slots\` of the work folder `work`'s root, under the drain lock; a root with
/// no `slots\` or no `drain\` holds none, no slot having been taken without them, and takes no lock. Else why not.
fn released(work: &str, session: &str) -> Result<(), String> {
    let Some((root, held)) = drain::held_root_of(work)? else {
        return Ok(());
    };
    let path = root.join(SLOTS);
    let Some(folder) = drain::existing(&held, SLOTS).map_err(|error| cannot("read", &path, &error))? else {
        return Ok(());
    };
    let drain_path = root.join(DRAIN);
    if drain::existing(&held, DRAIN)
        .map_err(|error| cannot("read", &drain_path, &error))?
        .is_none()
    {
        return Ok(());
    }
    let _lock = drain::take_drain_lock(&drain_path.join(LOCK))?;
    let names = gate_win32::names_beneath(&folder).map_err(|error| cannot("list", &path, &error))?;
    for name in names.iter().filter_map(|name| name.to_str()).filter(|name| {
        Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    }) {
        let read = drain::read_text(&folder, name).map_err(|error| cannot("read", &path.join(name), &error))?;
        let own = read.is_some_and(|(text, _)| Claim::of(&text).is_ok_and(|claim| claim.session() == Some(session)));
        if own {
            drain::removed(&folder, name).map_err(|error| cannot("remove", &path.join(name), &error))?;
        }
    }
    Ok(())
}

/// A slot a hold holds: its lock, open and shared with no one, and its claim, both let go when it drops, a failure said
/// as `gate: slot release failed: {why}` on the box's stderr.
struct HeldSlot {
    lock: File,
    folder: File,
    n: usize,
}

impl Drop for HeldSlot {
    fn drop(&mut self) {
        // While the lock is held no taker judges the slot free, so the claim is removed without the drain lock; a claim
        // or lock left behind is dead once this process ends, and the next taker removes it.
        let name = claim_name(self.n);
        let failures = [
            drain::removed(&self.folder, &name).err().map(|error| (name, error)),
            gate_win32::delete_opened(&self.lock).err().map(|error| (lock_name(self.n), error)),
        ];
        for (name, error) in failures.into_iter().flatten() {
            let why = format!("cannot remove {}: {}", escaped(&name), escaped(&error.to_string()));
            stamping::err(&mut Console::new(), &format!("gate: slot release failed: {why}"));
        }
    }
}

/// What one look for a free slot found.
enum Looked {
    /// A slot, held from now on.
    Slot(HeldSlot),
    /// None: this many slots are held.
    Full(usize),
}

/// One look, under the drain lock, for a free slot of the `count` under the root `root` for the hold `hold`.
fn look(root: &HeldRoot<'_>, count: u32, hold: &HoldRequest, seams: &BoxSeams, console: &mut Console) -> Result<Looked, String> {
    let claim = Claim::Hold(HoldClaim {
        holder: hold.holder.clone(),
        ceiling: hold.ceiling,
        pid: std::process::id(),
        started: Timestamp::now().to_string(),
    });
    drain::under_drain_lock(root, seams, console, |session_live| {
        let (folder, security) = slots_folder(root.held, root.path)?;
        let path = root.path.join(SLOTS);
        let n = match chosen(&judged(&folder, &path, session_live)?, count, None) {
            Take::Full { held } => return Ok(Looked::Full(held)),
            Take::Free(n) | Take::Kept(n) => n,
        };
        let (lock_file, name) = (lock_name(n), claim_name(n));
        let lock = drain::held_share_none(&folder, &lock_file, &security).map_err(|error| cannot("hold", &path.join(&lock_file), &error))?;
        let slot = HeldSlot { lock, folder, n };
        drain::write_text(&slot.folder, &name, &claim.text(), &security).map_err(|error| cannot("write", &path.join(&name), &error))?;
        Ok(Looked::Slot(slot))
    })
}

/// Answers `gate box hold`: takes one of the builder's own slots under its root and runs its command while it holds
/// it, then lets it go. The exit status: the command's own; [`USAGE_STATUS`] after [`NO_COUNT_LINE`] for a root with
/// no count file (after the line saying why, for one that cannot be read), or after the holder line for a holder
/// [`holder_refusal`] refuses; [`REFUSED_STATUS`] after the `full` tag and the full line when no slot came within its
/// wait ([`waited`]); [`CEILING_STATUS`] after the ceiling line when its ceiling ended the command;
/// [`NOT_STARTED_STATUS`] after the line saying why the command could not start; [`FAILED_STATUS`] after
/// `gate: hold failed: {why}`.
pub(super) fn hold(request: &BoxRequest, console: &mut Console) -> u32 {
    let (Some(root), Some(hold)) = (request.root.as_deref(), request.hold.as_ref()) else {
        // The parse gives `hold` its root and its words always; a request without them is the box's usage error.
        stamping::err(console, &gate_args::Problem::BoxUsage.to_string());
        return u32::from(USAGE_STATUS);
    };
    let seams = &request.seams;
    let (held, count) = match counted(root, console) {
        Ok(counted) => counted,
        Err(status) => return status,
    };
    if let Some(why) = holder_refusal(&hold.holder) {
        stamping::err(console, &holder_line(&hold.holder, &why));
        return u32::from(USAGE_STATUS);
    }
    if let Err(why) = gate_win32::check_work_root(root, seams.root_walk_stop.as_deref()) {
        return failed(&why.to_string(), console);
    }
    let held_root = HeldRoot { held: &held, path: root };
    match waited(&held_root, count, hold, seams, console) {
        Ok(slot) => {
            let status = held_run(&hold.command, hold.ceiling, console);
            drop(slot);
            status
        }
        Err(status) => status,
    }
}

/// A slot of the `count` under the root `root` for the hold `hold`, looked for until its wait ends, each look
/// [`HOLD_POLL`] apart (`--test-box-hold-poll-ms`) and the waiting line said every [`HOLD_LINE_POLLS`] looks. Else the
/// status to exit with, its lines said: the `full` tag and the full line and [`REFUSED_STATUS`] when none came, or
/// `gate: hold failed: {why}` and [`FAILED_STATUS`].
fn waited(root: &HeldRoot<'_>, count: u32, hold: &HoldRequest, seams: &BoxSeams, console: &mut Console) -> Result<HeldSlot, u32> {
    let poll = seams.hold_poll_ms.map_or(HOLD_POLL, Duration::from_millis);
    let Some(until) = Instant::now().checked_add(Duration::from_secs(hold.wait)) else {
        return Err(failed(&format!("its wait of {} s cannot be timed", hold.wait), console));
    };
    let mut looks: u64 = 0;
    loop {
        match look(root, count, hold, seams, console) {
            Ok(Looked::Slot(slot)) => return Ok(slot),
            Ok(Looked::Full(held)) if Instant::now() >= until => {
                stamping::protocol(console, &refused_line(HOLD_NONCE, Refusal::Full));
                stamping::protocol(console, &full_line(held, count));
                return Err(REFUSED_STATUS);
            }
            Ok(Looked::Full(held)) => {
                if looks.is_multiple_of(HOLD_LINE_POLLS) {
                    stamping::err(console, &waiting_line(held, count));
                }
                looks = looks.saturating_add(1);
                thread::sleep(poll);
            }
            Err(why) => return Err(failed(&why, console)),
        }
    }
}

/// The root `root`, held ([`drain::held_root_at`]), and its slot count; else the status to exit with, its lines said:
/// [`NO_COUNT_LINE`] and [`USAGE_STATUS`] for a root that is not there or has no count file that reads, after the line
/// saying why for one that does not read; `gate: hold failed: {why}` and [`FAILED_STATUS`] for a root that cannot be
/// held.
fn counted(root: &Path, console: &mut Console) -> Result<(File, u32), u32> {
    let no_count = |console: &mut Console| {
        stamping::err(console, NO_COUNT_LINE);
        u32::from(USAGE_STATUS)
    };
    let held = match drain::held_root_at(root) {
        Ok(Some(held)) => held,
        Ok(None) => return Err(no_count(console)),
        Err(why) => return Err(failed(&why, console)),
    };
    match count_at(&held, root) {
        Ok(Some(count)) => Ok((held, count)),
        Ok(None) => Err(no_count(console)),
        Err(why) => {
            stamping::err(console, &unread_count_line(&why));
            Err(no_count(console))
        }
    }
}

/// Says `gate: hold failed: {why}`; [`FAILED_STATUS`].
fn failed(why: &str, console: &mut Console) -> u32 {
    stamping::err(console, &format!("gate: hold failed: {why}"));
    u32::from(FAILED_STATUS)
}

/// Runs `command` in a job of its own that ends every process in it once closed, its stdout and stderr passed on to
/// the hold's own, and ends the job at `ceiling` seconds; the command's status, or [`CEILING_STATUS`] after the ceiling
/// line, or [`NOT_STARTED_STATUS`] after the line saying why it could not start.
fn held_run(command: &[OsString], ceiling: u64, console: &mut Console) -> u32 {
    let shown = command.first().map(|word| word.to_string_lossy().into_owned()).unwrap_or_default();
    let (job, running, stdout, stderr) = match launched(command) {
        Ok(launched) => launched,
        Err(why) => {
            stamping::err(console, &format!("gate: hold could not start {}: {}", escaped(&shown), escaped(&why)));
            return NOT_STARTED_STATUS;
        }
    };
    let (ended, passed) = thread::scope(|scope| {
        let out = scope.spawn(move || passed_on(stdout, io::stdout()));
        let err = scope.spawn(move || passed_on(stderr, io::stderr()));
        let ended = ended_by(&running, &job, ceiling);
        // Closing the job's one handle ends every process left in it, so no pipe stays open past here.
        drop(job);
        let passed: Vec<io::Result<()>> = [out.join(), err.join()]
            .into_iter()
            .map(|joined| joined.unwrap_or_else(|_| Err(io::Error::other("its reader panicked"))))
            .collect();
        (ended, passed)
    });
    for error in passed.into_iter().filter_map(Result::err) {
        stamping::err(console, &format!("gate: hold could not pass on its command's output: {error}"));
    }
    match ended {
        Ended::Status(code) => code,
        Ended::Ceiling(ended) => {
            if let Err(error) = ended {
                stamping::err(console, &format!("gate: hold could not end its command's job: {error}"));
            }
            stamping::err(console, &ceiling_line(ceiling));
            CEILING_STATUS
        }
        Ended::Unwaited(error) => failed(&format!("cannot wait for its command: {error}"), console),
    }
}

/// How a hold's command ended.
enum Ended {
    /// With its own status.
    Status(u32),
    /// At the ceiling, its job ended, or why it could not be.
    Ceiling(io::Result<()>),
    /// The wait for it failed.
    Unwaited(io::Error),
}

/// Waits for `running` up to `ceiling` seconds; at the ceiling, ends `job` with [`CEILING_STATUS`].
fn ended_by(running: &Running, job: &Job, ceiling: u64) -> Ended {
    match running.wait(Duration::from_secs(ceiling)) {
        Ok(Some(code)) => Ended::Status(code),
        Ok(None) => Ended::Ceiling(job.terminate(CEILING_STATUS)),
        Err(error) => Ended::Unwaited(error),
    }
}

/// `command` started in a new kill-on-close job, its program found along the hold's `PATH` from its working folder,
/// with the hold's environment, its stdin `NUL` and its stdout and stderr pipes: the job, the command, and the read
/// ends of its stdout and stderr. Else why not.
fn launched(command: &[OsString]) -> Result<(Job, Running, File, File), String> {
    let (first, rest) = command.split_first().ok_or("the hold has no command")?;
    let word = first.to_str().ok_or("its program's name is not Unicode")?;
    let variables = Variables::from_process();
    let directory = std::env::current_dir().map_err(|error| format!("cannot read the working folder: {error}"))?;
    let program = find_program(word, &variables, &directory)?;
    let (application, command_line) = match written(&program, rest, &variables) {
        Written::Line(application, command_line) => (application, command_line),
        Written::Refused(line) | Written::NoShell(line) => return Err(line),
    };
    let environment = variables.block().map_err(|error| format!("cannot build its environment: {error}"))?;
    let launch = Launch {
        application,
        command_line,
        directory,
        environment,
        output: LaunchOutput::Pipes,
        priority: None,
    };
    let job = Job::anonymous_kill_on_close().map_err(|error| format!("cannot create its job: {error}"))?;
    let mut suspended = launch.start_suspended().map_err(|error| error.to_string())?;
    let (stdout, stderr) = suspended.take_output().ok_or("its output pipes were not made")?;
    let running = suspended.assign_and_resume(&job).map_err(|error| error.to_string())?;
    Ok((job, running, stdout, stderr))
}

/// Copies `from` to `to` until its end, each read written and flushed as it comes, so the hold's own lines between
/// interleave with it; a pipe whose writers all closed is its end.
fn passed_on(mut from: File, mut to: impl Write) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => return Err(error),
        };
        to.write_all(&buffer[..read])?;
        to.flush()?;
    }
}
