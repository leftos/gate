//! A run that asks for the builder's desktop: the console's session read before the checkout, the stage between the
//! run's readiness and its command, which resolves the program, takes the builder's desktop lock within the wait the
//! run gave (a locked run; a session run takes none) and reads the console's session once more, and then the
//! hand-off of the command to the desktop helper ([`hand_off`]). Every failure of the desktop's machinery before the
//! command's start marker is a refusal: the box's `desktop` tag and its line, exit 75, and never a back commits line,
//! so the PC ends the run with the reason, never running a desktop command itself.

mod hand_off;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use gate_args::{BoxSeams, DesktopSession};
use gate_router::escaped;
use gate_win32::{Account, AcquireError, ConnectState, LockState, MutexGuard, PrivateMutex, PrivateName, QueryOnlyProcess, SessionState};
use jiff::Timestamp;
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::boxside::desktop::lines::{
    DesktopHolder, DesktopRefusal, Limits, abandoned_line, busy_timeout_line, busy_wait_line, capped_wait, session_verdict,
};
use crate::boxside::run::FILE_FLAG_OPEN_REPARSE_POINT;
use crate::boxside::run::{Beat, CLOSED_STATUS, One, REFUSED_STATUS, Ran, Request, STARTING_COMMAND, before_the_job, prepared};
use crate::boxside::stamping;
use crate::boxside::tools::write_record_file;
use crate::console::Console;
use crate::remote::{Refusal, refused_line};

/// The user name the session seam `other-user` puts at the console: one no account of the box has.
const OTHER_USER: &str = "gate-seam-other-user";

/// The name of the record of the run holding the builder's desktop, in the box's work root ([`HolderRecord`]).
const HOLDER_RECORD: &str = "desktop-holder.json";

/// How a desktop run that did not run its command ended, its lines said.
pub(super) enum DesktopEnd {
    /// The run was refused, its `desktop` tag said, or its `install` tag for a desktop helper that is not trusted: 75,
    /// and no back commits line.
    Refused,
    /// It ended otherwise, with this status: a stop, a payload's word refused, or the box's own failure.
    Ended(u32),
}

/// How a desktop run takes the builder's desktop, as its payload asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DesktopTake {
    /// It holds the builder's desktop lock for its whole run, waiting at most `wait` seconds for it.
    Locked { wait: u64 },
    /// It needs only the console's logon session: it never opens, waits for or holds the lock, and keeps no holder
    /// record. A `hidden` one runs its command on a desktop of its own the helper creates, which no one sees.
    Session { hidden: bool },
}

/// Why the console's session could not be read whole.
#[derive(Debug)]
enum Unread {
    /// A read of the system failed.
    Failed(io::Error),
    /// Another session was attached to the console between the box's two reads of which one is.
    Moved,
}

/// Why the console cannot take the run's command: the refusal, and the system's error behind a session it could not
/// read.
#[derive(Debug)]
struct ConsoleRefused {
    refusal: DesktopRefusal,
    error: Option<io::Error>,
}

impl ConsoleRefused {
    /// The lines refusing the run after its tag: the refusal's, then the system's error when there is one.
    fn lines(&self) -> Vec<String> {
        let mut lines = vec![self.refusal.line()];
        if let Some(error) = &self.error {
            lines.push(format!("gate: the console session read failed: {}", escaped(&error.to_string())));
        }
        lines
    }
}

impl From<DesktopRefusal> for ConsoleRefused {
    fn from(refusal: DesktopRefusal) -> ConsoleRefused {
        ConsoleRefused { refusal, error: None }
    }
}

/// The cheap read before the checkout: whether the console's session can take the run's command. `Err` with 75, the
/// `desktop` tag and why said, when it cannot, or when the box cannot read its own account. Nothing writes the box's
/// output yet, so the lines need no lock.
pub(super) fn console_takes_it(request: &Request<'_>, seams: &BoxSeams, console: &mut Console) -> Result<(), u32> {
    match account_and_console(seams) {
        Ok(_) => Ok(()),
        Err(refused) => {
            stamping::protocol(console, &refused_line(request.nonce, Refusal::Desktop));
            for line in refused.lines() {
                stamping::err(console, &line);
            }
            Err(REFUSED_STATUS)
        }
    }
}

/// The run's command on the builder's desktop, taken as `take` says: the stage, then the hand-off. The stage resolves
/// the program first, so a command that cannot start (127, or 2 for a batch word refused) never holds the desktop, and
/// looks at the stop signal. A locked run then takes the builder's desktop lock within its wait ([`locked`]); a
/// session run takes no lock. Either reads the console's session again, and the hand-off ([`hand_off::hand_off`]) runs
/// the command in that session and sees it to its end.
pub(super) fn desktop_command(
    request: &Request<'_>,
    one: &One<'_>,
    take: DesktopTake,
    beat: &Beat<'_>,
    console: &mut Console,
) -> Result<Ran, DesktopEnd> {
    let prepared = match prepared(request, one, console) {
        Ok(prepared) => prepared,
        Err(answer) => return answer.map_err(DesktopEnd::Ended),
    };
    // Looked at before the lock is opened, so a run already stopped never opens it.
    before_the_job(beat.stop.look(STARTING_COMMAND), beat.stop.job, beat.lock, console).map_err(DesktopEnd::Ended)?;
    let limits = run_limits(beat.seams);
    let hand = |session: u32, console: &mut Console| {
        let here = hand_off::Here {
            request,
            one,
            prepared: &prepared,
            beat,
            limits,
            session,
            take,
        };
        hand_off::hand_off(&here, console)
    };
    match take {
        DesktopTake::Locked { wait } => {
            let turns = Turns {
                total: capped_wait(Duration::from_secs(wait), request.timeout.map(Duration::from_secs)),
                step: limits.wait_step,
                now: Instant::now,
            };
            locked(request, &turns, beat, console, hand)
        }
        DesktopTake::Session { .. } => {
            let session = account_and_console(beat.seams).map_err(|refused| desktop_refused(request, &refused, beat, console))?;
            hand(session, console)
        }
    }
}

/// The stage's lock for a locked run: the builder's desktop lock taken within the run's wait, cut into `turns`
/// ([`wait_for_desktop`]), the holder record written and the console's session read again while it is held, then
/// `hand` given that session. The record is removed and the lock released once the hand-off is over, however it ended.
fn locked(
    request: &Request<'_>,
    turns: &Turns<impl Fn() -> Instant>,
    beat: &Beat<'_>,
    console: &mut Console,
    hand: impl FnOnce(u32, &mut Console) -> Result<Ran, DesktopEnd>,
) -> Result<Ran, DesktopEnd> {
    let mutex = desktop_name(beat.seams)
        .and_then(|name| PrivateMutex::open(&name))
        .map_err(|error| desktop_refused(request, &DesktopRefusal::DesktopLock(error.to_string()).into(), beat, console))?;
    let record = HolderRecord::of(request);
    let guard = match wait_for_desktop(&mutex, turns, &record, beat, console) {
        Ok(guard) => guard,
        Err(Unheld::Ended(status)) => return Err(DesktopEnd::Ended(status)),
        Err(Unheld::Busy) => return Err(said_refused(request, &[busy_timeout_line(turns.total)], beat, console)),
        Err(Unheld::Failed(error)) => {
            let refusal = DesktopRefusal::Machinery(format!("the builder's desktop lock cannot be waited for: {error}"));
            return Err(desktop_refused(request, &refusal.into(), beat, console));
        }
    };
    // Written while the lock is held and removed before it is let go, so no run ever removes the next holder's record.
    let mut say = |line: &str| beat.say(console, line);
    let session = held_session(&record, request.repo, || account_and_console(beat.seams), &mut say);
    let session_id = session.map_err(|refused| desktop_refused(request, &refused, beat, console))?;
    let ran = hand(session_id, console);
    record.remove(&mut |line: &str| beat.say(console, line));
    drop(guard);
    ran
}

/// The record of the run holding the builder's desktop, which a run waiting for it names in its busy-wait line and
/// decides nothing from: [`HOLDER_RECORD`] in the box's work root, the folder of the run's work folder. It is written,
/// its `.tmp` stage included, and read without following a link there and only as a plain file ([`write_record_file`],
/// [`shared_record`]). Each failure is said once on stderr through `say`, and none fails the run.
struct HolderRecord {
    /// The record's path; `None` for a work folder in no folder, which keeps none.
    path: Option<String>,
}

impl HolderRecord {
    /// The holder record of the work root of `request`'s work folder.
    fn of(request: &Request<'_>) -> HolderRecord {
        HolderRecord::in_root(Path::new(request.work).parent())
    }

    /// The holder record in the work root `root`.
    fn in_root(root: Option<&Path>) -> HolderRecord {
        HolderRecord {
            path: root.and_then(|root| root.join(HOLDER_RECORD).to_str().map(str::to_owned)),
        }
    }

    /// The holder the record names; `None` when there is none, said through `say` when the record cannot be read or is
    /// not a holder record.
    fn holder(&self, say: &mut impl FnMut(&str)) -> Option<DesktopHolder> {
        let path = self.path.as_deref()?;
        let text = match shared_record(path).and_then(|file| file.map(read_whole).transpose()) {
            Ok(text) => text?,
            Err(error) => {
                say(&record_failed_line(path, "read", &error.to_string()));
                return None;
            }
        };
        let holder = DesktopHolder::read(&text);
        if holder.is_none() {
            say(&record_failed_line(path, "read", "it is not a holder record"));
        }
        holder
    }

    /// Writes the record of a run of `repo` that takes the builder's desktop now, whole: to a `.tmp` stage first, then
    /// renamed over the record, which replaces a link there rather than following it. When the write fails, the record
    /// an earlier holder left is removed, so this run's hold is never said to be another's.
    fn write(&self, repo: &str, say: &mut impl FnMut(&str)) {
        let Some(path) = self.path.as_deref() else { return };
        let holder = DesktopHolder {
            repo: repo.to_owned(),
            since: Timestamp::now(),
        };
        let staged = format!("{path}.tmp");
        if let Err(error) = write_record_file(&staged, &holder.record()).and_then(|()| fs::rename(&staged, path)) {
            say(&record_failed_line(path, "written", &error.to_string()));
            self.remove(say);
        }
    }

    /// Removes the record; one already gone is left so.
    fn remove(&self, say: &mut impl FnMut(&str)) {
        let Some(path) = self.path.as_deref() else { return };
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => say(&record_failed_line(path, "removed", &error.to_string())),
            _ => {}
        }
    }
}

/// The holder record at `path` opened to be read, `Ok(None)` when there is none: without following a link there and
/// checked a plain file, as [`crate::boxside::tools::read_record`] opens a record, but sharing read, write and delete, so a waiter reading it
/// never stops the holder's rename over it or its removal, nor another waiter's read.
fn shared_record(path: &str) -> io::Result<Option<File>> {
    let file = match OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    gate_win32::check_plain_file(&file)?;
    Ok(Some(file))
}

/// The whole text of `file`.
fn read_whole(mut file: File) -> io::Result<String> {
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

/// The box's line saying the holder record at `path` could not be `done` (written, read or removed), and why.
fn record_failed_line(path: &str, done: &str, why: &str) -> String {
    format!(
        "gate: the builder's desktop holder record {} could not be {done}: {}",
        escaped(path),
        escaped(why)
    )
}

/// The console's session read again while the run holds the builder's desktop (`read`), its holder `record` of a run
/// of `repo` written first; a session that refuses the run removes the record before the lock is let go. Failures of
/// the record are said through `say`.
fn held_session(
    record: &HolderRecord,
    repo: &str,
    read: impl FnOnce() -> Result<u32, ConsoleRefused>,
    say: &mut impl FnMut(&str),
) -> Result<u32, ConsoleRefused> {
    record.write(repo, say);
    read().inspect_err(|_| record.remove(say))
}

/// Why the builder's desktop lock was not taken.
enum Unheld {
    /// The run ended on a stop, with this status, its line said, or on a gone client with [`CLOSED_STATUS`].
    Ended(u32),
    /// Another run held it for the whole wait.
    Busy,
    /// The wait for it failed.
    Failed(io::Error),
}

/// Takes the builder's desktop lock `mutex`, waiting in `turns`: the run's client-gone flag and then the stop signal
/// are looked at before each turn, a gone client ending the wait with [`CLOSED_STATUS`] and no line, the busy-wait
/// line, naming the holder `record` names when it names one, is said once when another run holds the lock and some of
/// the wait is left, and a lock taken from a run that ended holding it is said. A total of zero is one try, with no
/// wait and no busy-wait line.
fn wait_for_desktop<'m>(
    mutex: &'m PrivateMutex,
    turns: &Turns<impl Fn() -> Instant>,
    record: &HolderRecord,
    beat: &Beat<'_>,
    console: &mut Console,
) -> Result<MutexGuard<'m>, Unheld> {
    let look = |console: &mut Console| {
        // Nothing has started yet, and the box's output that found the client gone is no place for a line.
        if beat.client_gone() {
            return Err(CLOSED_STATUS);
        }
        before_the_job(beat.stop.look(STARTING_COMMAND), beat.stop.job, beat.lock, console)
    };
    let busy = |console: &mut Console| {
        let holder = record.holder(&mut |line: &str| beat.say(console, line));
        beat.say(console, &busy_wait_line(holder.as_ref(), Timestamp::now()));
    };
    let guard = waited(turns, console, look, |bound| mutex.acquire(bound), busy)?;
    if guard.abandoned() {
        beat.say(console, &abandoned_line());
    }
    Ok(guard)
}

/// How a wait for the builder's desktop is cut into turns: `total` in all, turns of at most `step`, on the clock `now`.
struct Turns<N> {
    total: Duration,
    step: Duration,
    now: N,
}

/// The turns of [`wait_for_desktop`]: before each, `look` at the stop signal; each `acquire` given at most a step and
/// never more than is left of the total; `busy` said once, after a turn that timed out with some of the wait left.
fn waited<G>(
    turns: &Turns<impl Fn() -> Instant>,
    console: &mut Console,
    look: impl Fn(&mut Console) -> Result<(), u32>,
    mut acquire: impl FnMut(Duration) -> Result<G, AcquireError>,
    busy: impl Fn(&mut Console),
) -> Result<G, Unheld> {
    let started = (turns.now)();
    let mut busy_said = false;
    loop {
        look(console).map_err(Unheld::Ended)?;
        let left = turns.total.saturating_sub((turns.now)().saturating_duration_since(started));
        match acquire(turns.step.min(left)) {
            Ok(guard) => return Ok(guard),
            Err(AcquireError::Timeout) => {
                if !busy_said && !left.is_zero() {
                    busy_said = true;
                    busy(console);
                }
                if (turns.now)().saturating_duration_since(started) >= turns.total {
                    return Err(Unheld::Busy);
                }
            }
            Err(AcquireError::Failed(error)) => return Err(Unheld::Failed(error)),
        }
    }
}

/// Refuses the run for the builder's desktop as `refused` says.
fn desktop_refused(request: &Request<'_>, refused: &ConsoleRefused, beat: &Beat<'_>, console: &mut Console) -> DesktopEnd {
    said_refused(request, &refused.lines(), beat, console)
}

/// Refuses the run for the builder's desktop: the `desktop` refusal tag, then `lines`, said together under the box's
/// output lock, so the PC ends the run with the reason, never running a desktop command itself.
fn said_refused(request: &Request<'_>, lines: &[String], beat: &Beat<'_>, console: &mut Console) -> DesktopEnd {
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    stamping::protocol(console, &refused_line(request.nonce, Refusal::Desktop));
    for line in lines {
        stamping::err(console, line);
    }
    DesktopEnd::Refused
}

/// The box's own account, then the console's session judged for it ([`read_console`]): the session's id, or why the
/// console cannot take the command.
fn account_and_console(seams: &BoxSeams) -> Result<u32, ConsoleRefused> {
    let account =
        gate_win32::current_account().map_err(|error| DesktopRefusal::Machinery(format!("the box's own account cannot be read: {error}")))?;
    read_console(|| console_session(seams, &account), &account)
}

/// The console's session as `read` gives it, judged for the box's `account`: the session's id when its desktop can
/// take the command, else why not. A session `read` cannot read, or one that changed while it read, is refused.
fn read_console(read: impl FnOnce() -> Result<Option<SessionState>, Unread>, account: &Account) -> Result<u32, ConsoleRefused> {
    let session = read().map_err(|unread| match unread {
        Unread::Failed(error) => ConsoleRefused {
            refusal: DesktopRefusal::SessionUnreadable,
            error: Some(error),
        },
        Unread::Moved => DesktopRefusal::SessionMoved.into(),
    })?;
    let state = session.ok_or(DesktopRefusal::NobodyLoggedOn)?;
    session_verdict(Some(&state), &account.name, &account.domain)?;
    Ok(state.id)
}

/// The session attached to the console, `None` when none is; under the session seam, or the desktop seam alone, the
/// state the seam names, made of the box's own session and `account`.
fn console_session(seams: &BoxSeams, account: &Account) -> Result<Option<SessionState>, Unread> {
    let seamed = seams.desktop_session.or(seams.desktop.map(|_| DesktopSession::Active));
    match seamed {
        Some(state) => seam_session(state, account).map(Some).map_err(Unread::Failed),
        None => match gate_win32::active_console_session() {
            Some(id) => settled(id, gate_win32::session_state(id), gate_win32::active_console_session()),
            None => Ok(None),
        },
    }
}

/// The state `read` of the session `id` the console had, as long as the console still has `id` after the read (`now`):
/// a session switch between the two reads leaves the state of a session the console no longer shows. The switch is
/// looked at first, so a session that ended between the reads, and so could not be read, is one that moved.
fn settled(id: u32, read: io::Result<SessionState>, now: Option<u32>) -> Result<Option<SessionState>, Unread> {
    if now != Some(id) {
        return Err(Unread::Moved);
    }
    read.map(Some).map_err(Unread::Failed)
}

/// The console session the seam `state` names: the box's own session, with the box's `account` logged on to it, but
/// nobody for `none` and another user for `other-user`, locked for `locked` and disconnected for `disconnected`.
fn seam_session(state: DesktopSession, account: &Account) -> io::Result<SessionState> {
    let id = QueryOnlyProcess::open(std::process::id())?.session()?;
    let (user, connect, lock) = match state {
        DesktopSession::Active => (account.name.as_str(), ConnectState::Active, LockState::Unlocked),
        DesktopSession::NoUser => ("", ConnectState::Active, LockState::Unlocked),
        DesktopSession::Locked => (account.name.as_str(), ConnectState::Active, LockState::Locked),
        DesktopSession::OtherUser => (OTHER_USER, ConnectState::Active, LockState::Unlocked),
        DesktopSession::Disconnected => (account.name.as_str(), ConnectState::Disconnected, LockState::Unlocked),
    };
    Ok(SessionState {
        id,
        connect,
        user: user.to_owned(),
        domain: account.domain.clone(),
        lock,
    })
}

/// The limits of a desktop run's waits: the built ones, or those the desktop limit and message seams set
/// ([`Limits::for_run`]).
fn run_limits(seams: &BoxSeams) -> Limits {
    Limits::for_run(
        seams.desktop_ms.map(Duration::from_millis),
        seams.desktop_message_ms.map(Duration::from_millis),
    )
}

/// The builder's desktop lock's name: the production `gate\desktop`, or, under the mutex seam, a namespace of the
/// box's own user named by the seam's unique part.
fn desktop_name(seams: &BoxSeams) -> io::Result<PrivateName> {
    match seams.desktop_mutex.as_deref() {
        None => Ok(PrivateName::desktop()),
        Some(unique) => seam_name(unique),
    }
}

/// The desktop lock of the box's own user named by `unique`, as the mutex seam asks for it.
#[cfg(feature = "test-seams")]
fn seam_name(unique: &str) -> io::Result<PrivateName> {
    PrivateName::for_user(&gate_win32::current_user_sid()?, unique)
}

/// A gate built without the test seams parses no mutex seam, so none reaches here.
#[cfg(not(feature = "test-seams"))]
fn seam_name(_unique: &str) -> io::Result<PrivateName> {
    Err(io::Error::other("the desktop mutex seam needs a gate built with the test seams"))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use gate_win32::{Account, AcquireError, ConnectState, LockState, SessionState};

    use super::{
        ConsoleRefused, HOLDER_RECORD, HolderRecord, Turns, Unheld, Unread, held_session, read_console, read_whole, settled, shared_record, waited,
    };
    use crate::boxside::desktop::lines::DesktopRefusal;
    use crate::console::Console;
    use gate_router::escaped;

    #[test]
    fn the_desktop_seams_set_the_runs_limits() {
        use gate_args::BoxSeams;

        use super::run_limits;
        use crate::boxside::desktop::lines::{CONNECT_LIMIT, END_LIMIT, Limits, MESSAGE_LIMIT, WAIT_STEP};

        let seamed = run_limits(&BoxSeams {
            desktop_ms: Some(240_000),
            desktop_message_ms: Some(5000),
            ..BoxSeams::default()
        });
        assert_eq!(
            (seamed.connect, seamed.message, seamed.end),
            (Duration::from_mins(4), Duration::from_secs(5), Duration::from_mins(4)),
            "the limit seam sets connect and end, the message seam the message limit"
        );
        assert_eq!(
            run_limits(&BoxSeams::default()),
            Limits {
                connect: CONNECT_LIMIT,
                message: MESSAGE_LIMIT,
                end: END_LIMIT,
                wait_step: WAIT_STEP
            },
            "no seams give the built limits"
        );
    }

    /// The box's own account in every case.
    fn account() -> Account {
        Account {
            name: "lefto".to_owned(),
            domain: "MACHINE".to_owned(),
        }
    }

    /// The box's own console session 3, connected, in `lock`.
    fn own(lock: LockState) -> SessionState {
        SessionState {
            id: 3,
            connect: ConnectState::Active,
            user: "lefto".to_owned(),
            domain: "MACHINE".to_owned(),
            lock,
        }
    }

    /// The lines `read` is refused with, or the session id it gives.
    fn judged(read: Result<Option<SessionState>, Unread>) -> Result<u32, Vec<String>> {
        read_console(|| read, &account()).map_err(|refused: ConsoleRefused| refused.lines())
    }

    /// The box's line refusing a console session, for `why`.
    fn refused(why: &str) -> String {
        format!("gate: not routed: the builder has no desktop session: {why}")
    }

    #[test]
    fn the_read_under_the_lock_refuses_what_the_console_now_is() {
        assert_eq!(judged(Ok(Some(own(LockState::Unlocked)))), Ok(3), "the box's own unlocked session");
        assert_eq!(
            judged(Ok(Some(own(LockState::Locked)))),
            Err(vec![refused("the console session is locked")]),
            "a console locked since the cheap read"
        );
        assert_eq!(
            judged(Ok(None)),
            Err(vec![refused("nobody is logged on at the console")]),
            "no session at the console"
        );
        assert_eq!(
            judged(Err(Unread::Failed(io::Error::other("the session\nis gone")))),
            Err(vec![
                refused("the console session cannot be read"),
                format!("gate: the console session read failed: {}", escaped("the session\nis gone"))
            ]),
            "a session the box cannot read, its error said escaped on a line of its own"
        );
        assert_eq!(
            judged(Err(Unread::Moved)),
            Err(vec![refused("the console session changed while the box read it")]),
            "a session switched while the box read it"
        );
    }

    #[test]
    fn a_console_that_shows_another_session_after_the_read_has_moved() {
        assert!(matches!(settled(3, Ok(own(LockState::Unlocked)), Some(3)), Ok(Some(state)) if state.id == 3));
        assert!(
            matches!(settled(3, Ok(own(LockState::Unlocked)), Some(4)), Err(Unread::Moved)),
            "another session"
        );
        assert!(
            matches!(settled(3, Ok(own(LockState::Unlocked)), None), Err(Unread::Moved)),
            "no session now"
        );
        assert!(
            matches!(settled(3, Err(io::Error::other("gone")), Some(3)), Err(Unread::Failed(_))),
            "a read that failed is said as such"
        );
        assert!(
            matches!(settled(3, Err(io::Error::other("gone")), Some(4)), Err(Unread::Moved)),
            "a session that ended as another took the console moved, though its read failed"
        );
        assert!(
            matches!(settled(3, Err(io::Error::other("gone")), None), Err(Unread::Moved)),
            "a session that ended leaving the console empty moved, though its read failed"
        );
    }

    /// A desktop lock another run holds for good, on a clock each turn runs to the bound it was given: the bounds are
    /// kept, and how many times the busy line was said.
    struct Held {
        clock: Cell<Instant>,
        bounds: RefCell<Vec<Duration>>,
        busy: Cell<u32>,
    }

    /// The bounds a wait of `total` in turns of `step` gives a lock that stays held, and how often it said busy.
    fn turns_against_a_held_lock(total: Duration, step: Duration) -> (Vec<Duration>, u32) {
        let held = Held {
            clock: Cell::new(Instant::now()),
            bounds: RefCell::new(Vec::new()),
            busy: Cell::new(0),
        };
        let turns = Turns {
            total,
            step,
            now: || held.clock.get(),
        };
        let acquire = |bound: Duration| -> Result<(), AcquireError> {
            held.bounds.borrow_mut().push(bound);
            held.clock.set(held.clock.get() + bound);
            Err(AcquireError::Timeout)
        };
        let ended = waited(&turns, &mut Console::new(), |_| Ok(()), acquire, |_| held.busy.set(held.busy.get() + 1));
        assert!(matches!(ended, Err(Unheld::Busy)), "a lock held for good is busy");
        (held.bounds.into_inner(), held.busy.get())
    }

    #[test]
    fn a_zero_desktop_wait_is_one_try_with_no_busy_line() {
        assert_eq!(
            turns_against_a_held_lock(Duration::ZERO, Duration::from_millis(100)),
            (vec![Duration::ZERO], 0),
            "one try, no wait, and nothing said"
        );
    }

    #[test]
    fn a_desktop_wait_never_runs_past_its_total() {
        let (bounds, busy) = turns_against_a_held_lock(Duration::from_millis(250), Duration::from_millis(100));

        assert_eq!(bounds, [100, 100, 50].map(Duration::from_millis), "the last turn takes only what is left");
        assert_eq!(busy, 1, "the busy line, once");
    }

    #[test]
    fn a_refusal_without_a_system_error_is_one_line() {
        assert_eq!(ConsoleRefused::from(DesktopRefusal::Locked).lines().len(), 1);
    }

    /// A work root of its own for the holder record cases, removed when dropped.
    struct Root(PathBuf);

    impl Root {
        #[expect(clippy::expect_used, reason = "a case whose folder cannot be made must fail loudly")]
        fn new(test: &str) -> Root {
            let folder = crate::test_support::temp_dir().join(format!("gate-holder-{}-{test}", std::process::id()));
            let _ = fs::remove_dir_all(&folder);
            fs::create_dir_all(&folder).expect("the work root can be made");
            Root(folder)
        }

        fn record(&self) -> HolderRecord {
            HolderRecord::in_root(Some(&self.0))
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_holder_record_is_written_named_and_removed_saying_nothing() {
        let root = Root::new("round_trip");
        let record = root.record();
        let mut said = Vec::new();
        let mut say = |line: &str| said.push(line.to_owned());

        assert_eq!(record.holder(&mut say), None, "no record names no holder");
        record.write("yaat", &mut say);
        assert_eq!(record.holder(&mut say).map(|holder| holder.repo), Some("yaat".to_owned()));
        record.remove(&mut say);
        record.remove(&mut say);
        assert_eq!(record.holder(&mut say), None, "a removed record names no holder");
        assert!(said.is_empty(), "nothing failed, and a record already gone is no failure: {said:?}");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a case whose files cannot be made must fail loudly")]
    fn a_holder_record_that_is_no_record_or_a_second_name_names_no_holder_and_says_so() {
        let root = Root::new("unread");
        let record = root.record();
        let mut said = Vec::new();

        fs::write(root.path(HOLDER_RECORD), "{").expect("the record is planted");
        assert_eq!(record.holder(&mut |line: &str| said.push(line.to_owned())), None);
        fs::remove_file(root.path(HOLDER_RECORD)).expect("the planted record is removed");
        let other = root.path("other.json");
        fs::write(&other, r#"{"repo": "other", "since": "2026-11-01T12:00:00Z"}"#).expect("the other file is made");
        fs::hard_link(&other, root.path(HOLDER_RECORD)).expect("the second name is made");
        assert_eq!(
            record.holder(&mut |line: &str| said.push(line.to_owned())),
            None,
            "a second name is not read"
        );

        let prefix = "gate: the builder's desktop holder record ";
        assert_eq!(said.len(), 2, "each failed read is said once: {said:?}");
        assert!(
            said[0].starts_with(prefix) && said[0].ends_with("could not be read: it is not a holder record"),
            "{said:?}"
        );
        assert!(said[1].starts_with(prefix) && said[1].contains("could not be read: "), "{said:?}");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a case whose files cannot be made must fail loudly")]
    fn a_failed_write_is_said_removes_the_earlier_record_and_the_run_goes_on() {
        let root = Root::new("write_failed");
        let record = root.record();
        fs::write(root.path(HOLDER_RECORD), r#"{"repo": "earlier", "since": "2026-11-01T12:00:00Z"}"#).expect("the record is planted");
        // A second name at the stage, which the write refuses rather than write through.
        let other = root.path("other.txt");
        fs::write(&other, "kept").expect("the other file is made");
        fs::hard_link(&other, root.path(&format!("{HOLDER_RECORD}.tmp"))).expect("the second name is made");
        let mut said = Vec::new();

        let session = held_session(&record, "yaat", || Ok(3), &mut |line: &str| said.push(line.to_owned()));

        assert_eq!(session.ok(), Some(3), "the run goes on to its session");
        assert_eq!(said.len(), 1, "the failed write, said once: {said:?}");
        assert!(said[0].contains("could not be written: "), "{said:?}");
        assert!(!root.path(HOLDER_RECORD).exists(), "the earlier holder's record is removed");
        assert_eq!(
            fs::read_to_string(&other).expect("the other file reads"),
            "kept",
            "nothing is written through the link"
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a case whose record cannot be opened must fail loudly")]
    fn a_waiter_reading_the_holder_record_never_stops_the_holder_or_another_waiter() {
        let root = Root::new("shared");
        let record = root.record();
        let path = root.path(HOLDER_RECORD).to_str().expect("the path is Unicode").to_owned();
        let mut said = Vec::new();
        record.write("first", &mut |line: &str| said.push(line.to_owned()));

        let held = shared_record(&path).expect("the record opens").expect("the record is there");
        let second = shared_record(&path).expect("a second waiter opens it too").expect("the record is there");
        assert!(
            read_whole(second).expect("the second waiter reads it").contains("first"),
            "the second waiter reads the record"
        );
        record.write("yaat", &mut |line: &str| said.push(line.to_owned()));
        let renamed = shared_record(&path).expect("the new record opens").expect("the new record is there");
        assert!(
            read_whole(renamed).expect("the new record reads").contains("yaat"),
            "the rename replaced the record"
        );
        let held_new = shared_record(&path).expect("the new record opens").expect("the new record is there");
        record.remove(&mut |line: &str| said.push(line.to_owned()));
        drop((held, held_new));

        assert!(said.is_empty(), "the rename over the open record and its removal both succeed: {said:?}");
        assert!(!root.path(HOLDER_RECORD).exists(), "the removed record is gone");
    }

    #[test]
    fn a_session_refused_under_the_lock_removes_the_holder_record() {
        let root = Root::new("refused");
        let record = root.record();
        let mut said = Vec::new();
        let mut written = false;

        let session = held_session(
            &record,
            "yaat",
            || {
                written = root.path(HOLDER_RECORD).is_file();
                Err(DesktopRefusal::Locked.into())
            },
            &mut |line: &str| said.push(line.to_owned()),
        );

        assert!(session.is_err(), "the locked console refuses the run");
        assert!(written, "the record was written before the session was read");
        assert!(!root.path(HOLDER_RECORD).exists(), "the refused run removed its record");
        assert!(said.is_empty(), "{said:?}");
    }
}
