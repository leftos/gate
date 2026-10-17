//! The output of the command and of the processes the box starts around it forwarded: one thread per pipe reads it
//! line by line (CR LF, LF and a lone CR each end a line, the bytes read as UTF-8 with U+FFFD for what is not) and
//! writes each line, LF-terminated, to the box's own stdout or stderr under one lock every such line takes, so no line
//! lands inside another. A write that fails ends that stream's writing and sets the flag a process's readers share, the
//! run's signal that the client closed the connection; its reader goes on draining the pipe, so the process never
//! blocks on a full one. Once the readers are stopped they drop every later line, one already waiting for the lock
//! included. A reader that has not reached the end of its pipe when a drain's limit passes is not taken for one a
//! leftover process still holds open while it is only behind — it holds bytes it has read and not written or dropped,
//! or it waits on no read at all, between reads or not yet run — and is waited for up to a hang guard. A read still
//! pending on a pipe holding nothing is a leftover's, and is cancelled; the reader is taken for a leftover only if
//! that read ended through the cancellation, a read that completes anyway forwarding its bytes and being judged
//! again, or if its thread exits without saying it reached the end. The threads start no process.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::OwnedHandle;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;

/// How long the readers get, once the command's main process has exited, to reach the end of its output.
pub(crate) const DRAIN: Duration = Duration::from_secs(2);

/// How long, from the start of a drain, a reader that is only behind is still waited for after the drain's limit: the
/// guard that ends a drain still waiting on a reader that would never finish.
const HANG_GUARD: Duration = Duration::from_secs(30);

/// How often, after the limit, a drain looks whether the readers that have not ended are still behind.
const SLICE: Duration = Duration::from_millis(50);

/// Whether `error` is a read the drain cancelled, which is not a read that failed.
fn was_cancelled(error: &io::Error) -> bool {
    error.raw_os_error() == i32::try_from(ERROR_OPERATION_ABORTED).ok()
}

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
    /// Set when a reader was taken for a leftover once the limit passed: a read of it ended through the cancellation
    /// the drain asked for, or the reader could not be asked. The drain's answer is not that every reader ended.
    leftover: AtomicBool,
    /// One per reader, in the order the readers started.
    behind: Vec<Behind>,
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
        let mut behind = Vec::new();
        for (pipe, stream) in pipes {
            let in_flight = Arc::new(AtomicBool::new(false));
            let ended_reader = Arc::new(AtomicBool::new(false));
            let aborted = Arc::new(AtomicBool::new(false));
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
                in_flight: Arc::clone(&in_flight),
                aborted: Arc::clone(&aborted),
            };
            let sender = sender.clone();
            let reader_ended = Arc::clone(&ended_reader);
            let thread = thread::Builder::new().name(format!("forward {}", stream.name())).spawn(move || {
                forwarder.pump(pipe);
                // Looked at before the send, so a drain asking after the send sees a reader that ended.
                ended_reader.store(true, Ordering::SeqCst);
                // The run stops listening once its drain is over; a reader that ends later has no one to tell.
                let _ = sender.send(());
            })?;
            behind.push(Behind {
                thread,
                in_flight,
                ended: reader_ended,
                aborted,
            });
        }
        Ok(Forwarders {
            stopped,
            pipe_closed,
            ended,
            leftover: AtomicBool::new(false),
            behind,
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
    ///
    /// A reader that has not ended when `limit` passes is waited for further, up to [`HANG_GUARD`] from the start of
    /// the drain, while it is only behind: it holds bytes it has read and has not written or dropped, or it waits on
    /// no read (see [`Forwarders::any_behind`]). A read still pending on a pipe holding nothing is a leftover's, so
    /// the drain cancels it; the reader is taken for a leftover only if that read ended through the cancellation, a
    /// read that completes anyway forwarding its bytes and being judged again. Once no reader is still behind, or the
    /// guard passes, they are stopped as above.
    pub(crate) fn drain(self, limit: Duration) -> Drained {
        self.drain_within(limit, HANG_GUARD)
    }

    /// [`Forwarders::drain`] with a `guard` in place of [`HANG_GUARD`], so a case can watch a reader that is kept
    /// behind stopped by it.
    fn drain_within(self, limit: Duration, guard: Duration) -> Drained {
        let start = Instant::now();
        let limit_end = start + limit;
        let guard_end = start + guard;
        while !self.all_ended() {
            let left = limit_end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self.ended.recv_timeout(left) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Every reader's sender is gone, so no reader will ever say it ended again.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        while !self.all_ended() {
            let left = guard_end.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.any_behind() {
                break;
            }
            match self.ended.recv_timeout(left.min(SLICE)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.stopped.store(true, Ordering::SeqCst);
        Drained {
            all_ended: self.all_ended(),
            pipe_closed: self.pipe_closed.load(Ordering::SeqCst),
        }
    }

    /// Whether every reader reached the end of its pipe, none of them ending through a cancellation the drain asked
    /// for and none left unresolved.
    fn all_ended(&self) -> bool {
        !self.leftover.load(Ordering::SeqCst) && self.behind.iter().all(Behind::reached_end)
    }

    /// Whether a reader that has not ended is only behind, telling a read the kernel has not completed (or one the
    /// reader has not issued yet, between reads or before its thread runs) from a read still pending on a pipe holding
    /// nothing, which only a process the exited command left keeps open: that read is cancelled, and the pump records
    /// whether it ended through the cancellation. `false` once every reader has ended or been taken for a leftover,
    /// which is when the drain stops.
    fn any_behind(&self) -> bool {
        let mut behind = false;
        for reader in &self.behind {
            if reader.resolved() {
                continue;
            }
            if reader.in_flight.load(Ordering::SeqCst) {
                behind = true;
                continue;
            }
            // A read was pending, or none was: the read can still complete with bytes to forward, or end through the
            // cancellation, which the pump marks. Either way the reader is waited for, and judged again. A reader
            // that cannot be asked is taken for a leftover, as one that did not end in time was before.
            if gate_win32::cancel_synchronous_io(&reader.thread).is_err() {
                self.leftover.store(true, Ordering::SeqCst);
                return false;
            }
            behind = true;
        }
        behind
    }
}

/// A reader a drain that has passed its limit watches: its thread, to tell a pending read from one not yet made, and
/// what it holds.
struct Behind {
    /// The reader's thread, whose handle the drain cancels a pending read on, and whose settling says the reader
    /// ended even if it never said so.
    thread: JoinHandle<()>,
    /// Set while the reader holds bytes it has read and has not written or dropped yet.
    in_flight: Arc<AtomicBool>,
    /// Set once the reader reached the end of its pipe, or failed to read it, and is about to say so.
    ended: Arc<AtomicBool>,
    /// Set by the pump when a read ended through the drain's cancellation: a leftover held its pipe open.
    aborted: Arc<AtomicBool>,
}

impl Behind {
    /// Whether the reader ended, it having said so or its thread having exited without saying anything: either way
    /// the drain stops waiting on it.
    fn resolved(&self) -> bool {
        self.ended.load(Ordering::SeqCst) || self.thread.is_finished()
    }

    /// Whether the reader said it reached the end of its pipe, not having ended through a read the drain cancelled.
    /// A reader whose thread exited without saying so reached no end the drain can see.
    fn reached_end(&self) -> bool {
        !self.aborted.load(Ordering::SeqCst) && self.ended.load(Ordering::SeqCst)
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
    /// Whether every reader reached the end of its pipe, within the limit or, behind, before the hang guard: it does
    /// not when a read ended through a cancellation the drain asked for — a process the forwarded process left
    /// running holding its pipe open — when a reader's thread exited without saying it reached the end, or when a
    /// reader could not be asked.
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
    /// Shared with the drain: set while this reader holds bytes it has read and has not written or dropped yet, and
    /// across every write it makes to the box's own output, so the drain never cancels one.
    in_flight: Arc<AtomicBool>,
    /// Shared with the drain: set when a read ended through the drain's cancellation, which is not a read failure.
    aborted: Arc<AtomicBool>,
}

impl Forwarder {
    /// Reads `pipe` to its end, writing each line as it is read. A read that fails ends the reading, said on stderr;
    /// a read the drain cancelled is the drain's own doing and is not said.
    fn pump(mut self, mut pipe: File) {
        let mut lines = LineSplitter::default();
        let mut chunk = [0_u8; 4096];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    self.in_flight.store(true, Ordering::SeqCst);
                    lines.feed(&chunk[..read], |line| self.write(line));
                    self.in_flight.store(false, Ordering::SeqCst);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // The drain cancelled this read: only a read that ended through it is a leftover's, and there is
                // nothing more to read of it.
                Err(error) if was_cancelled(&error) => {
                    self.aborted.store(true, Ordering::SeqCst);
                    break;
                }
                Err(error) => {
                    // Held while the failure is written, so the drain never cancels a write of the reader's.
                    self.in_flight.store(true, Ordering::SeqCst);
                    self.say_read_failure(&error);
                    self.in_flight.store(false, Ordering::SeqCst);
                    break;
                }
            }
        }
        self.in_flight.store(true, Ordering::SeqCst);
        lines.finish(|line| self.write(line));
        self.in_flight.store(false, Ordering::SeqCst);
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
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::windows::io::{AsRawHandle, OwnedHandle};
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Threading::{ResumeThread, SuspendThread};

    use super::{Behind, Forwarder, Forwarders, LineSplitter, OutputLock, Stream};

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

    /// One reader of `pipe`, its lines written to `sink`, wired as [`Forwarders::start_each`] wires a reader: the
    /// drain can be watched on it, and the flags it reports through are its own.
    fn one_reader(pipe: File, sink: Box<dyn Write + Send>, lock: &OutputLock) -> Forwarders {
        let stopped = Arc::new(AtomicBool::new(false));
        let pipe_closed = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(AtomicBool::new(false));
        let reader_ended = Arc::new(AtomicBool::new(false));
        let aborted = Arc::new(AtomicBool::new(false));
        let forwarder = Forwarder {
            lock: Arc::clone(lock),
            stopped: Arc::clone(&stopped),
            pipe_closed: Arc::clone(&pipe_closed),
            stream: Stream::Err,
            source: "the test's",
            sink,
            write_failed: false,
            in_flight: Arc::clone(&in_flight),
            aborted: Arc::clone(&aborted),
        };
        let (sender, ended_rx) = mpsc::channel();
        let ended_by_close = Arc::clone(&reader_ended);
        let thread = thread::spawn(move || {
            forwarder.pump(pipe);
            ended_by_close.store(true, Ordering::SeqCst);
            let _ = sender.send(());
        });
        Forwarders {
            stopped,
            pipe_closed,
            ended: ended_rx,
            leftover: AtomicBool::new(false),
            behind: vec![Behind {
                thread,
                in_flight,
                ended: reader_ended,
                aborted,
            }],
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
            in_flight: Arc::default(),
            aborted: Arc::default(),
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

    /// The drain's limit in the cases: far shorter than [`HOLD`], so the reader is still behind when it passes.
    const LIMIT: Duration = Duration::from_millis(100);

    /// How long the case keeps the output lock from the reader, which has by then read the line and waits to write it.
    const HOLD: Duration = Duration::from_secs(1);

    /// The guard the case gives [`Forwarders::drain_within`] in place of [`super::HANG_GUARD`].
    const GUARD: Duration = Duration::from_secs(1);

    /// How often the guard case's stand-in writes a line, keeping the reader it holds busy past the guard.
    const KEEP_WRITING: Duration = Duration::from_millis(20);

    /// How long the starved case keeps its reader suspended, so the read the kernel completed for it is not picked up
    /// before the drain looks.
    const SUSPENDED: Duration = Duration::from_secs(1);

    /// The bound every wait in the starved case gets, so a regression there fails rather than hangs.
    const CASE_GUARD: Duration = Duration::from_secs(10);

    /// How long the blocking-write case lets the reader wait in its write before the case reads the sink, letting the
    /// write finish.
    const RELEASE: Duration = Duration::from_secs(1);

    /// How long the line the blocking-write case writes is: far more than any pipe holds, so the reader's write of it
    /// blocks in the kernel.
    const LONG_LINE: usize = 1 << 20;

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_still_behind_at_the_drain_limit_on_a_closed_pipe_is_waited_for_not_taken_for_a_leftover() {
        let (reader, mut writer) = io::pipe().expect("the pipe is made");
        writer.write_all(b"last\n").expect("the process's last line is written");
        // The only write end is closed: no process can hold the pipe open, as with a command that exited alone.
        drop(writer);
        let lock = OutputLock::default();
        let (held_tx, held_rx) = mpsc::channel();
        let holder = {
            let lock = lock.clone();
            thread::spawn(move || {
                let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
                held_tx.send(()).expect("the case waits for the hold");
                thread::sleep(HOLD);
            })
        };
        held_rx.recv().expect("the lock is held");
        let pipe = File::from(OwnedHandle::from(reader));
        let forwarders = Forwarders::start_each([(pipe, Stream::Err)], "the test's", &lock).expect("the reader starts");

        let drained = forwarders.drain(LIMIT);

        holder.join().expect("the holder does not panic");
        assert!(
            drained.all_ended,
            "a reader that was only behind, its pipe's write end closed, reaches the end of what the process wrote"
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_idle_on_a_pipe_a_stand_in_holds_open_is_still_taken_for_a_leftover() {
        let (reader, writer) = io::pipe().expect("the pipe is made");
        let lock = OutputLock::default();
        let pipe = File::from(OwnedHandle::from(reader));
        let forwarders = Forwarders::start_each([(pipe, Stream::Err)], "the test's", &lock).expect("the reader starts");
        let started = Instant::now();

        let drained = forwarders.drain(LIMIT);

        let elapsed = started.elapsed();
        drop(writer);
        assert!(
            !drained.all_ended,
            "a pipe a leftover still holds open with nothing to read is a leftover"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "a reader idle on an empty pipe is not waited for: {elapsed:?}"
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_kept_behind_past_the_hang_guard_is_stopped_not_waited_for_forever() {
        let (reader, mut writer) = io::pipe().expect("the pipe is made");
        let lock = OutputLock::default();
        // Held for the whole drain, so the reader read its first line and waits to write it: it never finishes.
        let held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let stop = Arc::new(AtomicBool::new(false));
        let writing = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if writer.write_all(b"more\n").is_err() {
                        break;
                    }
                    thread::sleep(KEEP_WRITING);
                }
            })
        };
        let pipe = File::from(OwnedHandle::from(reader));
        let forwarders = Forwarders::start_each([(pipe, Stream::Err)], "the test's", &lock).expect("the reader starts");
        let started = Instant::now();

        let drained = forwarders.drain_within(LIMIT, GUARD);

        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        writing.join().expect("the writer does not panic");
        drop(held);
        assert!(!drained.all_ended, "a reader kept behind past the guard is stopped with its bytes unread");
        assert!(
            elapsed >= GUARD,
            "the drain waits for a reader that is behind until the guard passes: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(10), "the guard ends the wait, not the reader: {elapsed:?}");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_starved_after_its_read_completed_is_waited_for_and_forwards_the_line() {
        let (reader, mut writer) = io::pipe().expect("the pipe is made");
        // The channel has been used once before the reader starts, so nothing the case sends once the reader may
        // hold the heap allocates.
        let (go, go_rx) = mpsc::channel();
        go.send(()).expect("the channel is primed");
        let lock = OutputLock::default();
        let written = Recording::default();
        let forwarders = one_reader(File::from(OwnedHandle::from(reader)), Box::new(written.clone()), &lock);
        // The resumer holds the reader and lets it go itself: nothing it does between the two allocates, so a reader
        // suspended inside the heap cannot keep it from resuming. It says which it did in `held`.
        let handle_number = forwarders.behind[0].thread.as_raw_handle() as usize;
        let held = Arc::new(AtomicU8::new(0));
        let resumer = {
            let held = Arc::clone(&held);
            thread::spawn(move || {
                go_rx.recv().expect("the channel's prime is taken");
                go_rx.recv().expect("the case says the first line is forwarded");
                let handle = std::ptr::with_exposed_provenance_mut::<std::ffi::c_void>(handle_number);
                // SAFETY: the join handle keeping this thread's handle open lives in the forwarders on the test
                // thread until the drain returns, well after this resume.
                let suspended = unsafe { SuspendThread(handle) } != u32::MAX;
                held.store(if suspended { 1 } else { 2 }, Ordering::SeqCst);
                thread::sleep(SUSPENDED);
                // SAFETY: as above.
                unsafe { ResumeThread(handle) };
            })
        };
        // The reader forwards one line first, so the read it is held in is the one the second line completes.
        writer.write_all(b"first\n").expect("the first line is written");
        let forwarded = Instant::now() + CASE_GUARD;
        while written.0.lock().unwrap_or_else(PoisonError::into_inner).as_slice() != b"first\n".as_slice() {
            assert!(Instant::now() < forwarded, "the first line is forwarded");
            thread::sleep(Duration::from_millis(1));
        }
        go.send(()).expect("the resumer is told");
        let holding = Instant::now() + CASE_GUARD;
        while held.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < holding, "the reader is held");
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(held.load(Ordering::SeqCst), 1, "the reader is suspended");
        writer.write_all(b"second\n").expect("the second line is written");
        // The command exited: no process holds the pipe open.
        drop(writer);

        let drained = forwarders.drain_within(LIMIT, CASE_GUARD);

        resumer.join().expect("the resumer does not panic");
        assert!(
            drained.all_ended,
            "a reader starved before writing what it read is waited for, not taken for a leftover"
        );
        assert_eq!(
            written.0.lock().unwrap_or_else(PoisonError::into_inner).as_slice(),
            b"first\nsecond\n",
            "both lines the reader forwarded are written"
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_blocked_in_a_write_is_waited_for_not_cancelled() {
        let (line_reader, mut line_writer) = io::pipe().expect("the line's pipe is made");
        let (sink_reader, sink_writer) = io::pipe().expect("the sink's pipe is made");
        let lock = OutputLock::default();
        let forwarders = one_reader(File::from(OwnedHandle::from(line_reader)), Box::new(sink_writer), &lock);
        // Far more than any pipe holds, so the reader's write of it blocks in the kernel.
        let mut line = vec![b'x'; LONG_LINE];
        line.push(b'\n');
        line_writer.write_all(&line).expect("the line is written");
        // The command exited: its pipe's write end is closed, so the reader blocks in writing the line on, only.
        drop(line_writer);
        // Reads the sink from the release on, so the reader's blocked write finishes once the drain has waited a while.
        let released = thread::spawn(move || {
            thread::sleep(RELEASE);
            let mut sink_reader = sink_reader;
            let mut got = Vec::new();
            let mut chunk = vec![0_u8; 64 * 1024];
            loop {
                match sink_reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => got.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            got
        });

        let drained = forwarders.drain(LIMIT);

        let got = released.join().expect("the reader of the sink does not panic");
        assert!(drained.all_ended, "a reader blocked in a write is waited for, not taken for a leftover");
        assert!(
            !drained.pipe_closed,
            "the drain waited for the reader's write instead of cutting it short"
        );
        assert_eq!(got, line, "the line the reader waited to write is forwarded whole");
    }

    #[test]
    fn a_reader_whose_thread_exited_without_saying_it_ended_is_not_counted_as_ended() {
        let thread = thread::spawn(|| {});
        let deadline = Instant::now() + CASE_GUARD;
        while !thread.is_finished() {
            assert!(Instant::now() < deadline, "the reader's thread exits");
            thread::sleep(Duration::from_millis(1));
        }
        // The channel is disconnected and no end signal is set: the drain stops waiting on the reader, and, its
        // thread having exited without saying it reached the end, counts it as not ended.
        let (_, ended) = mpsc::channel::<()>();
        let forwarders = Forwarders {
            stopped: Arc::default(),
            pipe_closed: Arc::default(),
            ended,
            leftover: AtomicBool::new(false),
            behind: vec![Behind {
                thread,
                in_flight: Arc::default(),
                ended: Arc::default(),
                aborted: Arc::default(),
            }],
        };
        let started = Instant::now();

        let drained = forwarders.drain(LIMIT);

        assert!(
            !drained.all_ended,
            "a reader whose thread exited without saying it ended is not counted as ended"
        );
        assert!(started.elapsed() < CASE_GUARD, "the drain returns without spinning");
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
