//! The gate's own argv: the subcommand CLI (`gate run`, `gate stop-tree`, `gate cutover`, the hidden `gate box`,
//! `--version`, `--help`) and the compatibility layer that reads the PowerShell gate's `-Log <path> -TimeoutSeconds <n> -Slot
//! <kind> -- <command>` form. Parsing is pure: it reads the words it is given, opens no file and reads no environment,
//! so it builds and tests everywhere. A usage error is a list of [`Problem`]s, each printed on its own `gate: ` line
//! before the [`USAGE`]; a malformed `gate box` is the one [`Problem::BoxUsage`], its own `usage: ` line said alone.
//!
//! It also owns the rules for the command's words, as pure functions over injected inputs: their C-runtime quoting
//! ([`command_line`]), their cmd.exe quoting for a `.cmd` or `.bat` target ([`is_batch_target`],
//! [`batch_command_line`]) with the words such a target refuses ([`batch_refusal`]), and program resolution through
//! `PATH` and `PATHEXT` ([`resolve_program`]).

mod batch;
mod boxcmd;
mod cli;
mod cmdline;
mod compat;
mod problem;
mod program;
mod request;
mod run;
mod usage;
mod values;

use std::ffi::OsString;

pub use batch::{BatchRefusal, batch_command_line, batch_refusal, is_batch_target, quote_batch_arg, quote_batch_arg_str};
pub use cmdline::{command_line, command_text, quote_arg, quote_arg_str};
pub use problem::{MAX_WHOLE_NUMBER, Problem, Required, Spelling};
pub use program::{ProgramSearch, resolve_program};
pub use request::{
    BoxRequest, BoxSeams, BoxVerb, CutoverRequest, CutoverSeams, DEFAULT_STALL_SECONDS, DEFAULT_TAIL_LINES, Invocation, RunRequest, Slot,
    StopRequest, TestSeams,
};
pub use usage::USAGE;

/// Reads the gate's arguments, without the program name.
///
/// The first word picks the parser: `--version`, `--help` and `help` first, then a subcommand name (`run`,
/// `stop-tree`, `cutover`, the hidden `box`) selects the CLI and a first word starting with `-` selects the
/// compatibility layer.
///
/// # Errors
///
/// Every problem found, in the order found; the caller prints each, then the usage, and exits 2. An empty list means
/// no arguments were given. A malformed `gate box` is the one problem [`Problem::BoxUsage`], printed alone, without
/// the usage.
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
        "cutover" => cli::parse_cutover(words).map(Invocation::Cutover),
        "box" => boxcmd::parse(words).map(Invocation::Box),
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
