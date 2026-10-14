//! The hidden `gate box <verb> <payload>`: the builder side of a routed run. Only the words are read here; the payload
//! word is passed on unread, since decoding it is the binary's. A `test-seams` build also reads the hidden
//! `--test-heartbeat-ms <n>`, `--test-box-console fail-before`, `--test-box-token refused`, `--test-box-park-launch <path>`,
//! `--test-outputs-budget-bytes <n>` and `--test-dotnet-limit-ms <n>` (each also as `--<seam>=<value>`) before the verb;
//! without the feature each is no verb, and so the box usage.

use std::ffi::{OsStr, OsString};
use std::iter::Peekable;

use crate::problem::Problem;
use crate::request::{BoxRequest, BoxSeams, BoxVerb};

/// Reads the words after `gate box`: the seams, then a verb, then one non-empty payload word, and nothing after it. A
/// seam refused is its own problem, as `gate run` reports it; any other shape is the one box usage problem, after
/// those.
pub(crate) fn parse(words: impl Iterator<Item = OsString>) -> Result<BoxRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut problems = Vec::new();
    let seams = seams(&mut words, &mut problems);
    let verb = words.next().as_deref().and_then(OsStr::to_str).and_then(verb);
    let payload = words.next().filter(|payload| !payload.is_empty());
    match (verb, payload, words.next()) {
        (Some(verb), Some(payload), None) if problems.is_empty() => Ok(BoxRequest { verb, payload, seams }),
        (Some(_), Some(_), None) => Err(problems),
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
    /// `--test-dotnet-limit-ms <n>`: how long the preflight's `dotnet --list-sdks` may take.
    DotnetLimitMs,
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
            "--test-dotnet-limit-ms" => Some(Seam::DotnetLimitMs),
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
            Seam::DotnetLimitMs => "--test-dotnet-limit-ms",
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

/// Sets `seam` from `value`: whole milliseconds above 0 for the heartbeat and the dotnet limit, `fail-before` for the
/// console, `refused` for the token, any path for the park, whole bytes above 0 for the outputs budget. An empty or
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
        Seam::DotnetLimitMs => seams.dotnet_limit_ms = Some(crate::values::above_zero(name, &value)?),
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
