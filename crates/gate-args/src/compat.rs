//! The compatibility layer: the PowerShell gate's `-Log <path> -TimeoutSeconds <n> -Slot <kind> [...] -- <command>`
//! and `-StopTree <pid>`. Exactly its twelve options, `-Cd <folder>`, `-Env NAME[=value]`, `-Alone` and `-Reason <why>`
//! among them, are read, by name in any ASCII case, in the `-Name value` and the `-Name:value` forms; `--` is required
//! before the command. A `test-seams`
//! build also reads the hidden `--test-print-request` switch, spelled exactly so, so the suite can see what a compat
//! run asked for, and the hidden `--test-parent-job-trusted` switch, spelled exactly so, so a compat run under a
//! marker naming the test process trusts it.

use std::ffi::OsString;
use std::iter::Peekable;

use crate::problem::Problem;
use crate::request::Invocation;
use crate::run::{RunBuilder, RunOption, SwitchOption, ValueOption, next_text, next_value};
use crate::values;

#[derive(Clone, Copy)]
enum CompatOption {
    Run(RunOption),
    StopTree,
}

const OPTIONS: [(&str, CompatOption); 12] = [
    ("Log", CompatOption::Run(RunOption::Value(ValueOption::Log))),
    ("TimeoutSeconds", CompatOption::Run(RunOption::Value(ValueOption::Timeout))),
    ("Slot", CompatOption::Run(RunOption::Value(ValueOption::Slot))),
    ("StallSeconds", CompatOption::Run(RunOption::Value(ValueOption::Stall))),
    ("Tail", CompatOption::Run(RunOption::Value(ValueOption::Tail))),
    ("NoMarkers", CompatOption::Run(RunOption::Switch(SwitchOption::NoMarkers))),
    ("Local", CompatOption::Run(RunOption::Switch(SwitchOption::Local))),
    ("Alone", CompatOption::Run(RunOption::Switch(SwitchOption::Alone))),
    ("Cd", CompatOption::Run(RunOption::Value(ValueOption::Cd))),
    ("Env", CompatOption::Run(RunOption::Value(ValueOption::Env))),
    ("Reason", CompatOption::Run(RunOption::Value(ValueOption::Reason))),
    ("StopTree", CompatOption::StopTree),
];

/// The words read so far.
struct Compat {
    builder: RunBuilder,
    stop_tree: Option<Result<u32, Problem>>,
}

/// Reads the whole command line in the PowerShell form.
pub(crate) fn parse(words: impl Iterator<Item = OsString>) -> Result<Invocation, Vec<Problem>> {
    let mut words = words.peekable();
    let mut compat = Compat {
        builder: RunBuilder::compat(),
        stop_tree: None,
    };
    let mut command = None;
    while let Some(text) = next_text(&mut words, &mut compat.builder) {
        if text == "--" {
            command = Some(words.collect::<Vec<OsString>>());
            break;
        }
        #[cfg(feature = "test-seams")]
        if text == "--test-print-request" {
            compat.builder.switch(SwitchOption::PrintRequest, &text, true);
            continue;
        }
        #[cfg(feature = "test-seams")]
        if text == "--test-parent-job-trusted" {
            compat.builder.switch(SwitchOption::ParentJobTrusted, &text, true);
            continue;
        }
        match text.strip_prefix('-') {
            Some(rest) => compat.read_option(&mut words, &text, rest),
            None => compat.builder.problems.push(Problem::UnexpectedArgument(text)),
        }
    }
    compat.finish(command)
}

impl Compat {
    fn read_option(&mut self, words: &mut Peekable<impl Iterator<Item = OsString>>, word: &str, rest: &str) {
        let (name, inline) = split_colon(rest);
        let spelled = format!("-{name}");
        let Some(option) = compat_option(name) else {
            self.builder.problems.push(Problem::UnknownOption(word.to_owned()));
            return;
        };
        match (option, inline) {
            (CompatOption::StopTree, _) if self.stop_tree.is_some() => {
                self.builder.problems.push(Problem::GivenTwice(spelled));
                if inline.is_none()
                    && let Err(problem) = next_value(words, is_compat_option)
                {
                    self.builder.problems.push(problem);
                }
            }
            (CompatOption::StopTree, Some(value)) => self.stop_tree = Some(read_pid(Some(value))),
            (CompatOption::StopTree, None) => {
                self.stop_tree = Some(next_value(words, is_compat_option).and_then(|value| read_pid(value.as_deref())));
            }
            (CompatOption::Run(RunOption::Switch(switch)), None) => self.builder.switch(switch, &spelled, true),
            (CompatOption::Run(RunOption::Switch(switch)), Some(value)) => match values::true_or_false(&spelled, value) {
                Ok(on) => self.builder.switch(switch, &spelled, on),
                Err(problem) => self.builder.problems.push(problem),
            },
            (CompatOption::Run(RunOption::Value(value_option)), Some(value)) => self.builder.value(value_option, &spelled, value),
            (CompatOption::Run(RunOption::Value(value_option)), None) => {
                self.builder.value_from_next(value_option, &spelled, words, is_compat_option);
            }
        }
    }

    /// A stop request when `-StopTree` came alone; a run request otherwise.
    fn finish(mut self, command: Option<Vec<OsString>>) -> Result<Invocation, Vec<Problem>> {
        let Some(pid) = self.stop_tree else {
            return self.builder.finish_run(command.unwrap_or_default()).map(Invocation::Run);
        };
        if self.builder.has_options() || command.is_some() {
            self.builder.problems.push(Problem::StopTreeNotAlone);
            if let Err(problem) = pid {
                self.builder.problems.push(problem);
            }
            return Err(self.builder.problems);
        }
        self.builder.finish_stop(pid).map(Invocation::StopTree)
    }
}

/// `Name:value` split at its first colon.
fn split_colon(rest: &str) -> (&str, Option<&str>) {
    match rest.split_once(':') {
        Some((name, value)) => (name, Some(value)),
        None => (rest, None),
    }
}

fn compat_option(name: &str) -> Option<CompatOption> {
    OPTIONS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|(_, option)| *option)
}

/// Whether `word` is one of the twelve options, `-Name` or `-Name:value`, and so never an option's value.
fn is_compat_option(word: &str) -> bool {
    word.strip_prefix('-').is_some_and(|rest| compat_option(split_colon(rest).0).is_some())
}

fn read_pid(value: Option<&str>) -> Result<u32, Problem> {
    match value {
        None | Some("") => Err(Problem::MissingPid),
        Some(value) => values::pid(value),
    }
}
