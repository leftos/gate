//! `gate box desktop-hold`: holds the builder's desktop lock for an agent that drives the console's desktop, so a
//! locked desktop run waits for it as for another run, naming it by the holder text in the holder record. The lock is
//! opened, waited for, held and let go on the one thread that answers the verb, as a Windows mutex is released only by
//! the thread that owns it; a helper thread only reads the box's standard input to its end and says so. The hold ends
//! when that input ends, which is the PC's gate letting it go or its ssh connection ending, or at its ceiling; either
//! way the record is removed before the lock is let go.

use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::{BoxRequest, Problem};
use gate_router::escaped;
use gate_win32::{MutexGuard, PrivateMutex};
use jiff::Timestamp;

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::desktop::lines::{Limits, abandoned_line, busy_wait_line};
use crate::boxside::run::REFUSED_STATUS;
use crate::boxside::run::desktop_run::{HolderRecord, Turns, Unheld, desktop_name, waited};
use crate::boxside::stamping;
use crate::console::Console;

/// A hold's status when its ceiling ended it.
const CEILING_STATUS: u32 = 124;
/// The line of a hold its input's end let go.
const RELEASED: &str = "gate: desktop hold released";
/// The line of a hold whose input ended before it held the lock.
const ENDED_BEFORE: &str = "gate: desktop hold ended before it held the desktop";

/// How a held hold ended.
enum Ended {
    /// Its standard input ended.
    Released,
    /// Its ceiling passed first.
    Ceiling,
}

/// Answers `gate box desktop-hold`: takes the builder's desktop lock within the hold's wait, writes the holder record
/// naming the hold's holder in its root, says the held line on stdout, and holds the lock until the box's standard
/// input ends or its ceiling passes, then removes the record and lets the lock go. The exit status: 0 after the
/// released line, or after the ended-before line for input that ended before the lock was held, which then writes no
/// record and holds nothing; [`CEILING_STATUS`] after the ceiling line; [`REFUSED_STATUS`] after the stayed-busy line when the
/// lock stayed held for the whole wait; [`FAILED_STATUS`] after `gate: desktop hold failed: {why}` when the lock cannot
/// be opened or waited for.
pub(super) fn desktop_hold(request: &BoxRequest, console: &mut Console) -> u32 {
    let (Some(root), Some(hold)) = (request.root.as_deref(), request.hold.as_ref()) else {
        // The parse gives `desktop-hold` its root and its words always; a request without them is the box's usage error.
        stamping::err(console, &Problem::BoxUsage.to_string());
        return u32::from(USAGE_STATUS);
    };
    let mutex = match desktop_name(&request.seams).and_then(|name| PrivateMutex::open(&name)) {
        Ok(mutex) => mutex,
        Err(error) => return failed(&format!("the builder's desktop lock cannot be opened: {error}"), console),
    };
    // Watched from before the take, so input that ends while the hold waits ends the wait.
    let input = match input_watched() {
        Ok(input) => input,
        Err(error) => return failed(&format!("its input cannot be watched: {error}"), console),
    };
    let record = HolderRecord::in_root(Some(root));
    let turns = Turns {
        total: Duration::from_secs(hold.wait),
        step: Limits::for_run(request.seams.desktop_ms.map(Duration::from_millis), None).wait_step,
        now: Instant::now,
    };
    let guard = match taken(&mutex, &turns, &record, &input, console) {
        Ok(guard) => guard,
        Err(status) => return status,
    };
    // Input that ended as the lock was taken lets it go before the hold is said or recorded.
    if !matches!(input.try_recv(), Err(TryRecvError::Empty)) {
        drop(guard);
        stamping::err(console, ENDED_BEFORE);
        return 0;
    }
    record.write(&hold.holder, &mut |line: &str| stamping::err(console, line));
    stamping::out(
        console,
        &format!(
            "gate: holding the builder's desktop as {} (ceiling {} s)",
            escaped(&hold.holder),
            hold.ceiling
        ),
    );
    let ended = ended_by(&input, Duration::from_secs(hold.ceiling));
    // Removed while the lock is held, so the hold never removes the next holder's record.
    record.remove(&mut |line: &str| stamping::err(console, line));
    drop(guard);
    match ended {
        Ended::Released => {
            stamping::err(console, RELEASED);
            0
        }
        Ended::Ceiling => {
            stamping::err(console, &format!("gate: desktop hold ended at its ceiling of {} s", hold.ceiling));
            CEILING_STATUS
        }
    }
}

/// The builder's desktop lock `mutex`, taken within the hold's wait cut into `turns`, its `input` looked at before
/// each turn: the busy-wait line, naming the holder `record` names, said once when another holds it and some of the
/// wait is left, and a lock taken from a holder that ended holding it said. Else the status to exit with, its line
/// said: 0 for input that ended first, which takes nothing.
fn taken<'m>(
    mutex: &'m PrivateMutex,
    turns: &Turns<impl Fn() -> Instant>,
    record: &HolderRecord,
    input: &Receiver<()>,
    console: &mut Console,
) -> Result<MutexGuard<'m>, u32> {
    let busy = |console: &mut Console| {
        let holder = record.holder(&mut |line: &str| stamping::err(console, line));
        stamping::err(console, &busy_wait_line(holder.as_ref(), Timestamp::now()));
    };
    // The only end the look makes is the input's, which ends the hold with 0.
    let look = |_: &mut Console| match input.try_recv() {
        Err(TryRecvError::Empty) => Ok(()),
        Ok(()) | Err(TryRecvError::Disconnected) => Err(0),
    };
    match waited(turns, console, look, |bound| mutex.acquire(bound), busy) {
        Ok(guard) => {
            if guard.abandoned() {
                stamping::err(console, &abandoned_line());
            }
            Ok(guard)
        }
        Err(Unheld::Busy) => {
            stamping::err(
                console,
                &format!("gate: desktop hold: the builder's desktop stayed busy for {} s", turns.total.as_secs()),
            );
            Err(REFUSED_STATUS)
        }
        Err(Unheld::Failed(error)) => Err(failed(&format!("the builder's desktop lock cannot be waited for: {error}"), console)),
        Err(Unheld::Ended(status)) => {
            stamping::err(console, ENDED_BEFORE);
            Err(status)
        }
    }
}

/// Says `gate: desktop hold failed: {why}`; [`FAILED_STATUS`].
fn failed(why: &str, console: &mut Console) -> u32 {
    stamping::err(console, &format!("gate: desktop hold failed: {why}"));
    u32::from(FAILED_STATUS)
}

/// The end of the box's standard input, which a helper thread reads to its end and then says on the channel. The
/// helper owns only its sender: it never touches the lock or the record, and one still reading when the hold ends ends
/// with the process. Else why the helper cannot start.
fn input_watched() -> io::Result<Receiver<()>> {
    let (ended, input_ended) = mpsc::channel();
    thread::Builder::new().name("desktop-hold-input".to_owned()).spawn(move || {
        read_to_end(io::stdin());
        // The hold may have ended at its ceiling and gone; nothing then listens.
        let _ = ended.send(());
    })?;
    Ok(input_ended)
}

/// Waits until the box's standard input ends, as `input` says, or `ceiling` passes.
fn ended_by(input: &Receiver<()>, ceiling: Duration) -> Ended {
    match input.recv_timeout(ceiling) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => Ended::Released,
        Err(RecvTimeoutError::Timeout) => Ended::Ceiling,
    }
}

/// Reads `input` until it ends or fails, dropping what it reads: a pipe whose writer closed reads as its end, and one
/// broken fails, which ends it the same.
fn read_to_end(mut input: impl Read) {
    let mut sink = [0_u8; 4096];
    loop {
        match input.read(&mut sink) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}
