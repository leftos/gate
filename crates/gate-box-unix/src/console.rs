//! Where the box says its lines: stdout for the lines the client parses, stderr for failures and notes. A captured
//! console keeps them instead, so a test reads exactly what a verb said.

use std::io::{self, Write};

/// The box's two output streams, written at once or kept for a test.
pub struct Console {
    echo: bool,
    /// The lines said on stdout, when captured.
    pub out: Vec<String>,
    /// The lines said on stderr, when captured.
    pub err: Vec<String>,
}

impl Console {
    /// A console that writes each line to the process's stdout or stderr as it is said.
    #[must_use]
    pub fn stdio() -> Console {
        Console {
            echo: true,
            out: Vec::new(),
            err: Vec::new(),
        }
    }

    /// A console that keeps each line in [`Console::out`] or [`Console::err`].
    #[must_use]
    pub fn captured() -> Console {
        Console {
            echo: false,
            out: Vec::new(),
            err: Vec::new(),
        }
    }

    /// Says `line` on stdout.
    pub fn out(&mut self, line: &str) {
        if self.echo {
            said(&mut io::stdout().lock(), line);
        } else {
            self.out.push(line.to_owned());
        }
    }

    /// Says `line` on stderr.
    pub fn err(&mut self, line: &str) {
        if self.echo {
            said(&mut io::stderr().lock(), line);
        } else {
            self.err.push(line.to_owned());
        }
    }
}

/// Writes `line` and a newline to `stream` and flushes it, so the client sees each line as it is said.
fn said(stream: &mut impl Write, line: &str) {
    // A stream that cannot be written is a client that has gone: there is no one left to tell, and the verb's status
    // still reaches the ssh session's end.
    let _ = writeln!(stream, "{line}").and_then(|()| stream.flush());
}
