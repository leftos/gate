//! The subcommand CLI: GNU-style `--name value` and `--name=value`, with `--` ending the gate's options.

use std::ffi::OsString;
use std::iter::Peekable;

use crate::problem::Problem;
use crate::request::{
    BurnRequest, CheckRequest, CutoverRequest, DrainRequest, ExpectRequest, PruneRequest, PushRequest, RunRequest, StopRequest, WarmRequest,
};
use crate::run::{RunBuilder, RunOption, SwitchOption, ValueOption, next_text};
use crate::values;

/// Reads the words after `gate run`.
pub(crate) fn parse_run(words: impl Iterator<Item = OsString>) -> Result<RunRequest, Vec<Problem>> {
    let mut builder = RunBuilder::default();
    let command = read_until_command(&mut builder, words, run_option);
    builder.finish_run(command)
}

/// Reads the words after `gate expect`: `--state-dir` and `--slot`, then the command after `--`.
pub(crate) fn parse_expect(words: impl Iterator<Item = OsString>) -> Result<ExpectRequest, Vec<Problem>> {
    let mut builder = RunBuilder::default();
    let command = read_until_command(&mut builder, words, expect_option);
    builder.finish_expect(command)
}

/// Reads the options `lookup` knows into `builder` up to the first `--`, and returns the words after it: none when there
/// is no `--`. A word before it that is not an option is a problem.
fn read_until_command(builder: &mut RunBuilder, words: impl Iterator<Item = OsString>, lookup: fn(&str) -> Option<RunOption>) -> Vec<OsString> {
    let mut words = words.peekable();
    while let Some(text) = next_text(&mut words, builder) {
        if text == "--" {
            return words.collect();
        }
        if text.starts_with('-') {
            read_option(builder, &mut words, &text, lookup);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    Vec::new()
}

/// Reads the words after `gate stop-tree`: the pid, and `--state-dir` before it or after it.
pub(crate) fn parse_stop_tree(words: impl Iterator<Item = OsString>) -> Result<StopRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    let mut positional = Vec::new();
    let mut options_ended = false;
    while let Some(text) = next_text(&mut words, &mut builder) {
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
    while let Some(text) = next_text(&mut words, &mut builder) {
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
    while let Some(text) = next_text(&mut words, &mut builder) {
        if text.starts_with('-') && text != "--" {
            read_option(&mut builder, &mut words, &text, push_option);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    builder.finish_push()
}

/// Reads the words after `gate drain`, which takes none: every word is a problem, `--` included, save `--lift`,
/// `--status`, `--builder <host>` and the hidden seams of a build with the `test-seams` feature.
pub(crate) fn parse_drain(words: impl Iterator<Item = OsString>) -> Result<DrainRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    while let Some(text) = next_text(&mut words, &mut builder) {
        if text.starts_with('-') && text != "--" {
            read_option(&mut builder, &mut words, &text, drain_option);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    builder.finish_drain()
}

/// Reads the words after `gate prune`, which takes none: every word is a problem, `--` included, save `--builders`,
/// which it needs, and the hidden seams of a build with the `test-seams` feature.
pub(crate) fn parse_prune(words: impl Iterator<Item = OsString>) -> Result<PruneRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    while let Some(text) = next_text(&mut words, &mut builder) {
        if text.starts_with('-') && text != "--" {
            read_option(&mut builder, &mut words, &text, prune_option);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    builder.finish_prune()
}

/// Reads the words after `gate check`, which takes none: every word is a problem, `--` included, save `--builder
/// <host>` and the hidden seams of a build with the `test-seams` feature.
pub(crate) fn parse_check(words: impl Iterator<Item = OsString>) -> Result<CheckRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    while let Some(text) = next_text(&mut words, &mut builder) {
        if text.starts_with('-') && text != "--" {
            read_option(&mut builder, &mut words, &text, check_option);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    builder.finish_check()
}

/// Reads the words after `gate warm`, which takes none: every word is a problem, `--` included, save `--cd <folder>`,
/// `--background`, `--state-dir <path>` and the hidden seams of a build with the `test-seams` feature.
pub(crate) fn parse_warm(words: impl Iterator<Item = OsString>) -> Result<WarmRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    while let Some(text) = next_text(&mut words, &mut builder) {
        if text.starts_with('-') && text != "--" {
            read_option(&mut builder, &mut words, &text, warm_option);
        } else {
            builder.problems.push(Problem::UnexpectedArgument(text));
        }
    }
    builder.finish_warm()
}

/// Reads the words after `gate burn`: the optional `--cores <n>` and the required `--seconds <s>`.
pub(crate) fn parse_burn(words: impl Iterator<Item = OsString>) -> Result<BurnRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut builder = RunBuilder::default();
    while let Some(text) = next_text(&mut words, &mut builder) {
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
        "cd" => RunOption::Value(ValueOption::Cd),
        "bring-tracked" => RunOption::Value(ValueOption::BringTracked),
        "env" => RunOption::Value(ValueOption::Env),
        "reason" => RunOption::Value(ValueOption::Reason),
        "no-markers" => RunOption::Switch(SwitchOption::NoMarkers),
        "local" => RunOption::Switch(SwitchOption::Local),
        "alone" => RunOption::Switch(SwitchOption::Alone),
        "builder" => RunOption::Value(ValueOption::Builder),
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

fn expect_option(name: &str) -> Option<RunOption> {
    match name {
        "slot" => Some(RunOption::Value(ValueOption::Slot)),
        "state-dir" => Some(RunOption::Value(ValueOption::StateDir)),
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
        "test-push-register" => Some(RunOption::Value(ValueOption::PushRegister)),
        _ => None,
    }
}

#[cfg(not(feature = "test-seams"))]
fn push_option(_name: &str) -> Option<RunOption> {
    None
}

fn drain_option(name: &str) -> Option<RunOption> {
    match name {
        "lift" => Some(RunOption::Switch(SwitchOption::Lift)),
        "status" => Some(RunOption::Switch(SwitchOption::Status)),
        "builder" => Some(RunOption::Value(ValueOption::Builder)),
        _ => drain_seam_option(name),
    }
}

/// The wait's poll and still-waiting period as cutover's, its re-raise period and interrupt, and the transport as
/// `gate check --builder` takes it.
#[cfg(feature = "test-seams")]
fn drain_seam_option(name: &str) -> Option<RunOption> {
    match name {
        "test-poll-ms" => Some(RunOption::Value(ValueOption::PollMs)),
        "test-heartbeat-ms" => Some(RunOption::Value(ValueOption::HeartbeatMs)),
        "test-drain-raise-ms" => Some(RunOption::Value(ValueOption::DrainRaiseMs)),
        "test-drain-interrupt-after-list" => Some(RunOption::Switch(SwitchOption::DrainInterruptAfterList)),
        "test-drain-interrupt-before-raise" => Some(RunOption::Switch(SwitchOption::DrainInterruptBeforeRaise)),
        "test-remote-transport" => Some(RunOption::Value(ValueOption::RemoteTransport)),
        "test-remote-ssh" => Some(RunOption::Value(ValueOption::RemoteSsh)),
        _ => None,
    }
}

#[cfg(not(feature = "test-seams"))]
fn drain_seam_option(_name: &str) -> Option<RunOption> {
    None
}

fn prune_option(name: &str) -> Option<RunOption> {
    match name {
        "builders" => Some(RunOption::Switch(SwitchOption::Builders)),
        "quiet" => Some(RunOption::Switch(SwitchOption::Quiet)),
        _ => prune_seam_option(name),
    }
}

/// The transport as `gate check --builder` takes it, and the least age of a gone worktree's mark.
#[cfg(feature = "test-seams")]
fn prune_seam_option(name: &str) -> Option<RunOption> {
    match name {
        "test-remote-transport" => Some(RunOption::Value(ValueOption::RemoteTransport)),
        "test-remote-ssh" => Some(RunOption::Value(ValueOption::RemoteSsh)),
        "test-prune-gone-age-ms" => Some(RunOption::Value(ValueOption::PruneGoneAgeMs)),
        _ => None,
    }
}

#[cfg(not(feature = "test-seams"))]
fn prune_seam_option(_name: &str) -> Option<RunOption> {
    None
}

fn check_option(name: &str) -> Option<RunOption> {
    match name {
        "builder" => Some(RunOption::Value(ValueOption::Builder)),
        _ => check_seam_option(name),
    }
}

#[cfg(feature = "test-seams")]
fn check_seam_option(name: &str) -> Option<RunOption> {
    match name {
        "test-probe-limit-ms" => Some(RunOption::Value(ValueOption::ProbeLimitMs)),
        "test-probe-drain-ms" => Some(RunOption::Value(ValueOption::ProbeDrainMs)),
        "test-sdk-limit-ms" => Some(RunOption::Value(ValueOption::SdkLimitMs)),
        "test-netfx-key" => Some(RunOption::Value(ValueOption::NetfxKey)),
        "test-remote-transport" => Some(RunOption::Value(ValueOption::RemoteTransport)),
        "test-remote-ssh" => Some(RunOption::Value(ValueOption::RemoteSsh)),
        "test-remote-timeout-ms" => Some(RunOption::Value(ValueOption::RemoteTimeoutMs)),
        _ => None,
    }
}

#[cfg(not(feature = "test-seams"))]
fn check_seam_option(_name: &str) -> Option<RunOption> {
    None
}

fn warm_option(name: &str) -> Option<RunOption> {
    match name {
        "cd" => Some(RunOption::Value(ValueOption::Cd)),
        "background" => Some(RunOption::Switch(SwitchOption::Background)),
        "state-dir" => Some(RunOption::Value(ValueOption::StateDir)),
        _ => warm_seam_option(name),
    }
}

/// The warm-up's own wait, then the client seams a routed run takes; `gate warm` has no request printer.
#[cfg(feature = "test-seams")]
fn warm_seam_option(name: &str) -> Option<RunOption> {
    match name {
        "test-warm-wait-ms" => Some(RunOption::Value(ValueOption::WarmWaitMs)),
        "test-print-request" => None,
        _ => seam_option(name),
    }
}

#[cfg(not(feature = "test-seams"))]
fn warm_seam_option(_name: &str) -> Option<RunOption> {
    None
}

#[cfg(feature = "test-seams")]
fn seam_option(name: &str) -> Option<RunOption> {
    match name {
        "test-sample-ms" => Some(RunOption::Value(ValueOption::SampleMs)),
        "test-route-hint-floor-ms" => Some(RunOption::Value(ValueOption::RouteHintFloorMs)),
        "test-load-window-ms" => Some(RunOption::Value(ValueOption::LoadWindowMs)),
        "test-heartbeat-ms" => Some(RunOption::Value(ValueOption::HeartbeatMs)),
        "test-poll-ms" => Some(RunOption::Value(ValueOption::PollMs)),
        "test-drain-poll-ms" => Some(RunOption::Value(ValueOption::DrainPollMs)),
        "test-stall-ms" => Some(RunOption::Value(ValueOption::StallMs)),
        "test-stall-from-output" => Some(RunOption::Switch(SwitchOption::StallFromOutput)),
        "test-diagnostics-budget-ms" => Some(RunOption::Value(ValueOption::DiagnosticsBudgetMs)),
        "test-interrupt-after-samples" => Some(RunOption::Value(ValueOption::InterruptAfterSamples)),
        "test-print-request" => Some(RunOption::Switch(SwitchOption::PrintRequest)),
        "test-interrupt-queued" => Some(RunOption::Switch(SwitchOption::InterruptQueued)),
        "test-interrupt-before-key" => Some(RunOption::Switch(SwitchOption::InterruptBeforeKey)),
        "test-interrupt-before-later" => Some(RunOption::Switch(SwitchOption::InterruptBeforeLater)),
        "test-interrupt-queued-after-fallback" => Some(RunOption::Switch(SwitchOption::InterruptAfterFallback)),
        "test-sampler-fail" => Some(RunOption::Switch(SwitchOption::SamplerFail)),
        "test-own-start-fails" => Some(RunOption::Switch(SwitchOption::OwnStartFails)),
        "test-remote-hint-from-pull" => Some(RunOption::Switch(SwitchOption::RemoteHintFromPull)),
        "test-parent-job-trusted" => Some(RunOption::Switch(SwitchOption::ParentJobTrusted)),
        "test-diagnostics-fail" => Some(RunOption::Switch(SwitchOption::DiagnosticsFail)),
        "test-job-breakaway" => Some(RunOption::Switch(SwitchOption::JobBreakaway)),
        "test-prune-sweep" => Some(RunOption::Switch(SwitchOption::PruneSweep)),
        "test-remote-client" => Some(RunOption::Value(ValueOption::RemoteClient)),
        "test-remote-wait-ms" => Some(RunOption::Value(ValueOption::RemoteWaitMs)),
        "test-key-wait-ms" => Some(RunOption::Value(ValueOption::KeyWaitMs)),
        "test-git-limit-ms" => Some(RunOption::Value(ValueOption::GitLimitMs)),
        "test-git-drain-ms" => Some(RunOption::Value(ValueOption::GitDrainMs)),
        "test-restore-limit-ms" => Some(RunOption::Value(ValueOption::RestoreLimitMs)),
        "test-beside-claim-wait" => Some(RunOption::Value(ValueOption::BesideClaimWait)),
        "test-dotnet-limit-ms" => Some(RunOption::Value(ValueOption::DotnetLimitMs)),
        "test-remote-snapshot" => Some(RunOption::Switch(SwitchOption::RemoteSnapshot)),
        "test-remote-transport" => Some(RunOption::Value(ValueOption::RemoteTransport)),
        "test-remote-ssh" => Some(RunOption::Value(ValueOption::RemoteSsh)),
        "test-remote-retry-pause-ms" => Some(RunOption::Value(ValueOption::RemoteRetryPauseMs)),
        "test-remote-bound-ms" => Some(RunOption::Value(ValueOption::RemoteBoundMs)),
        "test-remote-interrupt-at" => Some(RunOption::Value(ValueOption::RemoteInterruptAt)),
        "test-remote-watch-call" => Some(RunOption::Value(ValueOption::RemoteWatchCall)),
        "test-remote-timeout-ms" => Some(RunOption::Value(ValueOption::RemoteTimeoutMs)),
        "test-remote-backstop-ms" => Some(RunOption::Value(ValueOption::RemoteBackstopMs)),
        "test-remote-snapshot-age-ms" => Some(RunOption::Value(ValueOption::RemoteSnapshotAgeMs)),
        "test-outputs-progress-ms" => Some(RunOption::Value(ValueOption::OutputsProgressMs)),
        "test-local-sdks" => Some(RunOption::Value(ValueOption::LocalSdks)),
        "test-remote-fetch-cap-bytes" => Some(RunOption::Value(ValueOption::RemoteFetchCapBytes)),
        "test-remote-apply-cap-bytes" => Some(RunOption::Value(ValueOption::RemoteApplyCapBytes)),
        "test-remote-outputs-budget-bytes" => Some(RunOption::Value(ValueOption::RemoteOutputsBudgetBytes)),
        "test-remote-inputs-budget-bytes" => Some(RunOption::Value(ValueOption::RemoteInputsBudgetBytes)),
        "test-remote-lfs-budget-bytes" => Some(RunOption::Value(ValueOption::RemoteLfsBudgetBytes)),
        "test-remote-log-cap-bytes" => Some(RunOption::Value(ValueOption::RemoteLogCapBytes)),
        "test-remote-tamper" => Some(RunOption::Value(ValueOption::RemoteTamper)),
        _ => None,
    }
}

#[cfg(not(feature = "test-seams"))]
fn seam_option(_name: &str) -> Option<RunOption> {
    None
}
