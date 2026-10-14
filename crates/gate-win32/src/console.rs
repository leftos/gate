//! The UTF-8 console a command is created on. A console made for a new process takes the system's OEM code pages, and
//! they can be set only once a process runs on it, so a suspended command has none to set yet. So a donor, the system's
//! `cmd.exe` put in the command's job as soon as the job exists ([`Donor::start`]), makes a hidden console there; this
//! process leaves its own console, attaches to the donor's, sets its input and output code pages to UTF-8 (65001) and
//! creates the command to inherit it, then returns to its own console. The donor stays on its console for as long as
//! the caller keeps it, so the console never closes under the command, whenever the command attaches to it or leaves
//! it. This process returns through a keeper `cmd.exe` it starts on its own console and holds open meanwhile, so that
//! console never closes either.
//! Attaching to a console replaces the standard handles, so each one that was not a console handle (the pipes of an ssh
//! session) is put back. Both helpers read their stdin from a pipe nobody writes, so they wait without running
//! anything, and write to `NUL`; they start under the spawn lock as every launch does. The keeper is ended once this
//! process is home again; the donor when the caller drops it.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING};
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole, GetConsoleMode, GetConsoleProcessList, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCP, SetConsoleOutputCP, SetStdHandle,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, OpenProcess, PROCESS_CREATION_FLAGS, PROCESS_SYNCHRONIZE, TerminateProcess,
};

use crate::create::{Creation, DesktopPolicy, Inherited, create_inheriting};
use crate::event::signalled;
use crate::launch::{LaunchError, open_shared};
use crate::{EnvironmentBlock, Job, RestrictedToken, Running, Suspended, own_handle, system_directory};

/// The UTF-8 code page.
const UTF8: u32 = 65001;
/// How long each wait of the console's settling, and the wait for the caller's hold, lasts at most.
const CONSOLE_WAIT: Duration = Duration::from_secs(10);
/// Why the console is not entered when the caller's hold cannot be had within [`CONSOLE_WAIT`].
const BLOCKED: &str = "the box's output is blocked";
/// How often a wait looks again.
const STEP: Duration = Duration::from_millis(10);
/// The standard handles, in the order they are saved.
const STD_HANDLES: [STD_HANDLE; 3] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];

/// This process away on a donor's UTF-8 console: made by [`Utf8Console::enter`], left by [`Utf8Console::leave`], or
/// when dropped, which returns home all the same but has nowhere to say a failure to. `G` is what the caller holds
/// while this process's standard handles may point at the donor's console, such as the guard of its output's lock.
pub struct Utf8Console<G> {
    saved: [Saved; 3],
    keeper: Option<Helper>,
    /// A process on this process's own console to return through, held open; `None` when it had no console.
    home: Option<Home>,
    /// What the caller holds from just before this process leaves its own console until it is home again.
    held: Option<G>,
    /// Whether this process has left its own console.
    freed: bool,
    /// Whether it is attached to the donor's console.
    away: bool,
    /// Whether it is home again, its handles put back, its keeper ended and the caller's hold let go.
    left: bool,
}

impl<G> Utf8Console<G> {
    /// Leaves this process's own console for `donor`'s, whose code pages it sets to UTF-8: a command created now without
    /// a console of its own ([`crate::PipedConsole::Attached`]) inherits it. `hold` is tried just before this process
    /// leaves its own console, after its home is found and its keeper has started, every [`STEP`] until it gives what
    /// is held until this process is home again, for at most [`CONSOLE_WAIT`].
    ///
    /// # Errors
    ///
    /// Why the console could not be had: a keeper that cannot start or exits first, a hold not had in time (`the box's
    /// output is blocked`), a donor that exits first, a wait that times out or fails, or a call that fails. This
    /// process is home again by then, its handles put back and the hold let go; the donor is the caller's still.
    ///
    /// The keeper runs with `token` when given, as the command does, else with this process's own token.
    pub fn enter(donor: &Donor, token: Option<&RestrictedToken>, hold: impl FnMut() -> Option<G>) -> Result<Utf8Console<G>, String> {
        let mut console = Utf8Console {
            saved: save_std_handles(),
            keeper: None,
            home: None,
            held: None,
            freed: false,
            away: false,
            left: false,
        };
        match console.go_to_donor(donor, token, hold) {
            Ok(()) => Ok(console),
            Err(reason) => Err(match console.go_home() {
                Ok(()) => reason,
                Err(problem) => format!("{reason}; and {problem}"),
            }),
        }
    }

    /// Returns to this process's own console, through its home process or else its parent, puts back every standard
    /// handle that was not a console handle, ends the keeper and lets the hold go.
    ///
    /// # Errors
    ///
    /// What failed, as `could not ...`; the handles are put back and the keeper ended all the same.
    pub fn leave(mut self) -> Result<(), String> {
        self.go_home()
    }

    /// The home process when this process has a console, then the attach and the code pages ([`Utf8Console::attach`]).
    fn go_to_donor(&mut self, donor: &Donor, token: Option<&RestrictedToken>, hold: impl FnMut() -> Option<G>) -> Result<(), String> {
        if !console_processes().is_empty() {
            self.home = Some(self.find_home(&donor.feed, token)?);
        }
        self.attach(&donor.helper, hold)
    }

    /// Takes the hold, leaves this process's own console, attaches to `donor`'s and sets its code pages to UTF-8.
    fn attach(&mut self, donor: &Helper, hold: impl FnMut() -> Option<G>) -> Result<(), String> {
        self.held = Some(take_hold(hold, CONSOLE_WAIT)?);
        if self.home.is_some() {
            // SAFETY: the call takes no pointers.
            if unsafe { FreeConsole() } == 0 {
                return Err(format!("cannot leave this process's console: {}", io::Error::last_os_error()));
            }
            self.freed = true;
        }
        // SAFETY: the call takes no pointers; it fails until the donor's console is up.
        let attach = || unsafe { AttachConsole(donor.pid) } != 0;
        if !await_until(attach, || donor.exited(), "the console donor", CONSOLE_WAIT)? {
            return Err("the console donor exited before this process could attach to its console".to_owned());
        }
        self.away = true;
        // SAFETY: the call takes no pointers.
        if unsafe { SetConsoleCP(UTF8) } == 0 {
            return Err(format!(
                "cannot set the console's input code page to 65001: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: the call takes no pointers.
        if unsafe { SetConsoleOutputCP(UTF8) } == 0 {
            return Err(format!(
                "cannot set the console's output code page to 65001: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// A keeper started on this process's own console with `token` when given, reading `feed`, held open once it shows
    /// on the console. No process already on the console is taken: it may exit while this process is away, and another
    /// `gate box` there may be away on a donor's console of its own.
    fn find_home(&mut self, feed: &Feed, token: Option<&RestrictedToken>) -> Result<Home, String> {
        let keeper = start_helper(feed, 0, None, token, "keeper")?;
        let pid = keeper.pid;
        if !await_listed(pid, console_processes, || keeper.exited(), "the console keeper", CONSOLE_WAIT)? {
            return Err("the console keeper exited before it attached to this process's console".to_owned());
        }
        let home = Home::open(pid).map_err(|error| format!("cannot hold the console keeper open: {error}"))?;
        self.keeper = Some(keeper);
        Ok(home)
    }

    /// [`Utf8Console::leave`], once.
    fn go_home(&mut self) -> Result<(), String> {
        if self.left {
            return Ok(());
        }
        self.left = true;
        let returned = if self.freed || self.away {
            return_home(self.home.as_ref(), self.away)
        } else {
            Ok(())
        };
        (self.freed, self.away) = (false, false);
        let restored = restore_std_handles(&self.saved);
        self.held = None;
        self.keeper = None;
        match (returned, restored) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(problem), Ok(())) | (Ok(()), Err(problem)) => Err(problem),
            (Err(first), Err(second)) => Err(format!("{first}; {second}")),
        }
    }
}

impl<G> Drop for Utf8Console<G> {
    fn drop(&mut self) {
        // Reached without `leave` only when the caller panicked or returned early: there is nowhere left to say a
        // failure to get home, and the handles are put back and the keeper ended all the same.
        let _ = self.go_home();
    }
}

/// What `hold` gives, tried every [`STEP`] for at most `limit`; else [`BLOCKED`].
fn take_hold<G>(mut hold: impl FnMut() -> Option<G>, limit: Duration) -> Result<G, String> {
    let started = Instant::now();
    loop {
        if let Some(held) = hold() {
            return Ok(held);
        }
        if started.elapsed() >= limit {
            return Err(BLOCKED.to_owned());
        }
        thread::sleep(STEP);
    }
}

/// A standard handle as it was before this process left its console.
#[derive(Clone, Copy)]
struct Saved {
    id: STD_HANDLE,
    handle: HANDLE,
    /// Whether it is put back once this process is home again.
    restore: bool,
}

/// This process's standard handles now, each marked to be put back when it is set and is not a console handle.
fn save_std_handles() -> [Saved; 3] {
    STD_HANDLES.map(|id| {
        // SAFETY: the call takes no pointers.
        let handle = unsafe { GetStdHandle(id) };
        let mut mode = 0;
        // SAFETY: `mode` is writable; a handle that is no console's, null or invalid, only makes the call fail.
        let console = unsafe { GetConsoleMode(handle, &raw mut mode) } != 0;
        Saved {
            id,
            handle,
            restore: restores(handle, console),
        }
    })
}

/// Whether a standard handle is put back once this process is home again: one that is set and is not a console's.
fn restores(handle: HANDLE, console: bool) -> bool {
    !handle.is_null() && handle != INVALID_HANDLE_VALUE && !console
}

/// Puts back every saved handle marked so; else why one could not be.
fn restore_std_handles(saved: &[Saved; 3]) -> Result<(), String> {
    for saved in saved.iter().filter(|saved| saved.restore) {
        // SAFETY: the call takes no pointers; the handle was this process's own standard handle, which leaving and
        // attaching to a console do not close, since it is not a console handle.
        if unsafe { SetStdHandle(saved.id, saved.handle) } == 0 {
            return Err(format!("could not put back a standard handle: {}", io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Leaves the donor's console when `attached` to it, then returns to this process's own: through `home` unless it has
/// exited or cannot be waited on, else its parent's; none when it had none.
fn return_home(home: Option<&Home>, attached: bool) -> Result<(), String> {
    // SAFETY: the call takes no pointers.
    if attached && unsafe { FreeConsole() } == 0 {
        return Err(format!("could not leave the command's console: {}", io::Error::last_os_error()));
    }
    let Some(home) = home else {
        return Ok(());
    };
    let first = match home.exited() {
        Ok(true) => format!("its process {} has exited", home.pid),
        Err(error) => format!("cannot wait for its process {}: {error}", home.pid),
        Ok(false) => {
            // SAFETY: the call takes no pointers; the held handle keeps `home.pid` naming the home process.
            if unsafe { AttachConsole(home.pid) } != 0 {
                return Ok(());
            }
            io::Error::last_os_error().to_string()
        }
    };
    // SAFETY: the call takes no pointers.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0 {
        return Ok(());
    }
    Err(format!(
        "could not return to its own console: {first}; through its parent: {}",
        io::Error::last_os_error()
    ))
}

/// The processes on this process's console; none when it has no console, where the call fails.
fn console_processes() -> Vec<u32> {
    let mut buffer = vec![0_u32; 64];
    loop {
        let capacity = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: `buffer` is writable for `capacity` ids, and the call writes at most that many.
        let count = unsafe { GetConsoleProcessList(buffer.as_mut_ptr(), capacity) } as usize;
        if count <= buffer.len() {
            buffer.truncate(count);
            return buffer;
        }
        buffer.resize(count + 16, 0);
    }
}

/// Looks every [`STEP`] until `pid` is in what `list` gives, as [`await_until`] does.
fn await_listed(
    pid: u32,
    mut list: impl FnMut() -> Vec<u32>,
    exited: impl FnMut() -> io::Result<bool>,
    what: &str,
    limit: Duration,
) -> Result<bool, String> {
    await_until(|| list().contains(&pid), exited, what, limit)
}

/// Looks every [`STEP`] until `holds`: true once it holds, false once `exited` says the process waited on ended first.
/// Else, once `limit` has passed, that waiting for `what` timed out, or, when `exited` cannot tell, why.
fn await_until(mut holds: impl FnMut() -> bool, mut exited: impl FnMut() -> io::Result<bool>, what: &str, limit: Duration) -> Result<bool, String> {
    let started = Instant::now();
    loop {
        if holds() {
            return Ok(true);
        }
        if exited().map_err(|error| format!("cannot wait for {what}: {error}"))? {
            return Ok(false);
        }
        if started.elapsed() >= limit {
            return Err(format!("waiting for {what} timed out after {} ms", limit.as_millis()));
        }
        thread::sleep(STEP);
    }
}

/// What the helpers read and write: the read end of a pipe nobody writes, whose write end is held so it never ends,
/// and `NUL`. The [`Donor`] owns it, so the donor's input never ends while it lives: `cmd.exe /k` exits at the end of
/// its input.
struct Feed {
    read: OwnedHandle,
    _write: OwnedHandle,
    nul: OwnedHandle,
}

impl Feed {
    fn open() -> Result<Feed, String> {
        let cannot = |error: io::Error| format!("cannot create the console helpers' input pipe: {error}");
        let mut read: HANDLE = std::ptr::null_mut();
        let mut write: HANDLE = std::ptr::null_mut();
        // SAFETY: both handle pointers are writable; no security attributes are passed, so neither end is inheritable,
        // and size 0 asks for the default buffer.
        if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) } == 0 {
            return Err(cannot(io::Error::last_os_error()));
        }
        // SAFETY: a success returns two fresh handles nothing else owns; each gets exactly one owner here.
        let (read, write) = unsafe { (own_handle(read), own_handle(write)) };
        let (read, write) = (read.map_err(cannot)?, write.map_err(cannot)?);
        // Read and write, as cmd.exe ends with status 1 when a write to its output fails.
        let nul = open_shared(Path::new("NUL"), GENERIC_READ | GENERIC_WRITE, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL)
            .map_err(|error| format!("cannot open NUL for the console helpers: {error}"))?;
        Ok(Feed { read, _write: write, nul })
    }
}

/// The donor of a [`Utf8Console`]: a `cmd.exe` in the command's job that runs nothing and whose input never ends, so it
/// ends only when something ends it, such as a stop that terminates the job. Ended when dropped.
pub struct Donor {
    /// Declared first, so it is ended before its input closes.
    helper: Helper,
    feed: Feed,
}

impl Donor {
    /// Starts the donor, hidden and outside this process's job, with `token` when given, as the command it lends its
    /// console to, else with this process's own token, and puts it in `job` before it runs.
    ///
    /// # Errors
    ///
    /// Why it could not start, as `cannot ...`.
    pub fn start(job: &Job, token: Option<&RestrictedToken>) -> Result<Donor, String> {
        let feed = Feed::open()?;
        let helper = start_helper(&feed, CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW, Some(job), token, "donor")?;
        Ok(Donor { helper, feed })
    }

    /// Its process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.helper.pid
    }
}

/// A helper `cmd.exe`, ended when dropped.
struct Helper {
    process: OwnedHandle,
    pid: u32,
}

impl Helper {
    /// Whether it has exited.
    fn exited(&self) -> io::Result<bool> {
        signalled(&self.process)
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // SAFETY: the handle is open for the whole call, with the `PROCESS_TERMINATE` access a created process's handle
        // has. A helper that cannot be ended has ended already, which is all this asks.
        unsafe { TerminateProcess(self.process.raw(), 0) };
    }
}

/// The process this process returns to its own console through, held open with `SYNCHRONIZE` access only, so its id
/// names no other process meanwhile and its exit can be seen.
struct Home {
    process: OwnedHandle,
    pid: u32,
}

impl Home {
    fn open(pid: u32) -> io::Result<Home> {
        // SAFETY: the call takes no pointers.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        // SAFETY: a success returns a fresh handle nothing else owns; a failure returns null, which `own_handle` refuses.
        let process = unsafe { own_handle(handle) }?;
        Ok(Home { process, pid })
    }

    /// Whether it has exited.
    fn exited(&self) -> io::Result<bool> {
        signalled(&self.process)
    }
}

/// Starts `cmd.exe /d /q /k` from the system folder, in that folder, reading `feed` and writing `NUL`, created with
/// `flags` and with `token` when given, then puts it in `job` when given and resumes it. `what` names it in a failure.
fn start_helper(
    feed: &Feed,
    flags: PROCESS_CREATION_FLAGS,
    job: Option<&Job>,
    token: Option<&RestrictedToken>,
    what: &str,
) -> Result<Helper, String> {
    let cannot = |error: &dyn std::fmt::Display| format!("cannot start the console {what}: {error}");
    let system = system_directory().map_err(|error| cannot(&error))?;
    let shell = system.join("cmd.exe");
    let mut line: Vec<u16> = vec![u16::from(b'"')];
    line.extend(shell.as_os_str().encode_wide());
    line.extend("\" /d /q /k".encode_utf16());
    let environment = EnvironmentBlock::new(std::env::vars_os()).map_err(|error| cannot(&error))?;
    let creation = Creation {
        application: &shell,
        command_line: &line,
        directory: &system,
        environment: &environment,
        token,
    };
    let inherited = Inherited {
        stdio: [&feed.read, &feed.nul, &feed.nul],
        others: &[],
    };
    let suspended = create_inheriting(&creation, flags, &inherited, DesktopPolicy::Absent).map_err(|error| cannot(&error))?;
    resume(suspended, job).map_err(|error| cannot(&error))
}

/// Puts `suspended` in `job` when given, then lets it run; a failure ends it unstarted.
fn resume(suspended: Suspended, job: Option<&Job>) -> Result<Helper, LaunchError> {
    if let Some(job) = job {
        suspended.assign(job)?;
    }
    let Running { process, pid } = suspended.resume()?;
    Ok(Helper { process, pid })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make or open its handle must fail loudly")]
mod tests {
    use std::io;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    use super::{Home, await_listed, restores, signalled};
    use crate::own_handle;

    /// Long enough that no case below reaches it.
    const NEVER: Duration = Duration::from_secs(600);

    #[test]
    fn console_puts_back_only_a_set_handle_that_is_no_consoles() {
        let pipe = std::ptr::without_provenance_mut(0x1234);
        assert!(restores(pipe, false), "a pipe is put back");
        assert!(!restores(pipe, true), "a console handle is not");
        assert!(!restores(std::ptr::null_mut(), false), "no handle is not");
        assert!(!restores(INVALID_HANDLE_VALUE, false), "an invalid handle is not");
    }

    #[test]
    fn console_await_finds_the_process_once_the_list_holds_it() {
        let mut lists = [vec![], vec![7], vec![7, 42]].into_iter();
        let found = await_listed(42, || lists.next().unwrap_or_default(), || Ok(false), "the keeper", NEVER);
        assert_eq!(found, Ok(true));
        assert_eq!(lists.next(), None, "it looked three times");
    }

    #[test]
    fn console_await_of_a_process_that_exits_first_is_false() {
        let found = await_listed(42, || vec![7], || Ok(true), "the keeper", NEVER);
        assert_eq!(found, Ok(false));
    }

    #[test]
    fn console_await_times_out_when_the_process_never_shows() {
        let found = await_listed(42, || vec![7], || Ok(false), "the console keeper", Duration::ZERO);
        assert_eq!(found, Err("waiting for the console keeper timed out after 0 ms".to_owned()));
    }

    #[test]
    fn console_await_that_cannot_wait_says_why() {
        let found = await_listed(42, || vec![7], || Err(io::Error::other("no access")), "the console keeper", NEVER);
        assert_eq!(found, Err("cannot wait for the console keeper: no access".to_owned()));
    }

    #[test]
    fn console_wait_on_a_signalled_handle_is_true() {
        // SAFETY: no security attributes and no name are passed; a success is a fresh handle nothing else owns.
        let event = unsafe { own_handle(CreateEventW(std::ptr::null(), 1, 1, std::ptr::null())) }.expect("an event can be created");
        assert!(signalled(&event).expect("the event can be waited on"));
    }

    #[test]
    fn console_wait_on_a_running_process_is_false() {
        // SAFETY: the call takes no pointers.
        let home = Home::open(unsafe { GetCurrentProcessId() }).expect("this process can be opened");
        assert!(!home.exited().expect("this process can be waited on"));
    }

    #[test]
    fn console_wait_without_synchronize_access_is_an_error() {
        // SAFETY: the calls take no pointers; a success is a fresh handle nothing else owns.
        let process =
            unsafe { own_handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, GetCurrentProcessId())) }.expect("this process can be opened");
        assert!(signalled(&process).is_err(), "a handle that cannot be waited on is no answer");
    }
}
