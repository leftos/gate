//! Where the box says its lines: stdout for the lines the client parses, stderr for failures and notes. A captured
//! console keeps them instead, so a test reads exactly what a verb said.
//!
//! An [`Output`] may be shared by threads: each line, or each group said together, is written whole under one lock, so
//! no other thread's line splits it, and a write that finds the client gone is remembered.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

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
    let _ = written(stream, line);
}

/// Writes `line` and a newline to `stream` and flushes it; else why not.
fn written(stream: &mut impl Write, line: &str) -> io::Result<()> {
    writeln!(stream, "{line}")?;
    stream.flush()
}

/// One of the box's two output streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Out,
    /// Standard error.
    Err,
}

/// Where an [`Output`] writes its lines, from any thread.
pub trait LineSink: Send + Sync {
    /// Writes `line` and a newline to `stream`, flushed, so the client sees it as it is said.
    ///
    /// # Errors
    ///
    /// Why the line could not be written; a client that has gone is [`io::ErrorKind::BrokenPipe`].
    fn write_line(&self, stream: Stream, line: &str) -> io::Result<()>;
}

/// The process's stdout and stderr. A write to a closed pipe fails with `EPIPE` ([`io::ErrorKind::BrokenPipe`]), as
/// a Rust program ignores `SIGPIPE`.
#[derive(Debug, Default)]
pub struct StdioSink;

impl LineSink for StdioSink {
    fn write_line(&self, stream: Stream, line: &str) -> io::Result<()> {
        match stream {
            Stream::Out => written(&mut io::stdout().lock(), line),
            Stream::Err => written(&mut io::stderr().lock(), line),
        }
    }
}

/// Whether the client has gone: set by the first write that failed with a broken pipe, and never cleared.
#[derive(Debug, Default)]
pub struct ClientGone(AtomicBool);

impl ClientGone {
    /// Whether a write found the client gone.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Remembers that a write found the client gone.
    fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The box's output, shared by its threads: each line, or each group of lines said together, is written whole to the
/// sink under one lock, so no other thread's line comes between them; a write that fails with a broken pipe sets
/// [`ClientGone`].
#[derive(Debug)]
pub struct Output<S: LineSink> {
    sink: S,
    lock: Mutex<()>,
    gone: ClientGone,
}

impl<S: LineSink> Output<S> {
    /// An output writing to `sink`, its client not gone.
    #[must_use]
    pub fn new(sink: S) -> Output<S> {
        Output {
            sink,
            lock: Mutex::new(()),
            gone: ClientGone::default(),
        }
    }

    /// Says `line` on `stream`.
    ///
    /// # Errors
    ///
    /// Why the line could not be written.
    pub fn say(&self, stream: Stream, line: &str) -> io::Result<()> {
        self.say_together(&[(stream, line)])
    }

    /// Says `lines` in order under one hold of the lock, so no other line comes between them; the first line that
    /// cannot be written ends the group.
    ///
    /// # Errors
    ///
    /// Why a line could not be written.
    pub fn say_together(&self, lines: &[(Stream, &str)]) -> io::Result<()> {
        // A thread that panicked while holding the lock left no state behind it: the lock guards only the writes.
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        for (stream, line) in lines {
            if let Err(error) = self.sink.write_line(*stream, line) {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    self.gone.set();
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Whether a write found the client gone.
    #[must_use]
    pub fn client_gone(&self) -> bool {
        self.gone.is_set()
    }

    /// The sink the lines go to.
    #[must_use]
    pub fn sink(&self) -> &S {
        &self.sink
    }
}

/// A sink that keeps every line and its stream, in the order written, for a test to read.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct RecordingSink {
    lines: Mutex<Vec<(Stream, String)>>,
}

#[cfg(test)]
impl RecordingSink {
    /// The lines written so far, with their streams.
    #[must_use]
    pub fn lines(&self) -> Vec<(Stream, String)> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

#[cfg(test)]
impl LineSink for RecordingSink {
    fn write_line(&self, stream: Stream, line: &str) -> io::Result<()> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner).push((stream, line.to_owned()));
        Ok(())
    }
}

/// A sink whose every write fails with an error of its kind, for a test of a client that has gone or a stream that
/// fails otherwise.
#[cfg(test)]
#[derive(Debug)]
pub struct FailingSink {
    kind: io::ErrorKind,
}

#[cfg(test)]
impl FailingSink {
    /// A sink failing every write with `kind`.
    #[must_use]
    pub fn new(kind: io::ErrorKind) -> FailingSink {
        FailingSink { kind }
    }
}

#[cfg(test)]
impl LineSink for FailingSink {
    fn write_line(&self, _stream: Stream, _line: &str) -> io::Result<()> {
        Err(io::Error::from(self.kind))
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::thread;

    use super::{FailingSink, Output, RecordingSink, Stream};

    #[test]
    fn a_recording_sink_keeps_each_line_and_its_stream_in_order() {
        let output = Output::new(RecordingSink::default());
        assert!(output.say(Stream::Out, "gate-remote-started: n").is_ok());
        assert!(output.say(Stream::Err, "a note").is_ok());
        assert_eq!(
            output.sink().lines(),
            [(Stream::Out, "gate-remote-started: n".to_owned()), (Stream::Err, "a note".to_owned())]
        );
        assert!(!output.client_gone());
    }

    #[test]
    fn lines_said_together_are_never_split_by_another_thread_s_line() {
        const THREADS: usize = 8;
        const GROUPS: usize = 200;
        let output = Output::new(RecordingSink::default());
        thread::scope(|scope| {
            for thread in 0..THREADS {
                let output = &output;
                scope.spawn(move || {
                    for group in 0..GROUPS {
                        let (tag, human) = (format!("tag {thread} {group}"), format!("human {thread} {group}"));
                        assert!(output.say_together(&[(Stream::Err, &tag), (Stream::Err, &human)]).is_ok());
                    }
                });
            }
        });
        let lines = output.sink().lines();
        assert_eq!(lines.len(), THREADS * GROUPS * 2);
        for pair in lines.chunks(2) {
            let tag = pair[0].1.strip_prefix("tag ");
            assert!(tag.is_some(), "{pair:?}");
            assert_eq!(pair[1].1.strip_prefix("human "), tag, "{pair:?}");
        }
    }

    #[test]
    fn a_write_that_finds_a_broken_pipe_sets_client_gone() {
        let output = Output::new(FailingSink::new(io::ErrorKind::BrokenPipe));
        let failed = output.say(Stream::Err, "gate-remote-heartbeat: 5");
        assert_eq!(failed.map_err(|error| error.kind()), Err(io::ErrorKind::BrokenPipe));
        assert!(output.client_gone());
    }

    #[test]
    fn a_write_that_fails_otherwise_leaves_client_gone_unset() {
        let output = Output::new(FailingSink::new(io::ErrorKind::Other));
        assert!(output.say_together(&[(Stream::Out, "a"), (Stream::Err, "b")]).is_err());
        assert!(!output.client_gone());
    }
}
