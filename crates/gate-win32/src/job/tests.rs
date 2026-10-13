use std::io;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use jiff::Timestamp;

use super::{
    DRAIN_WAIT_MS, Departure, Job, Member, Message, Missed, Opened, Remembered, Settled, Unread, Until, drain_loop, holds, kept, member, open_any,
    remembered_as, settled, trouble,
};
use crate::{EnvironmentBlock, Launch, LaunchOutput, Running, Suspended, start_and_exit_of, system_directory, timestamp_of};

/// The longest a test waits for its member to end, for the job to remember it, or for the job's drop.
const HANG_GUARD: Duration = Duration::from_secs(600);

impl Job {
    /// Whether the job remembers a member of pid `pid`, running or ended.
    fn remembers(&self, pid: u32) -> bool {
        self.memory
            .as_ref()
            .is_some_and(|memory| memory.shared.lock().members.iter().any(|member| member.pid == pid))
    }

    /// How many remembered members still have their handle open.
    fn running_handles(&self) -> usize {
        self.memory.as_ref().map_or(0, |memory| {
            memory
                .shared
                .lock()
                .members
                .iter()
                .filter(|member| matches!(member.until, Until::Running(_)))
                .count()
        })
    }
}

/// cmd running `command`, created suspended.
fn suspended(command: &str) -> Suspended {
    let system = system_directory().expect("the system folder is read");
    Launch {
        application: system.join("cmd.exe"),
        command_line: format!("cmd /d /c {command}").encode_utf16().collect(),
        directory: system,
        environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
        output: LaunchOutput::Piped,
        priority: None,
    }
    .start_suspended()
    .expect("the member is created")
}

/// cmd running `command`, started in `job`.
fn started(job: &Job, command: &str) -> Running {
    suspended(command).assign_and_resume(job).expect("the member is assigned and resumed")
}

/// Waits until `done` says so, failing at the hang guard.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within the hang guard");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The start and the exit of the ended `process`, read from its own handle.
fn window(process: &Running) -> (Timestamp, Timestamp) {
    // SAFETY: a created process's handle has the query access the call asks for, and stays open for the whole call.
    let (start, exit) = unsafe { start_and_exit_of(process.process.raw()) }.expect("the member's times are read");
    (
        timestamp_of(start).expect("the start is in range"),
        timestamp_of(exit).expect("the exit is in range"),
    )
}

/// Second `second` of a test's minute.
fn at(second: u32) -> Timestamp {
    format!("2026-10-01T10:00:0{second}Z").parse().expect("a test timestamp parses")
}

/// The ended member `pid` that started at second `start` and ended at second `until`, put in the job at its start.
fn ended_member(pid: u32, start: u32, until: u32) -> Member {
    Member {
        pid,
        start: at(start),
        from: at(start),
        until: Until::Ended(at(until)),
    }
}

/// Times that must not be read: the process they would be read for is no member.
fn never<H>(_: &H) -> io::Result<(u64, u64)> {
    unreachable!("no member's times are read")
}

/// The FILETIME ticks of second `second` of a test's minute.
fn ticks(second: u32) -> u64 {
    // 1970-01-01 as FILETIME ticks, then 100 ns ticks since.
    let since_epoch = at(second).as_nanosecond() / 100;
    116_444_736_000_000_000 + u64::try_from(since_epoch).expect("a test time is after 1970")
}

#[test]
fn the_job_remembers_a_member_that_started_and_ended_before_the_job_was_listed() {
    let mut job = Job::anonymous_kill_on_close().expect("the job is created");
    job.remember().expect("the job remembers");
    let ended = started(&job, "exit 0");
    assert_eq!(ended.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");

    wait_until("the job lists the member as ended", || {
        job.departed().0.iter().any(|departed| departed.pid == ended.pid())
    });
    let (departed, trouble) = job.departed();

    let pids: Vec<u32> = departed.iter().map(|departed| departed.pid).collect();
    assert_eq!(pids, vec![ended.pid()], "the member that ended before any listing, alone");
    assert_eq!(trouble, None);
    let first = departed.first().copied().expect("one member is remembered");
    assert!(first.from >= job.created_at, "no member started a process before the job");
    assert!(first.until >= first.from, "it ended after it started");
}

#[test]
fn a_member_that_ended_before_any_look_keeps_its_exact_window_and_no_handle() {
    // As after a sampler failure: nothing reads the memory until the member has ended.
    let mut job = Job::anonymous_kill_on_close().expect("the job is created");
    job.remember().expect("the job remembers");
    let created = suspended("exit 0");
    created.assign(&job).expect("the member is put in the job");
    let pid = created.pid();
    wait_until("the job remembers the member", || job.remembers(pid));
    let ended = created.resume().expect("the member is resumed");
    assert_eq!(ended.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");

    let (departed, trouble) = job.departed();

    let (start, exit) = window(&ended);
    let exact = Departure {
        pid,
        start,
        from: start,
        until: exit,
    };
    assert_eq!(departed, vec![exact], "from its own start to its own exit, not the job's life");
    assert_eq!(trouble, None);
    assert_eq!(job.running_handles(), 0, "its handle is closed once its exit is kept");
}

#[test]
fn a_member_put_in_the_job_after_its_start_can_have_started_a_process_only_since_the_job_was_created() {
    let created = suspended("exit 0");
    // The job's clock is coarser than a process's start: the pause keeps the two apart.
    std::thread::sleep(Duration::from_millis(50));
    let mut job = Job::anonymous_kill_on_close().expect("the job is created");
    job.remember().expect("the job remembers");
    created.assign(&job).expect("the member is put in the job");
    let pid = created.pid();
    wait_until("the job remembers the member", || job.remembers(pid));
    let ended = created.resume().expect("the member is resumed");
    assert_eq!(ended.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");

    let (departed, _) = job.departed();

    let (start, exit) = window(&ended);
    assert!(start < job.created_at, "the member started before the job");
    let clamped = Departure {
        pid,
        start,
        from: job.created_at,
        until: exit,
    };
    assert_eq!(
        departed,
        vec![clamped],
        "from the job's creation, its start kept to tell it from a later holder of its pid"
    );
}

#[test]
fn a_member_found_gone_takes_no_one() {
    // A busy port never left the drain thread idle, so the job's creation at second 0 was its last idle moment. An
    // unrelated launcher with pid 7 started a long-lived child at second 1 and ended; the member then took pid 7 and
    // ended before the drain thread opened it. The child's parent pid names pid 7.
    assert_eq!(
        remembered_as(7, Opened::<u32>::Gone, never, at(0)),
        Err(Missed::Gone),
        "no process holds pid 7 now"
    );
    let holder_started = |_: &u32| Ok((ticks(4), 0));
    assert_eq!(
        remembered_as(7, Opened::Holder(9), holder_started, at(0)),
        Err(Missed::Gone),
        "a process outside the job holds pid 7 now"
    );
}

#[test]
fn a_member_whose_times_cannot_be_read_takes_no_one_and_is_said() {
    let mut remembered = Remembered::default();
    let denied = |_: &u32| Err(io::Error::other("denied"));

    let missed = remembered_as(7, Opened::Held(7), denied, at(0)).expect_err("no member");
    remembered.missed(7, missed);
    remembered.missed(8, Missed::Gone);
    remembered.missed(9, Missed::Unknown);

    assert_eq!(
        trouble(None, false, &remembered.unread).as_deref(),
        Some("the times of member 7 could not be read: denied"),
        "the member found gone and the one whose holder cannot be told are not said"
    );
    assert!(remembered.members.is_empty(), "none is remembered");
}

#[test]
fn a_member_that_exited_before_it_was_opened_keeps_its_exact_window_while_its_process_object_lives() {
    let job = Job::anonymous_kill_on_close().expect("the job is created");
    // This handle keeps the member's process object, and so its pid, after it has exited.
    let ended = started(&job, "exit 0");
    assert_eq!(ended.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");

    let process = open_any(ended.pid())
        .expect("the pid is opened")
        .expect("the exited member's object lives");
    let inside = holds(&job.handle, &process).expect("IsProcessInJob answers for an exited process");
    let found = member(&job.handle, ended.pid(), job.created_at).expect("a member");

    assert!(inside, "an exited process is still told to be in its job");
    let (start, exit) = window(&ended);
    assert_eq!((found.start, found.from, found.last()), (start, start, Some(exit)), "its exact window");
}

#[test]
fn the_drain_loop_hands_each_message_to_its_own_step_and_moves_nothing_on_another() {
    let mut script = vec![
        Message::Empty,
        Message::NewProcess(7),
        Message::Other,
        Message::Exited(7),
        Message::Other,
        Message::Empty,
        Message::Close,
    ]
    .into_iter();
    let mut waits = Vec::new();
    let mut found = Vec::new();
    let mut exited = Vec::new();
    let mut idle = 0;

    drain_loop(
        |wait| {
            waits.push(wait);
            Ok(script.next().expect("a scripted message"))
        },
        |pid| found.push(pid),
        |pid| exited.push(pid),
        || idle += 1,
    )
    .expect("the loop ends on the close");

    assert_eq!(found, vec![7], "the process that joined");
    assert_eq!(exited, vec![7], "the process that exited");
    assert_eq!(idle, 2, "each empty wait, and nothing else, lets the members settle");
    assert!(waits.iter().all(|&wait| wait == DRAIN_WAIT_MS), "no wait is endless: {waits:?}");
}

#[test]
fn an_exit_message_closes_the_handle_of_that_member_alone() {
    let job = Job::anonymous_kill_on_close().expect("the job is created");
    let first = started(&job, "exit 0");
    let second = started(&job, "exit 0");
    let mut remembered = Remembered::default();
    for process in [&first, &second] {
        assert_eq!(process.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");
        let handle = open_any(process.pid()).expect("the pid is opened").expect("the member's object lives");
        let (start, _) = window(process);
        remembered.add(Member {
            pid: process.pid(),
            start,
            from: start,
            until: Until::Running(handle),
        });
    }

    remembered.exited(first.pid());

    let lasts: Vec<Option<Timestamp>> = remembered.members.iter().map(Member::last).collect();
    assert_eq!(
        lasts,
        vec![Some(window(&first).1), None],
        "the second, though it has exited too, keeps its handle until its own message or an idle wait"
    );
}

#[test]
fn a_member_reported_twice_is_remembered_once_and_nothing_of_a_forgotten_one_is_kept() {
    let mut job = Job::anonymous_kill_on_close().expect("the job is created");
    job.remember().expect("the job remembers");
    let shared = Arc::clone(&job.memory.as_ref().expect("the job remembers").shared);
    shared.lock().add(ended_member(7, 1, 4));
    shared.lock().add(ended_member(7, 1, 4));
    shared.lock().add(ended_member(9, 2, 6));
    let pids = |job: &Job| job.departed().0.iter().map(|departed| departed.pid).collect::<Vec<u32>>();
    assert_eq!(pids(&job), vec![7, 9], "the second report of member 7 adds nothing");

    job.forget(&[(7, at(1), at(4))]);

    assert_eq!(pids(&job), vec![9], "only the member not forgotten");
    assert_eq!(shared.lock().members.len(), 1, "nothing of the forgotten member is kept");
}

#[test]
fn a_drain_keeps_a_reported_process_only_while_the_job_holds_it() {
    let held = |answer: io::Result<bool>| move |_: &u32| answer;

    assert_eq!(kept(Ok(Some(7)), held(Ok(true))), Opened::Held(7), "in the job");
    assert_eq!(
        kept(Ok(Some(7)), held(Ok(false))),
        Opened::Holder(7),
        "its pid went to a process outside the job"
    );
    assert_eq!(kept(Ok(None), held(Ok(true))), Opened::Gone, "no process has the pid");
}

#[test]
fn a_pid_whose_holder_cannot_be_told_gives_the_member_no_window() {
    let held = |answer: io::Result<bool>| move |_: &u32| answer;

    assert_eq!(
        kept(Ok(Some(7)), held(Err(io::Error::other("refused")))),
        Opened::Unknown,
        "in the job or not cannot be told"
    );
    assert_eq!(
        kept(Err(io::Error::other("denied")), held(Ok(true))),
        Opened::Unknown,
        "a process holds the pid"
    );
    assert_eq!(remembered_as(7, Opened::<u32>::Unknown, never, at(0)), Err(Missed::Unknown));
}

#[test]
fn an_exit_that_cannot_be_read_is_reported_not_waited_for() {
    // 1970-01-01 as FILETIME ticks.
    let epoch = 116_444_736_000_000_000;

    assert_eq!(
        settled(Err(io::Error::other("denied"))),
        Settled::Unread("denied".to_owned()),
        "not taken for running"
    );
    assert_eq!(settled(Ok((1, 0))), Settled::Running);
    assert_eq!(settled(Ok((1, epoch))), Settled::Ended(Timestamp::UNIX_EPOCH));
}

#[test]
fn a_drain_thread_that_ended_while_the_job_is_in_use_is_reported() {
    let unread = Unread {
        count: 1,
        named: vec!["unread".to_owned()],
    };

    assert_eq!(trouble(None, false, &Unread::default()), None);
    assert_eq!(
        trouble(None, true, &Unread::default()).as_deref(),
        Some("its drain thread ended on a panic")
    );
    assert_eq!(
        trouble(Some("refused"), true, &Unread::default()).as_deref(),
        Some("its drain thread stopped: refused")
    );
    assert_eq!(trouble(None, false, &unread).as_deref(), Some("unread"));
}

#[test]
fn the_members_a_memory_could_not_read_are_named_up_to_a_few_and_counted_after() {
    let mut unread = Unread::default();

    for pid in 1..=5 {
        unread.add(format!("member {pid}"));
    }

    assert_eq!(unread.named.len(), 3, "the lines kept do not grow with the members");
    assert_eq!(
        trouble(None, false, &unread).as_deref(),
        Some("member 1; member 2; member 3; 2 more member(s) could not be read")
    );
}

#[test]
fn dropping_a_remembering_job_ends_its_drain_thread_and_releases_what_it_shares() {
    let mut job = Job::anonymous_kill_on_close().expect("the job is created");
    job.remember().expect("the job remembers");
    let ended = started(&job, "exit 0");
    assert_eq!(ended.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");
    wait_until("the job remembers the member", || job.remembers(ended.pid()));
    let shared = Arc::downgrade(&job.memory.as_ref().expect("the job remembers").shared);

    let (sent, dropped) = mpsc::channel();
    std::thread::spawn(move || {
        drop(job);
        let _ = sent.send(());
    });

    dropped.recv_timeout(HANG_GUARD).expect("the drop returns within the hang guard");
    assert!(
        shared.upgrade().is_none(),
        "the thread has ended: the port, the job's copy and the members are released"
    );
}

#[test]
fn a_job_not_told_to_remember_remembers_nothing() {
    let job = Job::anonymous_kill_on_close().expect("the job is created");
    let ended = started(&job, "exit 0");
    assert_eq!(ended.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");

    assert_eq!(job.departed(), (Vec::new(), None), "no port, so no member");
}
