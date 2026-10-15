//! Named events. An auto-reset event has one waiter, woken by a set, which stays signalled until that waiter wakes. A
//! manual-reset event, once set, stays set for every look until it is closed, so a look that comes after the set,
//! however late, sees it.

use std::io;
use std::os::windows::io::BorrowedHandle;
use std::time::Duration;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, SetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};

use crate::interrupt::{Woken, wait_with};
use crate::{Interrupt, own_handle, wait_millis, wide};

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
    /// The interrupt was raised while the event was unset.
    Interrupted,
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

    /// Creates the manual-reset event `name` (such as `Local\gate-job-<pid>-stopped`), unset. A name some event has
    /// already is refused, so no set made before this call reaches the new event's holder.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::AlreadyExists`] when an event of that name exists already, whose handle is
    /// closed again at once; the system's error when the event cannot be created, as when another kind of object has
    /// the name.
    pub fn create_new_manual(name: &str) -> io::Result<NamedEvent> {
        let wide_name = wide(name);
        // SAFETY: `SetLastError` takes no pointers. It clears the last error so that a stale `ERROR_ALREADY_EXISTS`
        // cannot pass for this call's.
        unsafe { SetLastError(0) };
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; null security attributes are allowed, and the
        // one and the zero make the event manual-reset and initially unset.
        let created = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide_name.as_ptr()) };
        // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
        let error = unsafe { GetLastError() };
        // SAFETY: a success is a fresh handle nothing else owns, to a new event or to the one the name already held.
        let handle = unsafe { own_handle(created) }?;
        if error == ERROR_ALREADY_EXISTS {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("an event named {name} exists already"),
            ));
        }
        Ok(NamedEvent {
            handle,
            name: name.to_owned(),
        })
    }

    /// Opens the event `name` when it exists, and never creates it: a waker signals the event of a waiter that may
    /// already have gone, and must not bring an event into being for one that was never there. The handle can set the
    /// event and wait on it.
    ///
    /// # Errors
    ///
    /// The system's error when the open fails for any reason other than the event's absence.
    pub fn open_existing(name: &str) -> io::Result<Option<NamedEvent>> {
        NamedEvent::open(name, EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE)
    }

    /// Opens the event `name` when it exists, as [`NamedEvent::open_existing`] does, with `EVENT_MODIFY_STATE` access
    /// only: the handle can set the event and no more.
    ///
    /// # Errors
    ///
    /// The system's error when the open fails for any reason other than the event's absence.
    pub fn open_to_set(name: &str) -> io::Result<Option<NamedEvent>> {
        NamedEvent::open(name, EVENT_MODIFY_STATE)
    }

    /// The event `name` opened with `access`, or `None` when no event has that name.
    fn open(name: &str, access: SYNCHRONIZATION_ACCESS_RIGHTS) -> io::Result<Option<NamedEvent>> {
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; the call takes no other pointer.
        let opened = unsafe { OpenEventW(access, 0, wide_name.as_ptr()) };
        // SAFETY: a success is a fresh handle nothing else owns; a failure is null, which `own_handle` refuses.
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

    /// The event's handle, borrowed, for a wait on it among other handles, as the wake handles of a pipe's read or
    /// write are.
    #[must_use]
    pub fn as_handle(&self) -> BorrowedHandle<'_> {
        // SAFETY: the handle is open for as long as `self` lives, which the borrow cannot outlive.
        unsafe { BorrowedHandle::borrow_raw(self.handle.raw()) }
    }

    /// Signals the event. An auto-reset event wakes its waiter, and keeps a set that arrives before the wait, so a
    /// waiter that has not reached its wait yet still wakes; a manual-reset event stays set.
    ///
    /// # Errors
    ///
    /// The system's error when the set fails.
    pub fn set(&self) -> io::Result<()> {
        // SAFETY: the handle is open and owned by `self` alone.
        let set = unsafe { SetEvent(self.handle.raw()) };
        if set == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Whether the event is set, without waiting. A look at a set auto-reset event resets it, as a wake does.
    ///
    /// # Errors
    ///
    /// The system's error when the event cannot be waited on, as through a handle [`NamedEvent::open_to_set`] gave.
    pub fn is_set(&self) -> io::Result<bool> {
        signalled(&self.handle)
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

    /// Waits at most `timeout` for the event to be set or `interrupt` to be raised; a set event is
    /// [`EventWait::Signaled`] even when the interrupt was raised too.
    ///
    /// # Errors
    ///
    /// The system's error when the wait itself fails.
    pub fn wait_or_interrupt(&self, timeout: Duration, interrupt: Interrupt) -> io::Result<EventWait> {
        // SAFETY: the handle is open for as long as `self` lives, with the `SYNCHRONIZE` access a created or opened
        // event's handle has.
        Ok(match unsafe { wait_with(interrupt, self.handle.raw(), timeout) }? {
            Woken::Signaled => EventWait::Signaled,
            Woken::Interrupted => EventWait::Interrupted,
            Woken::TimedOut => EventWait::TimedOut,
        })
    }
}

/// Whether `handle`, such as an event's or a process's, is signalled, without waiting: true once a process has exited.
pub(crate) fn signalled(handle: &OwnedHandle) -> io::Result<bool> {
    // SAFETY: the handle is open for the whole call; one without `SYNCHRONIZE` access only makes the call fail.
    match unsafe { WaitForSingleObject(handle.raw(), 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose event cannot be made or set must fail loudly")]
mod tests {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    use super::NamedEvent;

    #[test]
    fn the_borrowed_handle_is_the_event_s_own() {
        let name = format!(r"Local\gate-win32-event-handle-{}", std::process::id());
        let event = NamedEvent::create_new_manual(&name).expect("the event is made");
        let handle = event.as_handle();
        // SAFETY: the borrowed handle is open for the whole call, and the wait takes no time.
        let before = unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) };

        event.set().expect("the event is set");

        // SAFETY: as above.
        let after = unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) };
        assert_eq!(before, WAIT_TIMEOUT, "the new event is unset");
        assert_eq!(after, WAIT_OBJECT_0, "the set is seen through the borrowed handle");
    }
}
