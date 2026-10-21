//! `gate-box box run`'s command: the payload's words started directly, with no shell, in the run's place, found along
//! the login shell's `PATH`, as the leader of a process group of its own that the bounded runner watches
//! ([`run_watched`]), both its streams forwarded line by line. The start marker is said first; the interrupted-command
//! record `<Work>.command` is written just before the start and removed only once the command ended clean; the status
//! the run ends on is the command's own, or the box's ([`command_status`]).

#[cfg(target_os = "macos")]
use std::ffi::c_int;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate_wire::{escaped, started_line};

use crate::console::{Forwarding, LineSink, Output, Stream};
use crate::files::{self, HeldLock};
use crate::lines::{CommandEnd, FAILED_STATUS, command_environment, command_status, round_trip, write_failure};
use crate::process::{EndedBy, Ending, Forward, GroupRecord, NotRan, Ran, Watch, run_watched};
use crate::run::group_record;

/// How long the command's group has, once asked to end with `SIGTERM`, before `SIGKILL`: the git steps' grace.
const END_GRACE: Duration = Duration::from_secs(2);

/// What the command of a run is started with.
pub struct Launch<'a> {
    /// The payload's words: the program, found along the command's `PATH`, then its arguments.
    pub words: &'a [String],
    /// The folder the command runs in.
    pub place: &'a Path,
    /// The work folder, `<root>/work/<key>`, beside which the run's records are.
    pub work: &'a str,
    /// The run's job name, written to the records and given to the command.
    pub job: &'a str,
    /// The run's nonce, which the start marker and the build-server run id name.
    pub nonce: &'a str,
    /// The payload's `Env` pairs.
    pub env: &'a [(String, String)],
    /// The login shell's `PATH`, `None` to keep the box's own.
    pub login_path: Option<&'a str>,
    /// The work lock, held: no record is written or removed once it is lost.
    pub lock: &'a HeldLock,
}

/// The interrupted-command record of the work folder `work`: `<work>.command`.
#[must_use]
pub fn command_record(work: &str) -> String {
    format!("{work}.command")
}

/// Runs the run's command as `launch` says, saying its lines through `output`, and gives the status the run ends on.
///
/// The start marker is said first; the client is probed, the interrupted-command record written
/// (`{UTC start} {job}`, [`files::replace_plain`]), and the client probed again, so a client gone at either probe
/// ends the run with [`crate::lines::GONE_STATUS`] and nothing started; then the command runs ([`run_watched`]) with standard input
/// null, its group written to `<work>.pgid` from the child, ended `SIGTERM` first on a gone client. The record is kept
/// unless the command exited of itself, the client still there and no leftover holding its output; it is removed for a
/// command that never started. Every removal looks first that the work lock is still held.
pub fn run_command<S: LineSink + 'static>(launch: &Launch<'_>, output: &Arc<Output<S>>) -> u8 {
    let Some((program, rest)) = launch.words.split_first() else {
        let _ = output.say(Stream::Err, &failed_line("the run has no command"));
        return FAILED_STATUS;
    };
    // A client that has gone has no one left to tell; the probe below ends the run.
    let _ = output.say(Stream::Out, &started_line(launch.nonce));
    if output.client_gone_probe() {
        return command_status(CommandEnd::Gone);
    }
    let record = command_record(launch.work);
    if let Err(failure) = files::replace_plain(Path::new(&record), record_text(launch.job).as_bytes()) {
        // The plain write failure, for an unsynced record too: it is removed below.
        let (files::ReplaceFailure::NotWritten(error) | files::ReplaceFailure::Unsynced(error)) = &failure;
        let _ = output.say(Stream::Err, &write_failure(&record, &error.to_string()));
        // A record put in place but not synced is still at the name: the command never ran.
        removed_if_held(&record, launch.lock, output);
        return FAILED_STATUS;
    }
    if output.client_gone_probe() {
        removed_if_held(&record, launch.lock, output);
        return command_status(CommandEnd::Gone);
    }
    let group = GroupRecord::new(PathBuf::from(group_record(launch.work)), launch.job, launch.lock);
    let forwarding: Arc<dyn Forwarding> = Arc::clone(output) as Arc<dyn Forwarding>;
    let watch = Watch {
        cap: 0,
        deadline: None,
        ending: Ending::TermThenKill(END_GRACE),
        output: Some(forwarding),
        forward: Forward::Both,
        ends_on_gone: true,
        record: Some(&group),
        check: None,
        on_leader_exit: None,
    };
    let ran = run_watched(command_of(launch, program, rest), &watch);
    ended(launch, program, &record, ran, output)
}

/// The status of the run whose command `program` ran as `ran`, its record `record` removed when the command ended clean
/// or never started, and the line of a start that failed or of the box's own failure said.
fn ended<S: LineSink>(launch: &Launch<'_>, program: &str, record: &str, ran: Result<Ran, NotRan>, output: &Output<S>) -> u8 {
    let ran = match ran {
        Ok(ran) => ran,
        Err(NotRan::Start(error)) => {
            removed_if_held(record, launch.lock, output);
            let line = format!("gate: remote could not start {}: {}", escaped(program), escaped(&error.to_string()));
            let _ = output.say(Stream::Err, &line);
            return command_status(CommandEnd::NotStarted);
        }
        Err(not) => {
            // The records stay: the group may still be there, or the lock is lost.
            if output.client_gone() {
                return command_status(CommandEnd::Gone);
            }
            let _ = output.say(Stream::Err, &failed_line(&unran(not)));
            return command_status(CommandEnd::Failed);
        }
    };
    let end = end_of(&ran, output.client_gone());
    if matches!(end, CommandEnd::Exited(_)) && ran.ended_by.is_none() && !ran.leftover {
        removed_if_held(record, launch.lock, output);
    }
    command_status(end)
}

/// How the command that ran as `ran` ended, its client gone when `gone`.
fn end_of(ran: &Ran, gone: bool) -> CommandEnd {
    if gone || ran.ended_by == Some(EndedBy::Gone) {
        return CommandEnd::Gone;
    }
    match (ran.status.code(), ran.status.signal()) {
        (Some(code), _) => CommandEnd::Exited(code),
        (None, Some(signal)) => CommandEnd::Signalled(signal),
        (None, None) => CommandEnd::Failed,
    }
}

/// Why the runner gave no exit for the command, `not`.
fn unran(not: NotRan) -> String {
    match not {
        NotRan::Failed(why) => why,
        NotRan::Start(error) => error.to_string(),
        NotRan::TimedOut { group } => format!("its process group {group} ran past its deadline"),
        NotRan::OverCap => "it printed more than the box keeps".to_owned(),
    }
}

/// The line of the box's own failure while it ran the command, as the Windows box says it.
fn failed_line(problem: &str) -> String {
    format!("gate: the gate failed while running the command: {problem}")
}

/// The interrupted-command record's text for a command of the job `job` starting now: `{UTC start} {job}`, the start in
/// .NET's round-trip format.
fn record_text(job: &str) -> String {
    // A clock before 1970 writes 1970: a later run reads any start as the command's own, as the clear only needs the job.
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
    format!("{} {job}", round_trip(seconds, i64::from(since.subsec_nanos() / 100)))
}

/// Removes the record `record` and syncs its folder while the work lock `lock` is still held, so a record another run
/// put at the name once this one's lock was lost is left; a removal that fails is said and changes nothing else. The
/// lock is looked at just before the unlink, not with it: a lock lost in the gap between the two is accepted.
fn removed_if_held<S: LineSink>(record: &str, lock: &HeldLock, output: &Output<S>) {
    if !matches!(lock.still_held(), Ok(true)) {
        return;
    }
    let path = Path::new(record);
    if let Err(error) = files::remove_if_there(path).and_then(|()| files::folder_synced(path)) {
        let line = format!("gate: remote could not remove {}: {}", escaped(record), escaped(&error.to_string()));
        // Said; the run's status stands.
        let _ = output.say(Stream::Err, &line);
    }
}

/// The command `program` with the arguments `rest`, run in `launch`'s place with the environment
/// [`command_environment`] composes, the build servers named by the run id `gate-box-<nonce>`.
fn command_of(launch: &Launch<'_>, program: &str, rest: &[String]) -> Command {
    let run = format!("gate-box-{}", launch.nonce);
    let environment = command_environment(std::env::vars_os().collect(), launch.login_path, launch.env, &run, launch.job);
    let mut command = Command::new(program);
    command.args(rest).current_dir(launch.place).env_clear().envs(environment);
    // SAFETY: the closure runs in the child between fork and exec, and calls only `sigaction`, `sigemptyset` and
    // `sigprocmask`, which are async-signal-safe, on values on its own stack; it allocates nothing and takes no lock.
    unsafe {
        command.pre_exec(signals_reset);
    }
    command
}

/// Puts `SIGTERM`, `SIGINT`, `SIGHUP` and `SIGQUIT` back to their default action and clears the signal mask, in the
/// child before its exec: an ignored signal stays ignored across `exec`, so a command whose ssh session ignored
/// `SIGTERM` would wait out every ending's grace; std resets only `SIGPIPE` and keeps the parent's mask.
fn signals_reset() -> io::Result<()> {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: an all-zero `sigaction` is a valid value: no flags and an empty mask; its handler is set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = libc::SIG_DFL;
        // SAFETY: `action` lives across the call, which only reads it; no old action is asked for.
        if unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: an all-zero `sigset_t` is a valid value, emptied by `sigemptyset` below.
    let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `empty` lives across both calls; `sigemptyset` writes only into it and `sigprocmask` only reads it.
    let cleared = unsafe {
        libc::sigemptyset(&raw mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &raw const empty, std::ptr::null_mut())
    };
    if cleared != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The box's `sysctl.proc_translated`: 1 under Rosetta, 0 native; no such name on an Intel Mac.
///
/// # Errors
///
/// Why it could not be read.
#[cfg(target_os = "macos")]
pub fn proc_translated() -> io::Result<i32> {
    let mut value: c_int = 0;
    let mut size = std::mem::size_of::<c_int>();
    // SAFETY: the name is a NUL-terminated string, and `value` and `size` live across the call, which fills `value` no
    // further than `size` bytes; no new value is given.
    let read = unsafe {
        libc::sysctlbyname(
            c"sysctl.proc_translated".as_ptr(),
            (&raw mut value).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read == 0 { Ok(value) } else { Err(io::Error::last_os_error()) }
}

/// Rosetta is macOS's alone: elsewhere the name is missing, which reads as native.
///
/// # Errors
///
/// Always [`io::ErrorKind::NotFound`].
#[cfg(not(target_os = "macos"))]
pub fn proc_translated() -> io::Result<i32> {
    Err(io::Error::from(io::ErrorKind::NotFound))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose folder or lock cannot be made must fail loudly")]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use gate_wire::started_line;

    use super::{Launch, command_record, removed_if_held, run_command};
    use crate::console::{HungUpSink, LineSink, Output, RecordingSink, Stream};
    use crate::files::{HeldLock, held_for_test};

    const NONCE: &str = "0123456789ab";
    const JOB: &str = "gate-remote-k-0123456789ab";

    /// A work folder `k` in a temp folder private to this process, its lock held, removed when the case ends.
    struct Case {
        base: PathBuf,
        place: PathBuf,
        work: String,
        lock_path: PathBuf,
        lock: HeldLock,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-command-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            let place = base.join("k");
            fs::create_dir_all(&place).expect("the work folder is made");
            let lock_path = base.join("k.lock");
            let lock = held_for_test(&lock_path);
            Case {
                work: place.display().to_string(),
                base,
                place,
                lock_path,
                lock,
            }
        }

        fn record(&self) -> PathBuf {
            PathBuf::from(command_record(&self.work))
        }

        fn run<S: LineSink + 'static>(&self, words: &[&str], env: &[(String, String)], login_path: Option<&str>, output: &Arc<Output<S>>) -> u8 {
            let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
            let launch = Launch {
                words: &words,
                place: &self.place,
                work: &self.work,
                job: JOB,
                nonce: NONCE,
                env,
                login_path,
                lock: &self.lock,
            };
            run_command(&launch, output)
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn recording() -> Arc<Output<RecordingSink>> {
        Arc::new(Output::new(RecordingSink::default()))
    }

    /// The lines `output` said on `stream`.
    fn said(output: &Output<RecordingSink>, stream: Stream) -> Vec<String> {
        output
            .sink()
            .lines()
            .into_iter()
            .filter(|(said, _)| *said == stream)
            .map(|(_, line)| line)
            .collect()
    }

    #[test]
    fn a_command_that_exits_3_gives_3_after_the_start_marker_and_leaves_no_record() {
        let case = Case::new("exit-3");
        let output = recording();
        let status = case.run(&["/bin/sh", "-c", "echo hi; echo there >&2; exit 3"], &[], None, &output);
        assert_eq!(status, 3);
        assert_eq!(said(&output, Stream::Out), [started_line(NONCE), "hi".to_owned()]);
        assert_eq!(said(&output, Stream::Err), ["there"]);
        assert!(!case.record().exists(), "a clean end removes the record");
        assert!(!PathBuf::from(format!("{}.pgid", case.work)).exists(), "the group record is removed");
    }

    #[test]
    fn a_command_killed_by_sigkill_gives_137_and_keeps_its_record() {
        let case = Case::new("sigkill");
        let output = recording();
        let status = case.run(&["/bin/sh", "-c", "kill -KILL $$"], &[], None, &output);
        assert_eq!(status, 137);
        let text = fs::read_to_string(case.record()).expect("the record is kept");
        let (start, job) = text.split_once(' ').expect("the record holds a start and a job");
        assert_eq!(job, JOB);
        assert_eq!(start.len(), "2026-10-21T12:34:56.1234567Z".len(), "{start}");
        assert!(start.ends_with('Z') && start.as_bytes().get(10) == Some(&b'T'), "{start}");
    }

    #[test]
    fn a_program_that_does_not_exist_gives_127_and_leaves_no_record() {
        let case = Case::new("missing");
        let output = recording();
        let status = case.run(&["gate-box-no-such-program", "an-argument"], &[], None, &output);
        assert_eq!(status, 127);
        assert!(!case.record().exists(), "a command that never started leaves no record");
        let lines = said(&output, Stream::Err);
        assert!(
            lines.len() == 1 && lines[0].starts_with("gate: remote could not start gate-box-no-such-program: "),
            "{lines:?}"
        );
    }

    #[test]
    fn a_client_gone_before_the_spawn_gives_255_and_starts_nothing() {
        let case = Case::new("gone");
        let marker = case.base.join("ran");
        let output = Arc::new(Output::new(HungUpSink));
        let status = case.run(&["/usr/bin/touch", &marker.display().to_string()], &[], None, &output);
        assert_eq!(status, 255);
        assert!(!marker.exists(), "nothing started");
        assert!(!case.record().exists(), "no record");
    }

    #[test]
    fn a_removal_once_the_lock_name_was_replaced_leaves_the_other_run_s_record() {
        let case = Case::new("lock-replaced");
        fs::remove_file(&case.lock_path).expect("the lock's name is freed");
        fs::write(&case.lock_path, "").expect("another run's lock takes the name");
        fs::write(case.record(), "other").expect("the other run writes its record");
        let output = recording();
        removed_if_held(&command_record(&case.work), &case.lock, &output);
        assert_eq!(fs::read_to_string(case.record()).expect("the other run's record is kept"), "other");
        assert!(output.sink().lines().is_empty(), "nothing is said: {:?}", output.sink().lines());
    }

    #[test]
    fn a_removal_while_the_lock_is_held_removes_the_record() {
        let case = Case::new("lock-held");
        fs::write(case.record(), "ours").expect("the record is written");
        let output = recording();
        removed_if_held(&command_record(&case.work), &case.lock, &output);
        assert!(!case.record().exists(), "the record is removed");
        assert!(output.sink().lines().is_empty(), "nothing is said: {:?}", output.sink().lines());
    }

    #[test]
    fn a_command_whose_parent_ignores_term_still_dies_on_term_and_keeps_its_record() {
        let case = Case::new("term-ignored");
        let output = recording();
        // SAFETY: setting a disposition touches no memory; nextest runs this case in a process of its own, and the
        // default is put back before any assertion.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        let status = case.run(&["/bin/sh", "-c", "kill -TERM $$; exit 0"], &[], None, &output);
        // SAFETY: as above.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
        }
        assert_eq!(status, 143, "the command took TERM with its default action");
        assert!(case.record().exists(), "a signalled command keeps its record");
    }

    #[test]
    fn the_command_s_environment_has_the_login_path_the_payload_s_env_and_the_box_s_names_once() {
        let case = Case::new("environment");
        let output = recording();
        let env = [
            ("sharedcompilationid".to_owned(), "theirs".to_owned()),
            ("MY_VAR".to_owned(), "v".to_owned()),
        ];
        let status = case.run(&["/usr/bin/env"], &env, Some("/login/first:/usr/bin:/bin"), &output);
        assert_eq!(status, 0);
        let lines = said(&output, Stream::Out);
        for wanted in [
            "PATH=/login/first:/usr/bin:/bin",
            "MY_VAR=v",
            "GATE_BOX_JOB=gate-remote-k-0123456789ab",
            "SharedCompilationId=gate-box-0123456789ab",
        ] {
            assert!(lines.iter().any(|line| line == wanted), "{wanted}: {lines:?}");
        }
        let named = lines
            .iter()
            .filter(|line| {
                line.split('=')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case("SharedCompilationId"))
            })
            .count();
        assert_eq!(named, 1, "one name: {lines:?}");
    }

    #[test]
    fn a_program_on_the_login_path_is_found_there() {
        let case = Case::new("login-path");
        let bin = case.base.join("bin");
        fs::create_dir_all(&bin).expect("the tool folder is made");
        let tool = bin.join("gate-box-test-tool");
        fs::write(&tool, "#!/bin/sh\nexit 4\n").expect("the tool is written");
        fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the tool is made runnable");
        let output = recording();
        let login = format!("{}:/usr/bin:/bin", bin.display());
        let status = case.run(&["gate-box-test-tool"], &[], Some(&login), &output);
        assert_eq!(status, 4, "{:?}", output.sink().lines());
    }
}
