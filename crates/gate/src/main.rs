//! `gate.exe`: runs a heavy command under a machine-wide queue of CPU slots. This build reads its command line,
//! resolves the state root and the log path, answers `--version` and `--help`, and runs a command under a slot in a
//! job of its own, watched for a stall on a load-adjusted clock, saying each line as it goes, and stops a process tree
//! with the gates in it. On a builder, the hidden `gate box` answers `boot`, `preflight`, `run` and `stop`. The
//! watchdog's ceiling and backstop and the box's `outputs` are not built yet.

mod acquire;
mod answer;
mod boxside;
mod console;
mod environment;
mod journal;
mod log;
mod request_json;
mod resolve;
mod run;
mod slots;
mod stop_tree;
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
