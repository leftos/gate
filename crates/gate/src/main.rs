//! `gate.exe`: runs a heavy command under a machine-wide queue of CPU slots. This build reads its command line,
//! resolves the state root and the log path, and answers `--version` and `--help`, saying each line as it goes;
//! running a command and stopping a tree are not built yet. A `test-seams` build journals a run it does not launch
//! (`--test-journal-run`).

mod answer;
mod console;
mod journal;
mod request_json;
mod resolve;

use std::process::ExitCode;

use crate::console::Console;
use crate::resolve::Environment;

fn main() -> ExitCode {
    let status = answer::answer(std::env::args_os().skip(1).collect(), &Environment::from_process(), &mut Console::new());
    ExitCode::from(status)
}
