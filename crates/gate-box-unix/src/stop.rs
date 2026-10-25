//! `gate-box box stop`: ends the process group of the run whose job the payload names, and the group of its tool
//! restore, `<job>-tools`. A run records each group beside its work folder, `<Work>.pgid` and `<Work>.tools.pgid`: the
//! job's name, the group, then the group's session in decimal, one per line. A group is signalled only while its
//! record names it for the payload's own job, so a stop that arrives late finds another run's record, or none, and
//! signals nothing; once a group ends, the stop follows the record to a later group of the same job.

use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use gate_wire::{Payload, ROOT_STATUS, root_unsafe_line};

use crate::console::Console;
use crate::files;
use crate::lines::{
    FAILED_STATUS, JobRecord, Recorded, USAGE_STATUS, WORK_ROOT_STEP, job_record, record_unread, record_without_group, root_flaw, signal_guard,
    work_layout,
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
        Err(error) => return Err((root_unsafe_line(&format!("cannot read {root}: {error}"), WORK_ROOT_STEP), ROOT_STATUS)),
        Ok(facts) => facts,
    };
    if let Some(why) = root_flaw(facts, root, &files::user_name(facts.euid)) {
        return Err((root_unsafe_line(&why, WORK_ROOT_STEP), ROOT_STATUS));
    }
    if !files::exists(parent) {
        return Ok(false);
    }
    files::unusable_folder(parent).map_or(Ok(true), |line| Err((line, FAILED_STATUS)))
}

/// The most process groups of one job a stop ends: once one has ended, the record may name the group of a later step
/// of the same run.
const STOP_GROUPS: usize = 3;

/// Stops the process groups the record `record` holds for the job `job` ([`groups_ended`]), and says how it went:
/// nothing for no record, another job's or a stale one, `gate: remote stopped job <job>` on stdout once the groups are
/// gone, else why on stderr. The status.
fn stopped(record: &Path, job: &str, console: &mut Console) -> u8 {
    stopped_through(&JobGroups { record, job }, job, console)
}

/// [`stopped`], reading and signalling the job `job`'s groups through `view`.
fn stopped_through(view: &impl StopView, job: &str, console: &mut Console) -> u8 {
    match groups_ended(view) {
        Ok(0) => 0,
        Ok(_) => {
            console.out(&format!("gate: remote stopped job {job}"));
            0
        }
        Err(why) => {
            console.err(&format!("gate: remote could not stop job {job}: {why}"));
            FAILED_STATUS
        }
    }
}

/// What a stop reads and signals of one job's groups: its record and the live groups ([`JobGroups`]), or a test's.
trait StopView {
    /// The group the record names for the job now ([`current_group`]).
    ///
    /// # Errors
    ///
    /// Why the stop cannot go on, as [`current_group`] gives it.
    fn current(&self) -> Result<Option<i32>, String>;

    /// Whether the record still names `group` for the job ([`still_named`]).
    ///
    /// # Errors
    ///
    /// Why the record could not be read again.
    fn still_named(&self, group: i32) -> Result<bool, String>;

    /// Sends `signal` to `group` ([`signalled`]): whether the group was there.
    ///
    /// # Errors
    ///
    /// Why the signal was refused.
    fn send(&self, group: i32, signal: i32) -> io::Result<bool>;

    /// Whether `group` is gone within `wait` ([`gone_within`]).
    fn gone_within(&self, group: i32, wait: Duration) -> bool;

    /// What is left of `group`, said when it could not be ended.
    fn left(&self, group: i32) -> String;
}

/// The job `job`'s record `record` and the live groups it names.
struct JobGroups<'a> {
    record: &'a Path,
    job: &'a str,
}

impl StopView for JobGroups<'_> {
    fn current(&self) -> Result<Option<i32>, String> {
        current_group(self.record, self.job)
    }

    fn still_named(&self, group: i32) -> Result<bool, String> {
        still_named(self.record, self.job, group)
    }

    fn send(&self, group: i32, signal: i32) -> io::Result<bool> {
        signalled(group, signal)
    }

    fn gone_within(&self, group: i32, wait: Duration) -> bool {
        gone_within(group, wait)
    }

    fn left(&self, group: i32) -> String {
        match group_pids(group) {
            Ok(left) => format!("{} process(es) left", left.len()),
            Err(why) => format!("its process group {group} is still there ({why})"),
        }
    }
}

/// Ends the group the record names for the job ([`ended`]), then, while the record names a later current group of the
/// job, that one too, up to [`STOP_GROUPS`] groups: how many the stop signalled, a group the record stopped naming
/// before its first signal not counted. Else why not: the record not read or malformed, a group no run makes, one not
/// ended, or a group still named after the last.
fn groups_ended(view: &impl StopView) -> Result<usize, String> {
    let (mut signalled, mut looked) = (0, 0);
    let mut last = None;
    while let Some(group) = view.current()? {
        if last == Some(group) {
            break;
        }
        if looked == STOP_GROUPS {
            return Err(format!(
                "its process group {group} is still running after {STOP_GROUPS} of its groups were stopped"
            ));
        }
        if ended(view, group)? {
            signalled += 1;
        }
        looked += 1;
        last = Some(group);
    }
    Ok(signalled)
}

/// The group the record `record` names for the job `job` now: `None` for no record, another job's, or a stale one
/// ([`record_current`]), which, holding no work lock, the stop leaves for the next run to remove. Else why the stop
/// cannot go on: the record not read or malformed, or a group no run makes ([`signal_guard`]).
fn current_group(record: &Path, job: &str) -> Result<Option<i32>, String> {
    let shown = record.display().to_string();
    let read = files::read_plain_dated(record, RECORD_LIMIT).map(|(bytes, written)| (job_record(&bytes, job), written));
    let (recorded, written) = match read {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Ok((JobRecord::Other, _)) => return Ok(None),
        Err(error) => return Err(record_unread(&shown, &error.to_string())),
        Ok((JobRecord::Malformed, _)) => return Err(record_without_group(&shown)),
        Ok((JobRecord::Group(recorded), written)) => (recorded, written),
    };
    signal_guard(recorded.group, own_group())?;
    Ok((record_current(recorded, written) == Record::Current).then_some(recorded.group))
}

/// Whether the record `record` still names `group` for the job `job`: not when it is gone or names another job or
/// group. Else why the stop cannot go on: the record not read, or malformed.
fn still_named(record: &Path, job: &str, group: i32) -> Result<bool, String> {
    let shown = record.display().to_string();
    match files::read_plain_dated(record, RECORD_LIMIT) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(record_unread(&shown, &error.to_string())),
        Ok((bytes, _)) => match job_record(&bytes, job) {
            JobRecord::Group(recorded) => Ok(recorded.group == group),
            JobRecord::Other => Ok(false),
            JobRecord::Malformed => Err(record_without_group(&shown)),
        },
    }
}

/// Ends the process group `group`, which the record `view` reads names for its job: `SIGTERM`, with `SIGCONT` right
/// after it so a stopped group takes it, a wait of [`TERM_WAIT`] for it to be gone, then `SIGKILL` and a wait of
/// [`KILL_WAIT`]. The record is read again before each signal, and a group it no longer names is not signalled: its run
/// has reaped it, so its id may be another's; a record that cannot be read again stops the stop, nothing signalled
/// after it. A group already gone at a signal is ended. A signal refused with `EPERM` to a group the record still names
/// counts as sent: only the run's unreaped leader is left, a zombie the box may not signal, and the group goes once the
/// run reaps it. Whether the stop signalled the group: not when the record stopped naming it before its `SIGTERM`. Else
/// why not: a signal refused otherwise, or the processes left.
fn ended(view: &impl StopView, group: i32) -> Result<bool, String> {
    for (signal, wait) in [(libc::SIGTERM, TERM_WAIT), (libc::SIGKILL, KILL_WAIT)] {
        if !view.still_named(group)? {
            return Ok(signal != libc::SIGTERM);
        }
        if !sent_unless_gone(view, group, signal)? {
            return Ok(true);
        }
        if signal == libc::SIGTERM {
            // The TERM's own result decides: a group gone since needs no SIGCONT.
            let _ = sent_unless_gone(view, group, libc::SIGCONT);
        }
        if view.gone_within(group, wait) {
            return Ok(true);
        }
    }
    Err(view.left(group))
}

/// Sends `signal` to `group`, which the record still names: whether the group was there, `EPERM` counting as there
/// (only its unreaped leader is left); else why the signal was refused.
fn sent_unless_gone(view: &impl StopView, group: i32, signal: i32) -> Result<bool, String> {
    match view.send(group, signal) {
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => Ok(true),
        sent => sent.map_err(|error| error.to_string()),
    }
}

/// Sends `signal` to the process group `group`: whether the group was there; else why the signal was refused.
fn signalled(group: i32, signal: i32) -> io::Result<bool> {
    // SAFETY: `killpg` takes two integers and touches no memory; `group` was read from the job's own record, which names
    // no group of 1 or below, and passed `signal_guard`, so it is neither init nor the box's own group.
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
    /// Its processes could not be listed, one's session could not be read, or every listed process was gone before its
    /// session was read on each listing while `killpg(group, 0)` still found the group ([`members_of`]).
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

/// What [`members_of`] looks at of one process group: the box's view of a live group ([`LiveGroup`]), or a test's.
trait GroupView {
    /// Lists the group's processes into `pids`.
    ///
    /// # Errors
    ///
    /// Why the group could not be listed.
    fn listed(&self, pids: &mut Vec<i32>) -> Result<(), String>;

    /// The session of the process `pid` (`getsid`), else the `errno` it failed with.
    ///
    /// # Errors
    ///
    /// The `errno` of the failed read, `None` when it gave none.
    fn session_of(&self, pid: i32) -> Result<i32, Option<i32>>;

    /// Whether the group has no process left: `killpg(group, 0)` fails with `ESRCH`.
    fn emptied(&self) -> bool;
}

/// The box's view of the live process group with this id.
struct LiveGroup(i32);

impl GroupView for LiveGroup {
    fn listed(&self, pids: &mut Vec<i32>) -> Result<(), String> {
        group_pids_into(self.0, pids)
    }

    fn session_of(&self, pid: i32) -> Result<i32, Option<i32>> {
        // SAFETY: `getsid` takes a pid and touches no memory.
        let sid = unsafe { libc::getsid(pid) };
        if sid < 0 {
            Err(io::Error::last_os_error().raw_os_error())
        } else {
            Ok(sid)
        }
    }

    fn emptied(&self) -> bool {
        // SAFETY: signal 0 only asks whether the group exists; it touches no memory.
        let sent = unsafe { libc::killpg(self.0, 0) };
        sent != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}

/// The processes of the process group `group`, and whether one is in the session `session` ([`members_of`]).
fn members(group: i32, session: Option<i32>) -> Members {
    members_of(&LiveGroup(group), session)
}

/// How many times a group is listed while every process listed is gone before its session is read.
const MEMBER_PASSES: usize = 3;
/// The pause between those listings.
const MEMBER_PAUSE: Duration = Duration::from_millis(50);

/// The processes of the group `view` shows, and whether one is in the session `session` (`getsid`); none is in no
/// session. A process gone before its session is read is not counted; when every listed one is, the group may have
/// gained a process it forked before it went, so unless `killpg(group, 0)` finds the group empty it is listed again,
/// up to [`MEMBER_PASSES`] times [`MEMBER_PAUSE`] apart, and a group still not judged then is [`Members::Unknown`].
fn members_of(view: &impl GroupView, session: Option<i32>) -> Members {
    let mut pids = Vec::new();
    for pass in 1..=MEMBER_PASSES {
        if view.listed(&mut pids).is_err() {
            return Members::Unknown;
        }
        if pids.is_empty() {
            return Members::Empty;
        }
        let Some(session) = session else {
            return Members::Listed { in_session: false };
        };
        if let Seen::Judged(members) = sessions_seen(view, &pids, session) {
            return members;
        }
        if view.emptied() {
            return Members::Empty;
        }
        if pass < MEMBER_PASSES {
            thread::sleep(MEMBER_PAUSE);
        }
    }
    Members::Unknown
}

/// What the sessions of a group's listed processes show.
enum Seen {
    /// They decide the group's members: one is in the record's session, one's could not be read, or one is in another.
    Judged(Members),
    /// Every listed process was gone before its session was read.
    AllGone,
}

/// Reads the session of each of `pids`, of the group `view` shows, against the record's `session`.
fn sessions_seen(view: &impl GroupView, pids: &[i32], session: i32) -> Seen {
    let (mut unread, mut read) = (false, false);
    for pid in pids {
        match view.session_of(*pid) {
            Ok(sid) if sid == session => return Seen::Judged(Members::Listed { in_session: true }),
            Ok(_) => read = true,
            Err(Some(libc::ESRCH)) => {}
            Err(_) => unread = true,
        }
    }
    if unread {
        Seen::Judged(Members::Unknown)
    } else if read {
        Seen::Judged(Members::Listed { in_session: false })
    } else {
        Seen::AllGone
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
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::io;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_wire::{PathForm, Payload, root_unsafe_line};
    use serde_json::json;

    use super::{
        GroupFacts, GroupView, LeaderFacts, Members, Record, StopView, groups_ended, members_of, own_group, record_current, record_decision,
        still_named, stop, stopped_through,
    };
    use crate::console::{Console, Output, RecordingSink, Stamp};
    use crate::files::{effective_uid, held_for_test, user_name};
    use crate::git::{Git, GitClass, RunGit};
    use crate::lines::{Recorded, WORK_ROOT_STEP, record_unread, record_without_group};
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

    /// Waits for the file `path` to be made, within a minute: a hang guard only.
    fn made(path: &Path) -> bool {
        let guard = Instant::now() + Duration::from_secs(60);
        while !path.exists() && Instant::now() < guard {
            thread::sleep(Duration::from_millis(10));
        }
        path.exists()
    }

    /// A perl leading a process group of its own that makes `ready` once it handles `SIGTERM`, and on each TERM makes
    /// `termed` and runs the perl `on_term`: with none, a TERM never ends it.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this test stands in for a run's step"
    )]
    fn term_handling_group(on_term: &str, ready: &Path, termed: &Path) -> Child {
        let script = format!("$SIG{{TERM}} = sub {{ open(my $f, '>', $ARGV[1]); {on_term} }}; open(my $r, '>', $ARGV[0]); close $r; sleep 1 while 1");
        let child = Command::new("perl")
            .args(["-e", &script])
            .arg(ready)
            .arg(termed)
            .process_group(0)
            .spawn()
            .expect("perl starts");
        assert!(made(ready), "perl handles SIGTERM");
        child
    }

    #[test]
    fn the_record_read_again_names_the_group_only_for_this_job_and_this_group() {
        let case = Case::new("still-named");
        let record = PathBuf::from(format!("{}.pgid", case.work()));
        let shown = record.display().to_string();
        let written = |text: String| fs::write(&record, text).expect("the record is written");
        assert_eq!(still_named(&record, JOB, 4242), Ok(false), "no record");

        written("gate-remote-repo-0123abcd-ffffffffffff\n4242\n77\n".to_owned());
        assert_eq!(still_named(&record, JOB, 4242), Ok(false), "another job's record");

        written(format!("{JOB}\n4243\n77\n"));
        assert_eq!(still_named(&record, JOB, 4242), Ok(false), "this job, another group");

        written(format!("{JOB}\n1\n77\n"));
        assert_eq!(still_named(&record, JOB, 4242), Err(record_without_group(&shown)), "a malformed record");

        written(format!("{JOB}\n4242\n77\n"));
        assert_eq!(still_named(&record, JOB, 4242), Ok(true), "this job, this group");
    }

    #[test]
    fn stop_follows_the_record_to_a_later_group_of_its_job_and_never_kills_the_group_whose_record_went() {
        // Group 5 outlives its TERM; before its KILL the record names the job's later group 6, which its TERM ends.
        let moved = FakeStop::new(&[Some(5), Some(6), None], &[true, false, true], &[], &[false, true]);
        let mut console = Console::captured();
        assert_eq!(stopped_through(&moved, JOB, &mut console), 0, "{:?}", console.err);
        assert_eq!(console.out, [format!("gate: remote stopped job {JOB}")]);
        let sent = moved.sent.borrow();
        assert!(
            !sent.contains(&(5, libc::SIGKILL)),
            "the group whose record went is never sent SIGKILL: {sent:?}"
        );
        assert_eq!(
            *sent,
            [(5, libc::SIGTERM), (5, libc::SIGCONT), (6, libc::SIGTERM), (6, libc::SIGCONT)],
            "the job's later group was ended by the stop"
        );
    }

    #[test]
    fn stop_never_signals_a_group_another_job_s_record_names() {
        // Group 5 outlives its TERM; before its KILL the record names another job's group, which the stop never reads
        // as its own.
        let moved = FakeStop::new(&[Some(5), None], &[true, false], &[], &[false]);
        let mut console = Console::captured();
        assert_eq!(stopped_through(&moved, JOB, &mut console), 0, "{:?}", console.err);
        let sent = moved.sent.borrow();
        assert!(
            !sent.contains(&(5, libc::SIGKILL)),
            "the group whose record went is never sent SIGKILL: {sent:?}"
        );
        assert!(
            sent.iter().all(|(group, _)| *group == 5),
            "another job's group is never signalled: {sent:?}"
        );
    }

    #[test]
    fn a_record_that_cannot_be_read_again_fails_the_stop_and_nothing_more_is_signalled() {
        let line = format!("gate: remote could not stop job {JOB}: {}", record_unread(FAKE_RECORD, FAKE_READ_ERROR));
        let before_term = FakeStop::new(&[Some(5)], &[true], &[], &[true]).unreadable_after(0);
        let mut console = Console::captured();
        assert_eq!(stopped_through(&before_term, JOB, &mut console), 1);
        assert_eq!(console.err, std::slice::from_ref(&line));
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert!(before_term.sent.borrow().is_empty(), "{:?}", before_term.sent.borrow());

        let before_kill = FakeStop::new(&[Some(5)], &[true], &[], &[false]).unreadable_after(1);
        let mut console = Console::captured();
        assert_eq!(stopped_through(&before_kill, JOB, &mut console), 1);
        assert_eq!(console.err, [line]);
        assert_eq!(
            *before_kill.sent.borrow(),
            [(5, libc::SIGTERM), (5, libc::SIGCONT)],
            "no SIGKILL after the failed read"
        );
    }

    #[test]
    fn stop_continues_a_stopped_group_so_its_sigterm_ends_it() {
        let case = Case::new("stopped-group");
        // A stopped process that handles SIGTERM holds it pending until it is continued.
        let child = term_handling_group("exit 7", &case.base.join("ready"), &case.base.join("termed"));
        let pid = i32::try_from(child.id()).expect("a pid is a pid_t");
        let mut waited = 0;
        // SAFETY: the perl is this case's own child; `waited` lives across the `waitpid`, which writes only into it.
        let stopped_child = unsafe {
            libc::kill(pid, libc::SIGSTOP);
            libc::waitpid(pid, &raw mut waited, libc::WUNTRACED)
        };
        assert_eq!(stopped_child, pid, "the perl is stopped");
        case.record(".pgid", JOB, &child.id().to_string());
        let reaper = thread::spawn(move || {
            let mut child = child;
            child.wait()
        });

        let (status, console) = stopped(&case);
        let ended = reaper.join().expect("the reaper ends").expect("the perl is reaped");
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(ended.code(), Some(7), "ended by its TERM's handler, not by the KILL: {ended:?}");
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

    /// A group whose listings answer `lists` in turn, then the last again, each process's session read from
    /// `sessions`, and `killpg(group, 0)` finding a process unless `emptied`.
    struct FakeView {
        lists: RefCell<Vec<Vec<i32>>>,
        sessions: Vec<(i32, Result<i32, Option<i32>>)>,
        emptied: bool,
    }

    impl FakeView {
        fn new(lists: &[&[i32]], sessions: &[(i32, Result<i32, Option<i32>>)], emptied: bool) -> FakeView {
            FakeView {
                lists: RefCell::new(lists.iter().rev().map(|list| list.to_vec()).collect()),
                sessions: sessions.to_vec(),
                emptied,
            }
        }
    }

    impl GroupView for FakeView {
        fn listed(&self, pids: &mut Vec<i32>) -> Result<(), String> {
            let mut lists = self.lists.borrow_mut();
            let list = if lists.len() > 1 { lists.pop() } else { lists.last().cloned() };
            *pids = list.unwrap_or_default();
            Ok(())
        }

        fn session_of(&self, pid: i32) -> Result<i32, Option<i32>> {
            self.sessions
                .iter()
                .find(|(listed, _)| *listed == pid)
                .map_or(Err(Some(libc::ESRCH)), |(_, session)| *session)
        }

        fn emptied(&self) -> bool {
            self.emptied
        }
    }

    /// The next of `answers`, given in turn, then the last of them again.
    fn next_of<T: Copy>(answers: &RefCell<Vec<T>>) -> Option<T> {
        let mut answers = answers.borrow_mut();
        if answers.len() > 1 { answers.pop() } else { answers.last().copied() }
    }

    /// The record a [`FakeStop`] fails to read, and why.
    const FAKE_RECORD: &str = "/w/k.pgid";
    const FAKE_READ_ERROR: &str = "Permission denied (os error 13)";

    /// A job whose record names `named` in turn, still names the group being ended as `still` says in turn (each read
    /// after the first [`FakeStop::unreadable_after`] fails), whose signals fail with the `errno` paired with them in
    /// `refused` and are sent otherwise, and whose groups are gone within a wait as `gone` says in turn; it notes each
    /// signal sent.
    struct FakeStop {
        named: RefCell<Vec<Option<i32>>>,
        still: RefCell<Vec<bool>>,
        reads: Cell<usize>,
        readable: usize,
        refused: Vec<(i32, i32)>,
        gone: RefCell<Vec<bool>>,
        sent: RefCell<Vec<(i32, i32)>>,
    }

    impl FakeStop {
        fn new(named: &[Option<i32>], still: &[bool], refused: &[(i32, i32)], gone: &[bool]) -> FakeStop {
            FakeStop {
                named: RefCell::new(named.iter().rev().copied().collect()),
                still: RefCell::new(still.iter().rev().copied().collect()),
                reads: Cell::new(0),
                readable: usize::MAX,
                refused: refused.to_vec(),
                gone: RefCell::new(gone.iter().rev().copied().collect()),
                sent: RefCell::new(Vec::new()),
            }
        }

        /// The job whose record reads again only `readable` times before a stop's signals; every later read fails.
        fn unreadable_after(self, readable: usize) -> FakeStop {
            FakeStop { readable, ..self }
        }
    }

    impl StopView for FakeStop {
        fn current(&self) -> Result<Option<i32>, String> {
            Ok(next_of(&self.named).flatten())
        }

        fn still_named(&self, _group: i32) -> Result<bool, String> {
            let reads = self.reads.get() + 1;
            self.reads.set(reads);
            if reads > self.readable {
                return Err(record_unread(FAKE_RECORD, FAKE_READ_ERROR));
            }
            Ok(next_of(&self.still).unwrap_or(false))
        }

        fn send(&self, group: i32, signal: i32) -> io::Result<bool> {
            self.sent.borrow_mut().push((group, signal));
            match self.refused.iter().find(|(refused, _)| *refused == signal) {
                Some((_, errno)) => Err(io::Error::from_raw_os_error(*errno)),
                None => Ok(true),
            }
        }

        fn gone_within(&self, _group: i32, _wait: Duration) -> bool {
            next_of(&self.gone).unwrap_or(false)
        }

        fn left(&self, group: i32) -> String {
            format!("group {group} left")
        }
    }

    #[test]
    fn a_group_left_with_only_its_unreaped_leader_refuses_signals_and_is_still_stopped() {
        let (term, cont, kill) = (libc::SIGTERM, libc::SIGCONT, libc::SIGKILL);
        let refused_term = FakeStop::new(&[Some(5), None], &[true], &[(term, libc::EPERM), (cont, libc::EPERM)], &[true]);
        assert_eq!(groups_ended(&refused_term), Ok(1), "EPERM: only the zombie leader is left");
        assert_eq!(*refused_term.sent.borrow(), [(5, term), (5, cont)]);

        let refused_kill = FakeStop::new(&[Some(5), None], &[true], &[(kill, libc::EPERM)], &[false, true]);
        assert_eq!(groups_ended(&refused_kill), Ok(1), "the KILL found only the zombie leader");
        assert_eq!(*refused_kill.sent.borrow(), [(5, term), (5, cont), (5, kill)]);

        let refused_otherwise = FakeStop::new(&[Some(5), None], &[true], &[(term, libc::EINVAL)], &[true]);
        assert!(groups_ended(&refused_otherwise).is_err(), "another refusal still fails the stop");
    }

    #[test]
    fn a_group_the_record_stops_naming_before_its_first_signal_is_not_counted() {
        let moved_away = FakeStop::new(&[Some(5), None], &[false], &[], &[true]);
        assert_eq!(groups_ended(&moved_away), Ok(0), "nothing was signalled, so nothing was stopped");
        assert!(moved_away.sent.borrow().is_empty());

        let moved_on = FakeStop::new(&[Some(5), Some(6), None], &[false, true], &[], &[true]);
        assert_eq!(groups_ended(&moved_on), Ok(1), "only the later group was signalled");
        assert_eq!(*moved_on.sent.borrow(), [(6, libc::SIGTERM), (6, libc::SIGCONT)]);
    }

    #[test]
    fn a_leaderless_group_whose_listed_members_all_went_is_listed_again_before_it_is_judged() {
        let gone = Err(Some(libc::ESRCH));
        let forked = FakeView::new(&[&[10], &[11]], &[(10, gone), (11, Ok(77))], false);
        assert_eq!(
            members_of(&forked, Some(77)),
            Members::Listed { in_session: true },
            "the member forked before it exited is found on the second listing"
        );

        let still_there = FakeView::new(&[&[10]], &[(10, gone)], false);
        let unknown = members_of(&still_there, Some(77));
        assert_eq!(unknown, Members::Unknown, "a group never seen empty is not judged gone");
        let written = SystemTime::UNIX_EPOCH + Duration::from_secs(5000);
        assert_eq!(
            record_decision(written, Some(77), &facts(None, LeaderFacts::Absent, unknown)),
            Record::Current
        );

        let emptied = FakeView::new(&[&[10]], &[(10, gone)], true);
        assert_eq!(members_of(&emptied, Some(77)), Members::Empty, "killpg finds no process left");

        let elsewhere = FakeView::new(&[&[10, 12]], &[(10, gone), (12, Ok(5))], false);
        assert_eq!(
            members_of(&elsewhere, Some(77)),
            Members::Listed { in_session: false },
            "a live member of another session: the id is another's"
        );
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
        assert_eq!(console.err, [root_unsafe_line(&why, WORK_ROOT_STEP)]);
        assert!(alive, "no signal is sent");
    }

    #[test]
    fn stop_in_a_root_that_is_a_link_is_refused_and_signals_nothing() {
        let case = Case::new("linked-root");
        let root = format!("{}/link", case.base.to_str().expect("the temp folder is UTF-8"));
        symlink(&case.base, &root).expect("the link is made");

        let (status, console, alive) = stopped_beside_a_sleep(&format!("{root}/work/{KEY}"));
        assert_eq!(status, 1, "{:?}", console.out);
        assert_eq!(console.err, [root_unsafe_line(&format!("{root} is a symbolic link"), WORK_ROOT_STEP)]);
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
            Stamp::Bare,
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
        let record = format!("{}.pgid", case.work());
        for group in ["1", "0", "-5"] {
            case.record(".pgid", JOB, group);
            let (status, console) = stopped(&case);
            assert_eq!(status, 1, "{group}");
            assert_eq!(
                console.err,
                [format!("gate: remote could not stop job {JOB}: {}", record_without_group(&record))],
                "no run records the group {group}"
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
