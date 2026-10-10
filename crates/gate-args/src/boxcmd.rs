//! The hidden `gate box <verb> <payload>`: the builder side of a routed run. Only the words are read here; the payload
//! word is passed on unread, since decoding it is the binary's. A `test-seams` build also reads the hidden
//! `--test-heartbeat-ms <n>` (or `--test-heartbeat-ms=<n>`) before the verb; without the feature it is no verb, and so
//! the box usage.

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
        _ => None,
    }
}

/// The seam that sets the heartbeat's period.
#[cfg(feature = "test-seams")]
const HEARTBEAT_MS: &str = "--test-heartbeat-ms";

/// Reads the seams before the verb, each problem in `problems`: a value from the word itself after `=` or from the next
/// word unless that word is another `--` option, whole milliseconds above 0, given once.
#[cfg(feature = "test-seams")]
fn seams(words: &mut Peekable<impl Iterator<Item = OsString>>, problems: &mut Vec<Problem>) -> BoxSeams {
    let mut seams = BoxSeams::default();
    let mut given = false;
    while let Some(word) = words.next_if(|word| is_heartbeat_ms(word)) {
        let value = match word.to_string_lossy().split_once('=') {
            Some((_, value)) => Ok(Some(value.to_owned())),
            None => crate::run::next_value(words, |next| next.starts_with("--")),
        };
        if given {
            problems.push(Problem::GivenTwice(HEARTBEAT_MS.to_owned()));
            continue;
        }
        given = true;
        match heartbeat_ms(value) {
            Ok(ms) => seams.heartbeat_ms = Some(ms),
            Err(problem) => problems.push(problem),
        }
    }
    seams
}

/// Whether `word` is `--test-heartbeat-ms` or `--test-heartbeat-ms=<value>`.
#[cfg(feature = "test-seams")]
fn is_heartbeat_ms(word: &OsStr) -> bool {
    word.to_str()
        .and_then(|text| text.strip_prefix(HEARTBEAT_MS))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
}

/// The heartbeat period read from `value`; an empty or missing value is no value.
#[cfg(feature = "test-seams")]
fn heartbeat_ms(value: Result<Option<String>, Problem>) -> Result<u64, Problem> {
    match value? {
        Some(value) if !value.is_empty() => crate::values::above_zero(HEARTBEAT_MS, &value),
        _ => Err(Problem::NeedsValue(HEARTBEAT_MS.to_owned())),
    }
}

/// No seams without the `test-seams` feature: a `--test-*` word is read as the verb, and refused as one.
#[cfg(not(feature = "test-seams"))]
fn seams(_words: &mut Peekable<impl Iterator<Item = OsString>>, _problems: &mut Vec<Problem>) -> BoxSeams {
    BoxSeams::default()
}
