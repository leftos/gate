//! The hidden `gate box <verb> --stdin`: the builder side of a routed run. Only the words are read here; the request
//! is stdin's first line, which the binary reads and decodes. A `test-seams` build also reads the hidden
//! `--test-heartbeat-ms <n>`, `--test-box-console fail-before`, `--test-box-token refused`, `--test-box-park-launch <path>`,
//! `--test-outputs-budget-bytes <n>`, `--test-inputs-budget-bytes <n>`, `--test-dotnet-limit-ms <n>`,
//! `--test-box-sample-ms <n>`, `--test-box-sampler-fail true|false`, `--test-box-free-percent <1-100>`,
//! `--test-box-kill-leaves <n>` and `--test-probe-limit-ms <n>` (each also as `--<seam>=<value>`) before the verb;
//! without the feature each is no verb, and so the box usage.

use std::ffi::{OsStr, OsString};
use std::iter::Peekable;

use crate::problem::Problem;
use crate::request::{BoxRequest, BoxSeams, BoxVerb};

/// The one word after the verb: the request is stdin's first line, never a word.
const STDIN: &str = "--stdin";

/// Reads the words after `gate box`: the seams, then a verb, then exactly `--stdin`, and nothing after it. A seam
/// refused is its own problem, as `gate run` reports it; any other shape is the one box usage problem, after those.
pub(crate) fn parse(words: impl Iterator<Item = OsString>) -> Result<BoxRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut problems = Vec::new();
    let seams = seams(&mut words, &mut problems);
    let verb = words.next().as_deref().and_then(OsStr::to_str).and_then(verb);
    let stdin = words.next().is_some_and(|word| word == STDIN);
    match (verb, stdin, words.next()) {
        (Some(verb), true, None) if problems.is_empty() => Ok(BoxRequest { verb, seams }),
        (Some(_), true, None) => Err(problems),
        _ => {
            problems.push(Problem::BoxUsage);
            Err(problems)
        }
    }
}

fn verb(text: &str) -> Option<BoxVerb> {
    match text {
        "preflight" => Some(BoxVerb::Preflight),
        "run" => Some(BoxVerb::Run),
        "stop" => Some(BoxVerb::Stop),
        "boot" => Some(BoxVerb::Boot),
        "outputs" => Some(BoxVerb::Outputs),
        "inflight" => Some(BoxVerb::Inflight),
        "inputs" => Some(BoxVerb::Inputs),
        _ => None,
    }
}

/// A seam `gate box` reads before the verb.
#[cfg(feature = "test-seams")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seam {
    /// `--test-heartbeat-ms <n>`: the heartbeat's period.
    HeartbeatMs,
    /// `--test-box-console fail-before`: the command's UTF-8 console fails before the command starts.
    Console,
    /// `--test-box-token refused`: the command's start with the restricted token is refused as access denied.
    Token,
    /// `--test-box-park-launch <path>`: the command's launch waits until the path exists.
    ParkLaunch,
    /// `--test-outputs-budget-bytes <n>`: the output folder's outputs size budget, in bytes.
    OutputsBudgetBytes,
    /// `--test-inputs-budget-bytes <n>`: the most bytes one inputs put unpacks, decompressed.
    InputsBudgetBytes,
    /// `--test-dotnet-limit-ms <n>`: how long the preflight's `dotnet --list-sdks` may take.
    DotnetLimitMs,
    /// `--test-box-sample-ms <n>`: the period of the run's samples of its machine.
    SampleMs,
    /// `--test-box-sampler-fail true|false`: every sample of the machine fails.
    SamplerFail,
    /// `--test-box-free-percent <1-100>`: the share of the machine the samples find free.
    FreePercent,
    /// `--test-box-kill-leaves <n>`: how many processes the ending of a job the ceiling killed reports left.
    KillLeaves,
    /// `--test-probe-limit-ms <n>`: how long the preflight's tool probes may take together.
    ProbeLimitMs,
}

#[cfg(feature = "test-seams")]
impl Seam {
    /// The seam `word` names, as `--<seam>` or `--<seam>=<value>`.
    fn named(word: &OsStr) -> Option<Seam> {
        let text = word.to_str()?;
        match text.split_once('=').map_or(text, |(name, _)| name) {
            "--test-heartbeat-ms" => Some(Seam::HeartbeatMs),
            "--test-box-console" => Some(Seam::Console),
            "--test-box-token" => Some(Seam::Token),
            "--test-box-park-launch" => Some(Seam::ParkLaunch),
            "--test-outputs-budget-bytes" => Some(Seam::OutputsBudgetBytes),
            "--test-inputs-budget-bytes" => Some(Seam::InputsBudgetBytes),
            "--test-dotnet-limit-ms" => Some(Seam::DotnetLimitMs),
            "--test-box-sample-ms" => Some(Seam::SampleMs),
            "--test-box-sampler-fail" => Some(Seam::SamplerFail),
            "--test-box-free-percent" => Some(Seam::FreePercent),
            "--test-box-kill-leaves" => Some(Seam::KillLeaves),
            "--test-probe-limit-ms" => Some(Seam::ProbeLimitMs),
            _ => None,
        }
    }

    /// Its option word.
    fn name(self) -> &'static str {
        match self {
            Seam::HeartbeatMs => "--test-heartbeat-ms",
            Seam::Console => "--test-box-console",
            Seam::Token => "--test-box-token",
            Seam::ParkLaunch => "--test-box-park-launch",
            Seam::OutputsBudgetBytes => "--test-outputs-budget-bytes",
            Seam::InputsBudgetBytes => "--test-inputs-budget-bytes",
            Seam::DotnetLimitMs => "--test-dotnet-limit-ms",
            Seam::SampleMs => "--test-box-sample-ms",
            Seam::SamplerFail => "--test-box-sampler-fail",
            Seam::FreePercent => "--test-box-free-percent",
            Seam::KillLeaves => "--test-box-kill-leaves",
            Seam::ProbeLimitMs => "--test-probe-limit-ms",
        }
    }
}

/// Reads the seams before the verb, in any order, each problem in `problems`: a value from the word itself after `=`
/// or from the next word unless that word is another `--` option, each seam given once.
#[cfg(feature = "test-seams")]
fn seams(words: &mut Peekable<impl Iterator<Item = OsString>>, problems: &mut Vec<Problem>) -> BoxSeams {
    let mut seams = BoxSeams::default();
    let mut given: Vec<Seam> = Vec::new();
    // Each word is named once: peeked, and taken only when it names a seam.
    while let Some((seam, word)) = words
        .peek()
        .and_then(|word| Seam::named(word))
        .and_then(|seam| words.next().map(|word| (seam, word)))
    {
        let value = match word.to_string_lossy().split_once('=') {
            Some((_, value)) => Ok(Some(value.to_owned())),
            None => crate::run::next_value(words, |next| next.starts_with("--")),
        };
        if given.contains(&seam) {
            problems.push(Problem::GivenTwice(seam.name().to_owned()));
            continue;
        }
        given.push(seam);
        if let Err(problem) = set_seam(&mut seams, seam, value) {
            problems.push(problem);
        }
    }
    seams
}

/// Sets `seam` from `value`: whole milliseconds above 0 for the heartbeat, the dotnet limit, the sample period and the
/// probe limit, `fail-before` for the console, `refused` for the token, any path for the park, whole bytes above 0 for the
/// outputs and the inputs budgets, `true` or `false` for the sampler's failure, a whole percent from 1 to 100 for the free share, a whole
/// count above 0 for the processes a kill leaves. An empty or
/// missing value is no value.
#[cfg(feature = "test-seams")]
fn set_seam(seams: &mut BoxSeams, seam: Seam, value: Result<Option<String>, Problem>) -> Result<(), Problem> {
    let name = seam.name();
    let value = match value? {
        Some(value) if !value.is_empty() => value,
        _ => return Err(Problem::NeedsValue(name.to_owned())),
    };
    match seam {
        Seam::HeartbeatMs => seams.heartbeat_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::OutputsBudgetBytes => seams.outputs_budget_bytes = Some(crate::values::above_zero(name, &value)?),
        Seam::InputsBudgetBytes => seams.inputs_budget_bytes = Some(crate::values::above_zero(name, &value)?),
        Seam::DotnetLimitMs => seams.dotnet_limit_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::SampleMs => seams.sample_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::SamplerFail => seams.sampler_fail = crate::values::true_or_false(name, &value)?,
        Seam::FreePercent => seams.free_percent = Some(crate::values::in_range(name, &value, 1, 100)?),
        Seam::KillLeaves => seams.kill_leaves = Some(crate::values::above_zero(name, &value)?),
        Seam::ProbeLimitMs => seams.probe_limit_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::Console => seams.fail_console = the_word(name, value, "fail-before", |option, value| Problem::BadConsoleFailure { option, value })?,
        Seam::Token => seams.refuse_token = the_word(name, value, "refused", |option, value| Problem::BadTokenRefusal { option, value })?,
        Seam::ParkLaunch => seams.park_launch = Some(std::path::PathBuf::from(value)),
    }
    Ok(())
}

/// `true` when `value`, given to the seam `name`, is `word`, the one value it takes; else the problem `bad` makes of
/// the seam's name and the value.
#[cfg(feature = "test-seams")]
fn the_word(name: &str, value: String, word: &str, bad: impl FnOnce(String, String) -> Problem) -> Result<bool, Problem> {
    if value == word { Ok(true) } else { Err(bad(name.to_owned(), value)) }
}

/// No seams without the `test-seams` feature: a `--test-*` word is read as the verb, and refused as one.
#[cfg(not(feature = "test-seams"))]
fn seams(_words: &mut Peekable<impl Iterator<Item = OsString>>, _problems: &mut Vec<Problem>) -> BoxSeams {
    BoxSeams::default()
}
