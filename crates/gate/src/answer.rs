//! What the gate answers for a command line: the lines it says on stdout and stderr, and the exit status.

use std::ffi::OsString;

use gate_args::{Invocation, Problem, RunRequest, StopRequest, USAGE, parse};

use crate::console::Console;
use crate::request_json;
use crate::resolve::{self, Environment};

/// The status of a usage error: a missing, unknown or invalid input, or a word the command cannot take.
pub(crate) const USAGE_STATUS: u8 = 2;
/// The status of a valid request for a mode this build does not have yet: `stop-tree`.
const NOT_BUILT_STATUS: u8 = 70;

/// Says the answer for `args`, the command line without the program name, through `console`, and returns the exit
/// status: a 32-bit code, since a run passes its command's whole exit code through.
pub(crate) fn answer(args: Vec<OsString>, environment: &Environment, console: &mut Console) -> u32 {
    match parse(args) {
        Ok(Invocation::Version) => {
            console.out(&format!("gate {}", env!("CARGO_PKG_VERSION")));
            0
        }
        Ok(Invocation::Help) => {
            console.out(USAGE);
            0
        }
        Ok(Invocation::Run(request)) => run(&request, environment, console),
        Ok(Invocation::StopTree(request)) => u32::from(stop_tree(&request, environment, console)),
        Err(problems) => u32::from(usage_error(&problems, console)),
    }
}

fn usage_error(problems: &[Problem], console: &mut Console) -> u8 {
    for problem in problems {
        console.err(&problem.to_string());
    }
    console.err(USAGE);
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
        Ok((log, root)) => crate::run::run(request, &log, &root, console),
    }
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
        Ok(_) => {
            console.err("gate: stop-tree is not built yet");
            NOT_BUILT_STATUS
        }
    }
}
