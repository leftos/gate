//! What the box checks of the command the helper reported before it takes it, and which processes it ends after a
//! hand-off that failed. Every fact is read through [`Facts`], so a fact that cannot be read is a fact that does not
//! hold, and nothing the box cannot prove the helper's is ever ended.

use std::io;
use std::path::PathBuf;

use jiff::Timestamp;
use proc_table::ProcessInfo;

/// The prefix that gives a Windows path its extended form, which names the same file as the path without it.
const EXTENDED: &str = r"\\?\";

/// What the box reads of the command the helper reported, one method per fact of it, each read from the box's own
/// handles: the implementation holds the process and the thread handle it pulled out of the helper, which are then the
/// handles [`gate_win32::Suspended::adopt`] takes.
///
/// Every method is a read of the machine and fails when the fact cannot be read at all, or is not in the form the fact
/// needs; a caller of [`verify`] answers each fact through this, so a fact that cannot be read is a fact that does not
/// hold.
///
/// What the checks need beside these reads — the helper's pid and creation time, and the builder's desktop session —
/// comes from [`Reported`], which the box reads for itself, never from a helper message.
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

    /// The path of the image the command runs.
    fn image_path(&self) -> io::Result<PathBuf>;

    /// Whether the command's first thread is suspended exactly once.
    fn suspended_once(&self) -> io::Result<bool>;

    /// Whether the box's own job holds the command.
    fn in_job(&self) -> io::Result<bool>;
}

/// What the box verified the command against, as the box itself read it rather than as a helper message said it.
///
/// `pid` is the pid the helper reported for its command, which the box's own reads are checked against. `helper`,
/// `helper_created` and `session` are the box's own: the pid and the creation time of the helper process the box
/// started and pinned a handle of, and the session the box read for the builder's desktop. `program` is the program
/// the box sent the helper in the first place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reported<'a> {
    /// The pid the helper reported for its command.
    pub(crate) pid: u32,
    /// The helper's own pid, whose handle the box holds from the task start on.
    pub(crate) helper: u32,
    /// When the box's helper process started: the command has to be later.
    pub(crate) helper_created: Timestamp,
    /// The session the box read for the builder's desktop, never one a helper message named: the command has to be in
    /// it.
    pub(crate) session: u32,
    /// The program the box asked the helper to run.
    pub(crate) program: &'a str,
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

/// Verifies the command the helper reported, through `facts`: `Ok(())` when every fact of it holds, else the first
/// fact that does not, in the order the box checks them.
///
/// Each check reads its one fact as it is reached, so no read runs after the first fact that fails:
/// [`Facts::suspended_once`] suspends and resumes a thread, and it never touches the thread of a command that has
/// already failed a check. A read that fails answers its question with `false`: nothing the box could not read is ever
/// taken.
pub(crate) fn verify(facts: &impl Facts, reported: &Reported<'_>) -> Result<(), WrongFact> {
    let pid = reported.pid;
    let helper = reported.helper;
    let created = reported.helper_created;
    let session = reported.session;
    let program = reported.program;
    let checks: [(&dyn Fn() -> io::Result<bool>, WrongFact); 8] = [
        (&|| facts.process_pid().map(|id| id == pid), WrongFact::NotTheProcess),
        (&|| facts.thread_pid().map(|id| id == pid), WrongFact::NotTheProcess),
        (&|| facts.parent_pid().map(|parent| parent == helper), WrongFact::NotTheHelpersChild),
        (&|| facts.created().map(|at| at > created), WrongFact::CreatedBefore),
        (&|| facts.session().map(|id| id == session), WrongFact::AnotherSession),
        (
            &|| {
                facts
                    .image_path()
                    .map(|path| path.to_str().is_some_and(|image| same_image(image, program)))
            },
            WrongFact::NotTheProgram,
        ),
        (&|| facts.suspended_once(), WrongFact::NotSuspended),
        (&|| facts.in_job().map(|holds| !holds), WrongFact::InTheJob),
    ];
    for (check, wrong) in checks {
        if !check().unwrap_or(false) {
            return Err(wrong);
        }
    }
    Ok(())
}

/// Whether the image path `read` is the program `sent`: each with its extended prefix stripped, compared without case,
/// since Windows names the same file either way.
fn same_image(read: &str, sent: &str) -> bool {
    read.strip_prefix(EXTENDED).unwrap_or(read).to_lowercase() == sent.strip_prefix(EXTENDED).unwrap_or(sent).to_lowercase()
}

/// The processes a failed hand-off leaves to end, each with the creation time its snapshot holds: the pids of
/// `processes` whose parent is the helper, whose creation time is known and later than the helper's, and whose session
/// `session_of` reads as the one the builder's desktop runs in.
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
        .filter(|process| process.created.is_some_and(|created| created > helper_created))
        .filter(|process| session_of(process.pid) == Some(session))
        .filter_map(|process| process.created.map(|created| (process.pid, created)))
        .collect()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a case whose timestamps cannot be built must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::path::PathBuf;

    use jiff::Timestamp;
    use proc_table::ProcessInfo;

    use super::{EXTENDED, Facts, Reported, WrongFact, same_image, sweep_targets, verify};
    use crate::boxside::desktop::lines::DesktopRefusal;

    /// The pid the helper reports for the command of every verification case.
    const REPORTED_PID: u32 = 4242;
    /// The helper's pid in every verification case: the command's parent.
    const HELPER_PID: u32 = 4000;
    /// The image the process of every verification case runs: the program the box sent, in its extended form and in
    /// other case.
    const IMAGE: &str = r"\\?\C:\Gate\tool.exe";
    /// The program the box sent in every verification case.
    const PROGRAM: &str = r"C:\gate\tool.exe";

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
    fn reported() -> Reported<'static> {
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
        reported: Reported<'static>,
        /// The image path the command runs, which a case may set to one no `str` holds.
        image: PathBuf,
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
                image: PathBuf::from(IMAGE),
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
                return Err(io::Error::other("the fact cannot be read"));
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
            self.answer(Read::Created, command_created(), helper_created)
        }

        fn session(&self) -> io::Result<u32> {
            self.answer(Read::Session, self.reported.session, || self.reported.session + 1)
        }

        fn image_path(&self) -> io::Result<PathBuf> {
            self.answer(Read::Image, self.image.clone(), || PathBuf::from(r"C:\other.exe"))
        }

        fn suspended_once(&self) -> io::Result<bool> {
            self.suspensions.set(self.suspensions.get() + 1);
            self.answer(Read::Suspended, true, || false)
        }

        fn in_job(&self) -> io::Result<bool> {
            self.answer(Read::InJob, false, || true)
        }
    }

    /// A path Windows can name and Rust cannot read as text: a lone surrogate, which no `str` holds.
    fn not_text() -> PathBuf {
        use std::os::windows::ffi::OsStringExt;

        PathBuf::from(std::ffi::OsString::from_wide(&[0xD800]))
    }

    #[test]
    fn verification_refuses_each_wrong_fact() {
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
        assert_eq!(verify(&late, &late.reported), Err(WrongFact::NotTheProcess));
        assert_eq!(late.suspensions.get(), 0, "a read after the failing fact never happens");

        let facts = [
            (Read::ProcessPid, WrongFact::NotTheProcess),
            (Read::ThreadPid, WrongFact::NotTheProcess),
            (Read::Parent, WrongFact::NotTheHelpersChild),
            (Read::Created, WrongFact::CreatedBefore),
            (Read::Session, WrongFact::AnotherSession),
            (Read::Image, WrongFact::NotTheProgram),
            (Read::Suspended, WrongFact::NotSuspended),
            (Read::InJob, WrongFact::InTheJob),
        ];
        for (read, fact) in facts {
            for unreadable in [false, true] {
                let fake = Fake {
                    faulty: Some(read),
                    unreadable,
                    ..Fake::right()
                };
                assert_eq!(verify(&fake, &fake.reported), Err(fact), "{read:?}, unreadable {unreadable}");
            }
        }

        let no_text = Fake {
            image: not_text(),
            ..Fake::right()
        };
        assert_eq!(
            verify(&no_text, &no_text.reported),
            Err(WrongFact::NotTheProgram),
            "a path that is not valid text is not the program the box sent"
        );

        assert_eq!(
            DesktopRefusal::WrongCommand(WrongFact::NotTheProcess).line(),
            refused("the desktop helper failed: the command it started is not the process it reported"),
            "a command of the wrong process is said as the hand-off's own failure"
        );
        assert_eq!(EXTENDED, r"\\?\", "the prefix the comparison strips");
        assert!(
            same_image(IMAGE, PROGRAM),
            "the extended prefix and the case are not part of the comparison"
        );
        assert!(same_image(PROGRAM, IMAGE), "the prefix on the program the box sent is stripped too");
        assert!(same_image(&PROGRAM.to_uppercase(), PROGRAM), "a path in other case is the same file");
        assert!(
            !same_image(r"C:\gate\another.exe", PROGRAM),
            "another program is not the one the box sent"
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
            vec![(5, time(101))],
            "only 1's child of another process, 2's creation before the helper, 3's at its own instant, 4's unread \
             creation, 6's other session and 7's unread session are left alone"
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
}
