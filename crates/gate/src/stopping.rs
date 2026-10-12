//! Stopping one process a snapshot listed, as `gate stop-tree` and a kill of `gate run` both do: the process is opened,
//! and through that handle checked to be the one the snapshot saw (its start and its parent, and for a kill that it is
//! outside the command's job) before it is terminated, so a pid another process has taken since is passed over.

use std::io;
use std::time::Duration;

use gate_win32::{Job, StoppableProcess, win32_message};
use jiff::Timestamp;

/// How long a process whose termination failed is given to end before the failure counts.
const TERMINATE_CHECK: Duration = Duration::from_secs(1);

/// One process, as the snapshot saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) pid: u32,
    /// The id of the process that started it.
    pub(crate) parent: u32,
    pub(crate) created: Option<Timestamp>,
}

/// What stopping one process came to.
pub(crate) enum Stop {
    /// It had ended, ended within its grace, its pid now belongs to another process, or whether it does could not be
    /// read.
    Gone,
    /// This terminated it.
    Terminated,
    /// It could not be stopped: why.
    Failed(String),
}

/// What opening one process the snapshot listed came to.
pub(crate) enum Opened {
    /// No process has its pid now, or the one that has it is another process, or is in the job: passed over.
    Gone,
    /// The process the snapshot saw, open to be stopped.
    Same(StoppableProcess),
    /// Whether it is the process the snapshot saw could not be read: its start, its parent or its job. Left running.
    Unjudged,
    /// It could not be opened: why.
    Failed(String),
}

/// How the process behind a pid compares with the one the snapshot saw.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Judged {
    /// It is the process the snapshot saw, outside the job.
    Same,
    /// Another process, or one in the job: some reading differs.
    Other,
    /// No reading differs, but one could not be read.
    Unknown,
}

/// How the process read now compares with `node`: its start `created`, its parent `parent` and whether it is in the
/// job, `in_job`. Any reading that differs makes it another process; else any reading that failed, or a start the
/// snapshot does not have, leaves it unknown.
pub(crate) fn judged(node: &Node, created: &io::Result<Timestamp>, parent: &io::Result<u32>, in_job: &io::Result<bool>) -> Judged {
    let differs = created.as_ref().is_ok_and(|now| node.created.is_some_and(|seen| seen != *now))
        || parent.as_ref().is_ok_and(|now| *now != node.parent)
        || in_job.as_ref().is_ok_and(|inside| *inside);
    if differs {
        Judged::Other
    } else if node.created.is_none() || created.is_err() || parent.is_err() || in_job.is_err() {
        Judged::Unknown
    } else {
        Judged::Same
    }
}

/// Opens the process `node` and checks through the handle that it is the process the snapshot saw: its start and its
/// parent, and, given a `job`, that it is outside that job.
pub(crate) fn open_checked(node: &Node, job: Option<&Job>) -> Opened {
    let process = match StoppableProcess::open(node.pid) {
        Ok(Some(process)) => process,
        Ok(None) => return Opened::Gone,
        Err(error) => return Opened::Failed(failure(&format!("OpenProcess({})", node.pid), &error)),
    };
    let in_job = job.map_or(Ok(false), |job| process.in_job(job));
    match judged(node, &process.created(), &process.parent_pid(), &in_job) {
        Judged::Same => Opened::Same(process),
        Judged::Other => Opened::Gone,
        Judged::Unknown => Opened::Unjudged,
    }
}

/// Terminates the process `node` with `status` unless it has ended, ends within `grace`, or is not the process the
/// snapshot saw, or whether it is cannot be read. A failed termination counts as a failure only when the process is
/// still running [`TERMINATE_CHECK`] later.
pub(crate) fn stop_process(node: &Node, grace: Duration, status: u32) -> Stop {
    let process = match open_checked(node, None) {
        Opened::Same(process) => process,
        Opened::Gone | Opened::Unjudged => return Stop::Gone,
        Opened::Failed(error) => return Stop::Failed(error),
    };
    // A wait that fails has not seen the process end, so the process is terminated as one still running is.
    if matches!(process.wait(grace), Ok(true)) {
        return Stop::Gone;
    }
    match process.terminate(status) {
        Ok(()) => Stop::Terminated,
        Err(_) if matches!(process.wait(TERMINATE_CHECK), Ok(true)) => Stop::Gone,
        Err(error) => Stop::Failed(failure(&format!("TerminateProcess({})", node.pid), &error)),
    }
}

/// `<operation> failed with Win32 error <code>: <message>`, as the PowerShell gate words a failed call.
pub(crate) fn failure(operation: &str, error: &io::Error) -> String {
    match error.raw_os_error().and_then(|code| u32::try_from(code).ok()) {
        Some(code) => format!("{operation} failed with Win32 error {code}: {}", win32_message(code)),
        None => format!("{operation} failed: {error}"),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test timestamp that does not parse must fail loudly")]
mod tests {
    use std::io;

    use jiff::Timestamp;

    use super::{Judged, Node, judged};

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("a test timestamp parses")
    }

    fn unreadable<T>() -> io::Result<T> {
        Err(io::Error::other("unreadable"))
    }

    fn seen() -> Node {
        Node {
            pid: 40,
            parent: 7,
            created: Some(ts("2026-10-01T10:00:00Z")),
        }
    }

    /// How the process read now as `created`, `parent` and `in_job` compares with `node`.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "each case writes its readings as values, as a process gives them"
    )]
    fn judge(node: &Node, created: io::Result<Timestamp>, parent: io::Result<u32>, in_job: io::Result<bool>) -> Judged {
        judged(node, &created, &parent, &in_job)
    }

    #[test]
    fn only_the_same_start_and_parent_outside_the_job_is_the_same_process() {
        let start = ts("2026-10-01T10:00:00Z");
        assert_eq!(judge(&seen(), Ok(start), Ok(7), Ok(false)), Judged::Same);
        assert_eq!(
            judge(&seen(), Ok(ts("2026-10-01T10:00:05Z")), Ok(7), Ok(false)),
            Judged::Other,
            "a reused pid is passed over"
        );
        assert_eq!(
            judge(&seen(), Ok(start), Ok(8), Ok(false)),
            Judged::Other,
            "the snapshot's start read late for another parent's process"
        );
        assert_eq!(
            judge(&seen(), Ok(start), Ok(7), Ok(true)),
            Judged::Other,
            "a process in the job is the job's"
        );
    }

    #[test]
    fn a_reading_that_fails_leaves_the_process_unknown_unless_another_differs() {
        let start = ts("2026-10-01T10:00:00Z");
        assert_eq!(judge(&seen(), unreadable(), Ok(7), Ok(false)), Judged::Unknown, "its start");
        assert_eq!(judge(&seen(), Ok(start), unreadable(), Ok(false)), Judged::Unknown, "its parent");
        assert_eq!(judge(&seen(), Ok(start), Ok(7), unreadable()), Judged::Unknown, "its job");
        let no_start = Node { created: None, ..seen() };
        assert_eq!(
            judge(&no_start, Ok(start), Ok(7), Ok(false)),
            Judged::Unknown,
            "a start the snapshot lacks"
        );
        assert_eq!(
            judge(&seen(), unreadable(), Ok(8), Ok(false)),
            Judged::Other,
            "a parent that differs settles it"
        );
    }
}
