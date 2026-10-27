use std::io;
use std::sync::{Arc, PoisonError, mpsc};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, PostQueuedCompletionStatus};

use super::{
    DRAIN_WAIT_MS, Departure, ERROR_MORE_DATA, FINISH_KEY, Hold, JOBOBJECT_BASIC_PROCESS_ID_LIST, Job, JobEvent, Joined, Member, Message, Missed,
    Opened, Remembered, Settled, Unread, Until, drain_loop, holds, kept, listed_ids, member, open_any, remembered_as, settled, take_message, trouble,
};
use crate::{EnvironmentBlock, Launch, LaunchOutput, Running, Suspended, own_handle, start_and_exit_of, system_directory, timestamp_of};

/// The longest a test waits for its member to end, for the job to remember it, or for the job's drop.
const HANG_GUARD: Duration = Duration::from_secs(600);

impl Job {
    /// Whether the job remembers a member of pid `pid`, running or ended.
    fn remembers(&self, pid: u32) -> bool {
        self.memory
            .as_ref()
            .is_some_and(|memory| memory.shared.lock().members.iter().any(|member| member.pid == pid))
    }

    /// How many members the job remembers, running or ended.
    fn remembered(&self) -> usize {
        self.memory.as_ref().map_or(0, |memory| memory.shared.lock().members.len())
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
    job.remember(None).expect("the job remembers");
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
    job.remember(None).expect("the job remembers");
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
    job.remember(None).expect("the job remembers");
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
    let (found, _) = member(&job.handle, ended.pid(), job.created_at).expect("a member");

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
        || {},
    )
    .expect("the loop ends on the close");

    assert_eq!(found, vec![7], "the process that joined");
    assert_eq!(exited, vec![7], "the process that exited");
    assert_eq!(idle, 2, "each empty wait, and nothing else, lets the members settle");
    assert!(waits.iter().all(|&wait| wait == DRAIN_WAIT_MS), "no wait is endless: {waits:?}");
}

#[test]
fn the_drain_loop_hands_the_finish_to_its_finish_step_and_carries_on() {
    let mut script = vec![Message::Finish, Message::NewProcess(7), Message::Close].into_iter();
    let mut found = Vec::new();
    let mut finished = 0;

    drain_loop(
        |_| Ok(script.next().expect("a scripted message")),
        |pid| found.push(pid),
        |_| {},
        || {},
        || finished += 1,
    )
    .expect("the loop ends on the close");

    assert_eq!(finished, 1, "the finish ran once");
    assert_eq!(found, vec![7], "the loop took the message after the finish");
}

#[test]
fn take_message_reads_a_finish_packet_as_the_finish() {
    // SAFETY: an invalid file handle with no existing port asks for a new port, which takes no pointers. A success is a
    // fresh handle nothing else owns.
    let port = unsafe { own_handle(CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1)) }.expect("the port is created");
    // SAFETY: the port is open for the whole call, and no `OVERLAPPED` is passed.
    let posted = unsafe { PostQueuedCompletionStatus(port.raw(), 0, FINISH_KEY, std::ptr::null()) };
    assert_ne!(posted, 0, "the finish is posted");

    assert_eq!(take_message(&port, 0).expect("the packet is taken"), Message::Finish);
    assert_eq!(
        take_message(&port, 0).expect("the port is read"),
        Message::Empty,
        "one packet, taken once"
    );
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
        remembered.add(
            Member {
                pid: process.pid(),
                start,
                from: start,
                until: Until::Running(handle),
            },
            Joined::default(),
        );
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
fn a_running_member_is_traced_by_its_line_and_image_and_once_ended_only_with_its_last_times() {
    const MARKER: &str = "GATE_JOB_TRACE_MARKER";
    let mut job = Job::anonymous_kill_on_close().expect("the job is created");
    job.remember(None).expect("the job remembers");
    let member = started(&job, &format!("ping -n 600 127.0.0.1 >nul & rem {MARKER}"));
    let marked = |job: &Job| job.traced().into_iter().filter(|traced| traced.cmd.contains(MARKER)).collect::<Vec<_>>();

    wait_until("the job traces its running member", || !marked(&job).is_empty());
    let running = marked(&job);
    job.terminate(1).expect("the job's members are ended");
    assert!(member.wait(HANG_GUARD).expect("the wait answers").is_some(), "the member ended");
    wait_until("the job's members settle", || job.running_handles() == 0);
    let ended = marked(&job);

    assert_eq!(running.len(), 1, "{running:?}");
    let image = running[0].image.as_deref().expect("the member's image is read");
    assert!(image.to_ascii_lowercase().ends_with(r"\cmd.exe"), "{image}");
    assert_eq!(running[0].span.exited, None, "{running:?}");
    // A quiet member is dropped as it ends; one the machine kept alive 10 s is kept, with its exit.
    assert!(
        ended
            .iter()
            .all(|traced| traced.span.exited.is_some() && crate::traced::worth_naming(&traced.span, 0)),
        "{ended:?}"
    );
}

#[test]
fn a_burst_of_short_members_is_remembered_whole_while_they_are_named_apart() {
    const CHILDREN: u32 = 40;
    let mut job = Job::anonymous_kill_on_close().expect("the job is created");
    job.remember(None).expect("the job remembers");
    // Each child waits on stdin, a pipe whose write end the test holds, so none is gone before the drain thread opens it.
    let mut parent = suspended(&format!("for /L %i in (1,1,{CHILDREN}) do @start /b cmd /d /c set /p line="));
    let release = parent.take_pipes().expect("a piped launch has pipes");
    let parent = parent.assign_and_resume(&job).expect("the parent is assigned and resumed");
    assert_eq!(parent.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the burst started");
    let total = job.accounting().expect("the job's accounting is read").total_processes;
    let count = usize::try_from(total).expect("a small count");
    wait_until("the kill memory holds every member while the children wait", || job.remembered() >= count);

    drop(release);
    wait_until("every member counted in processes is found by the kill memory", || {
        job.departed().0.len() >= count
    });
    let (departed, trouble) = job.departed();

    assert_eq!(total, CHILDREN + 1, "the parent and each child joined the job");
    assert_eq!(departed.len(), usize::try_from(total).expect("a small count"), "{departed:?}");
    assert_eq!(trouble, None, "no member went unread");
}

#[test]
fn a_member_reported_twice_is_remembered_once_and_nothing_of_a_forgotten_one_is_kept() {
    let mut job = Job::anonymous_kill_on_close().expect("the job is created");
    job.remember(None).expect("the job remembers");
    let shared = Arc::clone(&job.memory.as_ref().expect("the job remembers").shared);
    shared.lock().add(ended_member(7, 1, 4), Joined::default());
    shared.lock().add(ended_member(7, 1, 4), Joined::default());
    shared.lock().add(ended_member(9, 2, 6), Joined::default());
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
    job.remember(None).expect("the job remembers");
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

/// A test's hold on a drain thread, released when dropped, so a panic in the test never leaves the thread held for the
/// job's drop to wait on.
struct Held<'a>(&'a Hold);

impl<'a> Held<'a> {
    /// Holds the drain thread of `hold` before it takes its next message.
    fn on(hold: &'a Hold) -> Self {
        hold.state.lock().unwrap_or_else(PoisonError::into_inner).held = true;
        Held(hold)
    }

    /// Whether the drain thread waits on the hold.
    fn parked(&self) -> bool {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner).parked
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner).held = false;
        self.0.changed.notify_all();
    }
}

/// A job that remembers, telling its events to the receiver returned with it.
fn telling_job() -> (Job, mpsc::Receiver<JobEvent>) {
    let mut job = Job::anonymous_kill_on_close().expect("the job is created");
    let (sink, events) = mpsc::channel();
    job.remember(Some(sink)).expect("the job remembers");
    (job, events)
}

/// Every event `events` holds until its sender is dropped, failing at the hang guard.
fn all_events(events: &mpsc::Receiver<JobEvent>) -> Vec<JobEvent> {
    let mut all = Vec::new();
    loop {
        match events.recv_timeout(HANG_GUARD) {
            Ok(event) => all.push(event),
            Err(error) => {
                assert_eq!(
                    error,
                    mpsc::RecvTimeoutError::Disconnected,
                    "the events end within the hang guard: {all:?}"
                );
                return all;
            }
        }
    }
}

/// The pid `event` is about.
fn pid_of(event: &JobEvent) -> Option<u32> {
    match event {
        JobEvent::Spawned { pid, .. } | JobEvent::Exited { pid, .. } | JobEvent::Missed { pid, .. } | JobEvent::Left { pid } => Some(*pid),
        JobEvent::Stopped { .. } => None,
    }
}

/// The pid of the first cmd.exe `events` say `parent` started.
fn cmd_started_by(events: &[JobEvent], parent: u32) -> Option<u32> {
    events.iter().find_map(|event| match event {
        JobEvent::Spawned {
            pid,
            ppid: Some(ppid),
            image: Some(image),
            ..
        } if *ppid == parent && image.eq_ignore_ascii_case("cmd.exe") => Some(*pid),
        _ => None,
    })
}

#[test]
fn each_member_of_a_tree_is_spawned_before_it_exits_and_names_its_parent() {
    let (job, events) = telling_job();
    // The grandchild waits on stdin, a pipe whose write end the test holds, so each process runs while the drain opens it.
    let mut root = suspended("cmd /d /c cmd /d /c set /p line=");
    let release = root.take_pipes().expect("a piped launch has pipes");
    let root = root.assign_and_resume(&job).expect("the root is assigned and resumed");
    let mut seen = Vec::new();
    while cmd_started_by(&seen, root.pid()).and_then(|child| cmd_started_by(&seen, child)).is_none() {
        seen.push(events.recv_timeout(HANG_GUARD).expect("the next event within the hang guard"));
    }

    drop(release);
    assert!(root.wait(HANG_GUARD).expect("the wait answers").is_some(), "the tree ended");
    job.finish_events(HANG_GUARD);
    seen.extend(all_events(&events));

    let child = cmd_started_by(&seen, root.pid()).expect("the root started the child");
    let grandchild = cmd_started_by(&seen, child).expect("the child started the grandchild");
    assert_eq!(
        cmd_started_by(&seen, std::process::id()),
        Some(root.pid()),
        "the root names this process as its parent and cmd.exe as its image: {seen:?}"
    );
    for pid in [root.pid(), child, grandchild] {
        let spawn = seen
            .iter()
            .position(|event| matches!(event, JobEvent::Spawned { pid: of, .. } if *of == pid));
        let exit = seen
            .iter()
            .position(|event| matches!(event, JobEvent::Exited { pid: of, .. } if *of == pid));
        let (spawn, exit) = (spawn.expect("each is spawned"), exit.expect("each exits"));
        assert!(spawn < exit, "{pid} is spawned before it exits: {seen:?}");
    }
}

#[test]
fn a_member_s_exit_carries_its_code_its_exit_time_and_its_run_time() {
    let (job, events) = telling_job();
    let ended = started(&job, "exit 3");
    assert_eq!(ended.wait(HANG_GUARD).expect("the wait answers"), Some(3), "the member ended");
    wait_until("the job remembers the member", || job.remembers(ended.pid()));

    job.finish_events(HANG_GUARD);
    let mine: Vec<JobEvent> = all_events(&events)
        .into_iter()
        .filter(|event| pid_of(event) == Some(ended.pid()))
        .collect();

    assert_eq!(mine.len(), 2, "a spawn and an exit: {mine:?}");
    let JobEvent::Spawned { at: spawned, .. } = mine[0] else {
        unreachable!("the spawn comes first: {mine:?}")
    };
    let JobEvent::Exited { code, at, ran, .. } = mine[1] else {
        unreachable!("the exit comes second: {mine:?}")
    };
    assert_eq!(code, Some(3));
    let exited = at.expect("the exit time is read");
    assert!(exited >= spawned, "{mine:?}");
    assert_eq!(ran, Some(exited.duration_since(spawned)), "exit less create");
    assert!(!ran.expect("the run time is read").is_negative(), "{mine:?}");
}

#[test]
fn a_member_the_drain_thread_has_not_taken_by_the_finish_still_gets_its_spawn_and_exit() {
    let (job, events) = telling_job();
    let memory = job.memory.as_ref().expect("the job remembers");
    let held = Held::on(&memory.shared.hold);
    wait_until("the drain thread is parked on the hold", || held.parked());
    let late = started(&job, "exit 0");
    assert_eq!(late.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");

    assert!(job.post_finish(), "the finish is posted behind the member's messages");
    drop(held);
    // The drain thread itself drops the sink, so the caller's own finish below is not what sends the events.
    wait_until("the drain thread drops the sink", || memory.shared.lock().events.is_none());
    job.finish_events(HANG_GUARD);
    let mine: Vec<JobEvent> = all_events(&events)
        .into_iter()
        .filter(|event| pid_of(event) == Some(late.pid()))
        .collect();

    assert!(
        matches!(mine.as_slice(), [JobEvent::Spawned { .. }, JobEvent::Exited { code: Some(0), .. }]),
        "a spawn and an exit of the member the drain thread had not taken: {mine:?}"
    );
}

#[test]
fn a_member_still_running_at_the_finish_is_left_and_the_events_end_once() {
    let (job, events) = telling_job();
    // It waits on stdin, a pipe whose write end the test holds, until the test lets it go.
    let mut waiting = suspended("set /p line=");
    let release = waiting.take_pipes().expect("a piped launch has pipes");
    let waiting = waiting.assign_and_resume(&job).expect("the member is assigned and resumed");
    wait_until("the job remembers the member", || job.remembers(waiting.pid()));

    job.finish_events(Duration::from_millis(1));
    let all = all_events(&events);
    let mine: Vec<&JobEvent> = all.iter().filter(|event| pid_of(event) == Some(waiting.pid())).collect();
    job.finish_events(HANG_GUARD);

    assert!(
        matches!(mine.as_slice(), [JobEvent::Spawned { .. }, JobEvent::Left { .. }]),
        "spawned, then left: {mine:?}"
    );
    assert!(
        !all.iter().any(|event| matches!(event, JobEvent::Stopped { .. })),
        "a finish is no stop: {all:?}"
    );
    assert_eq!(
        events.try_recv(),
        Err(mpsc::TryRecvError::Disconnected),
        "the second finish sends nothing"
    );
    drop(release);
    assert!(
        waiting.wait(HANG_GUARD).expect("the wait answers").is_some(),
        "the member ended at its stdin's end"
    );
}

#[test]
fn a_job_dropped_before_its_events_are_finished_says_they_stopped_last() {
    let (job, events) = telling_job();
    let ended = started(&job, "exit 0");
    assert_eq!(ended.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");
    wait_until("the job remembers the member", || job.remembers(ended.pid()));

    drop(job);
    let all = all_events(&events);

    assert_eq!(
        all.last(),
        Some(&JobEvent::Stopped {
            why: "the job was dropped before its events were finished".to_owned()
        }),
        "the stop is the last event: {all:?}"
    );
    assert_eq!(all.iter().filter(|event| matches!(event, JobEvent::Stopped { .. })).count(), 1, "{all:?}");
    assert!(
        !all.iter().any(|event| matches!(event, JobEvent::Left { .. })),
        "a stop says no leftovers: {all:?}"
    );
}

#[test]
fn a_job_not_told_to_remember_remembers_nothing() {
    let job = Job::anonymous_kill_on_close().expect("the job is created");
    let ended = started(&job, "exit 0");
    assert_eq!(ended.wait(HANG_GUARD).expect("the wait answers"), Some(0), "the member ended");

    assert_eq!(job.departed(), (Vec::new(), None), "no port, so no member");
}

/// Where a `JOBOBJECT_BASIC_PROCESS_ID_LIST`'s ids begin, after its header.
fn first_id() -> usize {
    std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList) / std::mem::size_of::<usize>()
}

/// Lays into `buffer` the list the system would have written into it: its two counts, and `ids` after them.
fn list_in(buffer: &mut [usize], assigned: u32, ids: &[u32]) {
    let first = first_id();
    // SAFETY: `buffer` is a whole list and its ids, aligned as the list is, and the ids fit after its header.
    let list = unsafe { &mut *buffer.as_mut_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
    list.NumberOfAssignedProcesses = assigned;
    list.NumberOfProcessIdsInList = u32::try_from(ids.len()).expect("the test lists few ids");
    for (slot, &id) in buffer[first..].iter_mut().zip(ids) {
        *slot = usize::try_from(id).expect("a process id fits a usize");
    }
}

#[test]
fn a_successful_list_shorter_than_its_assigned_count_is_taken_as_it_is() {
    let mut reads = 0;
    let ids = listed_ids(|buffer, _length| {
        reads += 1;
        list_in(buffer, 12, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        Ok(())
    })
    .expect("a read with room to spare is complete");
    let listed: Vec<u32> = (1..=11).collect();

    assert_eq!(ids, listed, "the ids it listed, as they are");
    assert_eq!(reads, 1, "the list is not read again");
}

#[test]
fn a_list_that_needs_more_room_grows_to_the_assigned_count() {
    let mut reads = 0;
    let mut slots = Vec::new();
    let ids = listed_ids(|buffer, _length| {
        reads += 1;
        slots.push(buffer.len() - first_id());
        if reads == 1 {
            list_in(buffer, 100, &[]);
            return Err(ERROR_MORE_DATA);
        }
        list_in(buffer, 100, &(1..=100).collect::<Vec<u32>>());
        Ok(())
    })
    .expect("the second read holds the list");
    let listed: Vec<u32> = (1..=100).collect();

    assert_eq!(ids, listed, "every id the second read listed");
    assert_eq!(reads, 2, "the second read has the room the assigned count asks for");
    assert_eq!(
        slots,
        [64, 116],
        "the second buffer holds the 100 assigned and the margin of 16, not a doubling"
    );
}

#[test]
fn a_list_needing_more_room_with_a_smaller_count_still_grows() {
    let mut reads = 0;
    let mut slots = Vec::new();
    let ids = listed_ids(|buffer, _length| {
        reads += 1;
        slots.push(buffer.len() - first_id());
        if reads == 1 {
            list_in(buffer, 40, &[]);
            return Err(ERROR_MORE_DATA);
        }
        list_in(buffer, 40, &(1..=40).collect::<Vec<u32>>());
        Ok(())
    })
    .expect("the second read holds the list");
    let listed: Vec<u32> = (1..=40).collect();

    assert_eq!(ids, listed, "every id the second read listed");
    assert_eq!(reads, 2, "an assigned count below the buffer's own still asks for one more read");
    assert_eq!(slots, [64, 80], "the smaller count still grows the buffer by the margin of 16");
}

#[test]
fn a_successful_read_that_fills_its_buffer_reads_again() {
    let mut reads = 0;
    let mut slots = Vec::new();
    let ids = listed_ids(|buffer, _length| {
        reads += 1;
        slots.push(buffer.len() - first_id());
        if reads == 1 {
            list_in(buffer, 70, &(1..=64).collect::<Vec<u32>>());
            return Ok(());
        }
        list_in(buffer, 70, &(1..=70).collect::<Vec<u32>>());
        Ok(())
    })
    .expect("the second read holds the list");
    let listed: Vec<u32> = (1..=70).collect();

    assert_eq!(ids, listed, "every id the second read listed");
    assert_eq!(reads, 2, "a full buffer listing fewer ids than assigned is read again");
    assert_eq!(slots, [64, 86], "the second read has the assigned count and the margin of 16");
}

#[test]
fn a_list_that_keeps_needing_more_room_fails_after_eight_reads() {
    let mut reads = 0_u32;
    let error = listed_ids(|buffer, _length| {
        reads += 1;
        list_in(buffer, 100 * reads, &[]);
        Err(ERROR_MORE_DATA)
    })
    .expect_err("a job still outgrowing its buffer is an error");

    assert_eq!(reads, 8, "no ninth read is made");
    assert_eq!(error.to_string(), "the job's process list kept growing past 800 entries over 8 reads");
}

#[test]
fn a_failed_read_other_than_more_data_is_an_error() {
    let error = listed_ids(|buffer, _length| {
        list_in(buffer, 1, &[]);
        Err(ERROR_ACCESS_DENIED)
    })
    .expect_err("the system's error is reported");

    assert_eq!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED.cast_signed()));
}
