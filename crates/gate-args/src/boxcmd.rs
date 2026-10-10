//! The hidden `gate box <verb> <payload>`: the builder side of a routed run. Only the words are read here; the payload
//! word is passed on unread, since decoding it is the binary's.

use std::ffi::{OsStr, OsString};

use crate::problem::Problem;
use crate::request::{BoxRequest, BoxVerb};

/// Reads the words after `gate box`: a verb, then one non-empty payload word, and nothing after it. Any other shape is
/// the one box usage problem.
pub(crate) fn parse(mut words: impl Iterator<Item = OsString>) -> Result<BoxRequest, Vec<Problem>> {
    let verb = words.next().as_deref().and_then(OsStr::to_str).and_then(verb);
    let payload = words.next().filter(|payload| !payload.is_empty());
    match (verb, payload, words.next()) {
        (Some(verb), Some(payload), None) => Ok(BoxRequest { verb, payload }),
        _ => Err(vec![Problem::BoxUsage]),
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
