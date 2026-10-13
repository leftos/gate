//! The client a routed run calls when no test seam names one: the remote.ps1 bridge, or the Rust client over ssh when
//! the state root holds the remote-cutover flag. The switch looks for the flag once, on the run's first call of it,
//! which is [`RemoteClient::ready`] as the routed attempt starts, and sends that call and every later one of the run to
//! the client it chose. A flag whose presence cannot be read, or a Rust client whose ssh cannot be found, is why the
//! run falls back `client`, whichever call comes first.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use gate_args::TestSeams;
use gate_state::v1;
use gate_win32::OwnedMutexGuard;

use super::bridge::Bridge;
use super::client::RustClient;
use super::{RemoteCall, RemoteClient, RemoteOutcome};

/// The client the switch chose for its run.
enum Choice {
    /// No flag: the bridge.
    Bridge,
    /// The flag: the Rust client over ssh.
    Rust(Box<RustClient>),
    /// A flag whose presence cannot be read, or the flag and no ssh: why, which the run falls back `client` with.
    Failed(String),
}

/// The bridge, the flag that selects the Rust client in its place, and the client chosen once the run first calls.
pub(crate) struct Switch {
    seams: TestSeams,
    flag: PathBuf,
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
            bridge,
            choice: RefCell::new(None),
        }
    }
}

/// The client chosen for the run, held in `slot`: the one chosen before, else the one the flag at `flag` selects now
/// for a run with `seams`, the bridge when it is absent, else the Rust client over ssh, or why it cannot be made or
/// the flag cannot be read.
fn decided<'s>(slot: &'s mut Option<Choice>, flag: &Path, seams: &TestSeams) -> &'s mut Choice {
    slot.get_or_insert_with(|| match flag.try_exists() {
        Ok(false) => Choice::Bridge,
        Ok(true) => RustClient::ssh(seams).map_or_else(Choice::Failed, |client| Choice::Rust(Box::new(client))),
        Err(error) => Choice::Failed(format!("cannot read {}: {error}", flag.display())),
    })
}

impl RemoteClient for Switch {
    /// A failed choice takes no lock, so its run reaches [`Switch::run`], which falls back `client`.
    fn hold_key(&self, call: &RemoteCall<'_>) -> Result<Option<OwnedMutexGuard>, Option<String>> {
        match decided(&mut self.choice.borrow_mut(), &self.flag, &self.seams) {
            Choice::Bridge => self.bridge.hold_key(call),
            Choice::Rust(client) => client.hold_key(call),
            Choice::Failed(_) => Ok(None),
        }
    }

    fn ready(&self) -> Result<(), String> {
        match decided(&mut self.choice.borrow_mut(), &self.flag, &self.seams) {
            Choice::Bridge => self.bridge.ready(),
            Choice::Rust(client) => client.ready(),
            Choice::Failed(why) => Err(why.clone()),
        }
    }

    fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
        match decided(self.choice.get_mut(), &self.flag, &self.seams) {
            Choice::Bridge => self.bridge.run(call),
            Choice::Rust(client) => client.run(call),
            Choice::Failed(why) => RemoteOutcome::not_started(why.clone()),
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
