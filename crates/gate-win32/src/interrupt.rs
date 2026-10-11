//! Ctrl+C and Ctrl+Break, seen by the gate: a console handler that raises a flag and signals an event, and a wait that
//! ends on that event as well as on the handle it waits for. Each press is counted, and a second event is signalled
//! from the second press on, so a caller that let the first press through can wait for another. The console closing
//! raises the flag too, but Windows ends the process as soon as the handler returns, so a close ends the gate at once;
//! the next gate's dead-gate cleanup reclaims its ticket and claim.

use std::ffi::c_void;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};
use windows_sys::core::BOOL;

use crate::wait_millis;

/// Whether the console has asked the gate to stop.
static RAISED: AtomicBool = AtomicBool::new(false);
/// The manual-reset event signalled with [`RAISED`], null until [`Interrupt::install`] has made it. It is never closed:
/// the handler may run on a system thread at any moment until the process ends.
static EVENT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// How many times the interrupt has been raised.
static PRESSES: AtomicU32 = AtomicU32::new(0);
/// The manual-reset event signalled from the second raise on, null until [`Interrupt::install`] has made it; never
/// closed, as [`EVENT`] is not.
static AGAIN: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// Whether the console handler is installed; read and set only while [`INSTALLING`] is held.
static HANDLER_SET: AtomicBool = AtomicBool::new(false);
/// Held while [`Interrupt::install`] makes the event and installs the handler, so it happens once.
static INSTALLING: Mutex<()> = Mutex::new(());

/// The process's one console interrupt: raised by Ctrl+C or Ctrl+Break, and never lowered.
#[derive(Clone, Copy, Debug)]
pub struct Interrupt {
    _installed: (),
}

/// How a wait that an interrupt may end ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Woken {
    /// The handle waited for was signalled; it wins over an interrupt raised at the same time.
    Signaled,
    /// The interrupt was raised before or during the wait.
    Interrupted,
    /// Neither came before the timeout ran out.
    TimedOut,
}

impl Interrupt {
    /// Installs the console handler the first time it succeeds, and returns the interrupt it raises. From then on a
    /// Ctrl+C or Ctrl+Break no longer ends the process: the handler raises the interrupt and returns, and the caller
    /// decides what to do. A console close still ends the process once the handler returns.
    ///
    /// # Errors
    ///
    /// The system's error when the event cannot be created or the handler cannot be installed; a later call tries again,
    /// and no call returns an interrupt until the handler is set.
    pub fn install() -> io::Result<Interrupt> {
        // A panic while installing leaves nothing half done that a later call cannot redo.
        let _guard = INSTALLING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if HANDLER_SET.load(Ordering::Acquire) {
            return Ok(Interrupt { _installed: () });
        }
        for slot in [&EVENT, &AGAIN] {
            if slot.load(Ordering::Acquire).is_null() {
                // SAFETY: null security attributes and a null name are allowed; the 1 and 0 make the event
                // manual-reset and unset. A success is a handle this module keeps for the life of the process.
                let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
                if event.is_null() {
                    return Err(io::Error::last_os_error());
                }
                slot.store(event, Ordering::Release);
            }
        }
        // SAFETY: `handler` is a `extern "system"` function that lives as long as the process.
        if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        HANDLER_SET.store(true, Ordering::Release);
        Ok(Interrupt { _installed: () })
    }

    /// Whether the interrupt has been raised.
    #[must_use]
    pub fn is_raised(self) -> bool {
        RAISED.load(Ordering::Acquire)
    }

    /// How many times the interrupt has been raised: by Ctrl+C, Ctrl+Break, the console closing or [`Interrupt::raise`].
    #[must_use]
    pub fn presses(self) -> u32 {
        PRESSES.load(Ordering::Acquire)
    }

    /// Raises the interrupt, as the console handler does on a Ctrl+C.
    pub fn raise(self) {
        raise();
    }
}

/// Waits at most `timeout` for `handle` to be signalled or the interrupt to be raised; the `Interrupt` passed is the
/// proof that its event has been made.
///
/// # Safety
///
/// `handle` is open for the whole call, with the `SYNCHRONIZE` access a wait asks for.
pub(crate) unsafe fn wait_with(_installed: Interrupt, handle: HANDLE, timeout: Duration) -> io::Result<Woken> {
    // SAFETY: the caller guarantees the handle; the event is the one `install` made, never closed, and an `Interrupt`
    // exists only once it has been made.
    unsafe { wait_on(handle, EVENT.load(Ordering::Acquire), timeout) }
}

/// Waits at most `timeout` for `handle` to be signalled or the interrupt to be raised a second time; a second press
/// that came before the call ends it at once.
///
/// # Safety
///
/// `handle` is open for the whole call, with the `SYNCHRONIZE` access a wait asks for.
pub(crate) unsafe fn wait_again_with(_installed: Interrupt, handle: HANDLE, timeout: Duration) -> io::Result<Woken> {
    // SAFETY: as in `wait_with`, for the event of the second press, which `install` made with the first.
    unsafe { wait_on(handle, AGAIN.load(Ordering::Acquire), timeout) }
}

/// Waits at most `timeout` for `handle` or `event`, `handle` winning when both are signalled.
///
/// # Safety
///
/// Both handles are open for the whole call, with the `SYNCHRONIZE` access a wait asks for.
unsafe fn wait_on(handle: HANDLE, event: HANDLE, timeout: Duration) -> io::Result<Woken> {
    let handles = [handle, event];
    // SAFETY: the caller guarantees both handles.
    let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, wait_millis(timeout)) };
    match result {
        WAIT_OBJECT_0 => Ok(Woken::Signaled),
        WAIT_TIMEOUT => Ok(Woken::TimedOut),
        _ if result == WAIT_OBJECT_0 + 1 => Ok(Woken::Interrupted),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        _ => Err(io::Error::other(format!(
            "the wait on the command and the interrupt ended with the unexpected result 0x{result:x}"
        ))),
    }
}

/// The console handler: raises the interrupt for Ctrl+C, Ctrl+Break and the console closing, and passes every other
/// signal on. It runs on a thread the system starts, so it only stores a flag and sets an event. On a close, Windows
/// ends the process as soon as it returns, so the gate does not act on that interrupt.
unsafe extern "system" fn handler(signal: u32) -> BOOL {
    match signal {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT => {
            raise();
            1
        }
        _ => 0,
    }
}

/// Stores the flag, counts the press and signals the event, and from the second press on the event of a press again.
fn raise() {
    RAISED.store(true, Ordering::Release);
    let earlier = PRESSES.fetch_add(1, Ordering::AcqRel);
    let slots: &[&AtomicPtr<c_void>] = if earlier == 0 { &[&EVENT] } else { &[&EVENT, &AGAIN] };
    for slot in slots {
        let event = slot.load(Ordering::Acquire);
        if !event.is_null() {
            // SAFETY: the event was made by `install` and is never closed. A set that fails leaves the flag and the
            // count, which every wait's caller reads after its wait.
            unsafe { SetEvent(event) };
        }
    }
}
