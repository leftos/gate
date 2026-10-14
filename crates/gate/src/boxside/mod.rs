//! The hidden `gate box`: the builder side of a routed run, one call per verb over ssh, speaking the PowerShell
//! wrapper's lines and statuses. Every path it touches comes in the payload; it takes no state root and writes nothing
//! under one.

mod boot;
mod ceiling;
mod forward;
mod heartbeat;
mod inflight;
pub(crate) mod outputs;
pub(crate) mod payload;
mod preflight;
mod probe;
mod run;
mod stop;
mod tools;

use gate_args::{BoxRequest, BoxVerb};

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::payload::Payload;
use crate::console::Console;

/// Decodes the payload, then answers the verb through `console` and returns the exit status. The payload is decoded
/// before the verb is read, in the wrapper's order, so a payload that is not base64 JSON fails every verb; here it is
/// a usage error (exit 2), where the wrapper, stopping on the error, exits 1.
pub(crate) fn answer(request: &BoxRequest, console: &mut Console) -> u32 {
    let payload = match Payload::decode(&request.payload) {
        Ok(payload) => payload,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    match request.verb {
        BoxVerb::Preflight => preflight::preflight(&payload, &request.seams, console),
        BoxVerb::Boot => boot_status(boot::say(console)),
        BoxVerb::Run => run::run(&payload, &request.seams, console),
        BoxVerb::Stop => stop::stop(&payload, console),
        BoxVerb::Outputs => outputs::outputs(&payload, console),
        BoxVerb::Inflight => inflight::inflight(&payload, console),
    }
}

/// The status of `gate box boot`: 0 when the boot time was read, `FAILED_STATUS` when it was not, so a boot query the
/// client makes after a lost connection carries the failure rather than a status 0 with no time.
fn boot_status(read: bool) -> u32 {
    if read { 0 } else { u32::from(FAILED_STATUS) }
}

#[cfg(test)]
mod tests {
    use crate::answer::FAILED_STATUS;

    use super::boot_status;

    #[test]
    fn a_boot_time_that_cannot_be_read_is_status_1() {
        assert_eq!(boot_status(true), 0, "a read boot time is the boot line's own success");
        assert_eq!(
            boot_status(false),
            u32::from(FAILED_STATUS),
            "an unread boot time is the client's failure to report"
        );
    }
}
