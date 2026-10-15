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
pub(crate) mod probe;
mod run;
mod stop;
mod tools;

use std::io::{self, BufRead, Read};

use gate_args::{BoxRequest, BoxVerb};

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::payload::Payload;
use crate::console::Console;

/// The most bytes the request line on stdin may take, its newline included.
const REQUEST_LIMIT: u64 = 16 * 1024 * 1024;

/// Reads the request from stdin's first line and decodes it, then answers the verb through `console` and returns the
/// exit status. The request is read before the verb is, and before any lock is taken, so a client that stalls holds
/// nothing, and one missing or not base64 JSON fails every verb; here it is a usage error (exit 2), where the wrapper,
/// stopping on the error, exits 1. `run` reads its outputs manifest from the same stdin, as its next line.
pub(crate) fn answer(request: &BoxRequest, console: &mut Console) -> u32 {
    let payload = match read_request(io::stdin().lock()) {
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

/// The request on `input`'s first line, decoded; else the usage line saying why it cannot be read. The line ends at its
/// newline or at the end of the input, and one newline (and a carriage return before it) is not part of it. The read
/// has no deadline: a client that stops sending is ended by its own watchdog, which closes the connection.
fn read_request(input: impl BufRead) -> Result<Payload, String> {
    let mut line = match bounded_line(input, REQUEST_LIMIT) {
        Ok(line) if line.is_empty() => return Err("gate: the box request is missing on standard input".to_owned()),
        Ok(line) => line,
        Err(Unread::OverLimit) => return Err("gate: the box request on standard input reaches 16 MiB without a newline".to_owned()),
        Err(Unread::Failed(error)) => return Err(format!("gate: the box request on standard input could not be read: {error}")),
    };
    if line.ends_with(b"\n") {
        line.pop();
        if line.ends_with(b"\r") {
            line.pop();
        }
    }
    let word = std::str::from_utf8(&line).map_err(|error| format!("gate: box payload is not base64 JSON: the line is not UTF-8: {error}"))?;
    Payload::decode(word)
}

/// Why a line could not be read.
#[derive(Debug)]
enum Unread {
    /// The line reached its limit without a newline.
    OverLimit,
    /// The input failed.
    Failed(io::Error),
}

/// The next line of `input`, up to and with its newline, or to the end of the input; empty at the end of the input. It
/// never waits past the newline and never reads more than `limit` bytes: a line that reaches `limit` without its
/// newline cannot be read.
fn bounded_line(mut input: impl BufRead, limit: u64) -> Result<Vec<u8>, Unread> {
    let mut line = Vec::new();
    input.by_ref().take(limit).read_until(b'\n', &mut line).map_err(Unread::Failed)?;
    if !line.ends_with(b"\n") && line.len() as u64 >= limit {
        return Err(Unread::OverLimit);
    }
    Ok(line)
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
