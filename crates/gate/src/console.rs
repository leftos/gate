//! Where the gate says its lines: each one written to stdout or stderr and flushed as it is said, so a waiting gate
//! can speak and a killed gate's lines survive. The gate's own lines are also notes for its log, each stamped when it
//! is said: written to the log as it is said while a [`LogWriter`] is attached, else held, in order, until one is
//! attached or a run appends them to the end of its log.

use std::io::{self, Write};
use std::sync::Arc;

use gate_wire::stamp::{self, Tag};
use jiff::Zoned;

use crate::log_writer::LogWriter;

/// A note: the gate's own line, and when it was said.
pub(crate) struct Note {
    at: Zoned,
    text: String,
}

impl Note {
    /// The note as the log writes it: stamped when it was said and tagged as the gate's.
    pub(crate) fn line(&self) -> String {
        stamp::stamped(&self.at, Tag::Gat, &self.text)
    }
}

/// The gate's two output streams, the notes held since a run last took them, and the log they are written to as they
/// are said, when one is attached.
pub(crate) struct Console {
    stdout: io::Stdout,
    stderr: io::Stderr,
    notes: Vec<Note>,
    writer: Option<Arc<LogWriter>>,
}

impl Console {
    pub(crate) fn new() -> Console {
        Console {
            stdout: io::stdout(),
            stderr: io::stderr(),
            notes: Vec::new(),
            writer: None,
        }
    }

    /// Says `line` on stdout, without gathering it: an answer, a marker line or a line of the log's tail.
    pub(crate) fn out(&mut self, line: &str) {
        write_line(&mut self.stdout.lock(), line);
    }

    /// Says the gate's own `line` on stdout, and gathers it as a note: the passed line.
    pub(crate) fn note_out(&mut self, line: &str) {
        self.out(line);
        self.gather(&[line]);
    }

    /// Says `line` on stderr, and gathers it as a note.
    pub(crate) fn err(&mut self, line: &str) {
        write_line(&mut self.stderr.lock(), line);
        self.gather(&[line]);
    }

    /// Gathers `line` as a note for the log without saying it on stdout or stderr.
    pub(crate) fn note(&mut self, line: &str) {
        self.gather(&[line]);
    }

    /// Gathers `lines` as notes for the log, without saying them, written in one hold of the attached log so they
    /// stand together there: a kill's diagnostics, a load block.
    pub(crate) fn note_block(&mut self, lines: &[String]) {
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        self.gather(&lines);
    }

    /// Writes `lines` to the attached log in one hold, stamped there, or stamps them now and holds them when none is
    /// attached or it was closed.
    fn gather(&mut self, lines: &[&str]) {
        if let Some(writer) = &self.writer
            && writer.gate_now(lines)
        {
            return;
        }
        let at = stamp::now();
        self.notes.extend(lines.iter().map(|text| Note {
            at: at.clone(),
            text: (*text).to_owned(),
        }));
    }

    /// Writes the held notes to `writer` in order, each with the stamp of when it was said, then writes every later
    /// note to it as it is said, until [`Console::detach`]. Notes the writer does not take stay held.
    pub(crate) fn attach(&mut self, writer: &Arc<LogWriter>) {
        if writer.gate(&self.notes.iter().map(Note::line).collect::<Vec<_>>()) {
            self.notes.clear();
        }
        self.writer = Some(Arc::clone(writer));
    }

    /// Holds every later note again, as before [`Console::attach`].
    pub(crate) fn detach(&mut self) {
        self.writer = None;
    }

    /// The texts of the notes held since the last take, in the order they were said.
    #[cfg(test)]
    pub(crate) fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes).into_iter().map(|note| note.text).collect()
    }

    /// The notes held since the last take, in the order they were said.
    pub(crate) fn take_held(&mut self) -> Vec<Note> {
        std::mem::take(&mut self.notes)
    }

    /// Puts back `notes` a run took but could not write, before any gathered since, so the next take has them.
    pub(crate) fn put_back_notes(&mut self, mut notes: Vec<Note>) {
        notes.append(&mut self.notes);
        self.notes = notes;
    }
}

/// Where a reader of the state root says a problem line it meets: on stderr and in the log, through a [`Console`], or
/// kept in a list, as a routing decision keeps its figures' problems among its notes for the log.
pub(crate) trait Say {
    /// Says `line`, or keeps it.
    fn say(&mut self, line: &str);
}

impl Say for Console {
    fn say(&mut self, line: &str) {
        self.err(line);
    }
}

impl Say for Vec<String> {
    fn say(&mut self, line: &str) {
        self.push(line.to_owned());
    }
}

fn write_line(stream: &mut impl Write, line: &str) {
    // A write that fails leaves no stream to say so on; the status still carries the gate's answer.
    let _ = writeln!(stream, "{line}").and_then(|()| stream.flush());
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot write or read its files must fail loudly")]
mod tests {
    use std::fs;

    use gate_wire::stamp::{Tag, unstamped};

    use super::Console;
    use crate::log_writer::LogWriter;

    #[test]
    fn a_note_is_gathered_in_order_with_the_lines_said() {
        let mut console = Console::new();
        console.err("gate: said first");
        console.note("gate: noted, not said");
        console.err("gate: said last");

        assert_eq!(console.take_notes(), ["gate: said first", "gate: noted, not said", "gate: said last"]);
        assert!(console.take_notes().is_empty(), "a take empties the notes");
    }

    #[test]
    fn a_held_note_keeps_the_stamp_of_when_it_was_said() {
        let mut console = Console::new();
        console.err("gate: first");

        let held = console.take_held();

        let line = held[0].line();
        let parts = unstamped(&line);
        assert_eq!((parts.tag, parts.text), (Some(Tag::Gat), "gate: first"), "{line}");
    }

    #[test]
    fn an_attached_console_writes_its_held_notes_then_each_new_one_until_detached() {
        let folder = crate::test_support::temp_dir().join(format!("gate-console-attach-{}", std::process::id()));
        fs::create_dir_all(&folder).expect("the test folder can be made");
        let path = folder.join("out.log");
        let _ = fs::remove_file(&path);
        let writer = LogWriter::open(&path).expect("the log opens");
        let mut console = Console::new();
        console.err("gate: held");

        console.attach(&writer);
        console.note("gate: live");
        console.note_block(&["gate: one".to_owned(), "gate: two".to_owned()]);
        writer.close();
        console.note("gate: after the close");
        console.detach();
        console.note("gate: detached");

        let text = String::from_utf8(fs::read(&path).expect("the log reads")).expect("the log is UTF-8");
        let texts: Vec<_> = text.lines().map(|line| unstamped(line).text.to_owned()).collect();
        assert_eq!(texts, ["gate: held", "gate: live", "gate: one", "gate: two"], "{text}");
        assert!(text.lines().all(|line| unstamped(line).tag == Some(Tag::Gat)), "{text}");
        assert_eq!(console.take_notes(), ["gate: after the close", "gate: detached"]);
        fs::remove_dir_all(&folder).expect("the test folder can be removed");
    }
}
