//! `gate-box box stop`: ends the process group of the run whose job the payload names, and the group of its tool
//! restore, `<job>-tools`. A run records each group beside its work folder, `<Work>.pgid` and `<Work>.tools.pgid`: the
//! job's name, then the group in decimal. A group is signalled only when its record names the payload's own job, so a
//! stop that arrives late finds another run's record, or none, and signals nothing.

use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use gate_wire::Payload;

use crate::console::Console;
use crate::files;
use crate::lines::{
    FAILED_STATUS, JobRecord, ROOT_STATUS, USAGE_STATUS, job_record, record_unread, record_without_group, root_flaw, root_unsafe_line, signal_guard,
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
fn records_folder(root: &str, parent: &Path) -> Result<bool, (String, u8)> {
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
    let group = match files::read_plain(record, RECORD_LIMIT).map(|bytes| job_record(&bytes, job)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return 0,
        Ok(JobRecord::Other) => return 0,
        Err(error) => {
            console.err(&could_not(&record_unread(&record.display().to_string(), &error.to_string())));
            return FAILED_STATUS;
        }
        Ok(JobRecord::Malformed) => {
            console.err(&could_not(&record_without_group(&record.display().to_string())));
            return FAILED_STATUS;
        }
        Ok(JobRecord::Group(group)) => group,
    };
    if let Err(why) = signal_guard(group, own_group()) {
        console.err(&could_not(&why));
        return FAILED_STATUS;
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
    Err(match group_size(group) {
        Ok(left) => format!("{left} process(es) left"),
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
fn own_group() -> i32 {
    // SAFETY: `getpgrp` takes nothing, touches no memory and always succeeds.
    unsafe { libc::getpgrp() }
}

/// The `proc_listpids` kind that lists one process group's processes, `PROC_PGRP_ONLY` of macOS's `<sys/proc_info.h>`,
/// which the `libc` crate does not define.
#[cfg(target_os = "macos")]
const PROC_PGRP_ONLY: u32 = 2;

/// How many processes the process group `group` holds, from `proc_listpids`.
#[cfg(target_os = "macos")]
fn group_size(group: i32) -> Result<usize, String> {
    let group = u32::try_from(group).map_err(|error| format!("the group {group} is not a process id: {error}"))?;
    let mut pids = vec![0_i32; 4096];
    let bytes = i32::try_from(pids.len() * std::mem::size_of::<i32>()).map_err(|error| error.to_string())?;
    // SAFETY: the buffer is `bytes` bytes of `pid_t`, which `proc_listpids` fills no further than that size.
    let filled = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), bytes) };
    let filled = usize::try_from(filled).map_err(|_| format!("proc_listpids failed: {}", io::Error::last_os_error()))?;
    pids.truncate(filled / std::mem::size_of::<i32>());
    Ok(pids.iter().filter(|pid| **pid != 0).count())
}

/// A process group's processes are counted only on macOS.
#[cfg(not(target_os = "macos"))]
fn group_size(_group: i32) -> Result<usize, String> {
    Err(format!("counting a process group is not built for {}", std::env::consts::OS))
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
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::thread;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_wire::{PathForm, Payload};
    use serde_json::json;

    use super::{own_group, stop};
    use crate::console::Console;
    use crate::files::{effective_uid, user_name};
    use crate::lines::root_unsafe_line;

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
