//! `gate box stop`: ends every process of the run whose job the payload names, as the PowerShell wrapper's
//! `Invoke-Stop` does. It sets the run's stop signal first, so a run whose job does not exist yet never starts its
//! command, then ends the run's job and the job its dotnet tool restore runs in ([`restore_job_name`]). The job name
//! carries the run's nonce, so a stop that arrives late finds no job, and never another run's. It takes no lock.

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use gate_win32::{Job, NamedEvent, OpenedJob};

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

/// Terminates the payload's job and its restore's job, and waits for their processes to be gone; returns the status: 0
/// when both jobs are gone or there were none, 1 when processes are left or a job could not be stopped, 2 for a
/// payload without a work-folder key or a job name of that work folder. The restore's job is opened before the run's
/// stop signal is set, so a run that ends its restore on seeing the signal cannot close that job before the stop
/// finds it; it is terminated only after the signal is set, so the run reads its end as the stop's.
pub(crate) fn stop(payload: &Payload, console: &mut Console) -> u32 {
    let name = match payload.key().and_then(|key| payload.job(key)) {
        Ok(name) => name,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    let restore_name = restore_job_name(name);
    let restore = OpenedJob::open(&restore_name);
    let command = said(name, stopped(name, console), console);
    let restore = restore.and_then(|job| job.map(|job| ended(&job, &restore_name, console)).transpose());
    command.max(said(&restore_name, restore, console))
}

/// Says how stopping the job `name` went, from how many processes it still held (`None` when there was no such
/// job): `stopped` on stdout when none, else why on stderr; the status, 0 when the job is gone or there was none, 1
/// when processes are left or it could not be stopped.
fn said(name: &str, stopped: io::Result<Option<usize>>, console: &mut Console) -> u32 {
    match stopped {
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
fn still_holding(name: &str, left: usize) -> String {
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

/// The name of the stop signal of the job `job`: a manual-reset event the run creates as soon as it holds the work
/// folder's lock and holds for the run, which a stop sets whether or not the job exists yet, before it terminates the
/// job. The run looks at it just before it creates the job and again once its command is in the job, so it sees any
/// stop that could have missed the command.
pub(crate) fn stop_event_name(job: &str) -> String {
    format!("{job}-stopped")
}

/// The name of the job the dotnet tool restore of the run whose job is `job` runs in, which a stop ends beside the
/// run's own.
pub(crate) fn restore_job_name(job: &str) -> String {
    format!("{job}-tools")
}

/// How [`terminate_and_wait`] ended.
pub(crate) struct Ending {
    /// How many processes the job still held.
    pub(crate) left: usize,
    /// Why a terminate after the first failed, the first such failure only: the wait went on counting past it.
    pub(crate) late: Option<io::Error>,
}

impl Ending {
    /// Says how many processes the job `name` still holds, when it holds any, then why a terminate after the first
    /// failed, when one did.
    pub(crate) fn say(&self, name: &str, console: &mut Console) {
        if self.left > 0 {
            console.err(&still_holding(name, self.left));
        }
        self.say_late(name, console);
    }

    /// Says why a terminate of the job `name` after the first failed, when one did.
    fn say_late(&self, name: &str, console: &mut Console) {
        if let Some(error) = &self.late {
            console.err(&format!("gate: remote could not terminate job {name} again: {error}"));
        }
    }
}

/// Ends every process of `job` with [`STOPPED`], again on every look, and looks every [`POLL`] for at most
/// [`STOP_WAIT`] until it holds none, so a process that joins the job late is ended within one look; how many it
/// still holds. Only the first terminate's failure fails the wait: a later one is kept in the [`Ending`] and the wait
/// goes on counting. Shared by a stop and a run whose client closed the connection, which stops the job through the
/// handle it holds, never one opened by name.
pub(crate) fn terminate_and_wait(job: &impl StoppableJob) -> io::Result<Ending> {
    let started = Instant::now();
    job.terminate(STOPPED)?;
    let mut late = None;
    loop {
        let left = job.process_ids()?.len();
        if left == 0 || started.elapsed() >= STOP_WAIT {
            return Ok(Ending { left, late });
        }
        thread::sleep(POLL);
        let terminated = job.terminate(STOPPED);
        if late.is_none() {
            late = terminated.err();
        }
    }
}

/// Sets the stop signal of the job `name` ([`signal`]), whether or not the job exists yet, then terminates the job
/// and waits for its processes to be gone; how many it still held, `None` when no job has that name. A terminate
/// after the first that failed is said.
fn stopped(name: &str, console: &mut Console) -> io::Result<Option<usize>> {
    signal(name, console);
    let Some(job) = OpenedJob::open(name)? else {
        return Ok(None);
    };
    ended(&job, name, console).map(Some)
}

/// Terminates `job`, named `name`, and waits for its processes to be gone; how many it still held. A terminate after
/// the first that failed is said.
fn ended(job: &OpenedJob, name: &str, console: &mut Console) -> io::Result<usize> {
    let ending = terminate_and_wait(job)?;
    ending.say_late(name, console);
    Ok(ending.left)
}

/// Sets the stop signal of the job `name`. A missing signal is no error, as a run that has not reached its lock or
/// has ended holds none; a signal that cannot be opened or set is said, and the stop goes on.
fn signal(name: &str, console: &mut Console) {
    let set = match NamedEvent::open_to_set(&stop_event_name(name)) {
        Ok(Some(signal)) => signal.set(),
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    };
    if let Err(error) = set {
        console.err(&format!("gate: remote could not signal the stop of job {name}: {error}"));
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its job or its signal must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::io;

    use gate_win32::{Job, NamedEvent};

    use super::{StoppableJob, stop_event_name, stopped, terminate_and_wait};
    use crate::console::Console;

    /// A job name no other test, and no run, has: this process's id and `what`.
    fn unique_job(what: &str) -> String {
        format!(r"Local\gate-unit-stop-{}-{what}", std::process::id())
    }

    /// A job whose terminate fails from the pass `fails_from` on, counting from 0, with the error `pass <n>`, and whose
    /// process list on pass n holds `counts[n]` processes, the last count repeating.
    struct Scripted {
        fails_from: usize,
        counts: Vec<usize>,
        passes: Cell<usize>,
    }

    impl Scripted {
        fn new(fails_from: usize, counts: &[usize]) -> Scripted {
            Scripted {
                fails_from,
                counts: counts.to_vec(),
                passes: Cell::new(0),
            }
        }
    }

    impl StoppableJob for Scripted {
        fn terminate(&self, _exit_code: u32) -> io::Result<()> {
            let pass = self.passes.get();
            self.passes.set(pass + 1);
            if pass >= self.fails_from {
                Err(io::Error::other(format!("pass {pass}")))
            } else {
                Ok(())
            }
        }

        fn process_ids(&self) -> io::Result<Vec<u32>> {
            let pass = self.passes.get().saturating_sub(1);
            let count = self.counts.get(pass).or(self.counts.last()).copied().unwrap_or_default();
            Ok(vec![1; count])
        }
    }

    #[test]
    fn stop_event_is_named_after_its_job() {
        assert_eq!(stop_event_name(r"Local\gate-job-k-0001"), r"Local\gate-job-k-0001-stopped");
    }

    #[test]
    fn stop_sets_the_signal_of_a_run_that_has_no_job_yet() {
        let name = unique_job("no-job-yet");
        let signal = NamedEvent::create_new_manual(&stop_event_name(&name)).expect("the run's signal is created");
        let mut console = Console::new();

        assert_eq!(stopped(&name, &mut console).map_err(|error| error.to_string()), Ok(None), "no job to end");
        assert!(
            signal.is_set().expect("the signal can be looked at"),
            "the run sees the stop before it creates its job"
        );
        assert!(console.take_notes().is_empty(), "nothing is said");
    }

    #[test]
    fn stop_of_a_job_without_a_signal_says_nothing_of_the_signal() {
        let name = unique_job("no-signal");
        let _job = Job::create(&name).expect("the job is created");
        let mut console = Console::new();

        assert_eq!(stopped(&name, &mut console).map_err(|error| error.to_string()), Ok(Some(0)));
        assert!(console.take_notes().is_empty(), "a job without a signal is no error");
    }

    #[test]
    fn terminate_and_wait_fails_when_the_first_terminate_fails() {
        let job = Scripted::new(0, &[1]);
        assert_eq!(
            terminate_and_wait(&job).map(|ending| ending.left).map_err(|error| error.to_string()),
            Err("pass 0".to_owned())
        );
    }

    #[test]
    fn terminate_and_wait_goes_on_counting_past_a_later_terminate_that_fails() {
        let job = Scripted::new(1, &[2, 1, 0]);
        let ending = terminate_and_wait(&job).map_err(|error| error.to_string());
        let ending = ending.map(|ending| (ending.left, ending.late.map(|error| error.to_string())));
        assert_eq!(
            ending,
            Ok((0, Some("pass 1".to_owned()))),
            "counted to none, the first later failure kept"
        );
    }
}
