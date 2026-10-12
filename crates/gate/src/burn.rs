//! `gate burn`: the CPU burn load-burn runs inside a critical gate, as one native process. It spins `n` threads on a
//! busy loop at this process's own priority until the deadline, or until the process that started it ends, taking no
//! slot and writing no claim, journal line or log of its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use gate_args::{BurnRequest, USAGE};
use gate_win32::{WaitableProcess, process_start};
use jiff::Timestamp;
use proc_table::Win32ProcessTable;

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::console::Console;
use crate::wait;

/// Runs `gate burn`: the status is 0 at the deadline, 1 when the process that started this one ended first, and 1 when
/// that process cannot be watched.
pub(crate) fn burn(request: &BurnRequest, console: &mut Console) -> u32 {
    let logical = logical_processors(console);
    let cores = request.cores.unwrap_or(logical);
    if cores > logical {
        console.err(&format!(
            "gate: --cores must be at most this machine's {logical} logical processors, got {cores}"
        ));
        console.err(USAGE);
        return u32::from(USAGE_STATUS);
    }
    let Some(parent_pid) = parent_pid() else {
        console.err("gate: burn: this process is not in the process table, so its parent is unknown");
        return u32::from(FAILED_STATUS);
    };
    let parent = match WaitableProcess::open(parent_pid) {
        Ok(Some(parent)) => parent,
        Ok(None) => {
            console.err(&format!("gate: burn: the parent process {parent_pid} is not running"));
            return u32::from(FAILED_STATUS);
        }
        Err(error) => {
            console.err(&format!("gate: burn: cannot open the parent process {parent_pid}: {error}"));
            return u32::from(FAILED_STATUS);
        }
    };
    if started_after(parent.created().ok(), process_start().ok()) {
        console.err(&format!("gate: burn: the parent process {parent_pid} is not running"));
        return u32::from(FAILED_STATUS);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let burners: Vec<thread::JoinHandle<()>> = (0..cores)
        .map(|_| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || spin(&stop))
        })
        .collect();
    console.out(&format!("gate: burn: {cores} thread(s) for {} s", request.seconds));
    let ended = parent.wait(Duration::from_secs(u64::from(request.seconds)));
    stop.store(true, Ordering::Relaxed);
    for burner in burners {
        // A burning thread cannot fail: its loop is one arithmetic step and one relaxed load.
        let _ = burner.join();
    }
    match ended {
        Ok(true) => {
            console.err(&format!("gate: burn: the parent process {parent_pid} ended"));
            u32::from(FAILED_STATUS)
        }
        Ok(false) => 0,
        Err(error) => {
            console.err(&format!("gate: burn: cannot wait for the parent process {parent_pid}: {error}"));
            u32::from(FAILED_STATUS)
        }
    }
}

/// This machine's logical processors, as the thread count a burn runs; a machine with more than `u32::MAX` of them is
/// taken as that many.
fn logical_processors(console: &mut Console) -> u32 {
    u32::try_from(wait::processors(console)).unwrap_or(u32::MAX)
}

/// The pid of the process that started this one, as the system's process table names it.
fn parent_pid() -> Option<u32> {
    let me = std::process::id();
    Win32ProcessTable
        .snapshot_without_command_lines()
        .into_iter()
        .find(|process| process.pid == me)
        .map(|process| process.parent_pid)
}

/// Spins the arithmetic load-burn's burners use, on this thread, until `stop` is set. `black_box` keeps every step, so
/// an optimised build does not drop the burn.
fn spin(stop: &AtomicBool) {
    let mut state: u64 = 1;
    while !stop.load(Ordering::Relaxed) {
        state = std::hint::black_box(state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407));
    }
}

/// Whether the parent that started at `parent` started after this process did, at `own`: one that started later took
/// the pid of the process that started this burn, so the process this burn opened is not its parent. A start time
/// either side cannot read is not taken for reuse, as the process tree does not judge a process of an unknown start.
fn started_after(parent: Option<Timestamp>, own: Option<Timestamp>) -> bool {
    match (parent, own) {
        (Some(parent), Some(own)) => parent > own,
        (None, _) | (_, None) => false,
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose fixed time cannot be read must fail loudly")]
mod tests {
    use jiff::Timestamp;

    use super::started_after;

    /// The moment `text` names.
    fn at(text: &str) -> Timestamp {
        text.parse().expect("a fixed time")
    }

    #[test]
    fn a_parent_that_started_after_this_process_took_its_pid() {
        let now = at("2026-10-01T10:00:00Z");
        assert!(started_after(Some(at("2026-10-01T10:00:01Z")), Some(now)));
        assert!(
            !started_after(Some(at("2026-10-01T09:59:59Z")), Some(now)),
            "an earlier parent is the process that started this burn"
        );
        assert!(!started_after(Some(now), Some(now)), "one moment is one process");
        assert!(
            !started_after(None, Some(now)),
            "a parent whose start cannot be read is not taken for reuse"
        );
        assert!(
            !started_after(Some(at("2026-10-01T10:00:01Z")), None),
            "nor against this process's own unreadable start"
        );
    }
}
