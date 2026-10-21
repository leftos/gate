//! The covered marker: the file a gate running under a parent gate's slot keeps under the covered folder while its
//! command runs, so ops-tower sees a gate that takes no slot and writes no ticket or claim. Its format is gate-state's
//! ([`v1::CoveredMarker`]).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use gate_args::Slot;
use gate_state::JournalIdentity;
use gate_state::v1::{self, CoveredMarker};
use jiff::Timestamp;

use crate::console::Console;
use crate::slots::{self, ThisGate, epoch_ms};

/// The covered marker of this gate, written as its covered run starts and deleted as it ends: by [`Covered::end`] on
/// every ordinary end, by [`Drop`] after a panic or an end that skipped it.
pub(crate) struct Covered {
    /// Where the marker is, and what it says; `None` once it was deleted, or when it was never written.
    written: Option<(PathBuf, CoveredMarker)>,
}

impl Covered {
    /// Writes the marker of the gate `gate` running under the slot of the gate `parent_pid`, under the state root
    /// `root` ([`Covered::write_as`]).
    pub(crate) fn write(root: &Path, gate: &ThisGate, parent_pid: u32, console: &mut Console) -> Covered {
        Covered::write_as(root, gate.journal_identity().as_ref(), parent_pid, console)
    }

    /// Writes the marker of the gate `identity` names running under the slot of the gate `parent_pid`, under the state
    /// root `root`. A gate whose identity could not be read (`Err`, its working directory unreadable) writes no marker,
    /// as it writes no `covered` line, and says nothing more: the marker's `cwd` field cannot be filled, and its journal
    /// says why it wrote nothing. A marker that cannot be written is said, and the run goes on without one.
    pub(crate) fn write_as(root: &Path, identity: Result<&JournalIdentity, &String>, parent_pid: u32, console: &mut Console) -> Covered {
        let Ok(identity) = identity else {
            return Covered { written: None };
        };
        let marker = CoveredMarker {
            pid: identity.pid,
            process_start: epoch_ms(identity.process_start),
            parent_pid,
            command: identity.command.clone(),
            cwd: identity.cwd.clone(),
            since: epoch_ms(Timestamp::now()),
            kind: Some(identity.kind.clone()),
        };
        let path = v1::covered_path(root, marker.pid, marker.process_start);
        match v1::write_covered(&path, &marker) {
            Ok(()) => Covered {
                written: Some((path, marker)),
            },
            Err(error) => {
                console.err(&format!("gate: cannot write the covered marker {}: {error}", path.display()));
                Covered { written: None }
            }
        }
    }

    /// Whether the marker was written and is still there.
    pub(crate) fn written(&self) -> bool {
        self.written.is_some()
    }

    /// Rewrites the marker with the kind `none`, as a gate that holds no slot and takes none writes it: this gate gave
    /// up watching its parent, so it takes no slot when that parent ends, and no gate started inside its command may run
    /// under it in that parent's place ([`crate::nesting::enclosing`]). A marker that cannot be rewritten is said, and
    /// the run goes on.
    pub(crate) fn disown(&mut self, console: &mut Console) {
        let Some((path, marker)) = &mut self.written else {
            return;
        };
        marker.kind = Some(Slot::None.as_str().to_owned());
        if let Err(error) = v1::write_covered(path, marker) {
            console.err(&format!("gate: cannot rewrite the covered marker {}: {error}", path.display()));
        }
    }

    /// Deletes the marker, saying so when it cannot be deleted, and forgets it, so [`Drop`] has nothing left to do.
    pub(crate) fn end(&mut self, console: &mut Console) {
        if let Some((path, _)) = self.written.take()
            && let Err(error) = slots::delete(&path)
        {
            console.err(&format!("gate: cannot delete the covered marker {}: {error}", path.display()));
        }
    }
}

impl Drop for Covered {
    /// Deletes a marker a panic left behind, [`Covered::end`] having cleared the path on every ordinary end. The line
    /// for a failure is said on stderr alone, the run's console being out of reach by now.
    fn drop(&mut self) {
        let Some((path, _)) = self.written.take() else {
            return;
        };
        if let Err(error) = slots::delete(&path) {
            // Nothing is left to report the failure through but the stream itself; a failure to write there is lost.
            let _ = writeln!(io::stderr(), "gate: cannot delete the covered marker {}: {error}", path.display());
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot build or read its scratch folder must fail loudly")]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use gate_state::JournalIdentity;
    use gate_state::v1;
    use jiff::Timestamp;

    use super::Covered;
    use crate::console::Console;

    /// A scratch state root for one test, emptied first.
    fn scratch(test: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-covered-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
    }

    #[test]
    fn a_gate_whose_working_directory_cannot_be_read_writes_no_marker_and_says_nothing() {
        let root = scratch("unread-cwd");
        let mut console = Console::new();
        let reason = "cannot read the working directory: denied".to_owned();

        let mut covered = Covered::write_as(&root, Err(&reason), 52000, &mut console);
        covered.end(&mut console);

        assert!(!v1::covered_dir(&root).exists(), "no marker, and no folder for one");
        assert!(console.take_notes().is_empty(), "the journal says why; the marker says nothing more");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_gate_with_its_identity_writes_its_marker_until_it_ends() {
        let root = scratch("identity");
        let mut console = Console::new();
        let start: Timestamp = "2026-10-01T11:00:00.5Z".parse().expect("the start parses");
        let identity = JournalIdentity {
            pid: 54000,
            process_start: start,
            kind: "light".to_owned(),
            cwd: r"C:\work\repo".to_owned(),
            command: "cargo fmt --check".to_owned(),
            commit: false,
        };

        let mut covered = Covered::write_as(&root, Ok(&identity), 52000, &mut console);
        let read = v1::read_covered(&v1::covered_dir(&root)).expect("the covered folder lists");
        let markers: Vec<(u32, u64, u32, &str, &str)> = read
            .iter()
            .map(|(_, marker)| {
                (
                    marker.pid,
                    marker.process_start,
                    marker.parent_pid,
                    marker.command.as_str(),
                    marker.cwd.as_str(),
                )
            })
            .collect();
        assert_eq!(markers, [(54000, 1_790_852_400_500, 52000, "cargo fmt --check", r"C:\work\repo")]);
        let kinds: Vec<Option<&str>> = read.iter().map(|(_, marker)| marker.kind.as_deref()).collect();
        assert_eq!(kinds, [Some("light")], "the marker names the gate's own kind");

        covered.end(&mut console);
        assert!(
            v1::read_covered(&v1::covered_dir(&root)).expect("the covered folder lists").is_empty(),
            "the marker is gone"
        );
        assert!(console.take_notes().is_empty(), "nothing failed, so nothing is said");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_disowned_marker_names_the_kind_none_until_it_ends() {
        let root = scratch("disowned");
        let mut console = Console::new();
        let identity = JournalIdentity {
            pid: 54000,
            process_start: "2026-10-01T11:00:00.5Z".parse().expect("the start parses"),
            kind: "heavy".to_owned(),
            cwd: r"C:\work\repo".to_owned(),
            command: "cargo build".to_owned(),
            commit: false,
        };
        let kinds = || -> Vec<Option<String>> {
            v1::read_covered(&v1::covered_dir(&root))
                .expect("the covered folder lists")
                .into_iter()
                .map(|(_, marker)| marker.kind)
                .collect()
        };

        let mut covered = Covered::write_as(&root, Ok(&identity), 52000, &mut console);
        assert!(covered.written());
        covered.disown(&mut console);
        assert_eq!(kinds(), [Some("none".to_owned())], "the marker no longer names a kind that takes a slot");
        covered.end(&mut console);
        assert!(kinds().is_empty(), "the marker is gone");
        assert!(console.take_notes().is_empty(), "nothing failed, so nothing is said");
        let _ = fs::remove_dir_all(&root);
    }
}
