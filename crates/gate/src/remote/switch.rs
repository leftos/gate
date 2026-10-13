//! The client a routed run calls when no test seam names one: the remote.ps1 bridge, or the Rust client over ssh when
//! the state root holds the remote-cutover flag. The switch chooses once, on the run's first call of it,
//! [`RemoteClient::ready`] as the routed attempt starts, and sends that call and every later one of the run to the
//! client it chose. While the remote cutover's drain marker names a live drainer, or stands but cannot be opened, the
//! choice waits for the drain to end, holding nothing. A flag whose presence cannot be read, a Rust client whose ssh cannot be found, or a drain
//! marker whose drainer is not running is why the run falls back `client`; so is a call made before `ready`.

use std::cell::RefCell;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use gate_args::TestSeams;
use gate_state::{DrainMarker, read_drain_marker, v1};
use gate_win32::{Interrupt, OwnedMutexGuard};
use proc_table::Win32ProcessTable;

use super::bridge::Bridge;
use super::client::RustClient;
use super::{RemoteCall, RemoteClient, RemoteOutcome, Unready};
use crate::console::Console;

/// How often a run waiting for the remote cutover's drain reads the flag and the drain marker again.
const DRAIN_POLL: Duration = Duration::from_secs(1);
/// How many polls the reads of a drain marker that stands must be refused without a break, 5 s at [`DRAIN_POLL`],
/// before the run says it cannot read the marker; a shorter refusal, such as a pending delete, says nothing.
const REFUSED_POLLS: u32 = 5;
/// Why a run whose drain marker names a drainer that is not running, or cannot be read, does not route.
const NOT_DRAINING: &str = "the remote cutover's drain is not running";
/// Why a call made before [`RemoteClient::ready`] chose a client starts nothing.
const UNDECIDED: &str = "the remote client was called before it was ready";

/// The client the switch chose for its run.
enum Choice {
    /// No flag: the bridge.
    Bridge,
    /// The flag and no live drainer: the Rust client over ssh.
    Rust(Box<RustClient>),
    /// A flag whose presence cannot be read, or the flag and no ssh: why, which the run falls back `client` with.
    Failed(String),
    /// The flag and a drain marker whose drainer is not running, or which cannot be read: the run falls back `client`
    /// saying the drain should be run again.
    Undrained,
}

/// The bridge, the flag that selects the Rust client in its place, the state root that holds the drain marker the
/// choice waits on, and the client chosen once the run first calls.
pub(crate) struct Switch {
    seams: TestSeams,
    flag: PathBuf,
    state_dir: PathBuf,
    bridge: Bridge,
    choice: RefCell<Option<Choice>>,
}

impl Switch {
    /// The switch of a run with `seams` in the state root `state_dir`, with `bridge` as the client it keeps without
    /// the flag.
    pub(crate) fn new(seams: &TestSeams, state_dir: &Path, bridge: Bridge) -> Switch {
        Switch {
            seams: seams.clone(),
            flag: v1::remote_cutover_flag_path(state_dir),
            state_dir: state_dir.to_owned(),
            bridge,
            choice: RefCell::new(None),
        }
    }

    /// The client the state root selects for the run now: the bridge without the flag; with it, the Rust client over
    /// ssh when no drain marker stands, or why it cannot be made. While the marker names a live drainer, the choice
    /// waits, saying so once on `console`, and reads the flag and the marker again every poll; a flag gone meanwhile is
    /// the bridge. A marker that stands but whose opening is refused is waited on too ([`Switch::marker`]); refusals
    /// that last [`REFUSED_POLLS`] polls without a break are said once ([`DrainWait::refused`]). A marker whose drainer
    /// is not running, or which cannot be read, is [`Choice::Undrained`], and a flag whose presence cannot be read a
    /// failed choice. `Err` when `interrupt` is raised during the wait.
    fn decide(&self, console: &mut Console, interrupt: Option<Interrupt>) -> Result<Choice, Unready> {
        let poll = self.seams.poll_ms.map_or(DRAIN_POLL, Duration::from_millis);
        let path = v1::remote_cutover_draining_path(&self.state_dir);
        let mut wait = DrainWait::default();
        loop {
            match self.flag.try_exists() {
                Ok(false) => return Ok(Choice::Bridge),
                Ok(true) => {}
                Err(error) => return Ok(Choice::Failed(format!("cannot read {}: {error}", self.flag.display()))),
            }
            match self.marker(&path) {
                Marker::Gone => return Ok(RustClient::ssh(&self.seams).map_or_else(Choice::Failed, |client| Choice::Rust(Box::new(client)))),
                Marker::NotRunning => return Ok(Choice::Undrained),
                Marker::Alive(pid) => wait.alive(pid, console),
                Marker::Refused(reason) => wait.refused(&path, &reason, console),
            }
            if interrupt.is_some_and(Interrupt::is_raised) {
                return Err(Unready::Interrupted);
            }
            thread::sleep(poll);
        }
    }

    /// The drain marker at `path`, under the state root, as the choice reads it ([`read_drain_marker`]): a marker read
    /// as none while its file still stands was refused, not gone, and one whose presence then cannot be read counts as
    /// not running, as an unreadable marker does.
    fn marker(&self, path: &Path) -> Marker {
        match read_drain_marker(&self.state_dir, &Win32ProcessTable) {
            Some(DrainMarker::Alive { pid, .. }) => Marker::Alive(pid),
            Some(DrainMarker::Dead | DrainMarker::Unreadable) => Marker::NotRunning,
            None => match path.try_exists() {
                Ok(false) => Marker::Gone,
                Ok(true) => Marker::Refused(refusal(path)),
                Err(_) => Marker::NotRunning,
            },
        }
    }
}

/// Why the drain marker at `path`, which stands, read as none: the error of opening it again, or, when that open is
/// not refused, that it reads as none all the same.
fn refusal(path: &Path) -> String {
    match File::open(path) {
        Err(error) => error.to_string(),
        Ok(_) => "it exists but reads as none".to_owned(),
    }
}

/// The remote cutover's drain marker as [`Switch::marker`] reads it.
enum Marker {
    /// No marker stands: the drain is over, or never began.
    Gone,
    /// The marker names a live drainer: its pid.
    Alive(u32),
    /// The marker stands but opening it is refused, as when its delete is pending or a handle on it shares no read: the
    /// drain may still run. Why it read as none ([`refusal`]).
    Refused(String),
    /// The marker names a drainer that is not running, or cannot be read.
    NotRunning,
}

/// What a run waiting for the drain has said, and how many polls its reads of the marker have been refused without a
/// break since the first of them.
#[derive(Default)]
struct DrainWait {
    said_alive: bool,
    refused_polls: Option<u32>,
    said_refused: bool,
}

impl DrainWait {
    /// A read named the live drainer `pid`: says once that the run waits for it, and breaks a run of refusals.
    fn alive(&mut self, pid: u32, console: &mut Console) {
        self.refused_polls = None;
        if !self.said_alive {
            console.err(&format!("gate: waiting for the remote cutover's drain (pid {pid}) before routing"));
            self.said_alive = true;
        }
    }

    /// Opening the marker at `path` was refused for `reason`: once refusals have lasted [`REFUSED_POLLS`] polls without
    /// a break, says once that the run waits for the remote cutover and cannot read the marker.
    fn refused(&mut self, path: &Path, reason: &str, console: &mut Console) {
        let polls = self.refused_polls.map_or(0, |polls| polls + 1);
        self.refused_polls = Some(polls);
        if polls >= REFUSED_POLLS && !self.said_refused {
            console.err(&format!(
                "gate: waiting for the remote cutover; cannot read {} ({reason})",
                path.display()
            ));
            self.said_refused = true;
        }
    }
}

impl RemoteClient for Switch {
    /// A failed or undrained choice, or none yet, takes no lock, so its run reaches [`Switch::run`], which falls back
    /// `client`.
    fn hold_key(&self, call: &RemoteCall<'_>) -> Result<Option<OwnedMutexGuard>, Option<String>> {
        match &*self.choice.borrow() {
            Some(Choice::Bridge) => self.bridge.hold_key(call),
            Some(Choice::Rust(client)) => client.hold_key(call),
            Some(Choice::Failed(_) | Choice::Undrained) | None => Ok(None),
        }
    }

    /// Chooses the client on the run's first call, waiting for a live drain first ([`Switch::decide`]); an interrupted
    /// wait chooses none.
    fn ready(&self, console: &mut Console, interrupt: Option<Interrupt>) -> Result<(), Unready> {
        let mut slot = self.choice.borrow_mut();
        let choice = match slot.take() {
            Some(choice) => choice,
            None => self.decide(console, interrupt)?,
        };
        match slot.insert(choice) {
            Choice::Bridge => self.bridge.ready(console, interrupt),
            Choice::Rust(client) => client.ready(console, interrupt),
            Choice::Failed(why) => Err(Unready::Missing(why.clone())),
            Choice::Undrained => Err(Unready::RunsHere(format!(
                "gate: {NOT_DRAINING}; run gate remote-cutover again; running here"
            ))),
        }
    }

    fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
        match self.choice.get_mut() {
            Some(Choice::Bridge) => self.bridge.run(call),
            Some(Choice::Rust(client)) => client.run(call),
            Some(Choice::Failed(why)) => RemoteOutcome::not_started(why.clone()),
            Some(Choice::Undrained) => RemoteOutcome::not_started(NOT_DRAINING.to_owned()),
            None => RemoteOutcome::not_started(UNDECIDED.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::LazyLock;

    use gate_args::TestSeams;
    use gate_state::{FallbackReason, StateRoot};

    use super::{Choice, Switch};
    use crate::remote::bridge::Bridge;
    use crate::remote::{RemoteCall, RemoteClient};

    /// A state root no gate uses, which a failed choice never reads.
    static STATE: LazyLock<StateRoot> = LazyLock::new(|| StateRoot::new(r"C:\gate-switch-tests", None));

    fn call() -> RemoteCall<'static> {
        RemoteCall {
            host: "builder",
            key: "app-1a2b3c4d",
            root: r"C:\gate-switch-tests\app",
            repo: "app",
            subfolder: "",
            words: &[],
            outputs: &[],
            outputs_withheld: false,
            bring_tracked: &[],
            log: Path::new(r"C:\gate-switch-tests\out.log"),
            timeout: 3600,
            stall: 0,
            tail: 0,
            no_markers: false,
            interrupt: None,
            state: &STATE,
            state_dir: Path::new(r"C:\gate-switch-tests"),
        }
    }

    #[test]
    fn a_failed_choice_takes_no_key_and_its_run_falls_back_client() {
        let mut switch = Switch {
            seams: TestSeams::default(),
            flag: PathBuf::from(r"C:\gate-switch-tests\remote-cutover.flag"),
            state_dir: PathBuf::from(r"C:\gate-switch-tests"),
            bridge: Bridge::new(None, None),
            choice: RefCell::new(Some(Choice::Failed("cannot run ssh: not found".to_owned()))),
        };

        let held = switch.hold_key(&call());
        let outcome = switch.run(&call());

        assert!(matches!(held, Ok(None)), "a failed choice is no busy key");
        assert_eq!(outcome.fallback(), Some(FallbackReason::Client), "{outcome:?}");
        assert_eq!(outcome.detail.as_deref(), Some("cannot run ssh: not found"));
    }
}
