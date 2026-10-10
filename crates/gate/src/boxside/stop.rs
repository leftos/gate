//! `gate box stop`: ends every process of the run whose job the payload names, as the PowerShell wrapper's
//! `Invoke-Stop` does. The job name carries the run's nonce, so a stop that arrives late finds no job, and never
//! another run's. It takes no lock.

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use gate_win32::{Job, OpenedJob};

use crate::answer::USAGE_STATUS;
use crate::boxside::payload::Payload;
use crate::console::Console;

/// The exit code the job's processes are ended with.
pub(crate) const STOPPED: u32 = 124;
/// How long the stop waits for the job's processes to be gone.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// How often it looks.
const POLL: Duration = Duration::from_millis(100);
/// The status of a stop that left processes behind or failed.
const FAILED_STATUS: u32 = 1;

/// Terminates the payload's job and waits for its processes to be gone; returns the status: 0 when the job is gone or
/// there was none, 1 when processes are left or the job could not be stopped, 2 for a payload without a work-folder
/// key or a job name of that work folder.
pub(crate) fn stop(payload: &Payload, console: &mut Console) -> u32 {
    let name = match payload.key().and_then(|key| payload.job(key)) {
        Ok(name) => name,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    match stopped(name) {
        Ok(None) => 0,
        Ok(Some(0)) => {
            console.out(&format!("gate: remote stopped job {name}"));
            0
        }
        Ok(Some(left)) => {
            console.err(&still_holding(name, left));
            FAILED_STATUS
        }
        Err(error) => {
            console.err(&format!("gate: remote could not stop job {name}: {error}"));
            FAILED_STATUS
        }
    }
}

/// The line said of a terminated job that still holds `left` processes after the wait.
pub(crate) fn still_holding(name: &str, left: usize) -> String {
    format!(
        "gate: remote job {name} still has {left} process(es) {} s after it was terminated",
        STOP_WAIT.as_secs()
    )
}

/// A job whose processes can be ended and listed, whether the run holds it or a stop opened it by name.
pub(crate) trait StoppableJob {
    /// Ends every process in the job, and in the jobs nested inside it, with `exit_code`.
    fn terminate(&self, exit_code: u32) -> io::Result<()>;

    /// The ids of every process in the job now.
    fn process_ids(&self) -> io::Result<Vec<u32>>;
}

impl StoppableJob for Job {
    fn terminate(&self, exit_code: u32) -> io::Result<()> {
        Job::terminate(self, exit_code)
    }

    fn process_ids(&self) -> io::Result<Vec<u32>> {
        Job::process_ids(self)
    }
}

impl StoppableJob for OpenedJob {
    fn terminate(&self, exit_code: u32) -> io::Result<()> {
        OpenedJob::terminate(self, exit_code)
    }

    fn process_ids(&self) -> io::Result<Vec<u32>> {
        OpenedJob::process_ids(self)
    }
}

/// Ends every process of `job` with [`STOPPED`] and looks every [`POLL`] for at most [`STOP_WAIT`] until it holds
/// none; how many it still holds. Shared by a stop and a run whose client closed the connection, which stops the job
/// through the handle it holds, never one opened by name.
pub(crate) fn terminate_and_wait(job: &impl StoppableJob) -> io::Result<usize> {
    job.terminate(STOPPED)?;
    let started = Instant::now();
    loop {
        let left = job.process_ids()?.len();
        if left == 0 || started.elapsed() >= STOP_WAIT {
            return Ok(left);
        }
        thread::sleep(POLL);
    }
}

/// Terminates the job `name` and waits for its processes to be gone; how many it still held, `None` when no job has
/// that name.
fn stopped(name: &str) -> io::Result<Option<usize>> {
    let Some(job) = OpenedJob::open(name)? else {
        return Ok(None);
    };
    terminate_and_wait(&job).map(Some)
}
