//! Where the gate says its lines: each one written to stdout or stderr and flushed as it is said, so a waiting gate
//! can speak and a killed gate's lines survive.

use std::io::{self, Write};

/// The gate's two output streams.
pub(crate) struct Console {
    stdout: io::Stdout,
    stderr: io::Stderr,
}

impl Console {
    pub(crate) fn new() -> Console {
        Console {
            stdout: io::stdout(),
            stderr: io::stderr(),
        }
    }

    /// Says `line` on stdout.
    pub(crate) fn out(&mut self, line: &str) {
        write_line(&mut self.stdout.lock(), line);
    }

    /// Says `line` on stderr.
    pub(crate) fn err(&mut self, line: &str) {
        write_line(&mut self.stderr.lock(), line);
    }
}

fn write_line(stream: &mut impl Write, line: &str) {
    // A write that fails leaves no stream to say so on; the status still carries the gate's answer.
    let _ = writeln!(stream, "{line}").and_then(|()| stream.flush());
}
