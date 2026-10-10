//! The hidden `gate box`: the builder side of a routed run, one call per verb over ssh, speaking the PowerShell
//! wrapper's lines and statuses. Every path it touches comes in the payload; it takes no state root and writes nothing
//! under one.

mod boot;
mod payload;
mod preflight;

use gate_args::{BoxRequest, BoxVerb};

use crate::answer::{NOT_BUILT_STATUS, USAGE_STATUS};
use crate::boxside::payload::Payload;
use crate::console::Console;

/// Decodes the payload, then answers the verb through `console` and returns the exit status. The payload is decoded
/// before the verb is read, in the wrapper's order, so a payload that is not base64 JSON fails every verb; here it is
/// a usage error (exit 2), where the wrapper, stopping on the error, exits 1. `run`, `stop` and `outputs` are not built
/// yet.
pub(crate) fn answer(request: &BoxRequest, console: &mut Console) -> u32 {
    let payload = match Payload::decode(&request.payload) {
        Ok(payload) => payload,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    match request.verb {
        BoxVerb::Preflight => preflight::preflight(&payload, console),
        BoxVerb::Boot => {
            boot::say(console);
            0
        }
        BoxVerb::Run | BoxVerb::Stop | BoxVerb::Outputs => {
            console.err(&format!("gate: box {} is not built yet", request.verb.as_str()));
            u32::from(NOT_BUILT_STATUS)
        }
    }
}
