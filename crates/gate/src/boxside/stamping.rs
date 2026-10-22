//! Which of the box's lines are stamped: in `box run` alone, every line but the protocol lines and the heartbeat is
//! written `<stamp> <tag> <text>` as a run's log holds it, so the PC writes it to its log unchanged. The command's
//! streams are `out` and `err`; the box's own lines, git's and the restores' forwarded output and the command job's
//! process lines are `gat`. Each site chooses: [`err`] and [`out`] say a box line, stamped while `box run` answers on
//! this thread ([`Stamps`]), and [`protocol`] a protocol line, never stamped. Every other verb says its lines as
//! they are.
//!
//! The command's job tells the box of each process joining and leaving it ([`ProcessLines`]); a thread of its own
//! writes each as a `gat` process line under the run's [`OutputLock`], until the run stops them, so no process line
//! follows the box's final lines.

use std::borrow::Cow;
use std::cell::Cell;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gate_win32::{Job, JobEvent};
use gate_wire::stamp::{self, Tag};
use proc_table::Win32ProcessTable;

use crate::boxside::forward::{ClientGone, OutputLock, Stamping};
use crate::console::Console;

thread_local! {
    /// Whether this thread answers `box run`, whose box lines are stamped.
    static STAMPED: Cell<bool> = const { Cell::new(false) };
}

/// While held, the box lines this thread says are stamped `gat`: `box run` holds it for its whole answer.
pub(crate) struct Stamps {
    was: bool,
}

impl Stamps {
    /// Stamps this thread's box lines until the returned hold is dropped.
    pub(crate) fn begin() -> Stamps {
        Stamps { was: STAMPED.replace(true) }
    }
}

impl Drop for Stamps {
    fn drop(&mut self) {
        STAMPED.set(self.was);
    }
}

/// Whether this thread's box lines are stamped.
fn stamped() -> bool {
    STAMPED.get()
}

/// `line` as the box writes it: stamped now and tagged `gat` while this thread's box lines are stamped, else as it is.
pub(crate) fn box_line(line: &str) -> Cow<'_, str> {
    if stamped() {
        Cow::Owned(stamp::stamped(&stamp::now(), Tag::Gat, line))
    } else {
        Cow::Borrowed(line)
    }
}

/// Says the box's own `line` on stderr, stamped `gat` while this thread's box lines are stamped.
pub(crate) fn err(console: &mut Console, line: &str) {
    console.err(&box_line(line));
}

/// Says the box's own `line` on stdout, stamped `gat` while this thread's box lines are stamped.
pub(crate) fn out(console: &mut Console, line: &str) {
    console.out(&box_line(line));
}

/// Says the protocol `line` on stderr exactly as it is, never stamped: a start marker, a refusal or kill tag, the
/// adjusted line, the outputs line.
pub(crate) fn protocol(console: &mut Console, line: &str) {
    console.err(line);
}

/// How a process the box starts around the command has its output forwarded: tagged `gat` while this thread's box
/// lines are stamped, else as it was read.
pub(crate) fn forwarded() -> Stamping {
    if stamped() { Stamping::Tags(Tag::Gat, Tag::Gat) } else { Stamping::None }
}

/// The box's root line, `gate pid=<pid> ppid=<ppid>`, which a run's process tree starts from.
pub(crate) fn root_line() -> String {
    stamp::root_text(std::process::id(), parent_pid())
}

/// The pid of the process that started this box, as the system's process table names it.
fn parent_pid() -> Option<u32> {
    let me = std::process::id();
    Win32ProcessTable
        .snapshot_without_command_lines()
        .into_iter()
        .find(|process| process.pid == me)
        .map(|process| process.parent_pid)
}

/// How long the end of the command's watch waits for the members of its job still running to end, so each gets its
/// exit line: the rest get a `left` line.
const EVENTS_WAIT: Duration = Duration::from_secs(2);

/// The process lines of the command's job: the thread writing them under the run's lock, and the flag that stops it,
/// set at [`ProcessLines::finish`] or on the drop, so no process line follows the box's final lines.
pub(crate) struct ProcessLines {
    lock: OutputLock,
    stopped: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
}

impl ProcessLines {
    /// Has `job`, before any process joins it, tell a thread of its own of each process that joins and leaves it,
    /// which writes each as a `gat` process line on stderr under `lock`, a failed write setting `gone`.
    ///
    /// # Errors
    ///
    /// Why the job cannot remember its processes, or the thread cannot be started.
    pub(crate) fn start(job: &mut Job, lock: &OutputLock, gone: &ClientGone) -> Result<ProcessLines, String> {
        let (events, received) = mpsc::channel();
        job.remember(Some(events))
            .map_err(|error| format!("cannot remember the processes of the command's job: {error}"))?;
        let stopped = Arc::new(AtomicBool::new(false));
        let writer = spawn_writer(Arc::clone(lock), Arc::clone(&stopped), Arc::clone(gone), received)
            .map_err(|error| format!("cannot start writing the job's process lines: {error}"))?;
        Ok(ProcessLines {
            lock: Arc::clone(lock),
            stopped,
            writer: Some(writer),
        })
    }

    /// Ends `job`'s process lines once the command's watch is over: its members still running get [`EVENTS_WAIT`] to
    /// end, so their exit lines are written, the rest a `left` line; the writer is joined, then stopped.
    pub(crate) fn finish(&mut self, job: &Job) {
        job.finish_events(EVENTS_WAIT);
        if let Some(writer) = self.writer.take()
            && writer.join().is_err()
        {
            let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            let line = stamp::stamped(&stamp::now(), Tag::Gat, "gate: the thread writing the job's process lines failed");
            // Nothing is left to say a failed write on.
            let _ = writeln!(io::stderr().lock(), "{line}");
        }
        self.stop();
    }

    /// Stops the writer, under the lock, so a line it waits to write is dropped.
    fn stop(&self) {
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.stopped.store(true, Ordering::SeqCst);
    }
}

impl Drop for ProcessLines {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the thread that writes each of `events` as a process line on stderr under `lock`, until the job drops its
/// sender, dropping every line once `stopped` is set and every line after a write that failed, which sets `gone`.
fn spawn_writer(lock: OutputLock, stopped: Arc<AtomicBool>, gone: ClientGone, events: Receiver<JobEvent>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name("box job events".to_owned()).spawn(move || {
        let mut failed = false;
        for event in events {
            let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if failed || stopped.load(Ordering::SeqCst) {
                continue;
            }
            let mut line = event_line(event);
            line.push('\n');
            let mut stderr = io::stderr().lock();
            if stderr.write_all(line.as_bytes()).and_then(|()| stderr.flush()).is_err() {
                failed = true;
                gone.store(true, Ordering::SeqCst);
            }
        }
    })
}

/// `event` as a process line: a spawn stamped at the process's kernel create time, an exit at its kernel exit time
/// when known, every other line now.
fn event_line(event: JobEvent) -> String {
    let (at, text) = match event {
        JobEvent::Spawned { pid, ppid, image, at } => (stamp::local(at), stamp::spawn_text(pid, ppid, image.as_deref())),
        JobEvent::Exited { pid, code, at, ran } => {
            let ended = code.map_or(stamp::Ended::Unknown, |code| stamp::Ended::Code(i64::from(code)));
            (at.map_or_else(stamp::now, stamp::local), stamp::exit_text(pid, ended, ran))
        }
        JobEvent::Missed { pid, why } => (stamp::now(), stamp::missed_text(pid, &why)),
        JobEvent::Left { pid } => (stamp::now(), stamp::left_text(pid)),
        JobEvent::Stopped { why } => (stamp::now(), stamp::stopped_text(&why)),
    };
    stamp::stamped(&at, Tag::Gat, &text)
}
