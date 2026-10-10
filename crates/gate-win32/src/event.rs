//! Named auto-reset events: one waiter per event, woken by a set, which stays signalled until that waiter wakes.

use std::io;
use std::time::Duration;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};

use crate::{own_handle, wait_millis, wide};

/// A handle to a named event, created or opened by name and closed when dropped.
pub struct NamedEvent {
    handle: OwnedHandle,
    name: String,
}

/// How a wait on an event ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventWait {
    /// The event was set before or during the wait.
    Signaled,
    /// The event was still unset when the timeout ran out.
    TimedOut,
}

impl NamedEvent {
    /// Creates the auto-reset event `name`, unset, or opens the event that already has that name. A new event starts
    /// unset; an event that already exists under the name is opened as it stands, so a waiter may see one wake that
    /// was set before it created the handle, which a waiter that re-checks its condition after every wake tolerates.
    ///
    /// # Errors
    ///
    /// The system's error when the event cannot be created or opened.
    pub fn create(name: &str) -> io::Result<NamedEvent> {
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; null security attributes are allowed, and the
        // zeroes make the event auto-reset and initially unset. A success is a fresh handle nothing else owns.
        let handle = unsafe { own_handle(CreateEventW(std::ptr::null(), 0, 0, wide_name.as_ptr())) }?;
        Ok(NamedEvent {
            handle,
            name: name.to_owned(),
        })
    }

    /// Opens the event `name` when it exists, and never creates it: a waker signals the event of a waiter that may
    /// already have gone, and must not bring an event into being for one that was never there.
    ///
    /// # Errors
    ///
    /// The system's error when the open fails for any reason other than the event's absence.
    pub fn open_existing(name: &str) -> io::Result<Option<NamedEvent>> {
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call. `EVENT_MODIFY_STATE` is what setting the event
        // asks for and `SYNCHRONIZE` what waiting on it asks for. A success is a fresh handle nothing else owns, which
        // the guard closes at once.
        let opened = unsafe { OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, 0, wide_name.as_ptr()) };
        match unsafe { own_handle(opened) } {
            Ok(handle) => Ok(Some(NamedEvent {
                handle,
                name: name.to_owned(),
            })),
            // `OpenEventW` reports a name no process holds as `ERROR_FILE_NOT_FOUND`.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The name it was created or opened by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Signals the event, waking its waiter. A set that arrives before the wait is kept by the auto-reset, so a
    /// waiter that has not reached its wait yet still wakes.
    ///
    /// # Errors
    ///
    /// The system's error when the set fails.
    pub fn set(&self) -> io::Result<()> {
        // SAFETY: the handle is open and owned by `self` alone.
        let set = unsafe { SetEvent(self.handle.raw()) };
        if set == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Waits at most `timeout` for the event to be set.
    ///
    /// # Errors
    ///
    /// The system's error when the wait itself fails.
    pub fn wait(&self, timeout: Duration) -> io::Result<EventWait> {
        // SAFETY: the handle is open for the whole call.
        let result = unsafe { WaitForSingleObject(self.handle.raw(), wait_millis(timeout)) };
        match result {
            WAIT_OBJECT_0 => Ok(EventWait::Signaled),
            WAIT_TIMEOUT => Ok(EventWait::TimedOut),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
