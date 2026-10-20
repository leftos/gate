//! The box's lines for the builder's desktop: why it refuses a run's command, what the Task Scheduler's answer means,
//! how long each wait lasts, and the line a command it could not end is reported on. Every value a builder outside the
//! box made is escaped into the line it lands in; nothing here reads a clock, a handle or a process.

use std::io::{self, Write};
use std::num::NonZeroU32;
use std::time::Duration;

use gate_desktop::HelperExit;
use gate_router::escaped;
use gate_win32::{ConnectState, EndReport, LockState, SessionState, StartedTask, TaskError};

use crate::boxside::desktop::verify::{Unproved, WrongFact};

/// The box's line refusing a run it cannot hand to the builder's desktop: the box's own prefix, then why.
const NO_DESKTOP: &str = "gate: not routed: the builder has no desktop session: ";

/// Why the desktop helper did not start when the Task Scheduler never answered, as the box words it after `the desktop
/// helper did not start: `.
const SCHEDULER_SILENT: &str = "the Task Scheduler did not answer in time";

/// Why the desktop helper did not start when the Task Scheduler started the task but gave no process for it, as the box
/// words it after `the desktop helper did not start: `.
const NO_PID: &str = "the Task Scheduler gave no process id";

/// The longest the box waits, in its own budget, for the helper to connect its control pipe, unless a test cap lowers
/// it. The waits the helper keeps are its own budgets, which no box limit changes.
pub(crate) const CONNECT_LIMIT: Duration = Duration::from_secs(10);

/// The longest the box waits, in its own budget, for one message from the helper, or for the helper to take one,
/// unless a test cap lowers it. The waits the helper keeps are its own budgets, which no box limit changes.
pub(crate) const MESSAGE_LIMIT: Duration = Duration::from_secs(5);

/// The longest the box waits, in its own budget, for the helper to end after it said `Done`, unless a test cap lowers
/// it. The waits the helper keeps are its own budgets, which no box limit changes.
pub(crate) const END_LIMIT: Duration = Duration::from_secs(10);

/// How long one turn of the wait for the builder's desktop lasts, unless a test cap lowers it.
pub(crate) const WAIT_STEP: Duration = Duration::from_millis(100);

/// Why the box cannot hand a run's command to the builder's desktop, in the box's own words.
///
/// Every value a builder outside the box made is held here as it came and escaped once, in [`DesktopRefusal::line`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DesktopRefusal {
    /// No session is attached to the console, or the one that is has nobody logged on to it.
    NobodyLoggedOn,
    /// Another user's session holds the console.
    AnotherUser,
    /// The console session's user is logged on, but no client or display is connected to it.
    Disconnected,
    /// The console session's desktop is the lock screen.
    Locked,
    /// The console session's lock state cannot be read, so nothing says its desktop is not the lock screen.
    LockUnknown,
    /// The helper's task, or its folder, is not registered.
    TaskNotRegistered,
    /// The helper's task is registered with a definition other than the one the box checks for; the text names the
    /// first setting that differs.
    TaskSetUpWrong(String),
    /// The helper did not start, for this reason.
    HelperDidNotStart(String),
    /// The helper that answered runs another build than the box.
    HelperOtherVersion {
        /// The version the helper reported for itself.
        helper: String,
        /// The version of the box that runs the run.
        box_version: String,
    },
    /// The helper is not on the console's Default desktop, so it cannot start the command there.
    HelperNotOnDesktop,
    /// The helper ended with this exit code.
    HelperFailed(u32),
    /// The command the helper started is not the one the box asked for, for this reason.
    WrongCommand(WrongFact),
    /// The command the helper started could not be taken, for this reason, with the system's error.
    WrongCommandFor(WrongFact, String),
    /// A fact of the command the helper started, so named, cannot be read, for this error.
    CommandUnread {
        /// The fact's name, such as `session`.
        fact: &'static str,
        /// The system's error.
        error: String,
    },
    /// The command the helper started passed every check, but the box could not put it in the run's job, for this
    /// error.
    NotPutInTheJob(String),
    /// The builder's desktop lock cannot be made, for this reason.
    DesktopLock(String),
    /// The console session cannot be read, so nothing says who is logged on to it or whether it is locked.
    SessionUnreadable,
    /// Another session was attached to the console while the box read which one is and what it is.
    SessionMoved,
    /// The box's own machinery for the builder's desktop failed, as this says.
    Machinery(String),
}

impl DesktopRefusal {
    /// The refusal's own words, after the box's `gate: not routed: the builder has no desktop session: ` prefix. A
    /// value from outside comes through as it came: [`DesktopRefusal::line`] escapes the whole line.
    fn why(&self) -> String {
        match self {
            DesktopRefusal::NobodyLoggedOn => "nobody is logged on at the console".to_owned(),
            DesktopRefusal::AnotherUser => "another user is logged on at the console".to_owned(),
            DesktopRefusal::Disconnected => "the console session is disconnected".to_owned(),
            DesktopRefusal::Locked => "the console session is locked".to_owned(),
            DesktopRefusal::LockUnknown => "the console session's lock state cannot be read".to_owned(),
            DesktopRefusal::TaskNotRegistered => "the desktop helper task is not registered (gate push registers it)".to_owned(),
            DesktopRefusal::TaskSetUpWrong(what) => format!("the desktop helper task is set up wrong: {what}"),
            DesktopRefusal::HelperDidNotStart(why) => format!("the desktop helper did not start: {why}"),
            DesktopRefusal::HelperOtherVersion { helper, box_version } => format!("the desktop helper runs gate {helper}, the box {box_version}"),
            DesktopRefusal::HelperNotOnDesktop => "the desktop helper is not on the console's Default desktop".to_owned(),
            DesktopRefusal::HelperFailed(code) => format!("the desktop helper failed: {}", helper_exit_why(*code)),
            DesktopRefusal::WrongCommand(fact) => format!("the desktop helper failed: the command it started {}", fact.why()),
            DesktopRefusal::WrongCommandFor(fact, error) => {
                format!("the desktop helper failed: the command it started {}: {error}", fact.why())
            }
            DesktopRefusal::CommandUnread { fact, error } => {
                format!("the desktop helper failed: the {fact} of the command it started cannot be read: {error}")
            }
            DesktopRefusal::NotPutInTheJob(error) => {
                format!("the desktop helper failed: the command it started cannot be put in the box's job: {error}")
            }
            DesktopRefusal::DesktopLock(error) => format!("the builder's desktop lock cannot be made: {error}"),
            DesktopRefusal::SessionUnreadable => "the console session cannot be read".to_owned(),
            DesktopRefusal::SessionMoved => "the console session changed while the box read it".to_owned(),
            DesktopRefusal::Machinery(why) => why.clone(),
        }
    }

    /// The box's line refusing the run, escaped into one line.
    #[must_use]
    pub(crate) fn line(&self) -> String {
        format!("{NO_DESKTOP}{}", escaped(&self.why()))
    }
}

impl From<Unproved<WrongFact>> for DesktopRefusal {
    /// The refusal of a command the box did not verify: the fact that does not hold, or the one that cannot be read.
    fn from(unproved: Unproved<WrongFact>) -> DesktopRefusal {
        match unproved {
            Unproved::Wrong(fact) => DesktopRefusal::WrongCommand(fact),
            Unproved::Unread { fact, error } => DesktopRefusal::CommandUnread { fact, error },
        }
    }
}

/// Whether the console session the box read can take the run's command: `Ok(())` when its desktop can be handed the
/// command, else why it cannot.
///
/// `session` is what [`gate_win32::active_console_session`] named, read with [`gate_win32::session_state`], and `None`
/// when it named no session at all; `user` and `domain` are the box's own account. The first rule that matches
/// decides, in this order: no session, or a session with nobody logged on to it, is nobody at the console; a session
/// whose account differs from the box's own, its domain included and ASCII case-insensitively, is another user; a
/// session whose connection state is not [`ConnectState::Active`] is disconnected; a locked session is locked; and a
/// session whose lock state cannot be read is refused, since nothing says its desktop is not the lock screen.
pub(crate) fn session_verdict(session: Option<&SessionState>, user: &str, domain: &str) -> Result<(), DesktopRefusal> {
    let Some(state) = session else {
        return Err(DesktopRefusal::NobodyLoggedOn);
    };
    if state.user.is_empty() {
        return Err(DesktopRefusal::NobodyLoggedOn);
    }
    if !state.user.eq_ignore_ascii_case(user) || !state.domain.eq_ignore_ascii_case(domain) {
        return Err(DesktopRefusal::AnotherUser);
    }
    if state.connect != ConnectState::Active {
        return Err(DesktopRefusal::Disconnected);
    }
    match state.lock {
        LockState::Locked => Err(DesktopRefusal::Locked),
        LockState::Unknown => Err(DesktopRefusal::LockUnknown),
        LockState::Unlocked => Ok(()),
    }
}

/// What the box does about the helper task it asked the Task Scheduler to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TaskVerdict {
    /// The Task Scheduler gave the helper's process.
    Started(NonZeroU32),
    /// Refuse with this: the task was not started at all, so no helper of this run is running.
    Refused(DesktopRefusal),
    /// Refuse with this, and a helper of this run may be running: the why may be that the Task Scheduler gave no
    /// process for a task it started. The caller drops its control pipe before it does anything else, so it never joins
    /// a helper started on a pipe it no longer serves.
    MaybeStarted(DesktopRefusal),
    /// No refusal: the run's own stop ended the wait for the Task Scheduler. The task may have been started all the
    /// same, so the caller drops its control pipe first, as for [`TaskVerdict::MaybeStarted`], and says its
    /// stopped-before-start line after that.
    Stopped,
}

/// What a task start means for the run: the process it gave, or why the box refuses the run for it.
///
/// A task the Scheduler did not start at all, one whose definition differs, and one whose start failed before anything
/// was started are plain refusals. Everything else leaves a helper possibly running: [`TaskError::StartedUnknown`],
/// [`TaskError::TimedOut`] and [`TaskError::Unanswered`] say the start may have been requested, and a task whose
/// process the Scheduler never reported ([`StartedTask::NoPid`]) may be running without one, so all of those become
/// [`TaskVerdict::MaybeStarted`]. A start ended by the run's own stop ([`TaskError::Woken`]) is
/// [`TaskVerdict::Stopped`], which the caller treats the same way.
pub(crate) fn task_verdict(started: Result<StartedTask, TaskError>) -> TaskVerdict {
    match started {
        Ok(StartedTask::Process(pid)) => TaskVerdict::Started(pid),
        Ok(StartedTask::NoPid) => TaskVerdict::MaybeStarted(DesktopRefusal::HelperDidNotStart(NO_PID.to_owned())),
        Err(TaskError::NotRegistered) => TaskVerdict::Refused(DesktopRefusal::TaskNotRegistered),
        Err(TaskError::SetUpWrong(what)) => TaskVerdict::Refused(DesktopRefusal::TaskSetUpWrong(what)),
        Err(TaskError::Failed(why)) => TaskVerdict::Refused(DesktopRefusal::HelperDidNotStart(why)),
        Err(TaskError::StartedUnknown(why) | TaskError::Unanswered(why)) => TaskVerdict::MaybeStarted(DesktopRefusal::HelperDidNotStart(why)),
        Err(TaskError::TimedOut) => TaskVerdict::MaybeStarted(DesktopRefusal::HelperDidNotStart(SCHEDULER_SILENT.to_owned())),
        Err(TaskError::Woken(_)) => TaskVerdict::Stopped,
    }
}

/// Why a helper that ended with the exit code `code` did so, for the box's `the desktop helper failed: ` line: a known
/// exit's own words, or that the code is no exit this build has.
fn helper_exit_why(code: u32) -> String {
    HelperExit::from_code(code).map_or_else(|| format!("it exited with code {code}"), |exit| exit.why().to_owned())
}

/// How long a run may wait for the builder's desktop: the wait it asked for, or its own timeout when that comes first.
/// A run with no timeout waits what it asked for.
#[must_use]
pub(crate) fn capped_wait(wait: Duration, timeout: Option<Duration>) -> Duration {
    match timeout {
        Some(timeout) => wait.min(timeout),
        None => wait,
    }
}

/// The limits one desktop run uses: how long it waits for the helper to connect, for each message, for the helper to
/// end after it said `Done`, and for one turn of the wait for the builder's desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// The wait for the helper to connect its control pipe.
    pub(crate) connect: Duration,
    /// The wait for one message from the helper, or for the helper to take one.
    pub(crate) message: Duration,
    /// The wait for the helper to end after it said `Done`.
    pub(crate) end: Duration,
    /// One turn of the wait for the builder's desktop.
    pub(crate) wait_step: Duration,
}

impl Limits {
    /// The built limits, each lowered to `cap` when a test cap gives one. A cap lowers every limit above it and leaves
    /// those below it alone.
    #[must_use]
    pub(crate) fn capped(cap: Option<Duration>) -> Limits {
        let lowered = |built: Duration| cap.map_or(built, |cap| built.min(cap));
        Limits {
            connect: lowered(CONNECT_LIMIT),
            message: lowered(MESSAGE_LIMIT),
            end: lowered(END_LIMIT),
            wait_step: lowered(WAIT_STEP),
        }
    }
}

/// The box's line, said once, when it starts waiting for the builder's desktop and another run holds it.
#[must_use]
pub(crate) fn busy_wait_line() -> String {
    "gate: remote waiting for the builder's desktop: another run holds it".to_owned()
}

/// The box's line refusing a run that waited `wait` for the builder's desktop with another run still holding it: the
/// wait in whole seconds.
#[must_use]
pub(crate) fn busy_timeout_line(wait: Duration) -> String {
    format!("gate: not routed: the builder's desktop stayed busy for {} s", wait.as_secs())
}

/// The box's line, said once, when it takes the builder's desktop from a run that ended without releasing it.
#[must_use]
pub(crate) fn abandoned_line() -> String {
    "gate: remote took the builder's desktop from a run that ended without releasing it".to_owned()
}

/// The box's line for a command it took and then could not end: the pid it could not end and the system's error, the
/// error escaped into the line.
#[must_use]
pub(crate) fn could_not_end_line(pid: u32, error: &io::Error) -> String {
    format!(
        "gate: remote could not end the desktop command (process {pid}): {}",
        escaped(&error.to_string())
    )
}

/// A reporter for a desktop command taken by [`gate_win32::Suspended::adopt`]: it writes [`could_not_end_line`] and its
/// newline to the box's stderr in one write, then flushes.
///
/// A command dropped without being ended has no caller left to return the failure to, so a write that fails here is
/// ignored, nothing panics, and the run's own output lock is not taken: a drop cannot wait on the run it belongs to.
pub(crate) fn end_report() -> EndReport {
    Box::new(|pid, error| {
        let mut text = could_not_end_line(pid, &error);
        text.push('\n');
        let mut stderr = io::stderr().lock();
        let _ = stderr.write_all(text.as_bytes());
        let _ = stderr.flush();
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a case whose ids cannot be built must fail loudly")]
mod tests {
    use std::io;
    use std::num::NonZeroU32;
    use std::time::Duration;

    use gate_desktop::HelperExit;
    use gate_win32::{ConnectState, EndReport, LockState, SessionState, StartedTask, TaskError};

    use super::{
        CONNECT_LIMIT, DesktopRefusal, END_LIMIT, Limits, MESSAGE_LIMIT, TaskVerdict, WAIT_STEP, abandoned_line, busy_timeout_line, busy_wait_line,
        capped_wait, could_not_end_line, end_report, session_verdict, task_verdict,
    };
    use gate_router::escaped;

    /// The box's own user in every session case.
    const BOX_USER: &str = "lefto";
    /// The box's own domain in every session case.
    const BOX_DOMAIN: &str = "MACHINE";
    /// The pid a task start gives the run in every task case.
    const REPORTED_PID: u32 = 4242;

    /// The box's line refusing a console session, for `why`.
    fn refused(why: &str) -> String {
        format!("gate: not routed: the builder has no desktop session: {why}")
    }

    /// The console session of `connect` and `lock`, with `user` and `domain` logged on to it.
    fn console(connect: ConnectState, lock: LockState, user: &str, domain: &str) -> SessionState {
        SessionState {
            id: 3,
            connect,
            user: user.to_owned(),
            domain: domain.to_owned(),
            lock,
        }
    }

    /// The line the box refuses the console session `session` with, or `None` when it can take the command.
    fn verdict(session: Option<&SessionState>) -> Option<String> {
        session_verdict(session, BOX_USER, BOX_DOMAIN).err().map(|refusal| refusal.line())
    }

    /// What a verdict refuses the run with, and whether it says a helper of this run may be running; `None` when the
    /// verdict refuses nothing.
    fn refusal_of(verdict: TaskVerdict) -> (Option<String>, bool) {
        match verdict {
            TaskVerdict::Started(_) | TaskVerdict::Stopped => (None, false),
            TaskVerdict::Refused(refusal) => (Some(refusal.line()), false),
            TaskVerdict::MaybeStarted(refusal) => (Some(refusal.line()), true),
        }
    }

    #[test]
    fn each_session_state_has_its_line() {
        let nobody = refused("nobody is logged on at the console");
        assert_eq!(verdict(None), Some(nobody.clone()), "no session is attached to the console");
        assert_eq!(
            verdict(Some(&console(ConnectState::Active, LockState::Unlocked, "", BOX_DOMAIN))),
            Some(nobody),
            "the console session has no user logged on to it"
        );

        assert_eq!(
            verdict(Some(&console(ConnectState::Disconnected, LockState::Locked, "someone", BOX_DOMAIN))),
            Some(refused("another user is logged on at the console")),
            "another user's locked and disconnected session: the account is decided first"
        );
        assert_eq!(
            verdict(Some(&console(ConnectState::Disconnected, LockState::Unlocked, BOX_USER, BOX_DOMAIN))),
            Some(refused("the console session is disconnected")),
            "the box's own session with no client or display connected"
        );
        assert_eq!(
            verdict(Some(&console(ConnectState::Disconnected, LockState::Locked, BOX_USER, BOX_DOMAIN))),
            Some(refused("the console session is disconnected")),
            "the box's own session, disconnected and locked at once: the connection is decided first"
        );
        assert_eq!(
            verdict(Some(&console(ConnectState::Active, LockState::Locked, BOX_USER, BOX_DOMAIN))),
            Some(refused("the console session is locked")),
            "the box's own locked session"
        );
        assert_eq!(
            verdict(Some(&console(ConnectState::Active, LockState::Unknown, BOX_USER, BOX_DOMAIN))),
            Some(refused("the console session's lock state cannot be read")),
            "a lock state the system does not say"
        );

        assert_eq!(
            verdict(Some(&console(ConnectState::Active, LockState::Unlocked, "LEFTO", "machine"))),
            None,
            "the account is compared without case"
        );
        assert_eq!(
            verdict(Some(&console(ConnectState::Active, LockState::Unlocked, BOX_USER, "OTHER"))),
            Some(refused("another user is logged on at the console")),
            "a session of the box's user in another domain is another user"
        );
    }

    #[test]
    fn a_console_session_that_cannot_be_read_has_its_line() {
        assert_eq!(
            DesktopRefusal::SessionUnreadable.line(),
            "gate: not routed: the builder has no desktop session: the console session cannot be read"
        );
        assert_eq!(
            DesktopRefusal::SessionMoved.line(),
            "gate: not routed: the builder has no desktop session: the console session changed while the box read it"
        );
    }

    #[test]
    fn a_failure_of_the_desktop_machinery_is_its_own_words_escaped() {
        assert_eq!(
            DesktopRefusal::Machinery("the box's own account cannot be read: access\ndenied".to_owned()).line(),
            refused(&format!("the box's own account cannot be read: {}", escaped("access\ndenied")))
        );
        assert_eq!(
            DesktopRefusal::Machinery("the builder's desktop lock cannot be waited for: the handle is invalid".to_owned()).line(),
            "gate: not routed: the builder has no desktop session: the builder's desktop lock cannot be waited for: the handle is invalid"
        );
    }

    #[test]
    fn a_task_error_maps_to_its_why_and_says_when_it_may_have_started() {
        let pid = NonZeroU32::new(REPORTED_PID).expect("the reported pid is not zero");
        let started = task_verdict(Ok(StartedTask::Process(pid)));
        assert!(
            matches!(started, TaskVerdict::Started(given) if given == pid),
            "the Task Scheduler's process is what the run gets"
        );

        let started_unknown = "the query failed after the start";
        let cases = [
            (
                TaskError::NotRegistered,
                (Some(refused("the desktop helper task is not registered (gate push registers it)")), false),
                "no task of the path is registered",
            ),
            (
                TaskError::SetUpWrong("the action runs another program".to_owned()),
                (
                    Some(refused("the desktop helper task is set up wrong: the action runs another program")),
                    false,
                ),
                "the definition differs",
            ),
            (
                TaskError::Failed("the Task Scheduler could not be reached".to_owned()),
                (
                    Some(refused("the desktop helper did not start: the Task Scheduler could not be reached")),
                    false,
                ),
                "nothing was started before the call failed",
            ),
            (
                TaskError::StartedUnknown(started_unknown.to_owned()),
                (Some(refused(&format!("the desktop helper did not start: {started_unknown}"))), true),
                "the start was requested, then a call failed",
            ),
            (
                TaskError::TimedOut,
                (
                    Some(refused("the desktop helper did not start: the Task Scheduler did not answer in time")),
                    true,
                ),
                "the Scheduler never answered",
            ),
            (
                TaskError::Unanswered("the wait failed".to_owned()),
                (Some(refused("the desktop helper did not start: the wait failed")), true),
                "the wait for the answer failed",
            ),
            (TaskError::Woken(1), (None, false), "the run's own stop woke the wait"),
        ];
        for (error, expected, why) in cases {
            assert_eq!(refusal_of(task_verdict(Err(error))), expected, "{why}");
        }

        assert_eq!(
            refusal_of(task_verdict(Ok(StartedTask::NoPid))),
            (
                Some(refused("the desktop helper did not start: the Task Scheduler gave no process id")),
                true
            ),
            "a task the Scheduler started but gave no process for may be running"
        );
    }

    #[test]
    fn the_three_lines_of_the_desktop_mutex_are_these() {
        assert_eq!(busy_wait_line(), "gate: remote waiting for the builder's desktop: another run holds it");
        assert_eq!(
            busy_timeout_line(Duration::from_mins(2)),
            "gate: not routed: the builder's desktop stayed busy for 120 s",
            "the wait is said in whole seconds"
        );
        assert_eq!(
            abandoned_line(),
            "gate: remote took the builder's desktop from a run that ended without releasing it"
        );
    }

    #[test]
    fn every_helper_exit_has_a_failed_line() {
        for code in [0, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22] {
            let exit = HelperExit::from_code(code).expect("every code of an exit of this build is one");
            assert_eq!(
                DesktopRefusal::HelperFailed(code).line(),
                refused(&format!("the desktop helper failed: {}", exit.why())),
                "exit code {code}"
            );
        }

        assert_eq!(HelperExit::from_code(1), None, "a code between the exits of this build is no exit");
        assert_eq!(HelperExit::from_code(23), None, "23 is no exit of this build");
        assert_eq!(
            DesktopRefusal::HelperFailed(23).line(),
            refused("the desktop helper failed: it exited with code 23"),
            "an exit this build does not know is its own code"
        );
    }

    #[test]
    fn the_wait_is_capped_at_the_timeout() {
        let wait = Duration::from_mins(10);
        assert_eq!(
            capped_wait(wait, Some(Duration::from_mins(15))),
            wait,
            "a timeout above the wait leaves the wait it asked for"
        );
        assert_eq!(
            capped_wait(wait, Some(Duration::from_mins(5))),
            Duration::from_mins(5),
            "a timeout below the wait caps it"
        );
        assert_eq!(capped_wait(wait, None), wait, "a run with no timeout waits what it asked for");
    }

    #[test]
    fn the_limits_are_the_built_ones_and_one_cap_lowers_them_all() {
        let built = Limits::capped(None);
        assert_eq!(
            (built.connect, built.message, built.end, built.wait_step),
            (CONNECT_LIMIT, MESSAGE_LIMIT, END_LIMIT, WAIT_STEP),
            "a run with no cap uses the built limits"
        );
        assert_eq!(
            (CONNECT_LIMIT, MESSAGE_LIMIT, END_LIMIT, WAIT_STEP),
            (
                Duration::from_secs(10),
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_millis(100)
            ),
            "the built table"
        );

        let cap = Duration::from_millis(50);
        let capped = Limits::capped(Some(cap));
        assert_eq!(
            (capped.connect, capped.message, capped.end, capped.wait_step),
            (cap, cap, cap, cap),
            "a cap below every limit lowers every one"
        );

        let between = Duration::from_secs(2);
        let partly = Limits::capped(Some(between));
        assert_eq!(
            (partly.connect, partly.message, partly.end, partly.wait_step),
            (between, between, between, WAIT_STEP),
            "a cap between the limits lowers those above it and leaves the step"
        );
    }

    #[test]
    fn every_line_escapes_what_comes_from_outside() {
        let bad = "a\nb\u{1b}c\u{202e}d";
        let safe = escaped(bad);
        assert_ne!(safe, bad, "the junk text is escaped by the lines, not left as it came");

        assert_eq!(
            DesktopRefusal::TaskSetUpWrong(bad.to_owned()).line(),
            refused(&format!("the desktop helper task is set up wrong: {safe}"))
        );
        assert_eq!(
            DesktopRefusal::HelperDidNotStart(bad.to_owned()).line(),
            refused(&format!("the desktop helper did not start: {safe}"))
        );
        assert_eq!(
            DesktopRefusal::HelperOtherVersion {
                helper: bad.to_owned(),
                box_version: bad.to_owned()
            }
            .line(),
            refused(&format!("the desktop helper runs gate {safe}, the box {safe}"))
        );
        assert_eq!(
            DesktopRefusal::DesktopLock(bad.to_owned()).line(),
            refused(&format!("the builder's desktop lock cannot be made: {safe}"))
        );
        assert_eq!(
            DesktopRefusal::HelperNotOnDesktop.line(),
            refused("the desktop helper is not on the console's Default desktop"),
            "the line that carries no value from outside is the same line"
        );
        assert_eq!(
            could_not_end_line(7, &io::Error::other(bad)),
            format!("gate: remote could not end the desktop command (process 7): {safe}")
        );
    }

    #[test]
    fn the_end_report_writes_one_escaped_line() {
        let sink: EndReport = end_report();
        drop(sink);

        assert_eq!(
            could_not_end_line(4321, &io::Error::other("access denied")),
            "gate: remote could not end the desktop command (process 4321): access denied"
        );
    }

    #[test]
    fn a_command_the_box_cannot_put_in_its_job_is_said_as_the_helpers_failure() {
        assert_eq!(
            DesktopRefusal::NotPutInTheJob("Access is denied.".to_owned()).line(),
            refused("the desktop helper failed: the command it started cannot be put in the box's job: Access is denied.")
        );
    }

    #[test]
    fn a_command_the_box_cannot_take_carries_the_systems_error() {
        assert_eq!(
            DesktopRefusal::WrongCommandFor(super::WrongFact::NotTheProcess, "The handle is invalid.".to_owned()).line(),
            refused("the desktop helper failed: the command it started is not the process it reported: The handle is invalid.")
        );
    }
}
