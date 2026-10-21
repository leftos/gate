//! `gate-box box stop`: ends the process group of the run whose job the payload names, and the group of its tool
//! restore, `<job>-tools`. A run records each group beside its work folder, `<Work>.pgid` and `<Work>.tools.pgid`: the
//! job's name, then the group in decimal. A group is signalled only when its record names the payload's own job, so a
//! stop that arrives late finds another run's record, or none, and signals nothing.

use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use gate_wire::Payload;

use crate::console::Console;
use crate::files;
use crate::lines::{
    FAILED_STATUS, JobRecord, ROOT_STATUS, Recorded, USAGE_STATUS, job_record, record_unread, record_without_group, root_flaw, root_unsafe_line,
    signal_guard, work_layout,
};

/// How long the group has to end after `SIGTERM`.
const TERM_WAIT: Duration = Duration::from_secs(10);
/// How long it has to end after `SIGKILL`.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// How often the stop looks whether the group is gone.
const POLL: Duration = Duration::from_millis(100);
/// The most bytes of a job record read.
const RECORD_LIMIT: u64 = 4096;

/// Stops the payload's job and its tool restore's job; the status: 0 when each is gone or there was none, 1 when one
/// is left or could not be stopped or the work root is refused, 2 for a payload whose `Key`, `Work` or `Job` does not
/// fit its work folder or whose `Work` is not `<root>/work/<key>`.
pub fn stop(payload: &Payload, console: &mut Console) -> u8 {
    let named = payload
        .work_folder()
        .and_then(|(key, work)| Ok((work, payload.job(key)?, work_layout(work)?)));
    let (work, job, (root, parent)) = match named {
        Ok(named) => named,
        Err(line) => {
            console.err(&line);
            return USAGE_STATUS;
        }
    };
    match records_folder(&root, parent) {
        Ok(true) => {}
        Ok(false) => return 0,
        Err((line, status)) => {
            console.err(&line);
            return status;
        }
    }
    let main = stopped(Path::new(&format!("{work}.pgid")), job, console);
    let tools = stopped(Path::new(&format!("{work}.tools.pgid")), &format!("{job}-tools"), console);
    main.max(tools)
}

/// Whether the job records may be read from `parent`, the work root `root`'s `<root>/work`, judged as the preflight
/// judges them and making nothing: `true` when the root is trusted ([`root_flaw`]) and `parent` is the box user's own
/// plain folder, `false` when either is missing, so no run of this builder recorded a job there. Else the line refusing
/// it and the status: the work-root line, [`ROOT_STATUS`], or the folder's line, [`FAILED_STATUS`].
pub(crate) fn records_folder(root: &str, parent: &Path) -> Result<bool, (String, u8)> {
    let facts = match files::facts(Path::new(root)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err((root_unsafe_line(&format!("cannot read {root}: {error}")), ROOT_STATUS)),
        Ok(facts) => facts,
    };
    if let Some(why) = root_flaw(facts, root, &files::user_name(facts.euid)) {
        return Err((root_unsafe_line(&why), ROOT_STATUS));
    }
    if !files::exists(parent) {
        return Ok(false);
    }
    files::unusable_folder(parent).map_or(Ok(true), |line| Err((line, FAILED_STATUS)))
}

/// Stops the process group the record `record` holds for the job `job`, and says how it went: nothing for no record or
/// another job's, `gate: remote stopped job <job>` on stdout once the group is gone, else why on stderr. The status.
fn stopped(record: &Path, job: &str, console: &mut Console) -> u8 {
    let could_not = |why: &str| format!("gate: remote could not stop job {job}: {why}");
    let read = files::read_plain_dated(record, RECORD_LIMIT).map(|(bytes, written)| (job_record(&bytes, job), written));
    let (recorded, written) = match read {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return 0,
        Ok((JobRecord::Other, _)) => return 0,
        Err(error) => {
            console.err(&could_not(&record_unread(&record.display().to_string(), &error.to_string())));
            return FAILED_STATUS;
        }
        Ok((JobRecord::Malformed, _)) => {
            console.err(&could_not(&record_without_group(&record.display().to_string())));
            return FAILED_STATUS;
        }
        Ok((JobRecord::Group(recorded), written)) => (recorded, written),
    };
    let group = recorded.group;
    if let Err(why) = signal_guard(group, own_group()) {
        console.err(&could_not(&why));
        return FAILED_STATUS;
    }
    if record_current(recorded, written) == Record::Stale {
        // A record naming no group of its run's is no job: the stop finds nothing, as for no record. It holds no work
        // lock, so the record is left for the next run, which removes it under the lock.
        return 0;
    }
    match ended(group) {
        Ok(()) => {
            console.out(&format!("gate: remote stopped job {job}"));
            0
        }
        Err(why) => {
            console.err(&could_not(&why));
            FAILED_STATUS
        }
    }
}

/// Ends the process group `group`: `SIGTERM`, a wait of [`TERM_WAIT`] for it to be gone, then `SIGKILL` and a wait of
/// [`KILL_WAIT`]. A group already gone at the first signal is ended. Else why not: a signal refused, or the processes
/// left.
fn ended(group: i32) -> Result<(), String> {
    for (signal, wait) in [(libc::SIGTERM, TERM_WAIT), (libc::SIGKILL, KILL_WAIT)] {
        match signalled(group, signal) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
        if gone_within(group, wait) {
            return Ok(());
        }
    }
    Err(match group_pids(group) {
        Ok(left) => format!("{} process(es) left", left.len()),
        Err(why) => format!("its process group {group} is still there ({why})"),
    })
}

/// Sends `signal` to the process group `group`: whether the group was there; else why the signal was refused.
fn signalled(group: i32, signal: i32) -> io::Result<bool> {
    // SAFETY: `killpg` takes two integers and touches no memory; `group` was read from the job's own record and passed
    // `signal_guard`, so it is neither init nor the box's own group.
    let sent = unsafe { libc::killpg(group, signal) };
    if sent == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// Whether the process group `group` is gone within `wait`, looked at every [`POLL`].
fn gone_within(group: i32, wait: Duration) -> bool {
    let started = Instant::now();
    loop {
        // A group the box may no longer signal is still there.
        if matches!(signalled(group, 0), Ok(false)) {
            return true;
        }
        if started.elapsed() >= wait {
            return false;
        }
        thread::sleep(POLL);
    }
}

/// The box's own process group.
pub(crate) fn own_group() -> i32 {
    // SAFETY: `getpgrp` takes nothing, touches no memory and always succeeds.
    unsafe { libc::getpgrp() }
}

/// The `proc_listpids` kind that lists one process group's processes, `PROC_PGRP_ONLY` of macOS's `<sys/proc_info.h>`,
/// which the `libc` crate does not define.
#[cfg(target_os = "macos")]
const PROC_PGRP_ONLY: u32 = 2;

/// The most processes of one group listed.
#[cfg(target_os = "macos")]
const GROUP_PIDS: usize = 4096;

/// The processes of the process group `group`, from `proc_listpids`.
pub(crate) fn group_pids(group: i32) -> Result<Vec<i32>, String> {
    let mut pids = Vec::new();
    group_pids_into(group, &mut pids)?;
    Ok(pids)
}

/// Lists the processes of the process group `group` into `pids`, from `proc_listpids`, its room reused.
#[cfg(target_os = "macos")]
pub(crate) fn group_pids_into(group: i32, pids: &mut Vec<i32>) -> Result<(), String> {
    let group = u32::try_from(group).map_err(|error| format!("the group {group} is not a process id: {error}"))?;
    pids.clear();
    pids.resize(GROUP_PIDS, 0);
    let bytes = i32::try_from(pids.len() * std::mem::size_of::<i32>()).map_err(|error| error.to_string())?;
    // SAFETY: the buffer is `bytes` bytes of `pid_t`, which `proc_listpids` fills no further than that size.
    let filled = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), bytes) };
    let filled = usize::try_from(filled).map_err(|_| format!("proc_listpids failed: {}", io::Error::last_os_error()))?;
    pids.truncate(filled / std::mem::size_of::<i32>());
    pids.retain(|pid| *pid != 0);
    Ok(())
}

/// A process group's processes are listed only on macOS.
#[cfg(not(target_os = "macos"))]
pub(crate) fn group_pids_into(_group: i32, _pids: &mut Vec<i32>) -> Result<(), String> {
    Err(format!("listing a process group is not built for {}", std::env::consts::OS))
}

/// Whether a group record still names the group its run made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    /// The group is the recorded one's: a run refuses `busy` for it, and a stop signals it.
    Current,
    /// No process of the recorded group is left, or the id now names another's: a stop finds no job, and the next run,
    /// under the work lock, removes the record.
    Stale,
}

/// What the box sees of the processes of a recorded process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Members {
    /// The group has no process.
    Empty,
    /// It has processes; one of them is in the session the record names when `in_session`.
    Listed {
        /// Whether a process of the group is in the record's session.
        in_session: bool,
    },
    /// Its processes could not be listed, or one's session could not be read.
    Unknown,
}

/// What the box can see of the process whose pid is a recorded group's id.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LeaderFacts {
    /// No process has that pid.
    Absent,
    /// A process has it: its effective user and when it started.
    Present {
        /// Its effective user id.
        uid: u32,
        /// When it started.
        started: SystemTime,
    },
    /// It could not be looked at.
    Unknown,
}

/// What the box sees of a recorded process group, for [`record_decision`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupFacts {
    /// The `errno` of `killpg(group, 0)`, `None` when it succeeded.
    pub(crate) probe: Option<i32>,
    /// When the machine booted, when known.
    pub(crate) boot: Option<SystemTime>,
    /// The process whose pid is the group's id.
    pub(crate) leader: LeaderFacts,
    /// The group's processes.
    pub(crate) members: Members,
    /// The box's effective user id.
    pub(crate) euid: u32,
}

/// How long after its record was written a recorded group's leader may have started: the record is written from the
/// leader itself, and file times and start times are kept at different grains.
const RECORD_SLACK: Duration = Duration::from_secs(1);

/// Whether the record written at `written` holding `recorded` still names the group its run made, as
/// [`record_decision`] judges it on what the box sees of the group now.
pub(crate) fn record_current(recorded: Recorded, written: SystemTime) -> Record {
    record_decision(written, recorded.session, &group_facts(recorded))
}

/// Whether a record written at `written`, made in the session `session`, names the group `facts` describe. Stale when:
/// no process is in the group (`ESRCH`); the box may not signal it (`EPERM`), as a run's groups are the box user's
/// own; the record was written before the machine booted; a process has the group's id but is another user's or
/// started more than [`RECORD_SLACK`] after the record was written, a pid reused since; or no process has the group's
/// id and none of the group's is in the record's session, the id another session's group's since (a two-line record,
/// which names no session, is stale then). Anything else the box cannot tell counts as current, so a live group is
/// never taken for gone.
pub(crate) fn record_decision(written: SystemTime, session: Option<i32>, facts: &GroupFacts) -> Record {
    if matches!(facts.probe, Some(libc::ESRCH | libc::EPERM)) {
        return Record::Stale;
    }
    if facts.boot.is_some_and(|boot| written < boot) {
        return Record::Stale;
    }
    match facts.leader {
        LeaderFacts::Present { uid, started } if uid != facts.euid || started > written + RECORD_SLACK => Record::Stale,
        LeaderFacts::Absent if session.is_none() => Record::Stale,
        LeaderFacts::Absent => match facts.members {
            Members::Empty | Members::Listed { in_session: false } => Record::Stale,
            Members::Listed { in_session: true } | Members::Unknown => Record::Current,
        },
        _ => Record::Current,
    }
}

/// What the box sees now of the process group `recorded` names.
fn group_facts(recorded: Recorded) -> GroupFacts {
    let group = recorded.group;
    // SAFETY: signal 0 only asks whether the group exists and may be signalled; it touches no memory.
    let probe = (unsafe { libc::killpg(group, 0) } != 0).then(|| io::Error::last_os_error().raw_os_error().unwrap_or(0));
    GroupFacts {
        probe,
        boot: boot_time(),
        leader: leader_facts(group),
        members: members(group, recorded.session),
        euid: files::effective_uid(),
    }
}

/// The processes of the process group `group`, and whether one is in the session `session` (`getsid`); none is in no
/// session. A process gone before its session is read is not counted.
fn members(group: i32, session: Option<i32>) -> Members {
    let mut pids = Vec::new();
    if group_pids_into(group, &mut pids).is_err() {
        return Members::Unknown;
    }
    if pids.is_empty() {
        return Members::Empty;
    }
    let Some(session) = session else {
        return Members::Listed { in_session: false };
    };
    let mut unread = false;
    for pid in pids {
        // SAFETY: `getsid` takes a pid and touches no memory.
        let sid = unsafe { libc::getsid(pid) };
        if sid == session {
            return Members::Listed { in_session: true };
        }
        unread |= sid < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    }
    if unread {
        Members::Unknown
    } else {
        Members::Listed { in_session: false }
    }
}

/// When the machine booted ([`crate::boot::boot_time`]); `None` when it cannot be read.
fn boot_time() -> Option<SystemTime> {
    let (seconds, micros) = crate::boot::boot_time().ok()?;
    let seconds = u64::try_from(seconds).ok()?;
    let micros = u32::try_from(micros).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, micros.saturating_mul(1000)))
}

/// The process whose pid is `pid`, from `proc_pidinfo(PROC_PIDTBSDINFO)`.
#[cfg(target_os = "macos")]
fn leader_facts(pid: i32) -> LeaderFacts {
    // SAFETY: an all-zero `proc_bsdinfo` is a valid value of a plain C struct.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let Ok(size) = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return LeaderFacts::Unknown;
    };
    // SAFETY: `info` is `size` bytes and lives across the call, which writes at most that much into it.
    let filled = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if filled != size {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) if filled <= 0 => LeaderFacts::Absent,
            _ => LeaderFacts::Unknown,
        };
    }
    let micros = u32::try_from(info.pbi_start_tvusec).unwrap_or(0);
    match SystemTime::UNIX_EPOCH.checked_add(Duration::new(info.pbi_start_tvsec, micros.saturating_mul(1000))) {
        Some(started) => LeaderFacts::Present { uid: info.pbi_uid, started },
        None => LeaderFacts::Unknown,
    }
}

/// A process's start is read only on macOS.
#[cfg(not(target_os = "macos"))]
fn leader_facts(_pid: i32) -> LeaderFacts {
    LeaderFacts::Unknown
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test whose temp folder, payload or sleep cannot be made must fail loudly"
)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_wire::{PathForm, Payload};
    use serde_json::json;

    use super::{GroupFacts, LeaderFacts, Members, Record, own_group, record_current, record_decision, stop};
    use crate::console::{Console, Output, RecordingSink};
    use crate::files::{effective_uid, held_for_test, user_name};
    use crate::git::{Git, GitClass, RunGit};
    use crate::lines::{Recorded, root_unsafe_line};
    use crate::process::GroupRecord;

    const KEY: &str = "repo-0123abcd";
    const JOB: &str = "gate-remote-repo-0123abcd-0123456789ab";

    /// A work folder's parent private to this process and the case `name`, removed when the case ends.
    struct Case {
        base: PathBuf,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-stop-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            fs::create_dir_all(base.join("work")).expect("the case folder is made");
            Case { base }
        }

        fn work(&self) -> String {
            format!("{}/work/{KEY}", self.base.to_str().expect("the temp folder is UTF-8"))
        }

        fn payload(&self) -> Payload {
            let json = json!({ "Key": KEY, "Work": self.work(), "Job": JOB });
            Payload::decode(&STANDARD.encode(json.to_string()), PathForm::Unix).expect("the payload decodes")
        }

        /// Writes the job record `<Work><suffix>` naming `job` and `group`.
        fn record(&self, suffix: &str, job: &str, group: &str) {
            fs::write(format!("{}{suffix}", self.work()), format!("{job}\n{group}\n")).expect("the record is written");
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// A `sleep 600` leading a process group of its own, as a run's command does.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this test stands in for a run"
    )]
    fn sleeping_group() -> Child {
        Command::new("sleep").arg("600").process_group(0).spawn().expect("sleep starts")
    }

    fn stopped(case: &Case) -> (u8, Console) {
        let mut console = Console::captured();
        let status = stop(&case.payload(), &mut console);
        (status, console)
    }

    #[test]
    fn stop_ends_the_recorded_group_and_says_so() {
        let case = Case::new("ends");
        let child = sleeping_group();
        case.record(".pgid", JOB, &child.id().to_string());
        // The run reaps its command; here a thread does, so the ended group leaves no zombie behind.
        let reaper = thread::spawn(move || {
            let mut child = child;
            child.wait()
        });

        let (status, console) = stopped(&case);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, [format!("gate: remote stopped job {JOB}")]);
        let ended = reaper.join().expect("the reaper ends").expect("the sleep is reaped");
        assert!(!ended.success(), "the sleep was ended by the stop: {ended:?}");
    }

    #[test]
    fn stop_ends_the_tool_restore_group_too() {
        let case = Case::new("tools");
        let child = sleeping_group();
        case.record(".tools.pgid", &format!("{JOB}-tools"), &child.id().to_string());
        let reaper = thread::spawn(move || {
            let mut child = child;
            child.wait()
        });

        let (status, console) = stopped(&case);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, [format!("gate: remote stopped job {JOB}-tools")]);
        assert!(reaper.join().expect("the reaper ends").is_ok());
    }

    #[test]
    fn stop_of_a_record_whose_group_id_is_a_process_started_after_it_was_written_signals_nothing_and_leaves_it() {
        let case = Case::new("reused");
        let mut child = sleeping_group();
        case.record(".pgid", JOB, &child.id().to_string());
        let record = format!("{}.pgid", case.work());
        let file = fs::File::options().write(true).open(&record).expect("the record opens");
        // The record is older than the process its id now names: a pid reused since the record was written.
        file.set_modified(SystemTime::now() - Duration::from_secs(120))
            .expect("the record's time is set");

        let (status, console) = stopped(&case);
        let alive = child.try_wait().expect("the sleep is looked at").is_none();
        // Best effort: the sleep is this case's own.
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!((status, console.out.len(), console.err.len()), (0, 0, 0), "{:?}", console.err);
        assert!(alive, "the process the stale record names is never signalled");
        assert!(
            PathBuf::from(&record).exists(),
            "a stop holds no work lock, so only the next run removes the stale record"
        );
    }

    /// The facts of a group led by `leader`, with `members` in it, its probe answering `probe`; the machine booted
    /// 1000 s after the epoch, and the box's user is 501.
    fn facts(probe: Option<i32>, leader: LeaderFacts, members: Members) -> GroupFacts {
        GroupFacts {
            probe,
            boot: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1000)),
            leader,
            members,
            euid: 501,
        }
    }

    #[test]
    fn a_record_is_stale_unless_its_group_is_the_one_its_run_made() {
        let written = SystemTime::UNIX_EPOCH + Duration::from_secs(5000);
        let own = |started: u64| LeaderFacts::Present {
            uid: 501,
            started: SystemTime::UNIX_EPOCH + Duration::from_secs(started),
        };
        let in_session = Members::Listed { in_session: true };
        let elsewhere = Members::Listed { in_session: false };
        let cases = [
            (
                "the leader started before the record",
                facts(None, own(4990), in_session),
                Record::Current,
            ),
            ("the leader started within the slack", facts(None, own(5001), in_session), Record::Current),
            (
                "no process in the group",
                facts(Some(libc::ESRCH), LeaderFacts::Absent, Members::Empty),
                Record::Stale,
            ),
            (
                "a group the box may not signal",
                facts(Some(libc::EPERM), own(4990), in_session),
                Record::Stale,
            ),
            (
                "a probe failing otherwise",
                facts(Some(libc::EINVAL), own(4990), in_session),
                Record::Current,
            ),
            ("a pid reused after the record", facts(None, own(5002), in_session), Record::Stale),
            (
                "another user's process",
                facts(
                    None,
                    LeaderFacts::Present {
                        uid: 0,
                        started: SystemTime::UNIX_EPOCH,
                    },
                    in_session,
                ),
                Record::Stale,
            ),
            (
                "the leader gone, a member in the record's session",
                facts(None, LeaderFacts::Absent, in_session),
                Record::Current,
            ),
            (
                "the leader gone, members of another session only: the id reused by another's group",
                facts(None, LeaderFacts::Absent, elsewhere),
                Record::Stale,
            ),
            (
                "the leader gone, the members' sessions not read",
                facts(None, LeaderFacts::Absent, Members::Unknown),
                Record::Current,
            ),
            (
                "the leader gone, no member left",
                facts(None, LeaderFacts::Absent, Members::Empty),
                Record::Stale,
            ),
            ("the leader not looked at", facts(None, LeaderFacts::Unknown, in_session), Record::Current),
        ];
        for (what, facts, wanted) in cases {
            assert_eq!(record_decision(written, Some(77), &facts), wanted, "{what}");
        }
        for members in [in_session, elsewhere, Members::Unknown] {
            assert_eq!(
                record_decision(written, None, &facts(None, LeaderFacts::Absent, members)),
                Record::Stale,
                "a two-line record whose leader is gone: {members:?}"
            );
        }
        assert_eq!(
            record_decision(written, None, &facts(None, own(4990), elsewhere)),
            Record::Current,
            "a two-line record whose leader is there is judged by its leader"
        );
        let before_boot = SystemTime::UNIX_EPOCH + Duration::from_secs(999);
        assert_eq!(
            record_decision(before_boot, Some(77), &facts(None, own(0), in_session)),
            Record::Stale,
            "written before the boot"
        );
        let unknown_boot = GroupFacts {
            boot: None,
            ..facts(None, own(0), in_session)
        };
        assert_eq!(
            record_decision(before_boot, Some(77), &unknown_boot),
            Record::Current,
            "no boot time to judge by"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_group_whose_leader_has_gone_is_current_only_while_a_member_is_in_the_record_s_session() {
        // The leader exits at once, leaving its background sleep alone in the group.
        #[expect(
            clippy::disallowed_methods,
            reason = "the spawn lock is the Windows gate's; this test stands in for a run"
        )]
        let mut leader = Command::new("/bin/sh")
            .args(["-c", "sleep 600 >/dev/null 2>&1 & exit 0"])
            .process_group(0)
            .spawn()
            .expect("sh starts");
        let group = i32::try_from(leader.id()).expect("a pid is a pid_t");
        leader.wait().expect("the leader is reaped");
        let written = SystemTime::now();
        let judged = |session| record_current(Recorded { group, session }, written);
        // SAFETY: `getsid(0)` asks for the caller's session and touches no memory.
        let own_session = unsafe { libc::getsid(0) };
        let (own, other, two_line) = (judged(Some(own_session)), judged(Some(1)), judged(None));
        // SAFETY: the group is this case's sleep's, its id held while the sleep is in it; the kill only ends it.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
        assert_eq!(own, Record::Current, "a member is in the record's session");
        assert_eq!(other, Record::Stale, "no member is in the session the record names");
        assert_eq!(two_line, Record::Stale, "a two-line record names no session");
    }

    #[test]
    fn stop_without_a_record_or_with_another_job_s_is_silent() {
        let case = Case::new("silent");
        let (status, console) = stopped(&case);
        assert_eq!((status, console.out.len(), console.err.len()), (0, 0, 0), "no record");

        case.record(".pgid", "gate-remote-repo-0123abcd-ffffffffffff", &own_group().to_string());
        let (status, console) = stopped(&case);
        assert_eq!((status, console.out.len(), console.err.len()), (0, 0, 0), "another run's record");
    }

    #[test]
    fn stop_of_a_work_folder_outside_a_work_root_is_refused_and_signals_nothing() {
        let case = Case::new("outside");
        let work = format!("{}/{KEY}", case.base.to_str().expect("the temp folder is UTF-8"));
        let (status, console, alive) = stopped_beside_a_sleep(&work);
        assert_eq!(status, 2, "{:?} {:?}", console.out, console.err);
        assert_eq!(
            console.err,
            [format!("gate: box payload Work must be <root>/work/<key> to lock its root down: {work}")]
        );
        assert!(alive, "no signal is sent");
    }

    /// Runs the stop of the work folder `work`; its status and console.
    fn stopped_at(work: &str) -> (u8, Console) {
        let json = json!({ "Key": KEY, "Work": work, "Job": JOB });
        let payload = Payload::decode(&STANDARD.encode(json.to_string()), PathForm::Unix).expect("the payload decodes");
        let mut console = Console::captured();
        let status = stop(&payload, &mut console);
        (status, console)
    }

    /// Records a `sleep 600` group as the job of the work folder `work`, stops `work`, and ends the sleep: the stop's
    /// status and console, and whether the sleep was still alive once the stop returned.
    fn stopped_beside_a_sleep(work: &str) -> (u8, Console, bool) {
        let mut child = sleeping_group();
        fs::write(format!("{work}.pgid"), format!("{JOB}\n{}\n", child.id())).expect("the record is written");
        let (status, console) = stopped_at(work);
        let alive = child.try_wait().expect("the sleep is looked at").is_none();
        // Best effort: the sleep is this case's own, ended whether or not the stop signalled it.
        let _ = child.kill();
        let _ = child.wait();
        (status, console, alive)
    }

    #[test]
    fn stop_in_a_root_others_may_write_in_is_refused_and_signals_nothing() {
        let case = Case::new("open-root");
        let root = case.base.to_str().expect("the temp folder is UTF-8").to_owned();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).expect("the root is opened to everyone");

        let (status, console, alive) = stopped_beside_a_sleep(&case.work());
        assert_eq!(status, 1, "{:?}", console.out);
        let why = format!("{root} is not owned by {} or is writable by others", user_name(effective_uid()));
        assert_eq!(console.err, [root_unsafe_line(&why)]);
        assert!(alive, "no signal is sent");
    }

    #[test]
    fn stop_in_a_root_that_is_a_link_is_refused_and_signals_nothing() {
        let case = Case::new("linked-root");
        let root = format!("{}/link", case.base.to_str().expect("the temp folder is UTF-8"));
        symlink(&case.base, &root).expect("the link is made");

        let (status, console, alive) = stopped_beside_a_sleep(&format!("{root}/work/{KEY}"));
        assert_eq!(status, 1, "{:?}", console.out);
        assert_eq!(console.err, [root_unsafe_line(&format!("{root} is a symbolic link"))]);
        assert!(alive, "no signal is sent");
    }

    #[test]
    fn stop_without_a_root_or_its_work_folder_is_silent() {
        let case = Case::new("missing");
        let base = case.base.to_str().expect("the temp folder is UTF-8");
        let (status, console) = stopped_at(&format!("{base}/absent/work/{KEY}"));
        assert_eq!((status, console.out.len(), console.err.len()), (0, 0, 0), "no root");

        fs::create_dir(format!("{base}/bare-root")).expect("a root without work is made");
        let (status, console) = stopped_at(&format!("{base}/bare-root/work/{KEY}"));
        assert_eq!((status, console.out.len(), console.err.len()), (0, 0, 0), "no <root>/work");
    }

    #[test]
    fn stop_ends_a_run_s_git_step_that_is_still_running() {
        let case = Case::new("git-step");
        let root = case.base.to_str().expect("the temp folder is UTF-8").to_owned();
        let parent = case.base.join("work");
        let record = PathBuf::from(format!("{}.pgid", case.work()));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let lock = format!("{}.lock", case.work());
        let held = held_for_test(Path::new(&lock));
        let git = RunGit::new(
            Git::of(&root, &parent),
            &lock,
            &output,
            Some(GroupRecord::new(record.clone(), JOB, &held)),
        );
        // Stands in for a long checkout: a step that would run for a minute.
        let words = ["-c", "alias.x=!sleep 60", "x"];

        thread::scope(|scope| {
            let started = Instant::now();
            let step = scope.spawn(|| git.forwarded("checkout", &words, GitClass::Long));
            while !record.exists() && started.elapsed() < Duration::from_secs(60) {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(record.exists(), "the step's group is recorded while it runs");

            let (status, console) = stopped(&case);
            assert_eq!(status, 0, "{:?}", console.err);
            assert_eq!(console.out, [format!("gate: remote stopped job {JOB}")]);
            let ended = step.join().expect("the step's thread ends");
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the step was ended, not run to its sleep's end"
            );
            let failed = ended.map(|ran| ran.status.success());
            assert!(!matches!(failed, Ok(true)), "the ended step did not pass: {failed:?}");
        });
        assert!(!record.exists(), "the record is removed once the step ends");
    }

    #[test]
    fn stop_never_signals_init_or_its_own_group() {
        let case = Case::new("guard");
        for group in ["1", "0", "-5"] {
            case.record(".pgid", JOB, group);
            let (status, console) = stopped(&case);
            assert_eq!(status, 1, "{group}");
            assert_eq!(
                console.err,
                [format!(
                    "gate: remote could not stop job {JOB}: its recorded process group {group} is not a run's"
                )]
            );
        }
        let own = own_group();
        case.record(".pgid", JOB, &own.to_string());
        let (status, console) = stopped(&case);
        assert_eq!(status, 1);
        assert_eq!(
            console.err,
            [format!(
                "gate: remote could not stop job {JOB}: its recorded process group {own} is the box's own"
            )]
        );
    }
}
