//! The builder's own slot pool on the Unix box, the twin of the Windows box's: the count `{root}/slots.json`, and the
//! claims `slots/{n}.json` in `{root}/slots/`, a folder readied as `drain/`'s own folders are, the box user's own at
//! mode 0700. A routed run's preflight takes a slot for its drain session ([`taken`]), its run holds it, taking a free
//! one again for a session whose claim lapsed and was taken ([`run_taken`]), and the run's end lets it go
//! ([`Release`]); `gate-box box hold` takes one for a command it runs ([`hold`]), holding an `flock` on `slots/{n}.lock`
//! for its whole life. Every take and judge runs under the drain's lock ([`drain::under_drain_lock`]), so two takers
//! never get one slot: a session claim is live while its session is live for a call of its own, a hold claim while its
//! lock is held, which the kernel lets go when the hold dies, and a dead claim is removed by the taker that finds it.
//! Every claim in `slots/` counts against the count, one past a lowered count too. With no count file, nothing here takes
//! a slot.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::{BoxRequest, BoxSeams, HoldRequest};
use gate_wire::drain::{DRAIN, LOCK, LOCK_RETRY};
use gate_wire::slots::{
    BoxSlots, COUNT_FILE, Claim, HOLD_LINE_POLLS, HOLD_POLL, HoldClaim, Judged, NO_COUNT_LINE, SLOTS, SessionClaim, Take, ceiling_line, chosen,
    claim_index, claim_name, count_of, full_line, holder_line, holder_refusal, lock_name, unread_count_line, waiting_line,
};
use gate_wire::{Payload, Refusal, escaped, refused_line};
use jiff::Timestamp;

use crate::console::Console;
use crate::drain::{self, Session, cannot};
use crate::files::{self, HeldLock, Locked};
use crate::lines::{FAILED_STATUS, USAGE_STATUS, version_line};
use crate::preflight;
use crate::run::REFUSED_STATUS;

/// The nonce a hold's `full` refusal tag carries, a hold having no payload to take one from.
const HOLD_NONCE: &str = "hold";
/// A hold's status when its ceiling ended its command.
const CEILING_STATUS: u8 = 124;
/// A hold's status when its command could not be started.
const NOT_STARTED_STATUS: u8 = 127;
/// What a command ended by a signal exits with, past the signal's number, as a shell says it.
const SIGNALLED_BASE: i32 = 128;

/// The slot count of the work root `root`: `None` with no count file. Else why the file cannot be read or is
/// malformed, which counts as no file.
fn count_at(root: &Path) -> Result<Option<u32>, String> {
    let path = root.join(COUNT_FILE);
    let Some((text, _)) = drain::read_text(&path)? else {
        return Ok(None);
    };
    count_of(&text)
        .map(Some)
        .map_err(|why| format!("{}: {why}", escaped(&path.to_string_lossy())))
}

/// `slots/` beneath the work root `root`, made at mode 0700 when missing and refused unless it is the box user's own
/// folder at mode 0700 ([`drain::private_folder`]).
fn slots_folder(root: &Path) -> Result<PathBuf, String> {
    let folder = root.join(SLOTS);
    drain::private_folder(&folder)?;
    Ok(folder)
}

/// The index of a slot's claim `{n}.json` or lock `{n}.lock` named `name`; `None` for any other name.
fn slot_index(name: &str) -> Option<usize> {
    let (stem, extension) = name.rsplit_once('.')?;
    let known = extension == "json" || extension == "lock";
    if !known || stem.is_empty() || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// Every slot with a claim or a lock in `folder`, whatever its index, judged under the drain lock ([`judged_slot`]).
fn judged(folder: &Path, session_live: &dyn Fn(&str) -> Result<bool, String>) -> Result<Vec<Judged>, String> {
    let names = drain::names_in(folder)?;
    let indexes: BTreeSet<usize> = names.iter().filter_map(|name| slot_index(name)).collect();
    indexes.into_iter().map(|n| judged_slot(folder, n, session_live)).collect()
}

/// Slot `n` in `folder`, judged under the drain lock: live while another open holds its lock's `flock`
/// ([`drain::holder_live`], which removes a dead lock) or its claim's session is live (`session_live`); a dead claim is
/// removed.
fn judged_slot(folder: &Path, n: usize, session_live: &dyn Fn(&str) -> Result<bool, String>) -> Result<Judged, String> {
    let (lock, name) = (folder.join(lock_name(n)), folder.join(claim_name(n)));
    let held = files::exists(&lock) && drain::holder_live(&lock)?;
    let read = drain::read_text(&name)?;
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
        drain::removed(&name)?;
    }
    Ok(Judged {
        n,
        live,
        session: session.filter(|_| live),
    })
}

/// Writes `claim` as the whole of the claim file `path` ([`files::replace_plain`]). Else why not.
fn claim_written(path: &Path, claim: &Claim) -> Result<(), String> {
    files::replace_plain(path, claim.text().as_bytes())
        .map_err(|failure| format!("cannot write {}: {}", escaped(&path.to_string_lossy()), escaped(&failure.to_string())))
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

/// Why a session holds no slot it needs.
pub(crate) enum Untaken {
    /// Every slot is held: `held` of `count`.
    Full { held: usize, count: u32 },
    /// The slot pool could not be read or written: why.
    Failed(String),
}

/// The slot count of the work root `root`; `None` with no count file, and for one that cannot be read, said on
/// `console`.
fn count_said(root: &Path, console: &mut Console) -> Option<u32> {
    match count_at(root) {
        Ok(count) => count,
        Err(why) => {
            console.err(&unread_count_line(&why));
            None
        }
    }
}

/// Takes a slot for `session` under the work root `root` when it has a count file ([`count_said`]): a session that
/// holds a slot keeps it, else it takes the lowest free one with its session claim. Whether the session now holds a
/// claim, false with no count file; else why it holds none.
fn session_taken(root: &Path, session: &Session<'_>, seams: &BoxSeams, console: &mut Console) -> Result<bool, Untaken> {
    let Some(count) = count_said(root, console) else {
        return Ok(false);
    };
    let claim = Claim::Session(SessionClaim::capped(
        session.id,
        session.pc,
        session.repo,
        session.command,
        &now_text(seams),
    ));
    let took = drain::under_drain_lock(root, seams, console, |session_live| {
        let folder = slots_folder(root)?;
        match chosen(&judged(&folder, session_live)?, count, Some(session.id)) {
            Take::Kept(_) => Ok(None),
            Take::Free(n) => claim_written(&folder.join(claim_name(n)), &claim).map(|()| None),
            Take::Full { held } => Ok(Some(held)),
        }
    });
    match took {
        Ok(None) => Ok(true),
        Ok(Some(held)) => Err(Untaken::Full { held, count }),
        Err(why) => Err(Untaken::Failed(why)),
    }
}

/// The `full` refusal tag of the nonce `nonce` and the full line of `held` of `count`, both said bare.
pub(crate) fn full_lines(nonce: &str, held: usize, count: u32) -> [String; 2] {
    [refused_line(nonce, Refusal::Full), full_line(held, count)]
}

/// The line of a slot pool that could not be read or written for `why`.
pub(crate) fn failed_line(why: &str) -> String {
    format!("gate: slots failed: {why}")
}

/// Takes a slot for the admitted preflight of `session` under the work root `root` when it has a count file
/// ([`session_taken`]): whether the session now holds a claim, which is false with no session or no count file. With
/// every slot held: the version line and the box slots line ([`looked`]) on stdout, then the `full` refusal tag of
/// `payload`'s nonce (left out for a payload without one) and the full line, and [`REFUSED_STATUS`]. A slot pool that
/// cannot be read or written fails the preflight: the version line, its line ([`failed_line`]), and [`FAILED_STATUS`].
pub(crate) fn taken(root: &Path, session: Option<&Session<'_>>, payload: &Payload, seams: &BoxSeams, console: &mut Console) -> Result<bool, u8> {
    let Some(session) = session else {
        return Ok(false);
    };
    let untaken = match session_taken(root, session, seams, console) {
        Ok(held) => return Ok(held),
        Err(untaken) => untaken,
    };
    console.out(&version_line());
    match untaken {
        Untaken::Full { held, count } => {
            look_said(looked(root, seams), console);
            if let Ok(nonce) = payload.nonce() {
                console.err(&refused_line(nonce, Refusal::Full));
            }
            console.err(&full_line(held, count));
            Err(REFUSED_STATUS)
        }
        Untaken::Failed(why) => {
            console.err(&failed_line(&why));
            Err(FAILED_STATUS)
        }
    }
}

/// The builder's own slots under the work root `root` as a look without the drain lock finds them, at the box's clock
/// (`--test-box-now`'s second): its count, or why its count file does not read, and every claim in `slots/` judged by
/// nobody, none removed; one that does not read, a link or anything but a plain file included, is counted unreadable.
/// `None` for a root with neither a count file nor anything at `slots/`. Else why `slots/` could not be listed, a link
/// or anything but a plain folder there included.
pub(crate) fn looked(root: &Path, seams: &BoxSeams) -> Result<Option<BoxSlots>, String> {
    let count = count_at(root);
    let folder = root.join(SLOTS);
    let claims = if files::plain_folder(&folder) {
        claims_read(&folder)?
    } else if files::exists(&folder) {
        // A link or a file where the folder belongs, which every take refuses too.
        return Err(format!("cannot list {}: it is not a plain folder", escaped(&folder.to_string_lossy())));
    } else if matches!(count, Ok(None)) {
        return Ok(None);
    } else {
        Vec::new()
    };
    Ok(Some(BoxSlots::looked(&now_text(seams), count, claims)))
}

/// Each claim `{n}.json` in `folder`, its slot and its text, `None` for one that could not be read; one gone since the
/// listing is left out. Else why the folder could not be listed.
fn claims_read(folder: &Path) -> Result<Vec<(usize, Option<String>)>, String> {
    let mut claims = Vec::new();
    for name in drain::names_in(folder)? {
        // Only the name a claim is written under, as the judge reads it.
        let Some(n) = claim_index(&name) else {
            continue;
        };
        match drain::read_text(&folder.join(&name)) {
            Ok(Some((text, _))) => claims.push((n, Some(text))),
            Ok(None) => {}
            // Counted as unreadable in the line, which is all a look says of a claim it cannot read.
            Err(_) => claims.push((n, None)),
        }
    }
    Ok(claims)
}

/// Says the look `look` ([`looked`]): its box slots line on stdout, nothing for a root without slots, or
/// `gate: slots look failed: {why}` on stderr.
pub(crate) fn look_said(look: Result<Option<BoxSlots>, String>, console: &mut Console) {
    match look {
        Ok(Some(slots)) => console.out(&slots.line()),
        Ok(None) => {}
        Err(why) => console.err(&format!("gate: slots look failed: {why}")),
    }
}

/// Holds a slot for the admitted run of the session `payload` names, its command text and job `run`, under the work
/// root `root`: the slot its preflight took, or a free one when its claim lapsed and was taken ([`session_taken`]):
/// whether the session now holds a claim. A run with no session, or under a root not there or with no count file,
/// takes none. Else why it holds none, for the run to say.
pub(crate) fn run_taken(root: &str, payload: &Payload, run: (&str, &str), seams: &BoxSeams, console: &mut Console) -> Result<bool, Untaken> {
    let Some(session) = drain::session_of(payload, run.0, run.1).map_err(Untaken::Failed)? else {
        return Ok(false);
    };
    let root = Path::new(root);
    if !drain::root_there(root).map_err(Untaken::Failed)? {
        return Ok(false);
    }
    session_taken(root, &session, seams, console)
}

/// The end of a run of a session, or of its preflight that failed: dropped, it removes the session's claim under the
/// drain lock, so the slot is free for the next taker, and says `gate: slot release failed: {why}` on the box's stderr
/// when it could not. Made after the call's admission and so dropped before it, while the session's holder is still
/// held.
pub(crate) struct Release {
    root: PathBuf,
    session: Option<String>,
}

impl Release {
    /// The end of the call under the work root `root` of the session `payload` names; one with no session lets
    /// nothing go.
    pub(crate) fn at_end(root: &Path, payload: &Payload) -> Release {
        Release {
            root: root.to_owned(),
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
            && let Err(why) = released(&self.root, session)
        {
            // A drop is given no console; the box's own stderr is the call's. A claim left behind is judged by its
            // session, which lapses at its lease.
            Console::stdio().err(&format!("gate: slot release failed: {why}"));
        }
    }
}

/// Removes every claim of `session` in the `slots/` of the work root `root`, under the drain lock; a root with no
/// `slots/` or no `drain/` holds none, no slot having been taken without them, and takes no lock. Else why not.
fn released(root: &Path, session: &str) -> Result<(), String> {
    let (folder, drain) = (root.join(SLOTS), root.join(DRAIN));
    if !files::plain_folder(&folder) || !files::plain_folder(&drain) {
        return Ok(());
    }
    let _lock = drain::take_drain_lock(&drain.join(LOCK), LOCK_RETRY)?;
    let claims = drain::names_in(&folder)?;
    // Case-sensitive, as the claims are written and as [`slot_index`] reads them.
    for name in claims.iter().filter(|name| Path::new(name).extension() == Some(OsStr::new("json"))) {
        let path = folder.join(name);
        let own = drain::read_text(&path)?.is_some_and(|(text, _)| Claim::of(&text).is_ok_and(|claim| claim.session() == Some(session)));
        if own {
            drain::removed(&path)?;
        }
    }
    Ok(())
}

/// A slot a hold holds: the `flock` on its lock, and its claim, both let go when it drops, a failure said as
/// `gate: slot release failed: {why}` on the box's stderr.
struct HeldSlot {
    lock: HeldLock,
    folder: PathBuf,
    n: usize,
}

impl Drop for HeldSlot {
    fn drop(&mut self) {
        // While the lock is held no taker judges the slot free, so the claim and the lock's file are removed without
        // the drain lock; either left behind is dead once this process ends, and the next taker removes it.
        let claim = self.folder.join(claim_name(self.n));
        let failures = [
            drain::removed(&claim).err(),
            files::remove_if_there(self.lock.path())
                .err()
                .map(|error| cannot("remove", self.lock.path(), &error)),
        ];
        for why in failures.into_iter().flatten() {
            Console::stdio().err(&format!("gate: slot release failed: {why}"));
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

/// One look, under the drain lock, for a free slot of the `count` under the work root `root` for the hold `hold`.
fn look(root: &Path, count: u32, hold: &HoldRequest, seams: &BoxSeams, console: &mut Console) -> Result<Looked, String> {
    let claim = Claim::Hold(HoldClaim {
        holder: hold.holder.clone(),
        ceiling: hold.ceiling,
        pid: std::process::id(),
        started: Timestamp::now().to_string(),
    });
    drain::under_drain_lock(root, seams, console, |session_live| {
        let folder = slots_folder(root)?;
        let n = match chosen(&judged(&folder, session_live)?, count, None) {
            Take::Full { held } => return Ok(Looked::Full(held)),
            Take::Free(n) | Take::Kept(n) => n,
        };
        let path = folder.join(lock_name(n));
        let lock = match files::take_lock(&path) {
            Ok(Locked::Held(lock)) => lock,
            Ok(Locked::Busy) => return Err(format!("cannot hold {}: another open holds it", escaped(&path.to_string_lossy()))),
            Err(error) => return Err(cannot("hold", &path, &error)),
        };
        let slot = HeldSlot { lock, folder, n };
        claim_written(&slot.folder.join(claim_name(n)), &claim)?;
        Ok(Looked::Slot(slot))
    })
}

/// Answers `gate-box box hold`: takes one of the builder's own slots under its root and runs its command while it holds
/// it, then lets it go. The exit status: the command's own (128 past the signal's number for one a signal ended);
/// [`USAGE_STATUS`] after [`NO_COUNT_LINE`] for a root with no count file (after the line saying why, for one that
/// cannot be read), or after the holder line for a holder [`holder_refusal`] refuses; [`REFUSED_STATUS`] after the
/// `full` tag and the full line when no slot came within its wait ([`waited`]); [`CEILING_STATUS`] after the ceiling
/// line when its ceiling ended the command; [`NOT_STARTED_STATUS`] after the line saying why the command could not
/// start; [`FAILED_STATUS`] after `gate: hold failed: {why}`.
pub(crate) fn hold(request: &BoxRequest, console: &mut Console) -> u8 {
    let (Some(root), Some(hold)) = (request.root.as_deref(), request.hold.as_ref()) else {
        // The parse gives `hold` its root and its words always; a request without them is the box's usage error.
        console.err(&gate_args::Problem::BoxUsage.to_string());
        return USAGE_STATUS;
    };
    let seams = &request.seams;
    let count = match counted(root, console) {
        Ok(count) => count,
        Err(status) => return status,
    };
    if let Some(why) = holder_refusal(&hold.holder) {
        console.err(&holder_line(&hold.holder, &why));
        return USAGE_STATUS;
    }
    if let Err(why) = root_trusted(root) {
        return failed(&why, console);
    }
    match waited(root, count, hold, seams, console) {
        Ok(slot) => {
            let status = held_run(&hold.command, hold.ceiling, console);
            drop(slot);
            status
        }
        Err(status) => status,
    }
}

/// Judges the work root `root` as the preflight judges it ([`preflight::trusted_work_root`]); else why not, its
/// `gate: ` left out.
fn root_trusted(root: &Path) -> Result<(), String> {
    let text = root
        .to_str()
        .ok_or_else(|| format!("the root {} is not Unicode", escaped(&root.to_string_lossy())))?;
    preflight::trusted_work_root(text).map_err(|refusal| match refusal {
        preflight::Refusal::Folder(line) | preflight::Refusal::Untrusted(line) => {
            line.strip_prefix("gate: ").map_or_else(|| line.clone(), str::to_owned)
        }
    })
}

/// A slot of the `count` under the work root `root` for the hold `hold`, looked for until its wait ends, each look
/// [`HOLD_POLL`] apart (`--test-box-hold-poll-ms`) and the waiting line said every [`HOLD_LINE_POLLS`] looks. Else the
/// status to exit with, its lines said: the `full` tag and the full line and [`REFUSED_STATUS`] when none came, or
/// `gate: hold failed: {why}` and [`FAILED_STATUS`].
fn waited(root: &Path, count: u32, hold: &HoldRequest, seams: &BoxSeams, console: &mut Console) -> Result<HeldSlot, u8> {
    let poll = seams.hold_poll_ms.map_or(HOLD_POLL, Duration::from_millis);
    let Some(until) = Instant::now().checked_add(Duration::from_secs(hold.wait)) else {
        return Err(failed(&format!("its wait of {} s cannot be timed", hold.wait), console));
    };
    let mut looks: u64 = 0;
    loop {
        match look(root, count, hold, seams, console) {
            Ok(Looked::Slot(slot)) => return Ok(slot),
            Ok(Looked::Full(held)) if Instant::now() >= until => {
                for line in full_lines(HOLD_NONCE, held, count) {
                    console.err(&line);
                }
                return Err(REFUSED_STATUS);
            }
            Ok(Looked::Full(held)) => {
                if looks.is_multiple_of(HOLD_LINE_POLLS) {
                    console.err(&waiting_line(held, count));
                }
                looks = looks.saturating_add(1);
                thread::sleep(poll);
            }
            Err(why) => return Err(failed(&why, console)),
        }
    }
}

/// The slot count of the work root `root`; else the status to exit with, its lines said: [`NO_COUNT_LINE`] and
/// [`USAGE_STATUS`] for a root that is not there or has no count file that reads, after the line saying why for one
/// that does not read; `gate: hold failed: {why}` and [`FAILED_STATUS`] for a root that is a link or no folder.
fn counted(root: &Path, console: &mut Console) -> Result<u32, u8> {
    let no_count = |console: &mut Console| {
        console.err(NO_COUNT_LINE);
        USAGE_STATUS
    };
    match drain::root_there(root) {
        Ok(true) => {}
        Ok(false) => return Err(no_count(console)),
        Err(why) => return Err(failed(&why, console)),
    }
    match count_at(root) {
        Ok(Some(count)) => Ok(count),
        Ok(None) => Err(no_count(console)),
        Err(why) => {
            console.err(&unread_count_line(&why));
            Err(no_count(console))
        }
    }
}

/// Says `gate: hold failed: {why}`; [`FAILED_STATUS`].
fn failed(why: &str, console: &mut Console) -> u8 {
    console.err(&format!("gate: hold failed: {why}"));
    FAILED_STATUS
}

/// Runs `command` in a process group of its own, its stdin `/dev/null` and its stdout and stderr the hold's own, with
/// its signals and its umask set back as a routed command's are ([`crate::command::signals_reset`]), watched until it
/// ends ([`watched`]): the command's status ([`status_of`]); [`CEILING_STATUS`] after the ceiling line when its ceiling
/// ended it; 128 past the signal's number when the hold was sent one of [`ENDING_SIGNALS`]; [`NOT_STARTED_STATUS`] after
/// the line saying why it could not start.
fn held_run(command: &[OsString], ceiling: u64, console: &mut Console) -> u8 {
    let Some((program, args)) = command.split_first() else {
        // The parse gives a hold one word of its command at least; none is said as the Windows box says it.
        return not_started("", "the hold has no command", console);
    };
    // Blocked before the command and the hold's threads start, so each thread inherits the block and a signal waits for
    // [`signal_waited`]; the command clears its mask before its exec.
    let caught = match ending_signals_blocked() {
        Ok(caught) => caught,
        Err(error) => return failed(&format!("cannot block its signals: {error}"), console),
    };
    let mut started = Command::new(program);
    started.args(args).stdin(Stdio::null()).process_group(0);
    // SAFETY: `signals_reset` calls only async-signal-safe functions (`umask`, `sigaction`, `sigemptyset`,
    // `sigprocmask`) and allocates nothing, as a hook between `fork` and `exec` must.
    unsafe {
        started.pre_exec(crate::command::signals_reset);
    }
    let child = match spawned(&mut started) {
        Ok(child) => child,
        Err(error) => return not_started(&program.to_string_lossy(), &error.to_string(), console),
    };
    match watched(child, ceiling, caught) {
        Ended::Status(status) => status_of(status),
        Ended::Ceiling(killed) => {
            if let Err(error) = killed {
                console.err(&format!("gate: hold could not end its command's group: {error}"));
            }
            console.err(&ceiling_line(ceiling));
            CEILING_STATUS
        }
        Ended::Caught(signal) => u8::try_from(SIGNALLED_BASE.saturating_add(signal)).unwrap_or(FAILED_STATUS),
        Ended::Unwaited(error) => failed(&format!("cannot wait for its command: {error}"), console),
    }
}

/// Says that the hold could not start `program` for `why`; [`NOT_STARTED_STATUS`].
fn not_started(program: &str, why: &str, console: &mut Console) -> u8 {
    console.err(&format!("gate: hold could not start {}: {}", escaped(program), escaped(why)));
    NOT_STARTED_STATUS
}

/// Starts a hold's command `command`.
#[expect(
    clippy::disallowed_methods,
    reason = "the spawn lock is the Windows gate's; a Unix box's hold starts its command here"
)]
fn spawned(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

/// The signals that end a hold, its command's group ended first: a CI runner's cancel, a terminal's Ctrl-C, a hang-up.
const ENDING_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Blocks [`ENDING_SIGNALS`] in the calling thread, and so in every thread it starts from now on; the set blocked.
fn ending_signals_blocked() -> io::Result<libc::sigset_t> {
    // SAFETY: an all-zero `sigset_t` is a valid value, emptied by `sigemptyset` below.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` lives across the calls, which write only into it.
    unsafe {
        libc::sigemptyset(&raw mut set);
        for signal in ENDING_SIGNALS {
            libc::sigaddset(&raw mut set, signal);
        }
    }
    // SAFETY: `set` lives across the call, which only reads it; no old mask is asked for.
    let blocked = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
    if blocked == 0 {
        Ok(set)
    } else {
        Err(io::Error::from_raw_os_error(blocked))
    }
}

/// The next of the blocked signals `set` sent to the hold, taken with `sigwait`. Else why not.
fn signal_waited(set: &libc::sigset_t) -> io::Result<libc::c_int> {
    let mut signal: libc::c_int = 0;
    // SAFETY: `set` and `signal` live across the call, which reads the one and writes only the other.
    let waited = unsafe { libc::sigwait(std::ptr::from_ref(set), &raw mut signal) };
    if waited == 0 {
        Ok(signal)
    } else {
        Err(io::Error::from_raw_os_error(waited))
    }
}

/// Waits for the child `id` to exit, leaving it unreaped, so its pid and its group's id stay its own:
/// `waitid(P_PID, WEXITED | WNOWAIT)`. Else why not.
fn exited_unreaped(id: libc::id_t) -> io::Result<()> {
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid value, which `waitid` fills.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` lives across the call, which writes only into it.
        let waited = unsafe { libc::waitid(libc::P_PID, id, &raw mut info, libc::WEXITED | libc::WNOWAIT) };
        if waited == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Sends `SIGKILL` to the group `group` and to its leader of the same id, which may have left it; a group already
/// gone is no failure. Its leader is unreaped, so neither id names another's process. Else why the group could not be
/// sent it.
fn group_killed(group: libc::pid_t) -> io::Result<()> {
    // SAFETY: `killpg` and `kill` take integers and touch no memory; the leader is not reaped yet, so `group` is its.
    let (sent, error) = unsafe {
        let sent = libc::killpg(group, libc::SIGKILL);
        let error = io::Error::last_os_error();
        libc::kill(group, libc::SIGKILL);
        (sent, error)
    };
    if sent == 0 || error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

/// What the hold heard while its command ran.
enum Event {
    /// The leader exited, left unreaped, or why its wait failed.
    Exited(io::Result<()>),
    /// The hold was sent this one of [`ENDING_SIGNALS`].
    Caught(libc::c_int),
}

/// How a hold's command ended.
enum Ended {
    /// With its own status.
    Status(ExitStatus),
    /// At the ceiling, its group ended, or why it could not be.
    Ceiling(io::Result<()>),
    /// The hold was sent this signal, and ended the group.
    Caught(libc::c_int),
    /// The wait for it failed.
    Unwaited(io::Error),
}

/// How the command's watch ended, before its leader is reaped.
enum Ending {
    /// Its leader ended on its own.
    Own,
    /// Its ceiling passed: the group ended, or why it could not be.
    Ceiling(io::Result<()>),
    /// The hold was sent this signal.
    Caught(libc::c_int),
}

/// Watches `child`, the leader of its own group, until it ends: its own end, the ceiling of `ceiling` seconds, or one
/// of the signals `caught` sent to the hold, which end the group ([`group_killed`]). Every end, its own included, ends
/// what is left of the group, so nothing of the command outlives the hold; then the leader is reaped.
fn watched(mut child: Child, ceiling: u64, caught: libc::sigset_t) -> Ended {
    let (Ok(group), Ok(id)) = (libc::pid_t::try_from(child.id()), libc::id_t::try_from(child.id())) else {
        return Ended::Unwaited(io::Error::other(format!("its pid {} is no pid_t", child.id())));
    };
    let (sent, events) = mpsc::channel();
    if let Err(error) = watchers(id, caught, &sent) {
        drop(group_killed(group));
        drop(child.wait());
        return Ended::Unwaited(error);
    }
    let ending = match events.recv_timeout(Duration::from_secs(ceiling)) {
        Ok(Event::Exited(Ok(()))) => {
            // The leftovers a leader left running are ended, as the run verb ends them.
            drop(group_killed(group));
            Ending::Own
        }
        Ok(Event::Caught(signal)) => {
            drop(group_killed(group));
            exit_heard(&events);
            Ending::Caught(signal)
        }
        Err(RecvTimeoutError::Timeout) => at_ceiling(group, &events),
        Ok(Event::Exited(Err(error))) => {
            drop(group_killed(group));
            return Ended::Unwaited(error);
        }
        Err(RecvTimeoutError::Disconnected) => return Ended::Unwaited(io::Error::other("its watchers ended unheard")),
    };
    match (child.wait(), ending) {
        (Ok(status), Ending::Own) => Ended::Status(status),
        (Ok(_), Ending::Ceiling(killed)) => Ended::Ceiling(killed),
        (Ok(_), Ending::Caught(signal)) => Ended::Caught(signal),
        (Err(error), _) => Ended::Unwaited(error),
    }
}

/// Starts the hold's two watchers, each sending what it hears on `sent`: one waits for the leader `id` to exit, left
/// unreaped ([`exited_unreaped`]), and one for a signal of `caught` ([`signal_waited`]). Neither is joined: the first
/// ends with the leader, the second with the hold's process. Else why one could not start.
fn watchers(id: libc::id_t, caught: libc::sigset_t, sent: &mpsc::Sender<Event>) -> io::Result<()> {
    let exits = sent.clone();
    // A receiver gone is a hold no longer watching; what it would have heard is no one's.
    thread::Builder::new().spawn(move || drop(exits.send(Event::Exited(exited_unreaped(id)))))?;
    let signals = sent.clone();
    thread::Builder::new().spawn(move || {
        if let Ok(signal) = signal_waited(&caught) {
            drop(signals.send(Event::Caught(signal)));
        }
    })?;
    Ok(())
}

/// The end at the ceiling of the group `group`: a leader that exited by now, unreaped, ended on its own, and only its
/// leftovers are ended; else the group and its leader are ended ([`group_killed`]) and the leader's exit is waited for.
fn at_ceiling(group: libc::pid_t, events: &mpsc::Receiver<Event>) -> Ending {
    // A look that fails is a leader still taken as running: ending it is what the ceiling asks.
    if crate::process::exit_seen(group).unwrap_or(false) {
        drop(group_killed(group));
        return Ending::Own;
    }
    let killed = group_killed(group);
    exit_heard(events);
    Ending::Ceiling(killed)
}

/// Waits on `events` for the leader's exit, after its group was ended; a signal heard meanwhile changes nothing.
fn exit_heard(events: &mpsc::Receiver<Event>) {
    while let Ok(event) = events.recv() {
        if matches!(event, Event::Exited(_)) {
            break;
        }
    }
}

/// The hold's exit status for its command's `status`: its own code, or 128 past the signal's number for one a signal
/// ended, as a shell says it.
fn status_of(status: ExitStatus) -> u8 {
    let code = status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNALLED_BASE.saturating_add(signal)))
        .unwrap_or(i32::from(FAILED_STATUS));
    u8::try_from(code).unwrap_or(FAILED_STATUS)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::{slot_index, status_of};

    #[test]
    fn a_slot_index_is_read_from_a_claim_or_a_lock_name_alone() {
        assert_eq!(slot_index("0.json"), Some(0));
        assert_eq!(slot_index("12.lock"), Some(12));
        assert_eq!(slot_index("007.json"), Some(7), "leading zeros are read as written");
        for other in [
            "",
            ".json",
            "a.json",
            "1.JSON",
            "1.Lock",
            "1.json.new",
            "1.json.new-42",
            "1",
            "-1.json",
            "1 .json",
            "１.json",
        ] {
            assert_eq!(slot_index(other), None, "{other:?}");
        }
        assert_eq!(slot_index("99999999999999999999999.json"), None, "past a usize");
    }

    #[test]
    fn a_hold_exits_with_its_command_s_code_or_128_past_its_signal() {
        assert_eq!(status_of(ExitStatus::from_raw(0)), 0);
        assert_eq!(status_of(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(status_of(ExitStatus::from_raw(255 << 8)), 255);
        assert_eq!(status_of(ExitStatus::from_raw(libc::SIGTERM)), 143);
        assert_eq!(status_of(ExitStatus::from_raw(libc::SIGKILL)), 137);
    }
}
