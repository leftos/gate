//! The command's output forwarded: one thread per pipe reads it line by line (CR LF, LF and a lone CR each end a line,
//! the bytes read as UTF-8 with U+FFFD for what is not) and writes each line, LF-terminated, to the box's own stdout
//! or stderr under one lock every such line takes, so no line lands inside another. A write that fails ends that
//! stream's writing, and its reader goes on draining the pipe, so the command never blocks on a full one. Once the
//! readers are stopped they drop every later line. The threads start no process.

use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// How long the readers get, once the command's main process has exited, to reach the end of its output.
pub(crate) const DRAIN: Duration = Duration::from_secs(2);

/// The lock every line written to the box's own stdout or stderr takes.
pub(crate) type OutputLock = Arc<Mutex<()>>;

/// Which of the box's own streams a pipe's lines go to.
#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Stream::Out => "stdout",
            Stream::Err => "stderr",
        }
    }
}

/// The two readers of a command's output.
pub(crate) struct Forwarders {
    stopped: Arc<AtomicBool>,
    ended: Receiver<()>,
    started: usize,
}

impl Forwarders {
    /// Starts a reader for the command's `stdout` pipe and one for its `stderr` pipe, writing under `lock`.
    ///
    /// # Errors
    ///
    /// The system's error when a thread cannot be started; a reader started before it goes on.
    pub(crate) fn start(stdout: File, stderr: File, lock: &OutputLock) -> io::Result<Forwarders> {
        let stopped = Arc::new(AtomicBool::new(false));
        let (sender, ended) = mpsc::channel();
        let mut started = 0;
        for (pipe, stream) in [(stdout, Stream::Out), (stderr, Stream::Err)] {
            let forwarder = Forwarder {
                lock: Arc::clone(lock),
                stopped: Arc::clone(&stopped),
                stream,
                write_failed: false,
            };
            let sender = sender.clone();
            thread::Builder::new().name(format!("forward {}", stream.name())).spawn(move || {
                forwarder.pump(pipe);
                // The run stops listening once its drain is over; a reader that ends later has no one to tell.
                let _ = sender.send(());
            })?;
            started += 1;
        }
        Ok(Forwarders { stopped, ended, started })
    }

    /// Waits at most `limit` for both readers to reach the end of the output, then stops them, so they drop every
    /// later line; whether both reached it, which they do not while a process the command left running holds a pipe.
    pub(crate) fn drain(self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut ended = 0;
        while ended < self.started {
            if self.ended.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_err() {
                break;
            }
            ended += 1;
        }
        self.stopped.store(true, Ordering::SeqCst);
        ended == self.started
    }
}

/// One pipe's reader: where its lines go, and whether its writing has ended.
struct Forwarder {
    lock: OutputLock,
    stopped: Arc<AtomicBool>,
    stream: Stream,
    /// Set by the first write to the box's stream that fails; no later line is written.
    write_failed: bool,
}

impl Forwarder {
    /// Reads `pipe` to its end, writing each line as it is read. A read that fails ends the reading, said on stderr.
    fn pump(mut self, mut pipe: File) {
        let mut lines = LineSplitter::default();
        let mut chunk = [0_u8; 4096];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => lines.feed(&chunk[..read], |line| self.write(line)),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    self.say_read_failure(&error);
                    break;
                }
            }
        }
        lines.finish(|line| self.write(line));
    }

    /// Writes `line` and a line feed to the box's stream and flushes it, unless the readers were stopped or a write
    /// failed before.
    fn write(&mut self, line: &[u8]) {
        if self.write_failed || self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let mut text = String::from_utf8_lossy(line).into_owned();
        text.push('\n');
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let written = match self.stream {
            Stream::Out => write_flushed(&mut io::stdout().lock(), text.as_bytes()),
            Stream::Err => write_flushed(&mut io::stderr().lock(), text.as_bytes()),
        };
        self.write_failed = written.is_err();
    }

    /// Says on stderr that the pipe could not be read; when stderr cannot take it either, nothing is left to say it on.
    fn say_read_failure(&self, error: &io::Error) {
        let line = format!("gate: remote could not read the command's {}: {error}\n", self.stream.name());
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = write_flushed(&mut io::stderr().lock(), line.as_bytes());
    }
}

fn write_flushed(stream: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    stream.write_all(bytes)?;
    stream.flush()
}

/// Cuts bytes into lines as .NET's `ReadLine` does: CR LF, LF and a lone CR each end a line, and the bytes after the
/// last end are a line of their own when there are any.
#[derive(Default)]
struct LineSplitter {
    line: Vec<u8>,
    after_cr: bool,
}

impl LineSplitter {
    /// Takes `bytes`, giving `emit` each line they end, without its end.
    fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        for &byte in bytes {
            let after_cr = std::mem::take(&mut self.after_cr);
            match byte {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => {
                    emit(&self.line);
                    self.line.clear();
                    self.after_cr = byte == b'\r';
                }
                _ => self.line.push(byte),
            }
        }
    }

    /// Gives `emit` the bytes after the last line end, when there are any.
    fn finish(self, mut emit: impl FnMut(&[u8])) {
        if !self.line.is_empty() {
            emit(&self.line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LineSplitter;

    /// The lines `chunks` read one after another give, each read as UTF-8 with U+FFFD for what is not.
    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut splitter = LineSplitter::default();
        for chunk in chunks {
            splitter.feed(chunk, |line| lines.push(String::from_utf8_lossy(line).into_owned()));
        }
        splitter.finish(|line| lines.push(String::from_utf8_lossy(line).into_owned()));
        lines
    }

    #[test]
    fn a_cr_lf_split_across_two_reads_ends_one_line() {
        assert_eq!(split(&[b"a\r", b"\nb"]), ["a", "b"]);
    }

    #[test]
    fn a_cr_then_a_cr_lf_end_two_lines() {
        assert_eq!(split(&[b"\r\r\n"]), ["", ""]);
    }

    #[test]
    fn the_bytes_after_the_last_end_are_a_line() {
        assert_eq!(split(&[b"x"]), ["x"]);
    }

    #[test]
    fn an_invalid_byte_reads_as_the_replacement_character() {
        assert_eq!(split(&[&[0xff, b'\n']]), ["\u{FFFD}"]);
    }
}
