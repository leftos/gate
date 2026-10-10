//! What the gate answers for a command line: the lines for stdout and stderr and the exit status.

use std::ffi::OsString;

use gate_args::{Invocation, Problem, RunRequest, StopRequest, USAGE, parse};

use crate::resolve::{self, Environment};
use crate::{journal, request_json};

/// The status of a usage error: a missing, unknown or invalid input.
const USAGE_STATUS: u8 = 2;
/// The status of a valid request for a mode this build does not have yet.
const NOT_BUILT_STATUS: u8 = 70;

/// The gate's answer, written by `main`: stdout's lines, then stderr's.
pub(crate) struct Report {
    pub status: u8,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

impl Report {
    fn stdout(status: u8, line: String) -> Report {
        Report {
            status,
            stdout: vec![line],
            stderr: Vec::new(),
        }
    }

    fn stderr(status: u8, line: String) -> Report {
        Report {
            status,
            stdout: Vec::new(),
            stderr: vec![line],
        }
    }
}

/// The answer for `args`, the command line without the program name.
pub(crate) fn report(args: Vec<OsString>, environment: &Environment) -> Report {
    match parse(args) {
        Ok(Invocation::Version) => Report::stdout(0, format!("gate {}", env!("CARGO_PKG_VERSION"))),
        Ok(Invocation::Help) => Report::stdout(0, USAGE.to_owned()),
        Ok(Invocation::Run(request)) => run(&request, environment),
        Ok(Invocation::StopTree(request)) => stop_tree(&request, environment),
        Err(problems) => usage_error(&problems),
    }
}

fn usage_error(problems: &[Problem]) -> Report {
    let mut stderr: Vec<String> = problems.iter().map(ToString::to_string).collect();
    stderr.push(USAGE.to_owned());
    Report {
        status: USAGE_STATUS,
        stdout: Vec::new(),
        stderr,
    }
}

fn run(request: &RunRequest, environment: &Environment) -> Report {
    let resolved = resolve::root(request.state_dir.as_deref(), environment).and_then(|root| Ok((resolve::log(&request.log)?, root)));
    match resolved {
        Err(line) => Report::stderr(USAGE_STATUS, line),
        Ok((log, root)) if request.print_request => Report::stdout(0, request_json::run(request, &log, &root)),
        Ok((log, root)) => match request.test_seams.journal_run {
            Some(status) => journal::seam_run(request, &log, &root, status),
            None => Report::stderr(NOT_BUILT_STATUS, "gate: run is not built yet".to_owned()),
        },
    }
}

fn stop_tree(request: &StopRequest, environment: &Environment) -> Report {
    match resolve::root(request.state_dir.as_deref(), environment) {
        Err(line) => Report::stderr(USAGE_STATUS, line),
        Ok(root) if request.print_request => Report::stdout(0, request_json::stop_tree(request, &root)),
        Ok(_) => Report::stderr(NOT_BUILT_STATUS, "gate: stop-tree is not built yet".to_owned()),
    }
}
