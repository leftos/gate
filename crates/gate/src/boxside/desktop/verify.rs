//! What the box checks of the helper process the Task Scheduler gave before it trusts it, of the command the helper
//! reported before it takes it, and which processes it ends after a hand-off that failed. Every fact is read through
//! [`Facts`] or [`HelperFacts`]: a fact that cannot be read refuses the hand-off as unread, a fact that does not hold
//! refuses it as wrong, and nothing the box cannot prove the helper's is ever ended.

use std::io;

use jiff::Timestamp;
use proc_table::ProcessInfo;

/// A file's identity: its volume's serial number and its 128-bit file id, the same for every path that reaches the
/// file, through a junction, a link or a short name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    /// The serial number of the volume the file is on.
    pub(crate) volume: u64,
    /// The file's id on that volume.
    pub(crate) file: [u8; 16],
}

/// What the box reads of the command the helper reported, one method per fact of it, each read from the box's own
/// handles: the implementation holds the process and the thread handle it pulled out of the helper, which are then the
/// handles [`gate_win32::Suspended::adopt`] takes.
///
/// Every method is a read of the machine and fails when the fact cannot be read at all, or is not in the form the fact
/// needs.
///
/// What the checks need beside these reads — the helper's pid and creation time, the builder's desktop session and
/// the program's identity — comes from [`Reported`], which the box reads for itself, never from a helper message.
pub(crate) trait Facts {
    /// The id of the process the box's own process handle stands for.
    fn process_pid(&self) -> io::Result<u32>;

    /// The id of the process the thread behind the box's own thread handle belongs to (`GetProcessIdOfThread`), which
    /// is the pid the helper reported, not the thread's own id.
    fn thread_pid(&self) -> io::Result<u32>;

    /// The id of the process that started the command.
    fn parent_pid(&self) -> io::Result<u32>;

    /// When the command started.
    fn created(&self) -> io::Result<Timestamp>;

    /// The id of the session the command runs in.
    fn session(&self) -> io::Result<u32>;

    /// The identity of the image file the command runs.
    fn image(&self) -> io::Result<FileIdentity>;

    /// Whether the command's first thread is suspended exactly once.
    fn suspended_once(&self) -> io::Result<bool>;

    /// Whether the box's own job holds the command.
    fn in_job(&self) -> io::Result<bool>;
}

/// What the box verified the command against, as the box itself read it rather than as a helper message said it.
///
/// `pid` is the pid the helper reported for its command, which the box's own reads are checked against. `helper`,
/// `helper_created` and `session` are the box's own: the pid and the creation time of the helper process the box
/// started and pinned a handle of, and the session the box read for the builder's desktop. `program` is the identity of
/// the program file the box sent the helper in the first place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reported {
    /// The pid the helper reported for its command.
    pub(crate) pid: u32,
    /// The helper's own pid, whose handle the box holds from the task start on.
    pub(crate) helper: u32,
    /// When the box's helper process started: the command cannot be earlier.
    pub(crate) helper_created: Timestamp,
    /// The session the box read for the builder's desktop, never one a helper message named: the command has to be in
    /// it.
    pub(crate) session: u32,
    /// The identity of the program file the box asked the helper to run.
    pub(crate) program: FileIdentity,
}

/// The one fact of a reported command that does not hold, which refuses the command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WrongFact {
    /// The process's id or its thread's is not the pid the helper reported.
    NotTheProcess,
    /// Its parent is not the helper.
    NotTheHelpersChild,
    /// It was created before the helper.
    CreatedBefore,
    /// It runs in another session than the builder's desktop.
    AnotherSession,
    /// Its image is not the program the box sent.
    NotTheProgram,
    /// It is not suspended exactly once.
    NotSuspended,
    /// The box's own job holds it already.
    InTheJob,
}

impl WrongFact {
    /// The fact in the box's own words, after `the desktop helper failed: the command it started `.
    pub(crate) fn why(self) -> &'static str {
        match self {
            WrongFact::NotTheProcess => "is not the process it reported",
            WrongFact::NotTheHelpersChild => "was not started by the helper",
            WrongFact::CreatedBefore => "was started before the helper",
            WrongFact::AnotherSession => "is in another session",
            WrongFact::NotTheProgram => "is not the program the box sent",
            WrongFact::NotSuspended => "is not suspended",
            WrongFact::InTheJob => "is in the box's job already",
        }
    }
}

/// Why the box refuses a process, the command or the helper: a fact of it that does not hold, or one that cannot be
/// read, named as the box's line names it, with the system's error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unproved<W> {
    /// The fact does not hold.
    Wrong(W),
    /// The fact, so named, cannot be read, for this error.
    Unread {
        /// The fact's name in the box's line, such as `session`.
        fact: &'static str,
        /// The system's error.
        error: String,
    },
}

impl<W> Unproved<W> {
    /// A fact named `fact` that cannot be read, for `error`.
    fn unread(fact: &'static str, error: &io::Error) -> Unproved<W> {
        Unproved::Unread {
            fact,
            error: error.to_string(),
        }
    }
}

/// One check of [`verify`]: a read, and whether its answer holds.
type Check<'c> = &'c dyn Fn() -> io::Result<bool>;

/// Verifies the command the helper reported, through `facts`: `Ok(())` when every fact of it holds, else the first
/// fact that does not hold or cannot be read, in the order the box checks them.
///
/// Each check reads its one fact as it is reached, so no read runs after the first fact that fails:
/// [`Facts::suspended_once`] suspends and resumes a thread, and it never touches the thread of a command that has
/// already failed a check. Nothing the box could not read is ever taken. The command may have been created at the
/// helper's own instant: creation stamps may be coarse, and the pid and the parent checks already tie the command to
/// the helper.
pub(crate) fn verify(facts: &impl Facts, reported: &Reported) -> Result<(), Unproved<WrongFact>> {
    let pid = reported.pid;
    let helper = reported.helper;
    let created = reported.helper_created;
    let session = reported.session;
    let program = reported.program;
    let checks: [(Check<'_>, WrongFact, &'static str); 8] = [
        (&|| facts.process_pid().map(|id| id == pid), WrongFact::NotTheProcess, "process id"),
        (
            &|| facts.thread_pid().map(|id| id == pid),
            WrongFact::NotTheProcess,
            "first thread's process id",
        ),
        (
            &|| facts.parent_pid().map(|parent| parent == helper),
            WrongFact::NotTheHelpersChild,
            "parent",
        ),
        (&|| facts.created().map(|at| at >= created), WrongFact::CreatedBefore, "creation time"),
        (&|| facts.session().map(|id| id == session), WrongFact::AnotherSession, "session"),
        (&|| facts.image().map(|image| image == program), WrongFact::NotTheProgram, "image"),
        (&|| facts.suspended_once(), WrongFact::NotSuspended, "suspend count"),
        (&|| facts.in_job().map(|holds| !holds), WrongFact::InTheJob, "job"),
    ];
    for (check, wrong, fact) in checks {
        match check() {
            Ok(true) => {}
            Ok(false) => return Err(Unproved::Wrong(wrong)),
            Err(error) => return Err(Unproved::unread(fact, &error)),
        }
    }
    Ok(())
}

/// What the box reads of the process the Task Scheduler gave for the helper, one method per fact, each read through
/// the handle the box opened of that pid at once and keeps for the whole hand-off. Every method fails when the fact
/// cannot be read.
pub(crate) trait HelperFacts {
    /// When the process was created.
    fn created(&self) -> io::Result<Timestamp>;

    /// The id of the session the process runs in.
    fn session(&self) -> io::Result<u32>;

    /// The identity of the image file the process runs.
    fn image(&self) -> io::Result<FileIdentity>;

    /// The process's user, as its token names it: the SID's string form.
    fn user(&self) -> io::Result<String>;
}

/// What the helper the box started has to be, as the box itself knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExpectedHelper<'a> {
    /// The instant the box read just before it asked for the task, on the coarse system clock, which is never ahead of
    /// the creation stamp of a process created after the read: the helper cannot have been created before it.
    pub(crate) before: Timestamp,
    /// The session the box read for the builder's desktop.
    pub(crate) session: u32,
    /// The identity of the helper's image: the gate's desktop helper beside the box's own program.
    pub(crate) image: FileIdentity,
    /// The box's own user, as a SID's string form.
    pub(crate) user: &'a str,
}

/// Why the process that answered for the helper is not the helper the box started, which refuses the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WrongHelper {
    /// It was created before the box asked for the task.
    CreatedBefore,
    /// It runs in another session than the builder's desktop.
    AnotherSession,
    /// Its image is not the gate's desktop helper.
    NotTheHelper,
    /// It runs as another user than the box.
    AnotherUser,
    /// Only processes other than the helper connected to the control pipe.
    NotTheClient,
}

impl WrongHelper {
    /// The reason in the box's own words, after `the desktop helper did not start: `.
    pub(crate) fn why(self) -> &'static str {
        match self {
            WrongHelper::CreatedBefore => "the process that answered was created before the task ran",
            WrongHelper::AnotherSession => "the process that answered is in another session",
            WrongHelper::NotTheHelper => "the process that answered is not the gate's desktop helper",
            WrongHelper::AnotherUser => "the process that answered runs as another user",
            WrongHelper::NotTheClient => "the pipe's client is not the helper the task started",
        }
    }
}

impl Unproved<WrongHelper> {
    /// The reason in the box's own words, after `the desktop helper did not start: `: the wrong fact's, or that the
    /// helper's fact cannot be read, with the system's error.
    pub(crate) fn why(&self) -> String {
        match self {
            Unproved::Wrong(wrong) => wrong.why().to_owned(),
            Unproved::Unread { fact, error } => format!("its {fact} cannot be read: {error}"),
        }
    }
}

/// Checks the process the Task Scheduler gave for the helper, through `facts`, against `expected`: its creation time,
/// for the box's own record of the helper, when every fact holds; else the first that does not hold or cannot be read,
/// in the order the box checks them. Its creation time may equal `expected.before`: the box reads `before` on the
/// coarse system clock, which is never ahead of a creation stamp, so only a clock stepped back can refuse the real
/// helper here.
pub(crate) fn verify_helper(facts: &impl HelperFacts, expected: &ExpectedHelper<'_>) -> Result<Timestamp, Unproved<WrongHelper>> {
    let created = facts.created().map_err(|error| Unproved::unread("creation time", &error))?;
    if created < expected.before {
        return Err(Unproved::Wrong(WrongHelper::CreatedBefore));
    }
    if facts.session().map_err(|error| Unproved::unread("session", &error))? != expected.session {
        return Err(Unproved::Wrong(WrongHelper::AnotherSession));
    }
    if facts.image().map_err(|error| Unproved::unread("image", &error))? != expected.image {
        return Err(Unproved::Wrong(WrongHelper::NotTheHelper));
    }
    let user = facts.user().map_err(|error| Unproved::unread("user", &error))?;
    if !user.eq_ignore_ascii_case(expected.user) {
        return Err(Unproved::Wrong(WrongHelper::AnotherUser));
    }
    Ok(created)
}

/// The processes a failed hand-off leaves to end, each with the creation time its snapshot holds: the pids of
/// `processes` whose parent is the helper, whose creation time is known and not earlier than the helper's, and
/// whose session `session_of` reads as the one the builder's desktop runs in.
///
/// A process whose creation time or session cannot be read is left alone: nothing the box cannot prove the helper's is
/// ever ended. A snapshot that holds a process at the helper's own pid whose creation time is known and is not the
/// helper's is a pid that has been reused: nothing in the snapshot can be proved the helper's, so nothing is selected.
///
/// The caller holds its own handle of the helper open from the task start until the last of these processes is ended,
/// and re-checks each pid by the handle it opens of it before it ends it: that the process's parent is still the
/// helper, and that its creation time is the one given here. Only through that same handle is the process ended, and a
/// process that fails either re-check is one that took the pid since the snapshot was taken, which is left alone.
#[must_use]
pub(crate) fn sweep_targets(
    processes: &[ProcessInfo],
    helper: u32,
    helper_created: Timestamp,
    session: u32,
    session_of: impl Fn(u32) -> Option<u32>,
) -> Vec<(u32, Timestamp)> {
    let reused = processes
        .iter()
        .any(|process| process.pid == helper && process.created.is_some_and(|created| created != helper_created));
    if reused {
        return Vec::new();
    }
    processes
        .iter()
        .filter(|process| process.parent_pid == helper)
        .filter(|process| process.created.is_some_and(|created| created >= helper_created))
        .filter(|process| session_of(process.pid) == Some(session))
        .filter_map(|process| process.created.map(|created| (process.pid, created)))
        .collect()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a case whose timestamps cannot be built must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::io;

    use jiff::Timestamp;
    use proc_table::ProcessInfo;

    use super::{ExpectedHelper, Facts, FileIdentity, HelperFacts, Reported, Unproved, WrongFact, WrongHelper, sweep_targets, verify, verify_helper};
    use crate::boxside::desktop::lines::DesktopRefusal;

    /// The pid the helper reports for the command of every verification case.
    const REPORTED_PID: u32 = 4242;
    /// The helper's pid in every verification case: the command's parent.
    const HELPER_PID: u32 = 4000;
    /// The identity of the program the box sent in every verification case.
    const PROGRAM: FileIdentity = FileIdentity { volume: 7, file: [1; 16] };
    /// The identity of a file that is not the program the box sent: another file on the same volume.
    const OTHER_FILE: FileIdentity = FileIdentity { volume: 7, file: [2; 16] };
    /// The error every unreadable fact of a case fails with.
    const UNREAD: &str = "the fact cannot be read";

    /// The box's line refusing a run, for `why`.
    fn refused(why: &str) -> String {
        format!("gate: not routed: the builder has no desktop session: {why}")
    }

    /// The timestamp of `second` after the epoch.
    fn time(second: i64) -> Timestamp {
        Timestamp::from_second(second).expect("a whole second is a timestamp")
    }

    /// When the helper of every verification case was created.
    fn helper_created() -> Timestamp {
        time(10)
    }

    /// When the command of every verification case was created: after the helper.
    fn command_created() -> Timestamp {
        time(11)
    }

    /// What the box asked for and what the helper reported in every verification case.
    fn reported() -> Reported {
        Reported {
            pid: REPORTED_PID,
            helper: HELPER_PID,
            helper_created: helper_created(),
            session: 3,
            program: PROGRAM,
        }
    }

    /// The one read of a [`Fake`] that is not the correct one.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Read {
        ProcessPid,
        ThreadPid,
        Parent,
        Created,
        Session,
        Image,
        Suspended,
        InJob,
    }

    /// A reader of the reported command, correct in every fact except the one `faulty` names.
    struct Fake {
        /// What the box asked for and what the helper reported.
        reported: Reported,
        /// When the command was created, as its correct read gives it.
        created: Timestamp,
        /// The one read that is not the correct one.
        faulty: Option<Read>,
        /// Whether the faulty read is one that cannot be read at all, rather than one answered wrongly.
        unreadable: bool,
        /// How many times `suspended_once` was read, which suspends and resumes a thread.
        suspensions: Cell<u32>,
    }

    impl Fake {
        /// A reader correct in every fact.
        fn right() -> Fake {
            Fake {
                reported: reported(),
                created: command_created(),
                faulty: None,
                unreadable: false,
                suspensions: Cell::new(0),
            }
        }

        /// The answer to one read: `right`, unless this is the faulty read, which is an error when `unreadable` holds
        /// and `wrong` otherwise.
        fn answer<T>(&self, read: Read, right: T, wrong: impl FnOnce() -> T) -> io::Result<T> {
            if self.faulty != Some(read) {
                return Ok(right);
            }
            if self.unreadable {
                return Err(io::Error::other(UNREAD));
            }
            Ok(wrong())
        }
    }

    impl Facts for Fake {
        fn process_pid(&self) -> io::Result<u32> {
            self.answer(Read::ProcessPid, self.reported.pid, || self.reported.pid + 1)
        }

        fn thread_pid(&self) -> io::Result<u32> {
            self.answer(Read::ThreadPid, self.reported.pid, || self.reported.pid + 1)
        }

        fn parent_pid(&self) -> io::Result<u32> {
            self.answer(Read::Parent, self.reported.helper, || self.reported.helper + 1)
        }

        fn created(&self) -> io::Result<Timestamp> {
            self.answer(Read::Created, self.created, || time(9))
        }

        fn session(&self) -> io::Result<u32> {
            self.answer(Read::Session, self.reported.session, || self.reported.session + 1)
        }

        fn image(&self) -> io::Result<FileIdentity> {
            self.answer(Read::Image, PROGRAM, || OTHER_FILE)
        }

        fn suspended_once(&self) -> io::Result<bool> {
            self.suspensions.set(self.suspensions.get() + 1);
            self.answer(Read::Suspended, true, || false)
        }

        fn in_job(&self) -> io::Result<bool> {
            self.answer(Read::InJob, false, || true)
        }
    }

    #[test]
    fn verification_refuses_each_wrong_fact_and_each_unread_one() {
        let right = Fake::right();
        assert_eq!(verify(&right, &right.reported), Ok(()), "a command correct in every fact is taken");
        assert_eq!(
            right.suspensions.get(),
            1,
            "the suspended-once read is reached once when every fact holds"
        );

        // No read runs after the first fact that fails: `suspended_once` suspends and resumes a thread, and nothing may
        // touch a thread of a command that is not the one the helper reported.
        let late = Fake {
            faulty: Some(Read::ProcessPid),
            ..Fake::right()
        };
        assert_eq!(verify(&late, &late.reported), Err(Unproved::Wrong(WrongFact::NotTheProcess)));
        assert_eq!(late.suspensions.get(), 0, "a read after the failing fact never happens");

        let facts = [
            (Read::ProcessPid, WrongFact::NotTheProcess, "process id"),
            (Read::ThreadPid, WrongFact::NotTheProcess, "first thread's process id"),
            (Read::Parent, WrongFact::NotTheHelpersChild, "parent"),
            (Read::Created, WrongFact::CreatedBefore, "creation time"),
            (Read::Session, WrongFact::AnotherSession, "session"),
            (Read::Image, WrongFact::NotTheProgram, "image"),
            (Read::Suspended, WrongFact::NotSuspended, "suspend count"),
            (Read::InJob, WrongFact::InTheJob, "job"),
        ];
        for (read, wrong, fact) in facts {
            let answered = Fake {
                faulty: Some(read),
                ..Fake::right()
            };
            assert_eq!(verify(&answered, &answered.reported), Err(Unproved::Wrong(wrong)), "{read:?}");
            let unreadable = Fake {
                faulty: Some(read),
                unreadable: true,
                ..Fake::right()
            };
            assert_eq!(
                verify(&unreadable, &unreadable.reported),
                Err(Unproved::Unread {
                    fact,
                    error: UNREAD.to_owned()
                }),
                "{read:?} that cannot be read is refused as unread, not as wrong"
            );
        }

        assert_eq!(
            DesktopRefusal::WrongCommand(WrongFact::NotTheProcess).line(),
            refused("the desktop helper failed: the command it started is not the process it reported"),
            "a command of the wrong process is said as the hand-off's own failure"
        );
    }

    #[test]
    fn a_command_created_at_the_helpers_own_instant_is_taken() {
        let tied = Fake {
            created: helper_created(),
            ..Fake::right()
        };

        assert_eq!(
            verify(&tied, &tied.reported),
            Ok(()),
            "a creation stamp equal to the helper's is no earlier"
        );
    }

    #[test]
    fn each_unread_fact_of_the_command_has_its_own_line() {
        let cases = [
            (
                "process id",
                "the desktop helper failed: the process id of the command it started cannot be read: denied",
            ),
            (
                "image",
                "the desktop helper failed: the image of the command it started cannot be read: denied",
            ),
        ];
        for (fact, line) in cases {
            let unread = Unproved::<WrongFact>::Unread {
                fact,
                error: "denied".to_owned(),
            };
            assert_eq!(DesktopRefusal::from(unread).line(), refused(line), "{fact}");
        }
        assert_eq!(
            DesktopRefusal::from(Unproved::Wrong(WrongFact::AnotherSession)).line(),
            refused("the desktop helper failed: the command it started is in another session"),
            "a wrong fact keeps its own line"
        );
    }

    /// A process of a sweep case: `pid` under `parent`, created at `second` after the epoch.
    fn process(pid: u32, parent: u32, second: i64) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: parent,
            name: "cmd.exe".to_owned(),
            command_line: None,
            created: Some(time(second)),
        }
    }

    /// A process of a sweep case whose creation time cannot be read.
    fn unknown(pid: u32) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: HELPER_PID,
            name: "cmd.exe".to_owned(),
            command_line: None,
            created: None,
        }
    }

    /// The session of a sweep case's process `pid`, as the box reads it.
    fn session_of(pid: u32) -> Option<u32> {
        match pid {
            6 => Some(9),
            7 => None,
            _ => Some(3),
        }
    }

    #[test]
    fn the_sweep_takes_only_what_is_provably_the_helpers() {
        let created = time(100);
        let processes = [
            process(1, 0, 200),
            process(2, HELPER_PID, 99),
            process(3, HELPER_PID, 100),
            unknown(4),
            process(5, HELPER_PID, 101),
            process(6, HELPER_PID, 101),
            process(7, HELPER_PID, 101),
        ];

        assert_eq!(
            sweep_targets(&processes, HELPER_PID, created, 3, session_of),
            vec![(3, time(100)), (5, time(101))],
            "3, created at the helper's own instant, and 5 are taken; 1's child of another process, 2's creation before \
             the helper, 4's unread creation, 6's other session and 7's unread session are left alone"
        );
        assert_eq!(
            sweep_targets(&[], HELPER_PID, created, 3, session_of),
            Vec::<(u32, Timestamp)>::new(),
            "a helper with no child leaves nothing to end"
        );

        let with_helper = [process(HELPER_PID, 0, 100), process(5, HELPER_PID, 101)];
        assert_eq!(
            sweep_targets(&with_helper, HELPER_PID, created, 3, session_of),
            vec![(5, time(101))],
            "the helper's own entry, created when the box read it started, is no reuse"
        );

        let reused = [process(HELPER_PID, 0, 90), process(5, HELPER_PID, 101)];
        assert_eq!(
            sweep_targets(&reused, HELPER_PID, created, 3, session_of),
            Vec::<(u32, Timestamp)>::new(),
            "a process at the helper's pid created at another time took the pid: nothing is proved the helper's"
        );

        let unread = [unknown(HELPER_PID), process(5, HELPER_PID, 101)];
        assert_eq!(
            sweep_targets(&unread, HELPER_PID, created, 3, session_of),
            vec![(5, time(101))],
            "an unread creation time at the helper's pid says nothing either way"
        );
    }

    #[test]
    fn the_sweep_takes_a_child_created_at_the_helpers_own_instant() {
        let tied = [process(5, HELPER_PID, 100)];

        assert_eq!(
            sweep_targets(&tied, HELPER_PID, time(100), 3, session_of),
            vec![(5, time(100))],
            "a creation stamp equal to the helper's is no earlier"
        );
    }

    /// The box's own user in every helper case.
    const BOX_SID: &str = "S-1-5-21-1-2-3-1001";

    /// The identity of the helper's image in every helper case, as the box expects it.
    const HELPER_IMAGE: FileIdentity = FileIdentity { volume: 7, file: [3; 16] };

    /// What the helper of every helper case has to be: created at or after second 10, in session 3.
    fn expected() -> ExpectedHelper<'static> {
        ExpectedHelper {
            before: time(10),
            session: 3,
            image: HELPER_IMAGE,
            user: BOX_SID,
        }
    }

    /// The facts of a helper case's process, each read as the case gives it.
    struct Helper {
        created: io::Result<Timestamp>,
        session: io::Result<u32>,
        image: io::Result<FileIdentity>,
        user: io::Result<String>,
    }

    impl Helper {
        /// The helper the box started: created at second 11, in session 3, the expected image, and the box's own user
        /// in other case.
        fn right() -> Helper {
            Helper {
                created: Ok(time(11)),
                session: Ok(3),
                image: Ok(HELPER_IMAGE),
                user: Ok(BOX_SID.to_lowercase()),
            }
        }
    }

    impl HelperFacts for Helper {
        fn created(&self) -> io::Result<Timestamp> {
            copied(&self.created)
        }

        fn session(&self) -> io::Result<u32> {
            copied(&self.session)
        }

        fn image(&self) -> io::Result<FileIdentity> {
            copied(&self.image)
        }

        fn user(&self) -> io::Result<String> {
            copied(&self.user)
        }
    }

    /// `read` again: its value, or an error of the same kind.
    fn copied<T: Clone>(read: &io::Result<T>) -> io::Result<T> {
        match read {
            Ok(value) => Ok(value.clone()),
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
        }
    }

    /// A read that fails.
    fn unread<T>() -> io::Result<T> {
        Err(io::Error::other(UNREAD))
    }

    #[test]
    fn the_helper_the_box_started_passes_and_gives_its_creation_time() {
        assert_eq!(verify_helper(&Helper::right(), &expected()), Ok(time(11)));
        let same_tick = Helper {
            created: Ok(time(10)),
            ..Helper::right()
        };
        assert_eq!(
            verify_helper(&same_tick, &expected()),
            Ok(time(10)),
            "a helper created at the instant the box read is no older process"
        );
    }

    #[test]
    fn each_fact_of_the_helper_that_does_not_hold_refuses_it() {
        let cases = [
            (
                Helper {
                    created: Ok(time(9)),
                    ..Helper::right()
                },
                WrongHelper::CreatedBefore,
            ),
            (
                Helper {
                    session: Ok(4),
                    ..Helper::right()
                },
                WrongHelper::AnotherSession,
            ),
            (
                Helper {
                    image: Ok(OTHER_FILE),
                    ..Helper::right()
                },
                WrongHelper::NotTheHelper,
            ),
            (
                Helper {
                    user: Ok("S-1-5-21-1-2-3-1002".to_owned()),
                    ..Helper::right()
                },
                WrongHelper::AnotherUser,
            ),
        ];
        for (helper, wrong) in cases {
            assert_eq!(verify_helper(&helper, &expected()), Err(Unproved::Wrong(wrong)), "{wrong:?}");
        }
    }

    #[test]
    fn a_fact_of_the_helper_that_cannot_be_read_refuses_it_as_unread() {
        let cases = [
            (
                Helper {
                    created: unread(),
                    ..Helper::right()
                },
                "creation time",
            ),
            (
                Helper {
                    session: unread(),
                    ..Helper::right()
                },
                "session",
            ),
            (
                Helper {
                    image: unread(),
                    ..Helper::right()
                },
                "image",
            ),
            (
                Helper {
                    user: unread(),
                    ..Helper::right()
                },
                "user",
            ),
        ];
        for (helper, fact) in cases {
            assert_eq!(
                verify_helper(&helper, &expected()),
                Err(Unproved::Unread {
                    fact,
                    error: UNREAD.to_owned()
                }),
                "{fact}"
            );
        }
    }

    #[test]
    fn each_refused_helper_is_said_as_the_helper_that_did_not_start() {
        let said = |refused: Unproved<WrongHelper>| DesktopRefusal::HelperDidNotStart(refused.why()).line();
        let start = "the desktop helper did not start:";
        let wrongs = [
            (WrongHelper::CreatedBefore, "the process that answered was created before the task ran"),
            (WrongHelper::AnotherSession, "the process that answered is in another session"),
            (WrongHelper::NotTheHelper, "the process that answered is not the gate's desktop helper"),
            (WrongHelper::AnotherUser, "the process that answered runs as another user"),
            (WrongHelper::NotTheClient, "the pipe's client is not the helper the task started"),
        ];
        for (wrong, why) in wrongs {
            assert_eq!(said(Unproved::Wrong(wrong)), refused(&format!("{start} {why}")), "{wrong:?}");
        }
        for fact in ["creation time", "session", "image", "user"] {
            let unread = Unproved::Unread {
                fact,
                error: "denied".to_owned(),
            };
            assert_eq!(said(unread), refused(&format!("{start} its {fact} cannot be read: denied")), "{fact}");
        }
    }
}
