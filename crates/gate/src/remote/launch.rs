//! How the remote client and git start a call: created suspended with its output piped back, in an anonymous
//! kill-on-close job of its own, and ended with that job; and how long a killed call and its readers are waited for.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use gate_win32::{EnvironmentBlock, Job, LaunchError, PipedConsole, PipedFed, PipedLaunch, PipedSuspended, Suspended, same_variable_name};

/// The status of a call the client killed, and of a run whose run call the watchdog killed; the exit code a killed
/// call's job ends its processes with.
pub(super) const KILLED: i32 = 124;
/// How long a killed call's output is waited for to end before the wait on it stops.
pub(crate) const DRAIN: Duration = Duration::from_secs(2);
/// How long a killed call is waited for to exit.
pub(super) const KILL_WAIT: Duration = Duration::from_secs(30);

/// An anonymous job of its own for a call, which ends every process in it when it is terminated or dropped, and which
/// no other process can open by a name. With `breakaway`, a process in it may start one outside it, as a box run on
/// this machine starts its command; without, it refuses that, so no ssh or git started in it can leave it and outlive
/// a kill.
pub(crate) fn call_job(breakaway: bool) -> io::Result<Job> {
    if breakaway {
        Job::anonymous_kill_on_close_breakaway()
    } else {
        Job::anonymous_kill_on_close()
    }
}

/// The launch of `command`, created suspended on `console`: its program, words and working folder, and this process's
/// environment with the command's own changes.
pub(crate) fn piped_launch(command: &Command, console: PipedConsole) -> io::Result<PipedLaunch> {
    let words: Vec<Vec<u16>> = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|word| word.encode_wide().collect())
        .collect();
    let directory = match command.get_current_dir() {
        Some(folder) => folder.to_path_buf(),
        None => std::env::current_dir()?,
    };
    Ok(PipedLaunch {
        application: PathBuf::from(command.get_program()),
        command_line: gate_args::command_line(&words),
        directory,
        environment: EnvironmentBlock::new(environment_of(command))?,
        console,
        token: None,
    })
}

/// `launch` created suspended within `job`: the process, the read ends of its stdout and stderr, and the write end of
/// its stdin when `fed`, else `None` and its stdin `NUL`.
pub(crate) fn launched(launch: &PipedLaunch, job: &Job, fed: bool) -> Result<(Suspended, File, File, Option<File>), LaunchError> {
    if fed {
        let PipedFed {
            suspended,
            stdin,
            stdout,
            stderr,
        } = launch.start_suspended_within_fed(job)?;
        return Ok((suspended, stdout, stderr, Some(stdin)));
    }
    let PipedSuspended { suspended, stdout, stderr } = launch.start_suspended_within(job)?;
    Ok((suspended, stdout, stderr, None))
}

/// This process's environment with `command`'s changes: each variable it sets replaces the one of its name, and each
/// it removes is gone, names compared as Windows compares them.
fn environment_of(command: &Command) -> Vec<(OsString, OsString)> {
    let mut variables: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (name, value) in command.get_envs() {
        variables.retain(|(kept, _)| !same_variable_name(kept, name));
        variables.extend(value.map(|value| (name.to_owned(), value.to_owned())));
    }
    variables
}
