//! `gate.exe`: runs a heavy command under a machine-wide queue of CPU slots. This build reads its command line,
//! resolves the state root and the log path, answers `--version` and `--help`, decides whether a heavy command routes
//! to a builder, and runs a command under a slot in a job of its own, watched for a stall, its ceiling and the
//! backstop, saying each line as it goes. A routed command runs on the builder through the remote client, and falls back
//! to running here when the builder turns it away. It stops a process tree with the gates in it, moves the machine from
//! the PowerShell gate to itself (`gate cutover`), and copies the installed exe to the builders (`gate push`). On a
//! builder, the hidden `gate box` answers `boot`, `preflight`, `run`, `stop`, `outputs` and `inflight`.

mod acquire;
mod answer;
mod boxside;
mod burn;
mod console;
mod covered;
mod cutover;
mod durations;
mod environment;
mod escaped;
mod expect;
mod journal;
mod late;
mod log;
mod machine;
mod nesting;
mod no_build;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the manifest's plain build serves only tests; without the test seams, the unpack and the manifest writer \
                  serve the dormant remote client too"
    )
)]
mod outputs;
mod pause;
mod priority;
mod push;
mod remote;
mod request_json;
mod resolve;
mod route;
mod run;
mod servers;
mod slots;
mod stop_tree;
mod stopping;
mod version;
mod wait;
mod watch;

use std::io::{self, Write};

use crate::console::Console;
use crate::resolve::Environment;

/// Exits through `std::process::exit`, because `ExitCode` carries 8 bits and the gate exits with its command's whole
/// 32-bit code.
fn main() {
    let status = answer::answer(std::env::args_os().skip(1).collect(), &Environment::from_process(), &mut Console::new());
    // Every line was flushed as it was said; these only guard the exit against one that was not, with no stream left
    // to report a failed flush on.
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    std::process::exit(status.cast_signed());
}
