//! The gate's own argv: the subcommand CLI (`gate run`, `gate stop-tree`, `--version`, `--help`) and the compatibility
//! layer that reads the PowerShell gate's `-Log <path> -TimeoutSeconds <n> -Slot <kind> -- <command>` form. Parsing is
//! pure: it reads the words it is given, opens no file and reads no environment, so it builds and tests everywhere.
//! A usage error is a list of [`Problem`]s, each printed on its own `gate: ` line before the [`USAGE`].

mod cli;
mod compat;
mod problem;
mod request;
mod run;
mod usage;
mod values;

use std::ffi::OsString;

pub use problem::{MAX_WHOLE_NUMBER, Problem, Required, Spelling};
pub use request::{DEFAULT_STALL_SECONDS, DEFAULT_TAIL_LINES, Invocation, RunRequest, Slot, StopRequest, TestSeams};
pub use usage::USAGE;

/// Reads the gate's arguments, without the program name.
///
/// The first word picks the parser: `--version`, `--help` and `help` first, then a subcommand name (`run`,
/// `stop-tree`) selects the CLI and a first word starting with `-` selects the compatibility layer.
///
/// # Errors
///
/// Every problem found, in the order found; the caller prints each, then the usage, and exits 2. An empty list means
/// no arguments were given.
pub fn parse(args: Vec<OsString>) -> Result<Invocation, Vec<Problem>> {
    let mut words = args.into_iter();
    let Some(first) = words.next() else {
        return Err(Vec::new());
    };
    let Some(text) = first.to_str() else {
        return Err(vec![Problem::Unreadable(first.to_string_lossy().into_owned())]);
    };
    match text {
        "--version" => alone(words, Invocation::Version),
        "--help" | "help" => alone(words, Invocation::Help),
        "run" => cli::parse_run(words).map(Invocation::Run),
        "stop-tree" => cli::parse_stop_tree(words).map(Invocation::StopTree),
        _ if text.starts_with('-') => compat::parse(std::iter::once(first).chain(words)),
        _ => Err(vec![Problem::UnknownSubcommand(text.to_owned())]),
    }
}

fn alone(rest: impl Iterator<Item = OsString>, invocation: Invocation) -> Result<Invocation, Vec<Problem>> {
    let problems: Vec<Problem> = rest
        .map(|word| Problem::UnexpectedArgument(word.to_string_lossy().into_owned()))
        .collect();
    if problems.is_empty() { Ok(invocation) } else { Err(problems) }
}
