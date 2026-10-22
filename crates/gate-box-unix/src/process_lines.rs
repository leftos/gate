//! The process lines of `box run`'s command ([`gate_wire::stamp`]): the box's own root line, the command's leader's
//! spawn and exit, and, on macOS, a spawn and an exit for each other process seen in its process group, all stamped
//! `gat` on standard error.
//!
//! The leader's lines come from the runner: its spawn as it starts, before its readers, and its exit from its reap.
//! The others come from a sampler on a thread of its own, never from the runner's waiting loop, which takes no output
//! lock: every [`TICK`] it lists the group, says a spawn for each process it has not seen, keyed on its pid and start
//! time as pids are reused, and watches each one's exit with a kqueue `EVFILT_PROC` knote, whose event gives the exit's
//! code or signal. The sampler is stopped once the group has ended: what is still in the group then is said as left.
//!
//! Every process line is handed to a writer thread of its own, which says it through the output, so neither the
//! runner's thread nor the sampler ever waits on the output lock. The lines end with [`ProcessLines::finish`], before
//! the run's final lines: the writer is given [`DRAIN`] to say what it holds, and no process line is written after.

use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
#[cfg(target_os = "macos")]
use std::time::Duration;

use gate_wire::stamp::{self, Ended, Tag};
use jiff::Timestamp;

use crate::console::{Forwarding, GAT, Stamp, Stream};
use crate::forward::DRAIN;
#[cfg(target_os = "macos")]
use crate::forward::HANG_GUARD;

/// How often the sampler lists the command's group.
#[cfg(target_os = "macos")]
const TICK: Duration = Duration::from_millis(10);

/// The once-per-run note of what the process lines can miss, on a system whose sampler lists the group.
#[cfg(target_os = "macos")]
const NOTE: &str = "process lines: the group is sampled every 10 ms; a process shorter than that, or one that leaves the group, may be missing";

/// The once-per-run note of what the process lines leave out, on a system with no sampler.
#[cfg(not(target_os = "macos"))]
const NOTE: &str = "process lines: descendants are not listed on this system; only the command's own process is";

/// Where the process lines go: the writer thread's queue.
#[derive(Clone)]
struct Lines {
    queue: mpsc::Sender<(Stamp, String)>,
}

impl Lines {
    /// Hands `text`, to be said on standard error as `stamp` says, to the writer, without waiting.
    fn say(&self, stamp: Stamp, text: &str) {
        // A writer that has ended has stopped the lines: nothing more is said.
        let _ = self.queue.send((stamp, text.to_owned()));
    }
}

/// A thread of the process lines while it runs, told to end through `quit` or by its queue closing: it says so on
/// `ended`.
struct Running {
    quit: Arc<AtomicBool>,
    ended: mpsc::Receiver<()>,
}

/// The process lines of one command.
pub struct ProcessLines {
    /// The writer's queue, until the lines finish; `None` when the writer could not start.
    lines: Mutex<Option<Lines>>,
    /// Set once the lines finish: the writer looks at it under the output lock before each line.
    stopped: Arc<AtomicBool>,
    /// The writer, while it runs.
    writer: Mutex<Option<Running>>,
    /// The leader's pid and start, once it started.
    leader: Mutex<Option<(u32, Timestamp)>>,
    /// The sampler, while it runs.
    sampler: Mutex<Option<Running>>,
}

impl ProcessLines {
    /// The process lines of a command whose lines go through `output`, their writer started; one that cannot start
    /// leaves the command with no process lines.
    #[must_use]
    pub fn new(output: Arc<dyn Forwarding>) -> ProcessLines {
        let stopped = Arc::new(AtomicBool::new(false));
        let (queue, queued) = mpsc::channel::<(Stamp, String)>();
        let (done, ended) = mpsc::channel();
        let flag = Arc::clone(&stopped);
        let started = thread::Builder::new().name("gate-box-process-lines".to_owned()).spawn(move || {
            for (stamp, text) in queued {
                // A client that has gone has no one left to tell; the run finds it gone at its own next line.
                let _ = output.say_unless(&flag, Stream::Err, stamp, &text);
            }
            // A finish that gave up waiting has dropped its receiver; nothing is left to tell.
            let _ = done.send(());
        });
        let (lines, writer) = match started {
            Ok(_detached) => (
                Some(Lines { queue }),
                Some(Running {
                    quit: Arc::clone(&stopped),
                    ended,
                }),
            ),
            Err(_) => (None, None),
        };
        ProcessLines {
            lines: Mutex::new(lines),
            stopped,
            writer: Mutex::new(writer),
            leader: Mutex::new(None),
            sampler: Mutex::new(None),
        }
    }

    /// Hands `text` to the writer as `stamp` says, nothing once the lines have finished.
    fn say(&self, stamp: Stamp, text: &str) {
        if let Some(lines) = self.lines.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            lines.say(stamp, text);
        }
    }

    /// Says the root line: the box's own pid and its parent's ([`stamp::root_text`]).
    pub fn root(&self) {
        // SAFETY: `getppid` takes nothing, touches no memory and always succeeds.
        let parent = u32::try_from(unsafe { libc::getppid() }).ok();
        self.say(GAT, &stamp::root_text(std::process::id(), parent));
    }

    /// The leader `pid`, running the program file named `image`, has just started, a child of the box: says its spawn
    /// line, stamped now, and the note of what the lines can miss, then starts the sampler of its group on macOS.
    pub fn leader_started(&self, pid: i32, image: Option<&str>) {
        let Ok(pid) = u32::try_from(pid) else {
            return;
        };
        let at = Timestamp::now();
        *self.leader.lock().unwrap_or_else(PoisonError::into_inner) = Some((pid, at));
        self.say(Stamp::At(Tag::Gat, at), &stamp::spawn_text(pid, Some(std::process::id()), image));
        self.say(GAT, NOTE);
        self.sampler_started(pid);
    }

    /// Starts the sampler of the group `group` on a thread of its own; a sampler that cannot start is said in its place.
    #[cfg(target_os = "macos")]
    fn sampler_started(&self, group: u32) {
        let Ok(group) = i32::try_from(group) else {
            return;
        };
        let Some(lines) = self.lines.lock().unwrap_or_else(PoisonError::into_inner).clone() else {
            return;
        };
        let sampler = match mac::Sampler::new(group, lines) {
            Ok(sampler) => sampler,
            Err(error) => {
                self.say(GAT, &stamp::stopped_text(&format!("the exit watch could not be made: {error}")));
                return;
            }
        };
        let quit = Arc::new(AtomicBool::new(false));
        let (done, ended) = mpsc::channel();
        let told = Arc::clone(&quit);
        let started = thread::Builder::new().name("gate-box-processes".to_owned()).spawn(move || {
            sampler.run(&told);
            // A stop that gave up waiting has dropped its receiver; nothing is left to tell.
            let _ = done.send(());
        });
        match started {
            Ok(_detached) => *self.sampler.lock().unwrap_or_else(PoisonError::into_inner) = Some(Running { quit, ended }),
            Err(error) => self.say(GAT, &stamp::stopped_text(&format!("the sampler could not start: {error}"))),
        }
    }

    /// No sampler runs on a system whose process groups the box cannot list.
    #[cfg(not(target_os = "macos"))]
    #[expect(clippy::unused_self, reason = "the macOS build starts its sampler here")]
    fn sampler_started(&self, _group: u32) {}

    /// The command's group has ended, or the runner gave up on it: stops the sampler and waits, at most
    /// [`HANG_GUARD`], for it to hand on what is still in the group as left. The sampler never waits on the output, so
    /// the bound only keeps a sampler stuck in a system call from holding the run.
    #[cfg(target_os = "macos")]
    pub fn group_ended(&self) {
        let Some(running) = self.sampler.lock().unwrap_or_else(PoisonError::into_inner).take() else {
            return;
        };
        running.quit.store(true, Ordering::SeqCst);
        // A sampler still not done is left behind; the finish stops whatever it hands on later.
        let _ = running.ended.recv_timeout(HANG_GUARD);
    }

    /// No sampler runs on a system whose process groups the box cannot list.
    #[cfg(not(target_os = "macos"))]
    #[expect(clippy::unused_self, reason = "the macOS build stops its sampler here")]
    pub fn group_ended(&self) {}

    /// The leader was reaped with `status`, or, for `None`, left unreaped: says its exit line, stamped now, or its left
    /// line.
    pub fn leader_ended(&self, status: Option<&ExitStatus>) {
        let Some((pid, start)) = *self.leader.lock().unwrap_or_else(PoisonError::into_inner) else {
            return;
        };
        let text = match status {
            Some(status) => stamp::exit_text(pid, ended_of(*status), Some(Timestamp::now().duration_since(start))),
            None => stamp::left_text(pid),
        };
        self.say(GAT, &text);
    }

    /// Ends the process lines, before the run's final lines: the writer's queue closed, the writer given [`DRAIN`] to
    /// say what it holds, then the lines stopped, so a writer still held by a client that is not reading writes
    /// nothing more. Nothing is said after; a second finish does nothing.
    pub fn finish(&self) {
        drop(self.lines.lock().unwrap_or_else(PoisonError::into_inner).take());
        if let Some(writer) = self.writer.lock().unwrap_or_else(PoisonError::into_inner).take() {
            // A writer still not done is left behind, stopped below.
            let _ = writer.ended.recv_timeout(DRAIN);
            writer.quit.store(true, Ordering::SeqCst);
        }
        self.stopped.store(true, Ordering::SeqCst);
    }
}

impl Drop for ProcessLines {
    fn drop(&mut self) {
        self.finish();
    }
}

/// How the exit `status` says its process ended.
fn ended_of(status: ExitStatus) -> Ended {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => Ended::Code(i64::from(code)),
        (None, Some(signal)) => Ended::Signal(signal),
        (None, None) => Ended::Unknown,
    }
}

/// The file name of the program at `path`, as a spawn line names it.
#[must_use]
pub fn image_of(path: &Path) -> Option<String> {
    path.file_name().map(|name| name.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
mod mac {
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use gate_wire::stamp::{self, Ended, Tag};
    use jiff::Timestamp;

    use super::{Lines, TICK, image_of};
    use crate::console::{GAT, Stamp};

    /// The most exit events read in one call.
    const EVENTS: usize = 64;

    /// How the sampler watches a process's exit.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Watching {
        /// Its knote is registered: its exit event gives its end.
        Knote,
        /// Its knote could not be attached, as it had already exited or is not the box's to watch: its exit is said,
        /// its end unknown, once it is gone from the listing.
        Unattached,
        /// Nothing more of it can be said: its missed line stood in for its spawn or its exit.
        Missed,
        /// Its exit was said; it stays listed until its parent reaps it, and is forgotten once it is gone.
        Exited,
    }

    /// A process of the group the sampler has seen.
    struct Known {
        /// Its start, which with its pid keys it; `None` when it could not be read.
        start: Option<Timestamp>,
        watching: Watching,
    }

    /// The sampler of one process group.
    pub(super) struct Sampler {
        group: i32,
        queue: OwnedFd,
        known: HashMap<i32, Known>,
        listed: Vec<i32>,
        lines: Lines,
    }

    impl Sampler {
        /// The sampler of `group`, its lines said through `lines`, with a kqueue of its own.
        ///
        /// # Errors
        ///
        /// Why the kqueue could not be made.
        pub(super) fn new(group: i32, lines: Lines) -> io::Result<Sampler> {
            // SAFETY: `kqueue` takes nothing and gives a new descriptor or -1.
            let queue = unsafe { libc::kqueue() };
            if queue < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Sampler {
                group,
                // SAFETY: `queue` is a descriptor just made, owned by nothing else.
                queue: unsafe { OwnedFd::from_raw_fd(queue) },
                known: HashMap::new(),
                listed: Vec::new(),
                lines,
            })
        }

        /// Samples the group every [`TICK`] until `quit` is set, then once more: what has left by then is said
        /// exited, its end unknown when no event gave it, and what is still in the group is said left. A listing or a
        /// read of the events that fails ends the lines with [`stamp::stopped_text`].
        pub(super) fn run(mut self, quit: &AtomicBool) {
            loop {
                let quitting = quit.load(Ordering::SeqCst);
                if let Err(why) = self.pass().and_then(|()| self.events(if quitting { Duration::ZERO } else { TICK })) {
                    self.lines.say(GAT, &stamp::stopped_text(&why));
                    return;
                }
                if quitting {
                    break;
                }
            }
            self.ended();
        }

        /// One look at the group: listed, each process not seen before said and watched, each one whose knote could
        /// not be attached said exited once it is gone from the listing.
        ///
        /// # Errors
        ///
        /// Why the group could not be listed.
        pub(super) fn pass(&mut self) -> Result<(), String> {
            let mut listed = std::mem::take(&mut self.listed);
            crate::stop::group_pids_into(self.group, &mut listed)?;
            listed.sort_unstable();
            for &pid in &listed {
                if pid != self.group {
                    self.looked_at(pid);
                }
            }
            let gone: Vec<i32> = self.known.keys().copied().filter(|pid| listed.binary_search(pid).is_err()).collect();
            for pid in gone {
                let Some(known) = self.known.get(&pid) else { continue };
                // A knote's exit comes as its event, which may still be on its way.
                if known.watching != Watching::Knote {
                    self.forgotten(pid);
                }
            }
            self.listed = listed;
            Ok(())
        }

        /// Looks at the listed process `pid`: a process not seen before, or one whose pid another took, is said.
        fn looked_at(&mut self, pid: i32) {
            let start = bsd_info(pid).map(|info| (info.pbi_ppid, started_at(&info)));
            match (self.known.get(&pid), start) {
                // A process seen before whose info can no longer be read has exited, its end still to come.
                (Some(_), None) => {}
                (Some(known), Some((_, start))) if known.start == start => {}
                (Some(_), Some(_)) => {
                    // Its pid was taken by a new process: the old one has ended.
                    self.forgotten(pid);
                    self.joined(pid, start);
                }
                (None, start) => self.joined(pid, start),
            }
        }

        /// Says the process `pid`, started by `start`'s parent at its moment, joined, and watches its exit; one whose
        /// start could not be read is said missed.
        fn joined(&mut self, pid: i32, start: Option<(u32, Option<Timestamp>)>) {
            let Ok(shown) = u32::try_from(pid) else { return };
            let Some((parent, at)) = start else {
                self.lines.say(GAT, &stamp::missed_text(shown, "it ended before it could be read"));
                self.known.insert(
                    pid,
                    Known {
                        start: None,
                        watching: Watching::Missed,
                    },
                );
                return;
            };
            let stamp = at.map_or(GAT, |at| Stamp::At(Tag::Gat, at));
            self.lines.say(stamp, &stamp::spawn_text(shown, Some(parent), image(pid).as_deref()));
            let watching = match self.attached(pid) {
                Ok(()) => Watching::Knote,
                Err(error) if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::EACCES)) => Watching::Unattached,
                Err(error) => {
                    self.lines
                        .say(GAT, &stamp::missed_text(shown, &format!("its exit could not be watched: {error}")));
                    Watching::Missed
                }
            };
            self.known.insert(pid, Known { start: at, watching });
        }

        /// The process `pid` has left the group: said exited, its end unknown, unless its exit was said already or it
        /// was missed.
        fn forgotten(&mut self, pid: i32) {
            let Some(known) = self.known.remove(&pid) else { return };
            if !matches!(known.watching, Watching::Missed | Watching::Exited)
                && let Ok(shown) = u32::try_from(pid)
            {
                self.lines.say(GAT, &stamp::exit_text(shown, Ended::Unknown, None));
            }
        }

        /// Registers the exit knote of `pid`.
        ///
        /// # Errors
        ///
        /// Why it could not be: `ESRCH` for a process already gone, `EACCES` for one not the box's to watch.
        fn attached(&self, pid: i32) -> io::Result<()> {
            let ident = usize::try_from(pid).map_err(io::Error::other)?;
            let change = libc::kevent {
                ident,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: one change is passed with its count and no event list, so the call reads only `change`.
            let done = unsafe { libc::kevent(self.queue.as_raw_fd(), &raw const change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            if done < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
        }

        /// Reads the exit events, waiting up to `wait` for the first, and says each process's exit, stamped as it is
        /// read.
        ///
        /// # Errors
        ///
        /// Why the events could not be read.
        fn events(&mut self, wait: Duration) -> Result<(), String> {
            let mut timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::c_long::from(wait.subsec_nanos()),
            };
            loop {
                // SAFETY: an all-zero `kevent` is a valid value, which the call overwrites.
                let mut events: [libc::kevent; EVENTS] = unsafe { std::mem::zeroed() };
                let room = libc::c_int::try_from(EVENTS).unwrap_or(1);
                // SAFETY: no change is passed; `events` has room for `room` events, which is all the call writes.
                let read = unsafe { libc::kevent(self.queue.as_raw_fd(), std::ptr::null(), 0, events.as_mut_ptr(), room, &raw const timeout) };
                let Ok(read) = usize::try_from(read) else {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        return Ok(());
                    }
                    return Err(format!("the exit events could not be read: {error}"));
                };
                for event in &events[..read] {
                    self.exited(event);
                }
                if read < EVENTS {
                    return Ok(());
                }
                timeout = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            }
        }

        /// Says the exit `event` gives, for a process watched by its knote.
        fn exited(&mut self, event: &libc::kevent) {
            if event.filter != libc::EVFILT_PROC || event.fflags & libc::NOTE_EXIT == 0 {
                return;
            }
            let (Ok(pid), Ok(shown)) = (i32::try_from(event.ident), u32::try_from(event.ident)) else {
                return;
            };
            if self.known.get(&pid).is_none_or(|known| known.watching != Watching::Knote) {
                return;
            }
            let Some(known) = self.known.get_mut(&pid) else { return };
            known.watching = Watching::Exited;
            let ran = known.start.map(|start| Timestamp::now().duration_since(start));
            let status = i32::try_from(event.data).ok();
            self.lines
                .say(GAT, &stamp::exit_text(shown, status.map_or(Ended::Unknown, ended_of), ran));
        }

        /// The group's watch is over: each process gone from the last listing said exited, its end unknown, and each
        /// still in it said left.
        fn ended(mut self) {
            let mut pids: Vec<i32> = self.known.keys().copied().collect();
            pids.sort_unstable();
            for pid in pids {
                if self.listed.binary_search(&pid).is_err() {
                    self.forgotten(pid);
                } else if let Some(known) = self.known.remove(&pid)
                    && !matches!(known.watching, Watching::Missed | Watching::Exited)
                    && let Ok(shown) = u32::try_from(pid)
                {
                    self.lines.say(GAT, &stamp::left_text(shown));
                }
            }
        }
    }

    /// How the wait status `status`, as an exit event's data holds it, says its process ended.
    fn ended_of(status: i32) -> Ended {
        if libc::WIFEXITED(status) {
            Ended::Code(i64::from(libc::WEXITSTATUS(status)))
        } else if libc::WIFSIGNALED(status) {
            Ended::Signal(libc::WTERMSIG(status))
        } else {
            Ended::Unknown
        }
    }

    /// The BSD info of `pid` (`proc_pidinfo(PROC_PIDTBSDINFO)`): its parent and its start; `None` when it cannot be read.
    fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
        // SAFETY: an all-zero `proc_bsdinfo` is a valid value, which the call fills.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `info` is `size` bytes, which is all the call writes.
        let read = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        (read == size).then_some(info)
    }

    /// The start `info` gives, `None` when it is out of range.
    fn started_at(info: &libc::proc_bsdinfo) -> Option<Timestamp> {
        let seconds = i64::try_from(info.pbi_start_tvsec).ok()?;
        let nanos = i32::try_from(info.pbi_start_tvusec.checked_mul(1_000)?).ok()?;
        Timestamp::new(seconds, nanos).ok()
    }

    /// The file name of the program `pid` runs (`proc_pidpath`); `None` when it cannot be read.
    fn image(pid: i32) -> Option<String> {
        let mut path = vec![0_u8; usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE).ok()?];
        let room = u32::try_from(path.len()).ok()?;
        // SAFETY: `path` has `room` bytes, which is all the call writes.
        let read = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), room) };
        let read = usize::try_from(read).ok().filter(|read| *read > 0)?;
        image_of(Path::new(OsStr::from_bytes(&path[..read])))
    }
}
