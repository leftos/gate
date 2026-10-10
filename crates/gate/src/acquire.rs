//! The one way the gate tries a slot's mutex, and the line it says for a mutex a killed gate had left held.

use std::io;
use std::time::Duration;

use gate_win32::{AcquireError, NamedMutex, OwnedMutexGuard};

use crate::console::Console;

/// Tries the slot mutex `name` once, without waiting: creates the handle, tries it with a zero timeout and drops the
/// handle when the try fails, so a waiting gate never keeps a slot handle open. `None` when another gate holds it.
///
/// # Errors
///
/// The system's error when the mutex cannot be created or opened, or when the try itself fails.
pub(crate) fn try_slot(name: &str) -> io::Result<Option<OwnedMutexGuard>> {
    match NamedMutex::open(name)?.acquire_owned(Duration::ZERO) {
        Ok(guard) => Ok(Some(guard)),
        Err(AcquireError::Timeout) => Ok(None),
        Err(AcquireError::Failed(error)) => Err(error),
    }
}

/// Says on stderr that this gate took the mutex `name` from a gate that died holding it.
pub(crate) fn say_taken_abandoned(console: &mut Console, name: &str) {
    console.err(&format!("gate: took {name}, which a killed gate had left held"));
}
