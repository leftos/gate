//! A run that asks for the builder's desktop: the console's session read before the checkout, the stage between the
//! run's readiness and its command, which resolves the program, takes the builder's desktop lock within the wait the
//! run gave and reads the console's session once more while it holds the lock, and then the hand-off of the command to
//! the desktop helper ([`hand_off`]). Every failure of the desktop's machinery before the command's start marker is a
//! refusal: the box's `desktop` tag and its line, exit 75, and never a back commits line, so the PC runs the command
//! itself.

mod hand_off;

use std::io;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use gate_args::{BoxSeams, DesktopSession};
use gate_win32::{Account, AcquireError, ConnectState, LockState, MutexGuard, PrivateMutex, PrivateName, QueryOnlyProcess, SessionState};

use crate::boxside::desktop::lines::{DesktopRefusal, Limits, abandoned_line, busy_timeout_line, busy_wait_line, capped_wait, session_verdict};
use crate::boxside::payload::escaped;
use crate::boxside::run::{Beat, One, REFUSED_STATUS, Ran, Request, STARTING_COMMAND, before_the_job, prepared};
use crate::console::Console;
use crate::remote::{Refusal, refused_line};

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
    session_id: u32,
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
/// ([`wait_for_desktop`]); and reads the console's session again while it holds the lock. The hand-off
/// ([`hand_off::hand_off`]) then runs the command in that session and sees it to its end; the lock is released once it
/// is over, however it ended.
pub(super) fn desktop_command(request: &Request<'_>, one: &One<'_>, wait: u64, beat: &Beat<'_>, console: &mut Console) -> Result<Ran, DesktopEnd> {
    let prepared = match prepared(request, one, console) {
        Ok(prepared) => prepared,
        Err(answer) => return answer.map_err(DesktopEnd::Ended),
    };
    // Looked at before the lock is opened, so a run already stopped never opens it.
    before_the_job(beat.stop.look(STARTING_COMMAND), beat.stop.job, beat.lock, console).map_err(DesktopEnd::Ended)?;
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
    let here = hand_off::Here {
        request,
        one,
        prepared: &prepared,
        beat,
        limits: hold.limits,
        session: hold.session_id,
    };
    let ran = hand_off::hand_off(&here, console);
    drop(hold);
    ran
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

/// Takes the builder's desktop lock `mutex`, waiting in turns of at most `step`, and never past `total`: the stop signal
/// is looked at before each turn, the busy-wait line is said once when another run holds the lock and some of the wait
/// is left, and a lock taken from a run that ended holding it is said. A `total` of zero is one try, with no wait and
/// no busy-wait line.
fn wait_for_desktop<'m>(
    mutex: &'m PrivateMutex,
    total: Duration,
    step: Duration,
    beat: &Beat<'_>,
    console: &mut Console,
) -> Result<MutexGuard<'m>, Unheld> {
    let turns = Turns {
        total,
        step,
        now: Instant::now,
    };
    let look = |console: &mut Console| before_the_job(beat.stop.look(STARTING_COMMAND), beat.stop.job, beat.lock, console);
    let busy = |console: &mut Console| beat.say(console, &busy_wait_line());
    let guard = waited(&turns, console, look, |bound| mutex.acquire(bound), busy)?;
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
    let id = gate_win32::session_of(QueryOnlyProcess::open(std::process::id())?.handle())?;
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
    use std::cell::{Cell, RefCell};
    use std::io;
    use std::time::{Duration, Instant};

    use gate_win32::{Account, AcquireError, ConnectState, LockState, SessionState};

    use super::{ConsoleRefused, Turns, Unheld, Unread, read_console, settled, waited};
    use crate::boxside::desktop::lines::DesktopRefusal;
    use crate::boxside::payload::escaped;
    use crate::console::Console;

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
}
