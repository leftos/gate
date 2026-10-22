//! `gate box register-desktop`: registers the desktop helper's task, `\gate\desktop-helper`, for the user running the
//! box, its action the helper beside this `gate.exe`, and says so in one `gate-register:` line on stdout. It reads no
//! request and never reads stdin.

use std::time::{Duration, Instant};

use gate_args::BoxSeams;
use gate_win32::TaskError;

use crate::answer::FAILED_STATUS;
use crate::boxside::desktop::lines::CONNECT_LIMIT;
use crate::boxside::stamping;
use crate::console::Console;

/// How long the Task Scheduler has to answer the registration: the limit a desktop run gives its task start.
const REGISTER_LIMIT: Duration = CONNECT_LIMIT;

/// The why of the failure `--test-box-register fail` stands in for.
const SEAM_REFUSED: &str = "the test seam refused";

/// Registers the task and says `gate-register: registered <path> for <user SID>` (exit 0), or
/// `gate-register: failed: <why>` (exit 1). A build with the `test-seams` feature given `--test-box-register` succeeds
/// or fails as it says in place of the Task Scheduler's work.
pub(crate) fn register(seams: &BoxSeams, console: &mut Console) -> u32 {
    match registered(seams) {
        Ok(user_sid) => {
            stamping::out(console, &format!("gate-register: registered {} for {user_sid}", gate_desktop::TASK_PATH));
            0
        }
        Err(why) => {
            stamping::out(console, &format!("gate-register: failed: {why}"));
            u32::from(FAILED_STATUS)
        }
    }
}

/// The SID of the user the task was registered for; else why it was not.
fn registered(seams: &BoxSeams) -> Result<String, String> {
    let user_sid = gate_win32::current_user_sid().map_err(|error| format!("the running user could not be read: {error}"))?;
    match seams.register_ok {
        Some(true) => return Ok(user_sid),
        Some(false) => return Err(SEAM_REFUSED.to_owned()),
        None => {}
    }
    let helper = gate_desktop::helper_path().map_err(|error| format!("the desktop helper's path could not be read: {error}"))?;
    gate_win32::check_installed_file(&helper).map_err(|why| why.to_string())?;
    gate_win32::register_task(gate_desktop::TASK_PATH, &helper, &user_sid, Instant::now() + REGISTER_LIMIT).map_err(|error| match error {
        TaskError::TimedOut => format!("the Task Scheduler did not answer in {} s", REGISTER_LIMIT.as_secs()),
        other => other.to_string(),
    })?;
    Ok(user_sid)
}
