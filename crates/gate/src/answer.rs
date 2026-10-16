//! What the gate answers for a command line: the lines it says on stdout and stderr, and the exit status.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, Prefix};

use gate_args::{ExpectRequest, Invocation, Problem, RunRequest, Spelling, StopRequest, USAGE, parse};

use crate::console::Console;
use crate::request_json;
use crate::resolve::{self, Environment};
use crate::version::build_version;

/// The status of a step that failed with no status of its own.
pub(crate) const FAILED_STATUS: u8 = 1;
/// The status of a usage error: a missing, unknown or invalid input, or a word the command cannot take.
pub(crate) const USAGE_STATUS: u8 = 2;

/// Says the answer for `args`, the command line without the program name, through `console`, and returns the exit
/// status: a 32-bit code, since a run passes its command's whole exit code through.
pub(crate) fn answer(args: Vec<OsString>, environment: &Environment, console: &mut Console) -> u32 {
    match parse(args) {
        Ok(Invocation::Version) => {
            console.out(&format!("gate {}", build_version()));
            0
        }
        Ok(Invocation::Help) => {
            console.out(USAGE);
            0
        }
        Ok(Invocation::Run(request)) => run(&request, environment, console),
        Ok(Invocation::StopTree(request)) => u32::from(stop_tree(&request, environment, console)),
        Ok(Invocation::Cutover(request)) => crate::cutover::cutover(&request, environment, console),
        Ok(Invocation::Push(request)) => crate::push::push(&request, environment, console),
        Ok(Invocation::Check(request)) => crate::check::check(&request, console),
        Ok(Invocation::Burn(request)) => crate::burn::burn(&request, console),
        Ok(Invocation::Expect(request)) => u32::from(expect(&request, environment, console)),
        Ok(Invocation::Box(request)) => crate::boxside::answer(&request, console),
        Err(problems) => u32::from(usage_error(&problems, console)),
    }
}

fn usage_error(problems: &[Problem], console: &mut Console) -> u8 {
    for problem in problems {
        console.err(&problem.to_string());
    }
    // `gate box` is hidden, so its own usage line is said alone, without the published usage.
    if !matches!(problems, [Problem::BoxUsage]) {
        console.err(USAGE);
    }
    USAGE_STATUS
}

fn run(request: &RunRequest, environment: &Environment, console: &mut Console) -> u32 {
    let resolved = resolve::root(request.state_dir.as_deref(), environment).and_then(|root| Ok((resolve::log(&request.log)?, root)));
    match resolved {
        Err(line) => {
            console.err(&line);
            u32::from(USAGE_STATUS)
        }
        Ok((log, root)) if request.print_request => {
            console.out(&request_json::run(request, &log, &root));
            0
        }
        Ok((log, root)) => match enter(request.cd.as_deref(), request.spelling) {
            Err(line) => {
                console.err(&line);
                u32::from(USAGE_STATUS)
            }
            Ok(()) => crate::run::run(request, &log, &root, environment, console),
        },
    }
}

/// `ERROR_PATH_NOT_FOUND`: a folder on the path is missing.
const ERROR_PATH_NOT_FOUND: i32 = 3;
/// `ERROR_DIRECTORY`: the path names a file, not a folder.
const ERROR_DIRECTORY: i32 = 267;

/// Makes `cd` (`--cd`, a relative path read against the caller's folder) the gate's working directory, so the run, its
/// routing, its command and its journal lines all take it as the folder the gate was started in. Called after the log
/// and the state root are made absolute, which stay the caller's, and before any slot is taken. Only a folder on a
/// drive is entered: a UNC, verbatim or device path is refused before any filesystem call. Each line names the flag as
/// the caller spelled it.
fn enter(cd: Option<&Path>, spelling: Spelling) -> Result<(), String> {
    let Some(given) = cd else {
        return Ok(());
    };
    let flag = match spelling {
        Spelling::Cli => "--cd",
        Spelling::Compat => "-Cd",
    };
    let cannot = |error: &io::Error| format!("gate: cannot run in {flag} {}: {error}", given.display());
    let folder = std::path::absolute(given).map_err(|error| cannot(&error))?;
    if !on_a_drive(&folder) {
        return Err(format!("gate: {flag} {} is not a folder on a drive", given.display()));
    }
    std::env::set_current_dir(&folder).map_err(|error| {
        if is_not_a_folder(&error) {
            format!("gate: {flag} {} is not a folder", given.display())
        } else {
            cannot(&error)
        }
    })
}

/// Whether the absolute `folder` starts with a drive letter, `X:`, and not a UNC, verbatim or device prefix.
fn on_a_drive(folder: &Path) -> bool {
    matches!(folder.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)))
}

/// Whether entering a folder failed because there is no folder there: nothing at the path, or a file.
fn is_not_a_folder(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
        || matches!(error.raw_os_error(), Some(ERROR_PATH_NOT_FOUND | ERROR_DIRECTORY))
}

fn stop_tree(request: &StopRequest, environment: &Environment, console: &mut Console) -> u8 {
    match resolve::root(request.state_dir.as_deref(), environment) {
        Err(line) => {
            console.err(&line);
            USAGE_STATUS
        }
        Ok(root) if request.print_request => {
            console.out(&request_json::stop_tree(request, &root));
            0
        }
        Ok(root) => crate::stop_tree::stop_tree(request.pid, &root.root, console),
    }
}

fn expect(request: &ExpectRequest, environment: &Environment, console: &mut Console) -> u8 {
    match resolve::root(request.state_dir.as_deref(), environment) {
        Err(line) => {
            console.err(&line);
            USAGE_STATUS
        }
        Ok(root) => crate::expect::expect(request, &root.path, console),
    }
}
