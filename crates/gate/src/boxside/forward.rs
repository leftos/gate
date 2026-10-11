//! The output of the command and of the processes the box starts around it forwarded: one thread per pipe reads it
//! line by line (CR LF, LF and a lone CR each end a line, the bytes read as UTF-8 with U+FFFD for what is not) and
//! writes each line, LF-terminated, to the box's own stdout or stderr under one lock every such line takes, so no line
//! lands inside another. A write that fails ends that stream's writing and sets the flag a process's readers share, the
//! run's signal that the client closed the connection; its reader goes on draining the pipe, so the process never
//! blocks on a full one. Once the readers are stopped they drop every later line, one already waiting for the lock
//! included. The threads start no process.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::OwnedHandle;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// How long the readers get, once the command's main process has exited, to reach the end of its output.
pub(crate) const DRAIN: Duration = Duration::from_secs(2);

/// The lock every line written to the box's own stdout or stderr from a thread other than the run's own takes: the
/// lines forwarded from the command, the restore and git, and the heartbeat, each written whole under it; the run holds
/// it while the box is away on the command's UTF-8 console, so no such line goes there. The run's own console lines need
/// not take it: each is said on the run's own thread, never while that thread is away, and written with its line feed
/// under std's lock on the stream, which every line written under this lock also takes, so no locked line lands inside
/// one. git, a process of its own, could write inside a heartbeat if it shared the box's stderr, so its output is
/// forwarded through [`ChildPipes`] instead.
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

/// The readers of one process's output, one per pipe.
pub(crate) struct Forwarders {
    stopped: Arc<AtomicBool>,
    /// Set by a reader whose write to the box's own output failed: the client's connection is gone.
    pipe_closed: Arc<AtomicBool>,
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
        Forwarders::start_each([(stdout, Stream::Out), (stderr, Stream::Err)], "the command's", lock)
    }

    /// Starts a reader for each of `pipes`, writing its lines to the box's stream it names under `lock`; a read that
    /// fails is said naming the pipe `source`'s, as in `the command's stdout`.
    fn start_each(pipes: impl IntoIterator<Item = (File, Stream)>, source: &'static str, lock: &OutputLock) -> io::Result<Forwarders> {
        let stopped = Arc::new(AtomicBool::new(false));
        let pipe_closed = Arc::new(AtomicBool::new(false));
        let (sender, ended) = mpsc::channel();
        let mut started = 0;
        for (pipe, stream) in pipes {
            let sink: Box<dyn Write + Send> = match stream {
                Stream::Out => Box::new(io::stdout()),
                Stream::Err => Box::new(io::stderr()),
            };
            let forwarder = Forwarder {
                lock: Arc::clone(lock),
                stopped: Arc::clone(&stopped),
                pipe_closed: Arc::clone(&pipe_closed),
                stream,
                source,
                sink,
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
        Ok(Forwarders {
            stopped,
            pipe_closed,
            ended,
            started,
        })
    }

    /// Whether a reader's write to the box's own output has failed: the client's connection is gone, and the run stops
    /// the command's job for it.
    pub(crate) fn pipe_closed(&self) -> bool {
        self.pipe_closed.load(Ordering::SeqCst)
    }

    /// Waits at most `limit` for every reader to reach the end of its pipe, then stops them, so they drop every later
    /// line; what they reached, and whether a write to the box's own output failed meanwhile, the last moment a
    /// reader can still write.
    pub(crate) fn drain(self, limit: Duration) -> Drained {
        let deadline = Instant::now() + limit;
        let mut ended = 0;
        while ended < self.started {
            if self.ended.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_err() {
                break;
            }
            ended += 1;
        }
        self.stopped.store(true, Ordering::SeqCst);
        Drained {
            all_ended: ended == self.started,
            pipe_closed: self.pipe_closed.load(Ordering::SeqCst),
        }
    }
}

/// The streams a child the box starts itself is given, and the readers forwarding what it writes to the box's own
/// streams line by line under the output lock, as the command's output is forwarded.
pub(crate) struct ChildPipes {
    /// The child's stdout: a pipe forwarded to the box's stdout, or, when the caller reads it itself,
    /// [`Stdio::piped`].
    pub(crate) stdout: Stdio,
    /// The child's stderr: a pipe forwarded to the box's stderr.
    pub(crate) stderr: Stdio,
    pub(crate) forwarders: Forwarders,
}

impl ChildPipes {
    /// A pipe forwarded to the box's stderr and, when `forward_stdout`, one forwarded to its stdout, written under
    /// `lock`, a read that fails said naming the pipe `source`'s. The readers end once every copy of the write ends is
    /// closed, the child's and the [`Stdio`]s given here.
    ///
    /// # Errors
    ///
    /// The system's error when a pipe cannot be made or a reader cannot be started.
    pub(crate) fn open(source: &'static str, forward_stdout: bool, lock: &OutputLock) -> io::Result<ChildPipes> {
        let (err_reader, err_writer) = io::pipe()?;
        let mut readers = Vec::new();
        let stdout = if forward_stdout {
            let (out_reader, out_writer) = io::pipe()?;
            readers.push((File::from(OwnedHandle::from(out_reader)), Stream::Out));
            Stdio::from(out_writer)
        } else {
            Stdio::piped()
        };
        readers.push((File::from(OwnedHandle::from(err_reader)), Stream::Err));
        let forwarders = Forwarders::start_each(readers, source, lock)?;
        Ok(ChildPipes {
            stdout,
            stderr: Stdio::from(err_writer),
            forwarders,
        })
    }
}

/// What one [`Forwarders::drain`] found.
pub(crate) struct Drained {
    /// Whether every reader reached the end of its pipe within the limit, which it does not while a process the
    /// forwarded process left running holds the pipe.
    pub(crate) all_ended: bool,
    /// Whether a reader's write to the box's own output failed meanwhile: the client's connection is gone.
    pub(crate) pipe_closed: bool,
}

/// One pipe's reader: where its lines go, and whether its writing has ended.
struct Forwarder {
    lock: OutputLock,
    stopped: Arc<AtomicBool>,
    /// Shared with the run: set when a write to the box's own output fails.
    pipe_closed: Arc<AtomicBool>,
    stream: Stream,
    /// Whose pipe it reads, as a read failure names it: `the command's`, `git's`.
    source: &'static str,
    /// The box's stream the lines go to, `stream`'s.
    sink: Box<dyn Write + Send>,
    /// Set by the first write to the box's stream that fails; no later line is written by this reader.
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
        // Looked at again under the lock: the readers may have been stopped while this one waited for it.
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        if write_flushed(&mut self.sink, text.as_bytes()).is_err() {
            self.write_failed = true;
            self.pipe_closed.store(true, Ordering::SeqCst);
        }
    }

    /// Says on stderr that the pipe could not be read; when stderr cannot take it either, nothing is left to say it on.
    fn say_read_failure(&self, error: &io::Error) {
        let line = format!("gate: remote could not read {} {}: {error}\n", self.source, self.stream.name());
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
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::thread;
    use std::time::Duration;

    use super::{Forwarder, LineSplitter, OutputLock, Stream};

    /// How long the stop case lets its reader run before it stops the readers, so the reader is past its first look at
    /// the stop and waits for the lock: only the case's power to go red rests on it. A reader slower than this looks
    /// after the stop and writes nothing either way, so no delay can turn the case red.
    const HEAD_START: Duration = Duration::from_millis(200);

    /// A sink that keeps every byte written to it.
    #[derive(Clone, Default)]
    struct Recording(Arc<Mutex<Vec<u8>>>);

    impl Write for Recording {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose reader thread fails must fail loudly")]
    fn a_reader_waiting_for_the_lock_when_the_readers_stop_writes_nothing() {
        let lock = OutputLock::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let written = Recording::default();
        let mut forwarder = Forwarder {
            lock: Arc::clone(&lock),
            stopped: Arc::clone(&stopped),
            pipe_closed: Arc::default(),
            stream: Stream::Err,
            source: "the test's",
            sink: Box::new(written.clone()),
            write_failed: false,
        };
        let held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (writing, started) = mpsc::channel();
        let reader = thread::spawn(move || {
            writing.send(()).expect("the case listens until the reader starts");
            forwarder.write(b"late");
        });
        started.recv().expect("the reader starts");
        thread::sleep(HEAD_START);

        stopped.store(true, Ordering::SeqCst);
        drop(held);
        reader.join().expect("the reader does not panic");

        assert!(
            written.0.lock().unwrap_or_else(PoisonError::into_inner).is_empty(),
            "a line that waited for the lock while the readers stopped is dropped"
        );
    }

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
