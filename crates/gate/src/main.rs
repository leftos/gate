//! `gate.exe`: runs a heavy command under a machine-wide queue of CPU slots. This build reads its command line,
//! resolves the state root and the log path, and answers `--version` and `--help`; running a command and stopping a
//! tree are not built yet.

mod report;
mod request_json;
mod resolve;

use std::io::{self, Write};
use std::process::ExitCode;

use crate::report::Report;
use crate::resolve::Environment;

fn main() -> ExitCode {
    let report = report::report(std::env::args_os().skip(1).collect(), &Environment::from_process());
    // A write that fails leaves no stream to say so on; the status still carries the gate's answer.
    let _ = write_report(&report);
    ExitCode::from(report.status)
}

fn write_report(report: &Report) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    for line in &report.stdout {
        writeln!(stdout, "{line}")?;
    }
    stdout.flush()?;
    let mut stderr = io::stderr().lock();
    for line in &report.stderr {
        writeln!(stderr, "{line}")?;
    }
    stderr.flush()
}
