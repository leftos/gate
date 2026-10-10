//! Where the gate says its lines: each one written to stdout or stderr and flushed as it is said, so a waiting gate
//! can speak and a killed gate's lines survive. The gate's own lines are also gathered as notes, which a run appends
//! to the end of its log.

use std::io::{self, Write};

/// The gate's two output streams, and the notes gathered since a run last took them.
pub(crate) struct Console {
    stdout: io::Stdout,
    stderr: io::Stderr,
    notes: Vec<String>,
}

impl Console {
    pub(crate) fn new() -> Console {
        Console {
            stdout: io::stdout(),
            stderr: io::stderr(),
            notes: Vec::new(),
        }
    }

    /// Says `line` on stdout, without gathering it: an answer, a marker line or a line of the log's tail.
    pub(crate) fn out(&mut self, line: &str) {
        write_line(&mut self.stdout.lock(), line);
    }

    /// Says the gate's own `line` on stdout, and gathers it as a note: the passed line.
    pub(crate) fn note_out(&mut self, line: &str) {
        self.out(line);
        self.notes.push(line.to_owned());
    }

    /// Says `line` on stderr, and gathers it as a note.
    pub(crate) fn err(&mut self, line: &str) {
        write_line(&mut self.stderr.lock(), line);
        self.notes.push(line.to_owned());
    }

    /// The notes gathered since the last take, in the order they were said.
    pub(crate) fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// Puts back `notes` a run took but could not write, before any gathered since, so the next take has them.
    pub(crate) fn put_back_notes(&mut self, mut notes: Vec<String>) {
        notes.append(&mut self.notes);
        self.notes = notes;
    }
}

fn write_line(stream: &mut impl Write, line: &str) {
    // A write that fails leaves no stream to say so on; the status still carries the gate's answer.
    let _ = writeln!(stream, "{line}").and_then(|()| stream.flush());
}
