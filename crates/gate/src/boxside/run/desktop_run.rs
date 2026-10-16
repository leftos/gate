//! A run that asks for the builder's desktop: the console's session read before the checkout, and the stage between
//! the run's readiness and its command, which resolves the program, takes the builder's desktop lock within the wait
//! the run gave and reads the console's session once more while it holds the lock. Every failure of the desktop's
//! machinery before the command starts is a refusal: the box's `desktop` tag and its line, exit 75, and never a back
//! commits line, so the PC runs the command itself.

use std::io;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use gate_args::{BoxSeams, DesktopSession};
use gate_win32::{Account, AcquireError, ConnectState, LockState, MutexGuard, PrivateMutex, PrivateName, RemoteProcess, SessionState};

use crate::boxside::desktop::lines::{DesktopRefusal, Limits, abandoned_line, busy_timeout_line, busy_wait_line, capped_wait, session_verdict};
use crate::boxside::payload::escaped;
use crate::boxside::run::{Beat, One, Prepared, REFUSED_STATUS, Ran, Request, STARTING_COMMAND, before_the_job, prepared};
use crate::console::Console;
use crate::remote::{Refusal, refused_line};

/// Why the desktop helper did not start, while the box has no hand-off to it.
const NO_HAND_OFF: &str = "the box's hand-off is not built yet";

/// The user name the session seam `other-user` puts at the console: one no account of the box has.
const OTHER_USER: &str = "gate-seam-other-user";

/// How a desktop run that did not run its command ended, its lines said.
pub(super) enum DesktopEnd {
    /// The builder's desktop refused it, its `desktop` tag said: 75, and no back commits line.
    Refused,
    /// It ended otherwise, with this status: a stop, a payload's word refused, or the box's own failure.
    Ended(u32),
}

/// What a desktop run holds once the stage passed: the builder's desktop lock, the console session it read while it
/// held the lock, and the limits of its waits.
struct DesktopHold<'m> {
    _guard: MutexGuard<'m>,
    #[expect(dead_code, reason = "nothing reads the session until the run hands its command to the helper")]
    session_id: u32,
    #[expect(dead_code, reason = "nothing reads the limits until the run hands its command to the helper")]
    limits: Limits,
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
            console.err(&refused_line(request.nonce, Refusal::Desktop));
            for line in refused.lines() {
                console.err(&line);
            }
            Err(REFUSED_STATUS)
        }
    }
}

/// The run's command on the builder's desktop, which may wait `wait` seconds for it: the stage, then the hand-off.
/// The stage resolves the program first, so a command that cannot start (127, or 2 for a batch word refused) never
/// holds the desktop; looks at the stop signal; takes the builder's desktop lock within the run's wait
/// ([`wait_for_desktop`]); and reads the console's session again while it holds the lock. The hand-off is not built: a
/// run past the stage releases the lock and is refused.
pub(super) fn desktop_command(request: &Request<'_>, one: &One<'_>, wait: u64, beat: &Beat<'_>, console: &mut Console) -> Result<Ran, DesktopEnd> {
    let _prepared: Prepared<'_> = match prepared(request, one, console) {
        Ok(prepared) => prepared,
        Err(answer) => return answer.map_err(DesktopEnd::Ended),
    };
    // Looked at before the lock is opened, so a run already stopped never opens it.
    before_the_job(beat.stop.look(STARTING_COMMAND), beat.stop.job, console).map_err(DesktopEnd::Ended)?;
    let mutex = desktop_name(beat.seams)
        .and_then(|name| PrivateMutex::open(&name))
        .map_err(|error| desktop_refused(request, &DesktopRefusal::DesktopLock(error.to_string()).into(), beat, console))?;
    let limits = Limits::capped(beat.seams.desktop_ms.map(Duration::from_millis));
    let total = capped_wait(Duration::from_secs(wait), request.timeout.map(Duration::from_secs));
    let guard = match wait_for_desktop(&mutex, total, limits.wait_step, beat, console) {
        Ok(guard) => guard,
        Err(Unheld::Ended(status)) => return Err(DesktopEnd::Ended(status)),
        Err(Unheld::Busy) => return Err(said_refused(request, &[busy_timeout_line(total)], beat, console)),
        Err(Unheld::Failed(error)) => {
            let refusal = DesktopRefusal::Machinery(format!("the builder's desktop lock cannot be waited for: {error}"));
            return Err(desktop_refused(request, &refusal.into(), beat, console));
        }
    };
    let session_id = account_and_console(beat.seams).map_err(|refused| desktop_refused(request, &refused, beat, console))?;
    let hold = DesktopHold {
        _guard: guard,
        session_id,
        limits,
    };
    drop(hold);
    Err(desktop_refused(
        request,
        &DesktopRefusal::HelperDidNotStart(NO_HAND_OFF.to_owned()).into(),
        beat,
        console,
    ))
}

/// Why the builder's desktop lock was not taken.
enum Unheld {
    /// The run ended on a stop, with this status, its line said.
    Ended(u32),
    /// Another run held it for the whole wait.
    Busy,
    /// The wait for it failed.
    Failed(io::Error),
}

/// Takes the builder's desktop lock `mutex`, waiting in turns of `step` for at most `total`: the stop signal is looked
/// at before each turn, the busy-wait line is said once when another run holds the lock, and a lock taken from a run
/// that ended holding it is said.
fn wait_for_desktop<'m>(
    mutex: &'m PrivateMutex,
    total: Duration,
    step: Duration,
    beat: &Beat<'_>,
    console: &mut Console,
) -> Result<MutexGuard<'m>, Unheld> {
    let started = Instant::now();
    let mut busy_said = false;
    loop {
        before_the_job(beat.stop.look(STARTING_COMMAND), beat.stop.job, console).map_err(Unheld::Ended)?;
        match mutex.acquire(step) {
            Ok(guard) => {
                if guard.abandoned() {
                    beat.say(console, &abandoned_line());
                }
                return Ok(guard);
            }
            Err(AcquireError::Timeout) => {
                if !busy_said {
                    busy_said = true;
                    beat.say(console, &busy_wait_line());
                }
                if started.elapsed() >= total {
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
/// output lock, so the PC runs the command itself.
fn said_refused(request: &Request<'_>, lines: &[String], beat: &Beat<'_>, console: &mut Console) -> DesktopEnd {
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    console.err(&refused_line(request.nonce, Refusal::Desktop));
    for line in lines {
        console.err(line);
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
/// a session switch between the two reads leaves the state of a session the console no longer shows.
fn settled(id: u32, read: io::Result<SessionState>, now: Option<u32>) -> Result<Option<SessionState>, Unread> {
    let state = read.map_err(Unread::Failed)?;
    if now != Some(id) {
        return Err(Unread::Moved);
    }
    Ok(Some(state))
}

/// The console session the seam `state` names: the box's own session, with the box's `account` logged on to it, but
/// nobody for `none` and another user for `other-user`, locked for `locked` and disconnected for `disconnected`.
fn seam_session(state: DesktopSession, account: &Account) -> io::Result<SessionState> {
    let id = gate_win32::session_of(RemoteProcess::open(std::process::id())?.handle())?;
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
    use std::io;

    use gate_win32::{Account, ConnectState, LockState, SessionState};

    use super::{ConsoleRefused, Unread, read_console, settled};
    use crate::boxside::desktop::lines::DesktopRefusal;
    use crate::boxside::payload::escaped;

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
    }

    #[test]
    fn a_refusal_without_a_system_error_is_one_line() {
        assert_eq!(ConsoleRefused::from(DesktopRefusal::Locked).lines().len(), 1);
    }
}
