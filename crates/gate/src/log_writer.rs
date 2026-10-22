//! The log of a run here while its command runs: one handle to it, opened to append, behind one lock that every line
//! takes, so no line lands inside another. The command's output readers, the job's process lines and the console's
//! notes all write through it: a line or a block of lines in one hold. Once sealed, before the verdict reads the log
//! back, it drops every later line of the command and every process line, still taking the gate's own; once closed it
//! takes nothing, and a note said then is the console's to keep.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use gate_wire::stamp::{self, Tag};

/// The run's log, open to append, and whether it was sealed.
pub(crate) struct LogWriter {
    state: Mutex<State>,
}

struct State {
    /// The open log; `None` once closed.
    file: Option<File>,
    /// Set by [`LogWriter::seal`]: no line of the command and no process line is written after it.
    sealed: bool,
    /// The first write that failed, said when the log is closed.
    failure: Option<io::Error>,
}

impl LogWriter {
    /// Opens the log at `path` to append, creating it when it does not exist.
    ///
    /// # Errors
    ///
    /// The system's error when the log cannot be opened.
    pub(crate) fn open(path: &Path) -> io::Result<Arc<LogWriter>> {
        let file = OpenOptions::new().append(true).create(true).open(path)?;
        Ok(Arc::new(LogWriter {
            state: Mutex::new(State {
                file: Some(file),
                sealed: false,
                failure: None,
            }),
        }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes `bytes`, a line of the command's output or a process line with its line feed, unless the log was sealed
    /// or closed; the seal is looked at under the lock, so a line that waited for it while the log was sealed is dropped.
    pub(crate) fn command(&self, bytes: &[u8]) {
        let mut state = self.state();
        if state.sealed {
            return;
        }
        state.write(bytes);
    }

    /// Writes `text`, a line of the command's output, stamped now and tagged `tag` (as it is when `None`), unless the
    /// log was sealed or closed. The stamp is read under the lock, just before the write, so the stamps of the lines
    /// written live never decrease down the log.
    pub(crate) fn command_line(&self, tag: Option<Tag>, text: &str) {
        let mut state = self.state();
        if state.sealed {
            return;
        }
        let mut line = match tag {
            Some(tag) => stamp::stamped(&stamp::now(), tag, text),
            None => text.to_owned(),
        };
        line.push('\n');
        state.write(line.as_bytes());
    }

    /// Writes the gate's own `texts` as said now, each stamped under the lock and tagged `gat`, in one hold, sealed or
    /// not; `false` when the log was closed and nothing was written, so the caller keeps them.
    pub(crate) fn gate_now(&self, texts: &[&str]) -> bool {
        let mut state = self.state();
        if state.file.is_none() {
            return false;
        }
        let at = stamp::now();
        let mut block = String::new();
        for text in texts {
            block.push_str(&stamp::stamped(&at, Tag::Gat, text));
            block.push('\n');
        }
        state.write(block.as_bytes());
        true
    }

    /// Writes the gate's own `lines`, already stamped, each with a line feed, in one hold, sealed or not; `false` when
    /// the log was closed and nothing was written, so the caller keeps them.
    pub(crate) fn gate(&self, lines: &[String]) -> bool {
        let mut state = self.state();
        if state.file.is_none() {
            return false;
        }
        let mut text = Vec::new();
        for line in lines {
            text.extend_from_slice(line.as_bytes());
            text.push(b'\n');
        }
        state.write(&text);
        true
    }

    /// Seals the log: every later line of the command and every later process line is dropped.
    pub(crate) fn seal(&self) {
        self.state().sealed = true;
    }

    /// Seals and closes the log, so no handle to it is left open; the first write that failed, if one did.
    pub(crate) fn close(&self) -> Option<io::Error> {
        let mut state = self.state();
        state.sealed = true;
        state.file = None;
        state.failure.take()
    }
}

impl State {
    /// Writes `bytes` to the open log, keeping the first failure.
    fn write(&mut self, bytes: &[u8]) {
        let Some(file) = &mut self.file else {
            return;
        };
        if let Err(error) = file.write_all(bytes).and_then(|()| file.flush()) {
            self.failure.get_or_insert(error);
        }
    }
}

/// A reader's way into the log: each line is one of the command's ([`LogWriter::command_line`]). It never fails, so a
/// log that cannot be written never stops a reader draining its pipe.
pub(crate) struct LogSink(Arc<LogWriter>);

impl LogSink {
    pub(crate) fn new(writer: &Arc<LogWriter>) -> LogSink {
        LogSink(Arc::clone(writer))
    }

    /// The log the lines go to.
    pub(crate) fn writer(&self) -> &LogWriter {
        &self.0
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot write or read its files must fail loudly")]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;

    use gate_wire::stamp::{Tag, unstamped};

    use super::LogWriter;

    /// A fresh log path for one test, under the system's temporary folder.
    fn log(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-log-writer-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        fs::create_dir_all(&folder).expect("the test folder can be made");
        folder.join("out.log")
    }

    fn read(path: &PathBuf) -> String {
        String::from_utf8(fs::read(path).expect("the log reads")).expect("the log is UTF-8")
    }

    #[test]
    fn a_sealed_log_drops_the_commands_lines_and_keeps_the_gates() {
        let path = log("sealed");
        let writer = LogWriter::open(&path).expect("the log opens");
        writer.command(b"before\n");
        writer.seal();
        writer.command(b"after\n");

        assert!(writer.gate(&["gate: released".to_owned()]), "the gate's lines are taken after the seal");
        assert!(writer.close().is_none());
        assert_eq!(read(&path), "before\ngate: released\n");
    }

    #[test]
    fn a_closed_log_takes_nothing_and_holds_no_handle() {
        let path = log("closed");
        let writer = LogWriter::open(&path).expect("the log opens");
        writer.gate(&["one".to_owned(), "two".to_owned()]);
        writer.close();

        assert!(!writer.gate(&["late".to_owned()]), "a gate line after the close is the caller's to keep");
        writer.command(b"late\n");
        assert_eq!(read(&path), "one\ntwo\n");
        fs::remove_file(&path).expect("no handle is left open on the closed log");
    }

    /// How many lines each writer of the order case writes.
    const LINES_EACH: usize = 2_000;

    #[test]
    fn the_stamps_of_the_lines_written_live_never_decrease_down_the_log() {
        let path = log("order");
        let writer = LogWriter::open(&path).expect("the log opens");
        let writers: Vec<_> = [Some(Tag::Out), Some(Tag::Err), None]
            .into_iter()
            .map(|tag| {
                let writer = Arc::clone(&writer);
                thread::spawn(move || {
                    for index in 0..LINES_EACH {
                        let text = format!("line {index}");
                        match tag {
                            Some(tag) => writer.command_line(Some(tag), &text),
                            None => assert!(writer.gate_now(&[&text]), "the log is open"),
                        }
                    }
                })
            })
            .collect();
        for thread in writers {
            thread.join().expect("a writer does not panic");
        }
        writer.close();

        let text = read(&path);
        let parts: Vec<_> = text.lines().map(unstamped).collect();
        assert_eq!(parts.len(), 3 * LINES_EACH, "every line is written");
        for tag in [Tag::Out, Tag::Err, Tag::Gat] {
            let own: Vec<_> = parts
                .iter()
                .filter(|part| part.tag == Some(tag))
                .map(|part| part.text.to_owned())
                .collect();
            let written: Vec<_> = (0..LINES_EACH).map(|index| format!("line {index}")).collect();
            assert_eq!(own, written, "each stream's lines keep their own order");
        }
        let stamps: Vec<_> = parts.iter().map(|part| part.stamp.expect("every line is stamped")).collect();
        assert!(stamps.windows(2).all(|pair| pair[0] <= pair[1]), "the stamps never decrease down the log");
    }

    #[test]
    fn the_log_is_appended_to() {
        let path = log("append");
        fs::write(&path, "earlier\n").expect("the log can be written");
        let writer = LogWriter::open(&path).expect("the log opens");
        writer.command(b"later\n");
        writer.close();

        assert_eq!(read(&path), "earlier\nlater\n");
    }
}
