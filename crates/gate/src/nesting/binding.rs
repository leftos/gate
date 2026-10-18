//! Whether a gate runs inside the command of the parent gate its marker names: the parent's named job holds this gate
//! or one of its ancestors, each ancestor still the process the snapshot saw. A marker any process can set is honoured
//! only when it binds so, or when the binding cannot be judged: the parent's job is denied to this gate under a parent
//! that is its own ancestor, the snapshot does not list this gate, or a membership query fails on a process that
//! opened. A process the walk cannot open, for any reason, unbinds the marker: the snapshot opened it with the same
//! access, so it changed since, and a race a process can retry must not cover it.

use std::borrow::Borrow;
use std::io;

use gate_state::{KernelObject, StateRoot};
use gate_win32::{Membership, OpenedJob};
use jiff::Timestamp;
use proc_table::{ProcessTree, Win32ProcessTable};

use crate::slots::epoch_ms;

/// `ERROR_ACCESS_DENIED`, as a raw OS error.
const ERROR_ACCESS_DENIED: i32 = 5;

/// Whether this gate runs inside the command of the gate its marker names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Binding {
    /// It does: the marker is honoured.
    Inside,
    /// It does not: the gate ignores the marker and runs as a gate under no parent.
    Outside,
    /// It cannot be judged, for this reason: the marker is honoured as when the gate runs inside, and the gate says so.
    Unjudged(String),
}

/// The two gates a binding is judged between: this gate, by its pid and the creation time its own process handle
/// gives (`None` when that cannot be read), and the parent gate its marker names, by its pid and its start in Unix
/// epoch milliseconds.
pub(crate) struct Gates {
    pub own: u32,
    pub own_created: Option<Timestamp>,
    pub parent: u32,
    pub parent_start_ms: u64,
}

/// Whether this gate runs inside the command of the gate `pid` started at `start_ms` under `root`, as [`runs_inside`]
/// judges it from that gate's job, opened to list, this gate's own start and one snapshot without command lines.
pub(crate) fn runs_inside_command_of(root: &StateRoot, pid: u32, start_ms: u64) -> Binding {
    runs_inside_command_with(root, pid, start_ms, snapshot)
}

/// [`runs_inside_command_of`], the snapshot given by `tree` when it is needed, so several judgements can share one.
pub(crate) fn runs_inside_command_with<T: Borrow<ProcessTree>>(root: &StateRoot, pid: u32, start_ms: u64, tree: impl FnOnce() -> T) -> Binding {
    let name = root.object_name(KernelObject::Job { gate_pid: pid });
    let gates = Gates {
        own: std::process::id(),
        // A start this gate cannot read skips only the first question; the walk asks about this gate again by the
        // start the snapshot gives it.
        own_created: gate_win32::process_start().ok(),
        parent: pid,
        parent_start_ms: start_ms,
    };
    runs_inside(OpenedJob::open_to_list(&name), tree, &gates, OpenedJob::holds)
}

/// Whether the job named `name`, a box run's job ([`crate::environment::BOX_JOB_VARIABLE`]) opened to list by that
/// name, holds this gate's own process, by the start its own handle gives. No ancestor is asked and no snapshot is
/// taken: a box run's job lets no process break away, so a gate started inside the run is in its job itself. Outside
/// when no job has the name or the job does not hold this gate; unjudged, for the system's reason, when the job
/// exists and cannot be opened, this gate's start cannot be read or the query fails.
pub(crate) fn job_holds_this_gate(name: &str) -> Binding {
    let job = match OpenedJob::open_to_list(name) {
        Ok(Some(job)) => job,
        Ok(None) => return Binding::Outside,
        Err(error) => return Binding::Unjudged(error.to_string()),
    };
    let created = match gate_win32::process_start() {
        Ok(created) => created,
        Err(error) => return Binding::Unjudged(format!("cannot read this gate's start: {error}")),
    };
    match job.holds(std::process::id(), created) {
        Ok(Membership::Member) => Binding::Inside,
        Ok(Membership::Outside | Membership::NotOpened | Membership::Reused) => Binding::Outside,
        Err(error) => Binding::Unjudged(error.to_string()),
    }
}

/// A snapshot of the processes without their command lines, as the walk reads it.
pub(crate) fn snapshot() -> ProcessTree {
    ProcessTree::new(Win32ProcessTable.snapshot_without_command_lines())
}

/// Whether the job of the gate `pid` under `root` holds this gate's own process, by the start its own handle gives:
/// the question [`runs_inside`] asks first, with no snapshot. False when the job cannot be opened, this gate's start
/// cannot be read or the query fails, which the walk would ask about again.
pub(crate) fn holds_this_gate(root: &StateRoot, pid: u32) -> bool {
    let name = root.object_name(KernelObject::Job { gate_pid: pid });
    let Ok(Some(job)) = OpenedJob::open_to_list(&name) else {
        return false;
    };
    gate_win32::process_start().is_ok_and(|created| matches!(job.holds(std::process::id(), created), Ok(Membership::Member)))
}

/// Whether the gate `gates.own` runs inside the command whose job is `job`, as opened. Outside when no job has the name,
/// and when the name opens with an error other than access denied, as when another kind of object holds it. A job
/// this gate may not open, as an elevated parent's, is unjudged under a parent among this gate's ancestors in the
/// snapshot `tree` gives, and outside under any other. A job that opens is asked first whether it holds this gate
/// itself, by the start its own handle gives, with no snapshot taken; when it does not, the [`walk`] judges.
pub(crate) fn runs_inside<J, T: Borrow<ProcessTree>>(
    job: io::Result<Option<J>>,
    tree: impl FnOnce() -> T,
    gates: &Gates,
    holds: impl Fn(&J, u32, Timestamp) -> io::Result<Membership>,
) -> Binding {
    match job {
        Ok(Some(job)) => {
            // Any answer but a member, a failed query included, goes on to the walk, which asks again.
            let holds_itself = gates
                .own_created
                .is_some_and(|created| matches!(holds(&job, gates.own, created), Ok(Membership::Member)));
            if holds_itself {
                Binding::Inside
            } else {
                walk(&job, tree().borrow(), gates.own, holds)
            }
        }
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => denied(&error, tree().borrow(), gates),
        // No job has the name, or the name opens as something no gate made: whoever holds it is not this gate's parent.
        Ok(None) | Err(_) => Binding::Outside,
    }
}

/// The binding when the parent's job exists but this gate may not open it, for `error`: unjudged when the parent gate,
/// by its pid and its start, is among this gate's ancestors in `tree`, else outside.
fn denied(error: &io::Error, tree: &ProcessTree, gates: &Gates) -> Binding {
    let under_parent = tree
        .ancestors(gates.own)
        .any(|process| process.pid == gates.parent && process.created.map(epoch_ms) == Some(gates.parent_start_ms));
    if under_parent {
        Binding::Unjudged(error.to_string())
    } else {
        Binding::Outside
    }
}

/// Whether `job` holds the gate `own` or, walking up `tree`, the first of its ancestors it holds, each asked with the
/// creation time the snapshot gives it. Outside when the chain ends without a member, at an ancestor whose start the
/// snapshot lacks, and at a process that cannot be opened, for any reason, or is not the one the snapshot saw. Unjudged
/// when the snapshot does not list this gate (it was not taken) and when a membership query fails on a process that
/// opened.
fn walk<J>(job: &J, tree: &ProcessTree, own: u32, holds: impl Fn(&J, u32, Timestamp) -> io::Result<Membership>) -> Binding {
    let Some(gate) = tree.get(own) else {
        return Binding::Unjudged("the process snapshot did not list this gate".to_owned());
    };
    for process in std::iter::once(gate).chain(tree.ancestors(own)) {
        let Some(created) = process.created else {
            return Binding::Outside;
        };
        match holds(job, process.pid, created) {
            Ok(Membership::Member) => return Binding::Inside,
            Ok(Membership::Outside) => {}
            Ok(Membership::NotOpened | Membership::Reused) => return Binding::Outside,
            Err(error) => return Binding::Unjudged(error.to_string()),
        }
    }
    Binding::Outside
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test timestamp that does not parse must fail loudly")]
mod tests {
    use std::cell::RefCell;
    use std::io;

    use gate_win32::Membership;
    use jiff::Timestamp;
    use proc_table::{ProcessInfo, ProcessTree};

    use super::{Binding, Gates, runs_inside};

    /// This gate, its parent and its grandparent, each started after the one above it.
    const OWN: u32 = 30;
    const PARENT: u32 = 20;
    const GRANDPARENT: u32 = 10;
    /// When the grandparent and this gate started, as the snapshot and this gate's own handle give it.
    const GRANDPARENT_CREATED: &str = "2026-10-01T10:00:00Z";
    const OWN_CREATED: &str = "2026-10-01T10:00:02Z";
    /// `ERROR_ACCESS_DENIED` and `ERROR_INVALID_HANDLE`, as raw OS errors.
    const ACCESS_DENIED: i32 = 5;
    const INVALID_HANDLE: i32 = 6;

    fn time(text: &str) -> Timestamp {
        text.parse().expect("a test timestamp parses")
    }

    fn process(pid: u32, parent_pid: u32, created: Option<&str>) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            name: format!("p{pid}.exe"),
            command_line: None,
            created: created.map(time),
        }
    }

    /// The chain gate → parent → grandparent, the parent's start as `parent_created` gives it.
    fn chain(parent_created: Option<&str>) -> ProcessTree {
        ProcessTree::new(vec![
            process(GRANDPARENT, 1, Some(GRANDPARENT_CREATED)),
            process(PARENT, GRANDPARENT, parent_created),
            process(OWN, PARENT, Some(OWN_CREATED)),
        ])
    }

    fn known() -> ProcessTree {
        chain(Some("2026-10-01T10:00:01Z"))
    }

    /// [`OWN`], its own start unread, under the marker of the gate `parent` started at `parent_created`.
    fn gates_under(parent: u32, parent_created: &str) -> Gates {
        Gates {
            own: OWN,
            own_created: None,
            parent,
            parent_start_ms: time(parent_created).as_millisecond().unsigned_abs(),
        }
    }

    /// [`OWN`], its own start unread, under the grandparent's marker.
    fn gates() -> Gates {
        gates_under(GRANDPARENT, GRANDPARENT_CREATED)
    }

    /// The binding of `gates` to a job that exists, each process answered by `answer`, and the pids asked in order.
    fn walked_as(tree: ProcessTree, gates: &Gates, answer: impl Fn(u32) -> io::Result<Membership>) -> (Binding, Vec<u32>) {
        let asked = RefCell::new(Vec::new());
        let binding = runs_inside(
            Ok(Some(())),
            || tree,
            gates,
            |(): &(), pid, _: Timestamp| {
                asked.borrow_mut().push(pid);
                answer(pid)
            },
        );
        (binding, asked.into_inner())
    }

    /// The binding of [`OWN`] in `tree`, its own start unread, to a job that exists, each process answered by `answer`,
    /// and the pids asked in order.
    fn walked(tree: ProcessTree, answer: impl Fn(u32) -> io::Result<Membership>) -> (Binding, Vec<u32>) {
        walked_as(tree, &gates(), answer)
    }

    /// The binding of `gates` when the job cannot be opened with the raw OS error `code`.
    fn opened_with(code: i32, gates: &Gates) -> Binding {
        let failed: io::Result<Option<()>> = Err(io::Error::from_raw_os_error(code));
        runs_inside(failed, known, gates, |(): &(), _, _| Ok(Membership::Member))
    }

    #[test]
    fn a_gate_the_job_holds_itself_is_bound_at_depth_0() {
        let (bound, asked) = walked(known(), |pid| Ok(if pid == OWN { Membership::Member } else { Membership::Outside }));
        assert_eq!((bound, asked), (Binding::Inside, vec![OWN]));
    }

    #[test]
    fn a_gate_the_job_holds_by_its_own_start_is_bound_without_a_snapshot() {
        let snapshots = RefCell::new(0);
        let tree = || {
            *snapshots.borrow_mut() += 1;
            known()
        };
        let asked = RefCell::new(Vec::new());
        let own = Gates {
            own_created: Some(time(OWN_CREATED)),
            ..gates()
        };
        let binding = runs_inside(Ok(Some(())), tree, &own, |(): &(), pid, created| {
            asked.borrow_mut().push((pid, created));
            Ok(if pid == OWN { Membership::Member } else { Membership::Outside })
        });
        assert_eq!(binding, Binding::Inside);
        assert_eq!(
            asked.into_inner(),
            [(OWN, time(OWN_CREATED))],
            "the gate is asked about with its own start"
        );
        assert_eq!(snapshots.into_inner(), 0, "no snapshot is taken for a gate the job holds itself");
    }

    #[test]
    fn a_gate_whose_grandparent_the_job_holds_is_bound_at_depth_2() {
        let (bound, asked) = walked(known(), |pid| {
            Ok(if pid == GRANDPARENT { Membership::Member } else { Membership::Outside })
        });
        assert_eq!((bound, asked), (Binding::Inside, vec![OWN, PARENT, GRANDPARENT]));
    }

    #[test]
    fn a_chain_that_ends_without_a_member_is_unbound() {
        let (bound, asked) = walked(known(), |_| Ok(Membership::Outside));
        assert_eq!((bound, asked), (Binding::Outside, vec![OWN, PARENT, GRANDPARENT]));
    }

    #[test]
    fn an_ancestor_that_cannot_be_opened_unbinds_the_gate() {
        let (bound, asked) = walked(known(), |pid| {
            Ok(match pid {
                PARENT => Membership::NotOpened,
                GRANDPARENT => Membership::Member,
                _ => Membership::Outside,
            })
        });
        assert_eq!(
            (bound, asked),
            (Binding::Outside, vec![OWN, PARENT]),
            "the walk stops at the process it cannot open"
        );
    }

    #[test]
    fn an_ancestor_whose_creation_time_does_not_match_unbinds_the_gate() {
        let (bound, asked) = walked(known(), |pid| {
            Ok(match pid {
                PARENT => Membership::Reused,
                GRANDPARENT => Membership::Member,
                _ => Membership::Outside,
            })
        });
        assert_eq!((bound, asked), (Binding::Outside, vec![OWN, PARENT]), "a reused pid");
        let (bound, asked) = walked(chain(None), |pid| {
            Ok(if pid == GRANDPARENT { Membership::Member } else { Membership::Outside })
        });
        assert_eq!(
            (bound, asked),
            (Binding::Outside, vec![OWN]),
            "an ancestor whose start the snapshot lacks is not asked"
        );
    }

    #[test]
    fn a_membership_query_that_fails_cannot_be_judged() {
        let failing = |pid| {
            if pid == PARENT {
                Err(io::Error::other("the query failed"))
            } else {
                Ok(Membership::Outside)
            }
        };
        assert_eq!(
            walked(known(), failing),
            (Binding::Unjudged("the query failed".to_owned()), vec![OWN, PARENT])
        );
    }

    #[test]
    fn a_snapshot_that_does_not_list_this_gate_cannot_be_judged() {
        let (binding, asked) = walked(ProcessTree::new(Vec::new()), |_| Ok(Membership::Member));
        assert_eq!(
            (binding, asked),
            (Binding::Unjudged("the process snapshot did not list this gate".to_owned()), Vec::new())
        );
    }

    #[test]
    fn a_job_name_another_kind_of_object_holds_unbinds_the_gate() {
        assert_eq!(opened_with(INVALID_HANDLE, &gates()), Binding::Outside);
    }

    #[test]
    fn a_job_denied_to_a_gate_under_its_parent_cannot_be_judged() {
        assert_eq!(
            opened_with(ACCESS_DENIED, &gates()),
            Binding::Unjudged(io::Error::from_raw_os_error(ACCESS_DENIED).to_string())
        );
    }

    #[test]
    fn a_job_denied_to_a_gate_not_under_its_parent_unbinds_the_gate() {
        assert_eq!(
            opened_with(ACCESS_DENIED, &gates_under(99, GRANDPARENT_CREATED)),
            Binding::Outside,
            "a parent that is no ancestor"
        );
        assert_eq!(
            opened_with(ACCESS_DENIED, &gates_under(GRANDPARENT, OWN_CREATED)),
            Binding::Outside,
            "an ancestor's pid with another start"
        );
    }

    #[test]
    fn a_job_that_is_not_found_unbinds_the_gate_without_a_snapshot() {
        let snapshots = RefCell::new(0);
        let tree = || {
            *snapshots.borrow_mut() += 1;
            known()
        };
        assert_eq!(
            runs_inside(Ok(None::<()>), tree, &gates(), |(): &(), _, _| Ok(Membership::Member)),
            Binding::Outside
        );
        assert_eq!(snapshots.into_inner(), 0, "no snapshot is taken for a job that does not exist");
    }
}
