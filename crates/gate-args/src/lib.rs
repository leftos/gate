//! The gate's own argv: the subcommand CLI (`gate run`, `gate burn`, `gate expect`, `gate stop-tree`, `gate cutover`,
//! `gate push`, `gate check`, the hidden `gate box`, `--version`, `--help`/`-h`) and the compatibility layer that reads the PowerShell
//! gate's `-Log <path> -TimeoutSeconds <n> -Slot <kind> -- <command>` form. Parsing is pure: it reads the words it is
//! given, opens no file and reads no environment, so it builds and tests everywhere. A usage error is a list of
//! [`Problem`]s, each printed on its own `gate: ` line before the [`USAGE`]; a malformed `gate box` is the one
//! [`Problem::BoxUsage`], its own `usage: ` line said alone.
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
    BoxRequest, BoxSeams, BoxVerb, BurnRequest, Ceiling, CheckRequest, CheckSeams, CutoverRequest, CutoverSeams, DEFAULT_STALL_SECONDS,
    DEFAULT_TAIL_LINES, DesktopMode, DesktopSession, ExpectRequest, Invocation, PushRequest, PushSeams, RunRequest, Slot, StopRequest, TestSeams,
};
pub use usage::USAGE;

/// Reads the gate's arguments, without the program name.
///
/// The first word picks the parser: `--version`, `--help`, `-h` and `help` first, then a subcommand name (`run`,
/// `burn`, `expect`, `stop-tree`, `cutover`, `push`, `check`, the hidden `box`) selects the CLI and a first word
/// starting with `-` selects the compatibility layer.
///
/// A `--help` or `-h` word before the first `--` separator of a `run`, `stop-tree`, `cutover`, `push`, `check`, `burn`
/// or `expect` line is the usage too, whatever else the line holds. The words after `--` belong to the command the gate
/// runs, so `gate run --slot light -- tool --help` runs `tool --help`.
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
    let rest: Vec<OsString> = words.collect();
    match text {
        "--version" => alone(rest.into_iter(), Invocation::Version),
        "--help" | "-h" | "help" => alone(rest.into_iter(), Invocation::Help),
        "run" | "stop-tree" | "cutover" | "push" | "check" | "burn" | "expect" if help_before_separator(&rest) => Ok(Invocation::Help),
        "run" => cli::parse_run(rest.into_iter()).map(Invocation::Run),
        "stop-tree" => cli::parse_stop_tree(rest.into_iter()).map(Invocation::StopTree),
        "cutover" => cli::parse_cutover(rest.into_iter()).map(Invocation::Cutover),
        "push" => cli::parse_push(rest.into_iter()).map(Invocation::Push),
        "check" => cli::parse_check(rest.into_iter()).map(Invocation::Check),
        "burn" => cli::parse_burn(rest.into_iter()).map(Invocation::Burn),
        "expect" => cli::parse_expect(rest.into_iter()).map(Invocation::Expect),
        "box" => boxcmd::parse(rest.into_iter()).map(Invocation::Box),
        _ if text.starts_with('-') => compat::parse(std::iter::once(first).chain(rest)),
        _ => Err(vec![Problem::UnknownSubcommand(text.to_owned())]),
    }
}

/// Whether a word before the first `--` separator is exactly `--help` or `-h`.
///
/// The words after `--` belong to the command the gate runs, so `gate run --slot light -- tool --help` runs
/// `tool --help` rather than asking for the usage.
fn help_before_separator(words: &[OsString]) -> bool {
    for word in words {
        if word.to_str() == Some("--") {
            return false;
        }
        if matches!(word.to_str(), Some("--help" | "-h")) {
            return true;
        }
    }
    false
}

fn alone(rest: impl Iterator<Item = OsString>, invocation: Invocation) -> Result<Invocation, Vec<Problem>> {
    let problems: Vec<Problem> = rest
        .map(|word| Problem::UnexpectedArgument(word.to_string_lossy().into_owned()))
        .collect();
    if problems.is_empty() { Ok(invocation) } else { Err(problems) }
}
