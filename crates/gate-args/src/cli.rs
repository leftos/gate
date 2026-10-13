//! The subcommand CLI: GNU-style `--name value` and `--name=value`, with `--` ending the gate's options.

use std::ffi::OsString;
use std::iter::Peekable;

use crate::problem::{Problem, Spelling};
use crate::request::{BurnRequest, CutoverRequest, PushRequest, RunRequest, StopRequest};
use crate::run::{RunBuilder, RunOption, SwitchOption, ValueOption, next_text};
use crate::values;

/// Reads the words after `gate run`.
pub(crate) fn parse_run(words: impl Iterator<Item = OsString>) -> Result<RunRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    let mut command = Vec::new();
    while let Some(text) = next_text(&mut words, &mut builder.problems) {
        if text == "--" {
            command = words.collect();
            break;
        }
        if text.starts_with('-') {
            read_option(&mut builder, &mut words, &text, run_option);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    builder.finish_run(command, Spelling::Cli)
}

/// Reads the words after `gate stop-tree`: the pid, and `--state-dir` before it or after it.
pub(crate) fn parse_stop_tree(words: impl Iterator<Item = OsString>) -> Result<StopRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    let mut positional = Vec::new();
    let mut options_ended = false;
    while let Some(text) = next_text(&mut words, &mut builder.problems) {
        if options_ended || !text.starts_with('-') {
            positional.push(text);
        } else if text == "--" {
            options_ended = true;
        } else {
            read_option(&mut builder, &mut words, &text, stop_tree_option);
        }
    }
    let mut positional = positional.into_iter();
    let pid = positional.next().map_or(Err(Problem::MissingPid), |word| values::pid(&word));
    builder.problems.extend(positional.map(Problem::UnexpectedArgument));
    builder.finish_stop(pid)
}

/// Reads the words after `gate cutover`, which takes none: every word is a problem, `--` and `--state-dir` included,
/// save the hidden seams of a build with the `test-seams` feature.
pub(crate) fn parse_cutover(words: impl Iterator<Item = OsString>) -> Result<CutoverRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    while let Some(text) = next_text(&mut words, &mut builder.problems) {
        if text.starts_with('-') && text != "--" {
            read_option(&mut builder, &mut words, &text, cutover_option);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    builder.finish_cutover()
}

/// Reads the words after `gate push`, which takes none: every word is a problem, `--` included, save the hidden seams
/// of a build with the `test-seams` feature.
pub(crate) fn parse_push(words: impl Iterator<Item = OsString>) -> Result<PushRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    while let Some(text) = next_text(&mut words, &mut builder.problems) {
        if text.starts_with('-') && text != "--" {
            read_option(&mut builder, &mut words, &text, push_option);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    builder.finish_push()
}

/// Reads the words after `gate burn`: the optional `--cores <n>` and the required `--seconds <s>`.
pub(crate) fn parse_burn(words: impl Iterator<Item = OsString>) -> Result<BurnRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    while let Some(text) = next_text(&mut words, &mut builder.problems) {
        if text.starts_with('-') && text != "--" {
            read_option(&mut builder, &mut words, &text, burn_option);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    builder.finish_burn()
}

/// Reads one option word, `--name` or `--name=value`, taking its value from the next word when it needs one.
fn read_option(builder: &mut RunBuilder, words: &mut Peekable<impl Iterator<Item = OsString>>, word: &str, lookup: fn(&str) -> Option<RunOption>) {
    let Some(rest) = word.strip_prefix("--") else {
        builder.problems.push(Problem::UnknownOption(word.to_owned()));
        return;
    };
    let (name, inline) = match rest.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (rest, None),
    };
    let spelled = format!("--{name}");
    match (lookup(name), inline) {
        (Some(RunOption::Switch(option)), None) => builder.switch(option, &spelled, true),
        (Some(RunOption::Value(option)), Some(value)) => builder.value(option, &spelled, value),
        (Some(RunOption::Value(option)), None) => builder.value_from_next(option, &spelled, words, |_| false),
        (None | Some(RunOption::Switch(_)), _) => builder.problems.push(Problem::UnknownOption(word.to_owned())),
    }
}

fn run_option(name: &str) -> Option<RunOption> {
    let option = match name {
        "slot" => RunOption::Value(ValueOption::Slot),
        "timeout" => RunOption::Value(ValueOption::Timeout),
        "log" => RunOption::Value(ValueOption::Log),
        "stall" => RunOption::Value(ValueOption::Stall),
        "tail" => RunOption::Value(ValueOption::Tail),
        "state-dir" => RunOption::Value(ValueOption::StateDir),
        "no-markers" => RunOption::Switch(SwitchOption::NoMarkers),
        "local" => RunOption::Switch(SwitchOption::Local),
        _ => return seam_option(name),
    };
    Some(option)
}

fn burn_option(name: &str) -> Option<RunOption> {
    match name {
        "cores" => Some(RunOption::Value(ValueOption::Cores)),
        "seconds" => Some(RunOption::Value(ValueOption::Seconds)),
        _ => None,
    }
}

fn stop_tree_option(name: &str) -> Option<RunOption> {
    match name {
        "state-dir" => Some(RunOption::Value(ValueOption::StateDir)),
        #[cfg(feature = "test-seams")]
        "test-print-request" => Some(RunOption::Switch(SwitchOption::PrintRequest)),
        _ => None,
    }
}

#[cfg(feature = "test-seams")]
fn cutover_option(name: &str) -> Option<RunOption> {
    match name {
        "test-poll-ms" => Some(RunOption::Value(ValueOption::PollMs)),
        "test-heartbeat-ms" => Some(RunOption::Value(ValueOption::HeartbeatMs)),
        "test-print-request" => Some(RunOption::Switch(SwitchOption::PrintRequest)),
        "test-confirm-ms" => Some(RunOption::Value(ValueOption::ConfirmMs)),
        "test-cutover-interrupt-after-list" => Some(RunOption::Switch(SwitchOption::CutoverInterruptAfterList)),
        _ => None,
    }
}

#[cfg(not(feature = "test-seams"))]
fn cutover_option(_name: &str) -> Option<RunOption> {
    None
}

#[cfg(feature = "test-seams")]
fn push_option(name: &str) -> Option<RunOption> {
    match name {
        "test-push-ssh" => Some(RunOption::Value(ValueOption::PushSsh)),
        "test-push-bound-ms" => Some(RunOption::Value(ValueOption::PushBoundMs)),
        _ => None,
    }
}

#[cfg(not(feature = "test-seams"))]
fn push_option(_name: &str) -> Option<RunOption> {
    None
}

#[cfg(feature = "test-seams")]
fn seam_option(name: &str) -> Option<RunOption> {
    match name {
        "test-sample-ms" => Some(RunOption::Value(ValueOption::SampleMs)),
        "test-heartbeat-ms" => Some(RunOption::Value(ValueOption::HeartbeatMs)),
        "test-poll-ms" => Some(RunOption::Value(ValueOption::PollMs)),
        "test-stall-ms" => Some(RunOption::Value(ValueOption::StallMs)),
        "test-diagnostics-budget-ms" => Some(RunOption::Value(ValueOption::DiagnosticsBudgetMs)),
        "test-interrupt-after-samples" => Some(RunOption::Value(ValueOption::InterruptAfterSamples)),
        "test-print-request" => Some(RunOption::Switch(SwitchOption::PrintRequest)),
        "test-interrupt-queued" => Some(RunOption::Switch(SwitchOption::InterruptQueued)),
        "test-sampler-fail" => Some(RunOption::Switch(SwitchOption::SamplerFail)),
        "test-own-start-fails" => Some(RunOption::Switch(SwitchOption::OwnStartFails)),
        "test-parent-job-trusted" => Some(RunOption::Switch(SwitchOption::ParentJobTrusted)),
        "test-diagnostics-fail" => Some(RunOption::Switch(SwitchOption::DiagnosticsFail)),
        "test-job-breakaway" => Some(RunOption::Switch(SwitchOption::JobBreakaway)),
        "test-remote-client" => Some(RunOption::Value(ValueOption::RemoteClient)),
        "test-remote-wait-ms" => Some(RunOption::Value(ValueOption::RemoteWaitMs)),
        "test-git-limit-ms" => Some(RunOption::Value(ValueOption::GitLimitMs)),
        "test-git-drain-ms" => Some(RunOption::Value(ValueOption::GitDrainMs)),
        "test-servers-under" => Some(RunOption::Value(ValueOption::ServersUnder)),
        "test-server-image" => Some(RunOption::Value(ValueOption::ServerImage)),
        "test-remote-snapshot" => Some(RunOption::Switch(SwitchOption::RemoteSnapshot)),
        "test-remote-transport" => Some(RunOption::Value(ValueOption::RemoteTransport)),
        "test-remote-ssh" => Some(RunOption::Value(ValueOption::RemoteSsh)),
        "test-remote-retry-pause-ms" => Some(RunOption::Value(ValueOption::RemoteRetryPauseMs)),
        "test-remote-bound-ms" => Some(RunOption::Value(ValueOption::RemoteBoundMs)),
        "test-remote-interrupt-at" => Some(RunOption::Value(ValueOption::RemoteInterruptAt)),
        "test-remote-watch-call" => Some(RunOption::Value(ValueOption::RemoteWatchCall)),
        "test-remote-timeout-ms" => Some(RunOption::Value(ValueOption::RemoteTimeoutMs)),
        "test-local-sdks" => Some(RunOption::Value(ValueOption::LocalSdks)),
        "test-remote-fetch-cap-bytes" => Some(RunOption::Value(ValueOption::RemoteFetchCapBytes)),
        "test-remote-apply-cap-bytes" => Some(RunOption::Value(ValueOption::RemoteApplyCapBytes)),
        "test-remote-script" => Some(RunOption::Value(ValueOption::RemoteScript)),
        "test-bridge-pwsh" => Some(RunOption::Value(ValueOption::BridgePwsh)),
        _ => None,
    }
}

#[cfg(not(feature = "test-seams"))]
fn seam_option(_name: &str) -> Option<RunOption> {
    None
}
