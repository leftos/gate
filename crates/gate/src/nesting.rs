//! Nesting under a parent gate's slot: the marker `GATE_SLOT_HELD=<kind>:<pid>:<start>[:<h>]` a gate hands its command,
//! read once a run, and what a gate started under it does: runs under the parent's slot when that slot covers its kind,
//! is refused when it is critical and the slot does not cover it, and otherwise takes a slot of its own, as it does
//! under a parent that holds no slot. A marker is trusted only when this gate runs inside the command of the gate it
//! names; otherwise the gate says so and runs as a gate under no parent. A binding that cannot be judged is trusted,
//! and the gate says that too. A gate inside the job a box run names for its command (`GATE_BOX_JOB`) takes no slot at
//! all; a box job that does not hold it, or cannot be judged to, is said and ignored.

mod binding;

use std::cell::OnceCell;
use std::ffi::OsStr;

use gate_args::{Slot, Spelling};
use gate_state::StateRoot;
use gate_state::v1::{self, CoveredMarker};

use crate::console::Console;
use crate::nesting::binding::{Binding, holds_this_gate, runs_inside_command_of, runs_inside_command_with};
use crate::resolve::{Environment, ResolvedRoot};
use crate::slots::{Liveness, gate_liveness};

/// The most digits a marker's pid may have.
const PID_DIGITS: usize = 9;
/// The most digits a marker's start may have.
const START_DIGITS: usize = 18;
/// The most characters of a malformed marker its line shows.
const MARKER_SHOWN: usize = 64;

/// A live parent gate named by the marker: the slot kind it holds, its pid, the start it runs since in Unix epoch
/// milliseconds, its marker as written, why this gate's binding to it could not be judged, when it could not, whether
/// it stands in for the ended gate the marker names ([`Parent::standing_in`]), and whether it is the commit run whose
/// hooks this gate runs in ([`Nesting::marked_for_commit`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Parent {
    pub held: Slot,
    pub pid: u32,
    pub start_ms: u64,
    pub marker: String,
    pub unjudged: Option<String>,
    pub stand_in: bool,
    pub commit: bool,
}

/// What a gate does under the marker it was started with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Nesting {
    /// No marker, or one of another state root: the gate takes its own slot and says nothing of it.
    Own,
    /// A marker, as written, that is not `<kind>:<pid>:<start>[:<h>]`: the gate takes its own slot.
    Malformed(String),
    /// A marker naming a gate that no longer runs with its start: the gate takes its own slot.
    Ended { marker: String, pid: u32 },
    /// A marker naming a live process this gate does not run inside the command of: the gate takes its own slot as a
    /// gate under no parent does, and a critical one takes the machine.
    Unbound { marker: String, pid: u32 },
    /// A parent whose slot covers this gate's kind: the gate takes no slot and runs under the parent's.
    Covered(Parent),
    /// A parent whose slot does not cover this critical gate, which would wait for that slot for good: it does not run.
    Refused(Parent),
    /// A parent whose slot does not cover this gate's kind: the gate takes its own slot.
    Under(Parent),
    /// A parent that holds no slot: the gate takes its own slot as a gate under no parent would, and says nothing of it;
    /// a critical gate takes the machine. The parent lends it nothing.
    Beside(Parent),
    /// A gate inside the job of a box run on a builder (`GATE_BOX_JOB`), whatever slot it asks for: the routed run
    /// holds the builder for it, so it takes no slot, never waits and never routes, and hands its command no slot marker.
    /// Its command runs as the kind it asked for, but a critical one runs as heavy ([`Nesting::requested`]).
    InBox,
}

/// The line a gate inside a box run's job says of it.
const IN_BOX_LINE: &str = "gate: inside a routed run; running without a slot on this builder";
/// The line a gate says when the box job its environment names does not hold it, or is no box run's job name.
const NOT_HELD_LINE: &str = "gate: ignored GATE_BOX_JOB: this gate does not run inside its job";
/// The hex digits of the nonce a box run's job name ends with, as the client draws it.
const BOX_NONCE_DIGITS: usize = 12;

/// What a gate makes of the box job its environment names (`GATE_BOX_JOB`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BoxJob {
    /// No name, or an empty one: the gate runs as any other.
    Unset,
    /// A job that holds this gate: the gate runs inside the box run ([`Nesting::InBox`]).
    Inside,
    /// A job that does not, or a name that is no box run's job: the gate says so and runs as if the name were unset.
    NotHeld,
    /// A job that cannot be opened or listed, for this reason: the gate says so and runs as if the name were unset.
    Unjudged(String),
}

impl BoxJob {
    /// The line a gate says of this box job; only an ignored one has one.
    pub(crate) fn line(&self) -> Option<String> {
        match self {
            BoxJob::NotHeld => Some(NOT_HELD_LINE.to_owned()),
            BoxJob::Unjudged(reason) => Some(format!("gate: ignored GATE_BOX_JOB: cannot judge its job: {reason}")),
            BoxJob::Unset | BoxJob::Inside => None,
        }
    }
}

/// What a gate makes of the box job `name` its environment names, `held` judging whether the job of a name holds this
/// gate ([`binding::job_holds_this_gate`]). Only a box run's job name ([`box_job_shaped`]) is judged, and only a job
/// judged to hold the gate is trusted: a name of another shape, or one that is not Unicode, is not held, and a job that
/// cannot be judged is ignored too, so an unjudged box job never skips slots.
pub(crate) fn box_job(name: Option<&OsStr>, held: impl FnOnce(&str) -> Binding) -> BoxJob {
    let Some(name) = name.filter(|name| !name.is_empty()) else {
        return BoxJob::Unset;
    };
    let Some(name) = name.to_str().filter(|name| box_job_shaped(name)) else {
        return BoxJob::NotHeld;
    };
    match held(name) {
        Binding::Inside => BoxJob::Inside,
        Binding::Outside => BoxJob::NotHeld,
        Binding::Unjudged(reason) => BoxJob::Unjudged(reason),
    }
}

/// Whether `name` has the shape of a box run's job name: `Local\` or `Global\`, then `gate-remote-<key>-<nonce>` with a
/// key that is not empty and a nonce of [`BOX_NONCE_DIGITS`] hex digits, as the box checks a payload's `Job`.
fn box_job_shaped(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(r"Local\").or_else(|| name.strip_prefix(r"Global\")) else {
        return false;
    };
    let Some((key, nonce)) = rest.strip_prefix("gate-remote-").and_then(|rest| rest.rsplit_once('-')) else {
        return false;
    };
    !key.is_empty() && nonce.len() == BOX_NONCE_DIGITS && nonce.chars().all(|digit| digit.is_ascii_hexdigit())
}

/// A marker as read: the kind of slot its gate holds, the gate's pid and start, and the root hash after them.
struct Marker<'m> {
    held: Slot,
    pid: u32,
    start: u64,
    hash: Option<&'m str>,
}

impl Nesting {
    /// The nesting of a gate of `kind` under the state root `root`, from the marker in `environment`, the marker's gate
    /// judged by the start the system gives its pid and bound to this gate by its job; `job_trusted`, the
    /// `--test-parent-job-trusted` seam, counts every live marker as bound. A marker whose gate has ended, or whose live
    /// gate's command no longer holds this gate (its job closed as it ends), is taken up by a live heavy or light covered
    /// gate whose command this gate runs inside ([`enclosing`]), as that gate's late take stands in for the ended one:
    /// this gate is judged under that gate's slot as under a live parent's ([`Nesting::under`]), so a critical gate
    /// there is refused.
    ///
    /// A box job the gate's environment names (`GATE_BOX_JOB`) is judged first, by the job itself, never by the
    /// seam ([`Nesting::read_with`]).
    pub(crate) fn read(environment: &Environment, kind: Slot, root: &ResolvedRoot, job_trusted: bool, console: &mut Console) -> Nesting {
        Nesting::read_with(environment, kind, root, job_trusted, binding::job_holds_this_gate, console)
    }

    /// [`Nesting::read`], `held` judging whether the job of a name holds this gate: a box job the environment names that
    /// holds it ([`box_job`]) makes it [`Nesting::InBox`], whatever its marker; one ignored is said, and the marker is
    /// read as if no box job were named.
    fn read_with(
        environment: &Environment,
        kind: Slot,
        root: &ResolvedRoot,
        job_trusted: bool,
        held: impl FnOnce(&str) -> Binding,
        console: &mut Console,
    ) -> Nesting {
        let in_box = box_job(environment.box_job.as_deref(), held);
        if in_box == BoxJob::Inside {
            return Nesting::InBox;
        }
        if let Some(line) = in_box.line() {
            console.err(&line);
        }
        let inside = |pid, start_ms| bound(&root.root, pid, start_ms, job_trusted);
        let nesting = Nesting::decide(environment.slot_held.as_deref(), kind, root.root.hash(), gate_liveness, inside)
            .marked_for_commit(environment.commit_slot.as_deref());
        let (Nesting::Ended { marker, .. } | Nesting::Unbound { marker, .. }) = &nesting else {
            return nesting;
        };
        match enclosing_of(root, kind, &[], job_trusted, console) {
            Some(found) => Nesting::under(Parent::standing_in(marker.clone(), found), kind),
            None => nesting,
        }
    }

    /// The nesting of a gate of `kind` under the root hashed `root_hash` (`None` for the default root) for `marker`,
    /// unset or empty when there is none; `liveness_of` judges the gate of a pid and its start in Unix epoch
    /// milliseconds, and `inside` whether this gate runs inside the command of the live gate of a pid and its start.
    /// Only a gate judged gone ends the marker: one whose start cannot be read counts as running, so a child never
    /// queues behind the slot its own parent holds. A live gate this one does not run inside unbinds the marker; one
    /// whose binding cannot be judged keeps it, the reason kept with the parent.
    pub(crate) fn decide(
        marker: Option<&OsStr>,
        kind: Slot,
        root_hash: Option<&str>,
        liveness_of: impl Fn(u32, u64) -> Liveness,
        inside: impl Fn(u32, u64) -> Binding,
    ) -> Nesting {
        let Some(marker) = marker.filter(|marker| !marker.is_empty()) else {
            return Nesting::Own;
        };
        let text = marker.to_string_lossy();
        let Some(Marker { held, pid, start, hash }) = parse(&text) else {
            return Nesting::Malformed(text.into_owned());
        };
        if hash != root_hash {
            return Nesting::Own;
        }
        if matches!(liveness_of(pid, start), Liveness::Gone) {
            return Nesting::Ended {
                marker: text.into_owned(),
                pid,
            };
        }
        let unjudged = match inside(pid, start) {
            Binding::Inside => None,
            Binding::Unjudged(reason) => Some(reason),
            Binding::Outside => {
                return Nesting::Unbound {
                    marker: text.into_owned(),
                    pid,
                };
            }
        };
        Nesting::under(
            Parent {
                held,
                pid,
                start_ms: start,
                marker: text.into_owned(),
                unjudged,
                stand_in: false,
                commit: false,
            },
            kind,
        )
    }

    /// This nesting with its live parent marked as the commit run whose hooks this gate runs in, when its marker is
    /// `commit_marker`, the one the commit run hands its command (`GATE_COMMIT_SLOT`); it covers the gate as any parent
    /// holding its slot does. Any other nesting, and a parent with another marker, as it is.
    fn marked_for_commit(self, commit_marker: Option<&OsStr>) -> Nesting {
        let commit_s = |parent: &Parent| commit_marker == Some(OsStr::new(&parent.marker));
        let marked = |parent: Parent| Parent { commit: true, ..parent };
        match self {
            Nesting::Covered(parent) if commit_s(&parent) => Nesting::Covered(marked(parent)),
            Nesting::Refused(parent) if commit_s(&parent) => Nesting::Refused(marked(parent)),
            Nesting::Under(parent) if commit_s(&parent) => Nesting::Under(marked(parent)),
            other => other,
        }
    }

    /// The nesting of a gate of `kind` under the live parent `parent`, whose command it runs inside.
    fn under(parent: Parent, kind: Slot) -> Nesting {
        if parent.held == Slot::None {
            Nesting::Beside(parent)
        } else if covers(parent.held, kind) {
            Nesting::Covered(parent)
        } else if kind == Slot::Critical {
            Nesting::Refused(parent)
        } else {
            Nesting::Under(parent)
        }
    }

    /// This nesting for the gate once it asks for a heavy slot in place of the kind it was read for: a live parent's
    /// slot judged again for a heavy gate ([`Nesting::under`]), so a light parent no longer covers it; any other
    /// nesting as it is. A parent that stands in for an ended one is looked for again first, `search` finding the gate
    /// that stands in for a heavy gate ([`enclosing_of`]): a heavy covered gate this gate runs inside covers it where
    /// the light one found for its first kind does not. The parent found first stays when the search finds none.
    pub(crate) fn as_heavy(&self, search: impl FnOnce() -> Option<Enclosing>) -> Nesting {
        match self {
            Nesting::Covered(parent) | Nesting::Refused(parent) | Nesting::Under(parent) | Nesting::Beside(parent) => {
                let found = parent.stand_in.then(search).flatten();
                let parent = found.map_or_else(|| parent.clone(), |found| Parent::standing_in(parent.marker.clone(), found));
                Nesting::under(parent, Slot::Heavy)
            }
            Nesting::Own | Nesting::Malformed(_) | Nesting::Ended { .. } | Nesting::Unbound { .. } | Nesting::InBox => self.clone(),
        }
    }

    /// The slot kind a gate asked for `kind` runs as under this nesting: heavy for a critical gate inside a box run,
    /// which never raises itself to High priority on a builder, else `kind`.
    pub(crate) fn requested(&self, kind: Slot) -> Slot {
        match (self, kind) {
            (Nesting::InBox, Slot::Critical) => Slot::Heavy,
            _ => kind,
        }
    }

    /// The slot kind a gate running as `kind` takes under this nesting: none inside a box run, else `kind`.
    pub(crate) fn takes(&self, kind: Slot) -> Slot {
        match self {
            Nesting::InBox => Slot::None,
            _ => kind,
        }
    }

    /// The parent whose slot this gate runs under, when it covers the gate.
    pub(crate) fn covering(&self) -> Option<&Parent> {
        match self {
            Nesting::Covered(parent) => Some(parent),
            _ => None,
        }
    }

    /// The parent whose slot covers this gate when it is the commit run whose hooks this gate runs in
    /// ([`Nesting::marked_for_commit`]). A gate under it runs here under its slot, as any covered gate does, but routes as
    /// a gate under no parent would, so a build in a commit's hooks still routes. None for every other nesting, a parent
    /// standing in for an ended one included.
    pub(crate) fn under_commit(&self) -> Option<&Parent> {
        match self {
            Nesting::Covered(parent) if parent.commit => Some(parent),
            _ => None,
        }
    }

    /// Whether this gate runs inside the hooks of a live commit run, whether its slot covers the gate, refuses it or
    /// does not cover it ([`Nesting::marked_for_commit`]).
    pub(crate) fn in_commit(&self) -> bool {
        matches!(self, Nesting::Covered(parent) | Nesting::Refused(parent) | Nesting::Under(parent) if parent.commit)
    }

    /// The parent this gate takes a slot of its own under: one whose slot does not cover it, or one that holds no slot.
    /// The gate's `take` names it; a take of a slot or of the machine tells the parent not to judge its own slot by the
    /// CPU both used, while a no-slot gate's take, of nothing, leaves the parent's judgement as it is.
    pub(crate) fn uncovering(&self) -> Option<&Parent> {
        match self {
            Nesting::Under(parent) | Nesting::Beside(parent) => Some(parent),
            _ => None,
        }
    }

    /// The slot kind a gate of `kind` runs its command as: critical under a critical parent's slot, which it follows,
    /// else its own.
    pub(crate) fn runs_as(&self, kind: Slot) -> Slot {
        match self.covering() {
            Some(parent) if parent.held == Slot::Critical => Slot::Critical,
            _ => kind,
        }
    }

    /// The line a gate of `kind`, asked for in `spelling`, says of this nesting; none for [`Nesting::Own`] and
    /// [`Nesting::Beside`].
    pub(crate) fn line(&self, kind: Slot, spelling: Spelling) -> Option<String> {
        let going_on = match kind {
            Slot::None => "running on no slot".to_owned(),
            _ => format!("taking a {} slot", kind.as_str()),
        };
        let line = match self {
            Nesting::Own | Nesting::Beside(_) => return None,
            Nesting::Malformed(marker) => format!(
                "gate: ignored GATE_SLOT_HELD='{}', which is not <kind>:<pid>:<start>; {going_on}",
                shown_marker(marker)
            ),
            Nesting::Ended { marker, pid } => {
                format!("gate: ignored GATE_SLOT_HELD='{marker}': gate {pid} is no longer running; {going_on}")
            }
            Nesting::Unbound { marker, pid } => format!(
                "gate: ignored GATE_SLOT_HELD='{}': this gate does not run inside the command of gate {pid}; {going_on}",
                shown_marker(marker)
            ),
            Nesting::Covered(parent) => format!("gate: running under the {} slot of gate {}", parent.held.as_str(), parent.pid),
            Nesting::Refused(parent) => format!(
                "gate: {} critical cannot run under the {} slot of gate {}: it would wait for that slot for good; run the critical \
                 command outside that gate",
                slot_flag(spelling),
                parent.held.as_str(),
                parent.pid
            ),
            Nesting::Under(parent) => parent.uncovered_line(kind),
            Nesting::InBox => IN_BOX_LINE.to_owned(),
        };
        Some(line)
    }

    /// The line a gate says before any other of its nesting when it honours a marker whose binding it could not judge;
    /// none for every other nesting.
    pub(crate) fn unjudged_line(&self) -> Option<String> {
        match self {
            Nesting::Covered(parent) | Nesting::Refused(parent) | Nesting::Under(parent) | Nesting::Beside(parent) => parent.unjudged_line(),
            Nesting::Own | Nesting::Malformed(_) | Nesting::Ended { .. } | Nesting::Unbound { .. } | Nesting::InBox => None,
        }
    }
}

impl Parent {
    /// The parent `found` is once it stands in for the ended parent gate whose marker, `marker`, this gate was given:
    /// the slot of `found`'s own kind, which it holds once it took it, its binding to this gate judged.
    pub(crate) fn standing_in(marker: String, found: Enclosing) -> Parent {
        Parent {
            held: found.held,
            pid: found.pid,
            start_ms: found.start_ms,
            marker,
            unjudged: None,
            stand_in: true,
            commit: false,
        }
    }

    /// The line a gate says when it honours this parent's marker without having judged its binding to it, for the
    /// reason kept; none when the binding was judged.
    fn unjudged_line(&self) -> Option<String> {
        let reason = self.unjudged.as_deref()?;
        Some(format!(
            "gate: could not judge GATE_SLOT_HELD='{}' ({reason}); running under the {} slot of gate {}",
            shown_marker(&self.marker),
            self.held.as_str(),
            self.pid
        ))
    }

    /// The line a gate of `kind` this parent's slot does not cover says as it goes on to take a local slot of its own.
    pub(crate) fn uncovered_line(&self, kind: Slot) -> String {
        let kind = kind.as_str();
        format!(
            "gate: the {} slot of gate {} does not cover a {kind} gate; taking a {kind} slot",
            self.held.as_str(),
            self.pid
        )
    }

    /// The line a gate of `kind` this parent's slot covers says when the parent ends while its command still runs, as
    /// it goes on to take a slot of its own kind for the rest of its run.
    pub(crate) fn ended_line(&self, kind: Slot) -> String {
        format!(
            "gate: the parent gate {} ended; taking a {} slot for the rest of this run",
            self.pid,
            kind.as_str()
        )
    }

    /// The line a covered gate says when its parent gate `ended` has ended while its command runs, and this parent, a
    /// live covered gate whose command this gate runs inside, stands in for it with a slot of its own kind.
    pub(crate) fn taken_up_line(&self, ended: u32) -> String {
        format!(
            "gate: the parent gate {ended} ended; running under the {} slot of gate {}",
            self.held.as_str(),
            self.pid
        )
    }

    /// The line a gate this parent's slot covers says when it cannot watch the parent for its end, for `reason`, and so
    /// runs covered by it to its end.
    pub(crate) fn unwatched_line(&self, reason: &str) -> String {
        format!("gate: cannot watch the parent gate {} ({reason}); this run stays covered by it", self.pid)
    }

    /// The line a covered gate whose parent gate `ended` has ended says when it cannot watch this parent, found to stand
    /// in for that one, for `reason`, and so passes it over.
    pub(crate) fn unwatched_stand_in_line(&self, ended: u32, reason: &str) -> String {
        format!(
            "gate: cannot watch gate {} ({reason}) to run under its slot in place of the parent gate {ended}",
            self.pid
        )
    }
}

/// Whether this gate runs inside the command of the live gate `pid` started at `start_ms` under `root`
/// ([`runs_inside_command_of`]); every gate is, under the `--test-parent-job-trusted` seam `job_trusted`.
fn bound(root: &StateRoot, pid: u32, start_ms: u64, job_trusted: bool) -> Binding {
    if job_trusted {
        Binding::Inside
    } else {
        runs_inside_command_of(root, pid, start_ms)
    }
}

/// A live covered gate whose command this gate runs inside, which stands in for this gate's ended parent gate: its pid,
/// its start in Unix epoch milliseconds, and the slot kind it holds once its own parent has ended, its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Enclosing {
    pub pid: u32,
    pub start_ms: u64,
    pub held: Slot,
}

/// The gate that stands in for the ended parent gate of this gate of `kind` ([`enclosing`]), read from the covered
/// markers under `root`, none of the gates `skip` names by pid and start (this gate's own, when it keeps a marker
/// itself); a covered folder that cannot be read is said, and none stands in. Each candidate's job is asked first
/// whether it holds this gate's own process; the walk up this gate's ancestors that follows when none does reads one
/// process snapshot, taken once for every candidate.
pub(crate) fn enclosing_of(root: &ResolvedRoot, kind: Slot, skip: &[(u32, u64)], job_trusted: bool, console: &mut Console) -> Option<Enclosing> {
    let dir = v1::covered_dir(&root.path);
    let markers: Vec<CoveredMarker> = match v1::read_covered(&dir) {
        Ok(read) => read.into_iter().map(|(_, marker)| marker).collect(),
        Err(error) => {
            console.err(&format!("gate: cannot read the covered markers in {}: {error}", dir.display()));
            return None;
        }
    };
    let snapshot = OnceCell::new();
    let tree = || snapshot.get_or_init(binding::snapshot);
    let member = |pid, _| job_trusted || holds_this_gate(&root.root, pid);
    let inside = |pid, start_ms| {
        if job_trusted {
            Binding::Inside
        } else {
            runs_inside_command_with(&root.root, pid, start_ms, tree)
        }
    };
    enclosing(candidates(&markers, skip), kind, gate_liveness, member, inside)
}

/// The covered gates of `markers` that may stand in for an ended parent, but those `skip` names by pid and start:
/// those whose own kind is heavy or light ([`own_kind`]), the outermost of the nested gates first, by their start.
pub(crate) fn candidates(markers: &[CoveredMarker], skip: &[(u32, u64)]) -> Vec<Enclosing> {
    let mut candidates: Vec<Enclosing> = markers
        .iter()
        .filter(|marker| !skip.contains(&(marker.pid, marker.process_start)))
        .filter_map(|marker| {
            own_kind(marker.kind.as_deref()).map(|held| Enclosing {
                pid: marker.pid,
                start_ms: marker.process_start,
                held,
            })
        })
        .collect();
    candidates.sort_by_key(|candidate| (candidate.start_ms, candidate.pid));
    candidates
}

/// Of the `candidates`, outermost first, the gate that stands in for the ended parent gate of this gate of `kind`: one
/// that `liveness_of` does not judge gone and whose command holds this gate, first of those whose own kind covers
/// this gate's, else of the others, judged then as a live parent's slot that does not cover the gate is ([`Nesting::under`]).
/// Within each, a candidate whose job `member` says holds this gate's own process is taken first, as every ancestor's
/// job does, since jobs nest; then one whose command `inside` binds this gate to. A binding that cannot be judged binds
/// nothing here: a gate that queues is the safe failure once the parent has ended. None when no such gate runs.
pub(crate) fn enclosing(
    candidates: Vec<Enclosing>,
    kind: Slot,
    liveness_of: impl Fn(u32, u64) -> Liveness,
    member: impl Fn(u32, u64) -> bool,
    inside: impl Fn(u32, u64) -> Binding,
) -> Option<Enclosing> {
    let (covering, others): (Vec<Enclosing>, Vec<Enclosing>) = candidates
        .into_iter()
        .filter(|candidate| !matches!(liveness_of(candidate.pid, candidate.start_ms), Liveness::Gone))
        .partition(|candidate| covers(candidate.held, kind));
    [covering, others].into_iter().find_map(|group| {
        let held = group.iter().find(|candidate| member(candidate.pid, candidate.start_ms));
        held.or_else(|| {
            group
                .iter()
                .find(|candidate| inside(candidate.pid, candidate.start_ms) == Binding::Inside)
        })
        .copied()
    })
}

/// The slot a covered gate whose marker names its own kind `marker_kind` holds once its parent has ended: a heavy or
/// light one. A no-slot or critical gate takes none, so it stands in for no gate; nor does a gate that gave up watching
/// its parent, whose marker names `none`, or one whose marker names no kind, which a gate wrote before markers named
/// one and which never takes a slot of its own.
fn own_kind(marker_kind: Option<&str>) -> Option<Slot> {
    match marker_kind {
        Some("heavy") => Some(Slot::Heavy),
        Some("light") => Some(Slot::Light),
        _ => None,
    }
}

/// A marker as a line shows it: every character but an ASCII letter, a digit or `:` as `?`, so it cannot
/// forge a line or a failure marker in the log, and at most [`MARKER_SHOWN`] characters, `...` after a longer one.
fn shown_marker(marker: &str) -> String {
    let mut shown: String = marker
        .chars()
        .take(MARKER_SHOWN)
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == ':' {
                character
            } else {
                '?'
            }
        })
        .collect();
    if marker.chars().count() > MARKER_SHOWN {
        shown.push_str("...");
    }
    shown
}

/// Whether a parent holding a `held` slot covers a gate of `kind`: heavy covers heavy, light and no slot, light covers
/// light and no slot, critical covers every kind, and a parent holding no slot covers none.
pub(crate) fn covers(held: Slot, kind: Slot) -> bool {
    match held {
        Slot::Heavy => kind != Slot::Critical,
        Slot::Light => matches!(kind, Slot::Light | Slot::None),
        Slot::Critical => true,
        Slot::None => false,
    }
}

/// The slot flag as the caller spelled the command line.
fn slot_flag(spelling: Spelling) -> &'static str {
    match spelling {
        Spelling::Cli => "--slot",
        Spelling::Compat => "-Slot",
    }
}

/// `text` read as `<kind>:<pid>:<start>[:<h>]`: a lowercase kind, a pid of 1 to 9 digits, a start of 1 to 18 digits,
/// and an optional root hash that is not empty; `None` when it is not.
fn parse(text: &str) -> Option<Marker<'_>> {
    let mut parts = text.split(':');
    let held = match parts.next()? {
        "heavy" => Slot::Heavy,
        "light" => Slot::Light,
        "critical" => Slot::Critical,
        "none" => Slot::None,
        _ => return None,
    };
    let pid = digits(parts.next()?, PID_DIGITS)?;
    let start = digits(parts.next()?, START_DIGITS)?;
    let hash = parts.next();
    if hash.is_some_and(str::is_empty) || parts.next().is_some() {
        return None;
    }
    Some(Marker {
        held,
        pid: u32::try_from(pid).ok()?,
        start,
        hash,
    })
}

/// `text` as a whole number of 1 to `most` ASCII digits, no sign.
fn digits(text: &str, most: usize) -> Option<u64> {
    let fits = (1..=most).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit());
    fits.then(|| text.parse().ok()).flatten()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a decision that has no line must fail the test loudly")]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::path::PathBuf;

    use gate_args::{Slot, Spelling};
    use gate_state::StateRoot;
    use gate_state::v1::CoveredMarker;

    use super::binding::Binding;
    use super::{BoxJob, Enclosing, Nesting, Parent, box_job, candidates, enclosing};
    use crate::console::Console;
    use crate::resolve::{Environment, ResolvedRoot};
    use crate::slots::Liveness;

    /// The hash of the state root every decision here is made under.
    const HASH: &str = "0a1b2c3d";
    /// The one gate that runs, and since when, in Unix epoch milliseconds.
    const PID: u32 = 4242;
    const START: u64 = 1_759_439_760_571;
    /// A pid whose start cannot be read.
    const UNREADABLE: u32 = 5151;

    /// How the gate `pid` started at `start` is judged: live only as [`PID`] since [`START`], unknown as
    /// [`UNREADABLE`], else gone.
    fn judged(pid: u32, start: u64) -> Liveness {
        match pid {
            PID if start == START => Liveness::Live,
            UNREADABLE => Liveness::Unknown(io::Error::other("the system failed")),
            _ => Liveness::Gone,
        }
    }

    /// Every live gate this one is asked about runs it inside its command.
    fn bound(_: u32, _: u64) -> Binding {
        Binding::Inside
    }

    /// The decision of a gate of `kind` under the root hashed [`HASH`] for `marker`, run inside its parent's command.
    fn decided(marker: &str, kind: Slot) -> Nesting {
        Nesting::decide(Some(OsStr::new(marker)), kind, Some(HASH), judged, bound)
    }

    #[test]
    fn a_parent_whose_start_cannot_be_read_counts_as_running() {
        let marker = |held: &str| format!("{held}:{UNREADABLE}:{START}:{HASH}");
        let parent = |held: Slot| Parent {
            held,
            pid: UNREADABLE,
            start_ms: START,
            marker: marker(held.as_str()),
            unjudged: None,
            stand_in: false,
            commit: false,
        };
        assert_eq!(decided(&marker("heavy"), Slot::Light), Nesting::Covered(parent(Slot::Heavy)));
        assert_eq!(decided(&marker("heavy"), Slot::Critical), Nesting::Refused(parent(Slot::Heavy)));
        assert_eq!(decided(&marker("light"), Slot::Heavy), Nesting::Under(parent(Slot::Light)));
    }

    #[test]
    fn a_malformed_marker_is_shown_with_only_letters_digits_and_colons_and_at_most_64_characters() {
        let shown = |marker: &str| {
            Nesting::Malformed(marker.to_owned())
                .line(Slot::Light, Spelling::Cli)
                .expect("a malformed marker has a line")
        };
        let line = |echo: &str| format!("gate: ignored GATE_SLOT_HELD='{echo}', which is not <kind>:<pid>:<start>; taking a light slot");
        assert_eq!(shown("x\ngate: forged"), line("x?gate:?forged"), "a newline");
        assert_eq!(shown("x\u{1b}[31my"), line("x??31my"), "an escape character");
        assert_eq!(shown("x: error y"), line("x:?error?y"), "a failure marker");
        assert_eq!(shown(&"a".repeat(64)), line(&"a".repeat(64)), "64 characters are shown whole");
        assert_eq!(shown(&"é".repeat(65)), line(&format!("{}...", "?".repeat(64))), "a longer one is cut");
    }

    /// The marker of the running gate [`PID`] holding a `held` slot under the root hashed [`HASH`].
    fn marker(held: Slot) -> String {
        format!("{}:{PID}:{START}:{HASH}", held.as_str())
    }

    fn parent(held: Slot) -> Parent {
        Parent {
            held,
            pid: PID,
            start_ms: START,
            marker: marker(held),
            unjudged: None,
            stand_in: false,
            commit: false,
        }
    }

    #[test]
    fn an_unset_or_empty_marker_is_no_nesting() {
        assert_eq!(Nesting::decide(None, Slot::Heavy, Some(HASH), judged, bound), Nesting::Own);
        assert_eq!(
            Nesting::decide(Some(OsStr::new("")), Slot::Heavy, Some(HASH), judged, bound),
            Nesting::Own
        );
    }

    #[test]
    fn a_marker_reads_each_kind_and_caps_its_digits() {
        for held in [Slot::Heavy, Slot::Light, Slot::Critical] {
            assert_eq!(decided(&marker(held), Slot::Light).covering(), Some(&parent(held)), "{held:?}");
        }
        let longest = format!("heavy:123456789:123456789012345678:{HASH}");
        assert_eq!(
            decided(&longest, Slot::Heavy),
            Nesting::Ended {
                marker: longest.clone(),
                pid: 123_456_789
            },
            "a 9-digit pid and an 18-digit start are read"
        );
        let malformed = [
            format!("medium:{PID}:{START}:{HASH}"),
            format!("Heavy:{PID}:{START}:{HASH}"),
            format!("heavy:{PID}"),
            format!("heavy:{PID}:{START}:"),
            format!("heavy:{PID}:{START}:{HASH}:extra"),
            format!("heavy:1234567890:{START}:{HASH}"),
            format!("heavy:{PID}:1234567890123456789:{HASH}"),
            format!("heavy::{START}:{HASH}"),
            format!("heavy:+{PID}:{START}:{HASH}"),
            format!("heavy:{PID}:-{START}:{HASH}"),
            format!(" heavy:{PID}:{START}:{HASH}"),
        ];
        for text in malformed {
            assert_eq!(decided(&text, Slot::Heavy), Nesting::Malformed(text.clone()), "{text}");
        }
    }

    #[test]
    fn a_no_slot_marker_is_a_parent_beside_which_every_kind_takes_its_own_slot_silently() {
        for kind in [Slot::Heavy, Slot::Light, Slot::Critical, Slot::None] {
            let nesting = decided(&marker(Slot::None), kind);
            assert_eq!(nesting, Nesting::Beside(parent(Slot::None)), "a {kind:?} gate");
            assert_eq!(
                (nesting.line(kind, Spelling::Cli), nesting.covering(), nesting.runs_as(kind)),
                (None, None, kind),
                "a {kind:?} gate"
            );
        }
    }

    #[test]
    fn a_no_slot_gate_is_covered_by_a_heavy_light_or_critical_parent() {
        for held in [Slot::Heavy, Slot::Light, Slot::Critical] {
            assert_eq!(decided(&marker(held), Slot::None), Nesting::Covered(parent(held)), "{held:?}");
        }
    }

    #[test]
    fn a_marker_of_another_state_root_is_ignored_silently() {
        let other = format!("heavy:{PID}:{START}:ffffffff");
        let hashless = format!("heavy:{PID}:{START}");
        assert_eq!(decided(&other, Slot::Heavy), Nesting::Own, "another root's hash");
        assert_eq!(decided(&hashless, Slot::Heavy), Nesting::Own, "no hash under a root that has one");
        let under_default = |text: &str| Nesting::decide(Some(OsStr::new(text)), Slot::Heavy, None, judged, bound);
        assert_eq!(under_default(&marker(Slot::Heavy)), Nesting::Own, "a hash under the default root");
        assert_eq!(
            under_default(&hashless).covering(),
            Some(&Parent {
                held: Slot::Heavy,
                pid: PID,
                start_ms: START,
                marker: hashless.clone(),
                unjudged: None,
                stand_in: false,
                commit: false,
            }),
            "no hash under the default root"
        );
    }

    #[test]
    fn a_marker_of_a_gate_no_longer_running_is_ended() {
        let restarted = format!("heavy:{PID}:{}:{HASH}", START + 1);
        let gone = format!("heavy:{}:{START}:{HASH}", PID + 1);
        assert_eq!(
            decided(&restarted, Slot::Heavy),
            Nesting::Ended {
                marker: restarted.clone(),
                pid: PID
            },
            "a pid running since another start"
        );
        assert_eq!(
            decided(&gone, Slot::Heavy),
            Nesting::Ended {
                marker: gone.clone(),
                pid: PID + 1
            },
            "a pid that does not run"
        );
    }

    #[test]
    fn a_live_marker_this_gate_does_not_run_inside_is_unbound_whatever_its_kind() {
        let unbound = |text: &str, kind| Nesting::decide(Some(OsStr::new(text)), kind, Some(HASH), judged, |_, _| Binding::Outside);
        for held in [Slot::Heavy, Slot::Light, Slot::Critical, Slot::None] {
            for kind in [Slot::Heavy, Slot::Light, Slot::Critical, Slot::None] {
                let nesting = unbound(&marker(held), kind);
                assert_eq!(
                    nesting,
                    Nesting::Unbound {
                        marker: marker(held),
                        pid: PID
                    },
                    "a {kind:?} gate under a {held:?} marker"
                );
                assert_eq!(
                    (nesting.covering(), nesting.uncovering(), nesting.runs_as(kind)),
                    (None, None, kind),
                    "a {kind:?} gate under a {held:?} marker runs under no parent"
                );
            }
        }
        let gone = format!("heavy:{}:{START}:{HASH}", PID + 1);
        assert_eq!(
            unbound(&gone, Slot::Heavy),
            Nesting::Ended {
                marker: gone.clone(),
                pid: PID + 1
            },
            "a gate no longer running is ended before its binding is judged"
        );
        assert_eq!(
            unbound(&format!("heavy:{PID}:{START}:ffffffff"), Slot::Heavy),
            Nesting::Own,
            "another root's marker"
        );
    }

    #[test]
    fn heavy_covers_heavy_and_light_light_covers_light_and_critical_covers_all() {
        use Slot::{Critical, Heavy, Light};

        let covered = |held| Nesting::Covered(parent(held));
        let table = [
            (Heavy, Heavy, covered(Heavy)),
            (Heavy, Light, covered(Heavy)),
            (Heavy, Critical, Nesting::Refused(parent(Heavy))),
            (Light, Heavy, Nesting::Under(parent(Light))),
            (Light, Light, covered(Light)),
            (Light, Critical, Nesting::Refused(parent(Light))),
            (Critical, Heavy, covered(Critical)),
            (Critical, Light, covered(Critical)),
            (Critical, Critical, covered(Critical)),
        ];
        for (held, kind, expected) in table {
            assert_eq!(decided(&marker(held), kind), expected, "a {kind:?} gate under a {held:?} marker");
        }
    }

    #[test]
    fn each_decision_says_its_line() {
        let ended = Nesting::Ended {
            marker: "light:7:8".to_owned(),
            pid: 7,
        };
        let cases = [
            (Nesting::Own, Slot::Heavy, Spelling::Cli, None),
            (
                Nesting::Malformed("x:y".to_owned()),
                Slot::Light,
                Spelling::Cli,
                Some("gate: ignored GATE_SLOT_HELD='x:y', which is not <kind>:<pid>:<start>; taking a light slot"),
            ),
            (
                ended,
                Slot::Heavy,
                Spelling::Cli,
                Some("gate: ignored GATE_SLOT_HELD='light:7:8': gate 7 is no longer running; taking a heavy slot"),
            ),
            (
                Nesting::Malformed("x:y".to_owned()),
                Slot::None,
                Spelling::Cli,
                Some("gate: ignored GATE_SLOT_HELD='x:y', which is not <kind>:<pid>:<start>; running on no slot"),
            ),
            (
                Nesting::Ended {
                    marker: "light:7:8".to_owned(),
                    pid: 7,
                },
                Slot::None,
                Spelling::Cli,
                Some("gate: ignored GATE_SLOT_HELD='light:7:8': gate 7 is no longer running; running on no slot"),
            ),
            (
                Nesting::Unbound {
                    marker: "heavy:7:8".to_owned(),
                    pid: 7,
                },
                Slot::Critical,
                Spelling::Cli,
                Some("gate: ignored GATE_SLOT_HELD='heavy:7:8': this gate does not run inside the command of gate 7; taking a critical slot"),
            ),
            (
                Nesting::Unbound {
                    marker: "heavy:7:8".to_owned(),
                    pid: 7,
                },
                Slot::None,
                Spelling::Cli,
                Some("gate: ignored GATE_SLOT_HELD='heavy:7:8': this gate does not run inside the command of gate 7; running on no slot"),
            ),
            (
                Nesting::Covered(parent(Slot::Critical)),
                Slot::Light,
                Spelling::Cli,
                Some("gate: running under the critical slot of gate 4242"),
            ),
            (
                Nesting::Refused(parent(Slot::Heavy)),
                Slot::Critical,
                Spelling::Cli,
                Some(
                    "gate: --slot critical cannot run under the heavy slot of gate 4242: it would wait for that slot for good; run the \
                     critical command outside that gate",
                ),
            ),
            (
                Nesting::Refused(parent(Slot::Light)),
                Slot::Critical,
                Spelling::Compat,
                Some(
                    "gate: -Slot critical cannot run under the light slot of gate 4242: it would wait for that slot for good; run the \
                     critical command outside that gate",
                ),
            ),
            (
                Nesting::Under(parent(Slot::Light)),
                Slot::Heavy,
                Spelling::Cli,
                Some("gate: the light slot of gate 4242 does not cover a heavy gate; taking a heavy slot"),
            ),
        ];
        for (nesting, kind, spelling, line) in cases {
            assert_eq!(nesting.line(kind, spelling).as_deref(), line, "{nesting:?}");
            assert_eq!(nesting.unjudged_line(), None, "{nesting:?} was judged");
        }
    }

    #[test]
    fn a_covered_gate_says_when_its_parent_ends_or_cannot_be_watched() {
        let parent = parent(Slot::Heavy);
        assert_eq!(
            parent.ended_line(Slot::Light),
            "gate: the parent gate 4242 ended; taking a light slot for the rest of this run",
            "the gate's own kind, not the parent's"
        );
        assert_eq!(
            parent.ended_line(Slot::Heavy),
            "gate: the parent gate 4242 ended; taking a heavy slot for the rest of this run"
        );
        assert_eq!(
            parent.unwatched_line("Access is denied. (os error 5)"),
            "gate: cannot watch the parent gate 4242 (Access is denied. (os error 5)); this run stays covered by it"
        );
    }

    /// A covered marker of the gate `pid` started at `start`, its own kind `kind` (`None` for a marker naming none).
    fn covered_marker(pid: u32, start: u64, kind: Option<&str>) -> CoveredMarker {
        CoveredMarker {
            pid,
            process_start: start,
            parent_pid: 1,
            command: "cargo build".to_owned(),
            cwd: r"C:\work".to_owned(),
            since: start,
            kind: kind.map(str::to_owned),
        }
    }

    /// The stand-in `pid` started at `start_ms`, holding a `held` slot.
    fn found(pid: u32, start_ms: u64, held: Slot) -> Enclosing {
        Enclosing { pid, start_ms, held }
    }

    /// No candidate's job holds this gate's own process, so each is judged by `inside`'s walk.
    fn no_member(_: u32, _: u64) -> bool {
        false
    }

    /// The stand-in of `markers`, none skipped, for a gate of `kind`, judged by `liveness` and the walk `inside` alone.
    fn walked(
        markers: &[CoveredMarker],
        kind: Slot,
        skip: &[(u32, u64)],
        liveness: impl Fn(u32, u64) -> Liveness,
        inside: impl Fn(u32, u64) -> Binding,
    ) -> Option<Enclosing> {
        enclosing(candidates(markers, skip), kind, liveness, no_member, inside)
    }

    #[test]
    fn the_outermost_live_covered_gate_whose_command_holds_this_gate_stands_in_for_its_ended_parent() {
        let heavy = Some("heavy");
        let markers = [
            covered_marker(10, 300, heavy),
            covered_marker(11, 200, heavy),
            covered_marker(12, 100, heavy),
            covered_marker(14, 40, heavy),
            covered_marker(15, 30, heavy),
            covered_marker(16, 20, heavy),
        ];
        // 12 is gone, 14's command does not hold this gate, 15's start cannot be read and its command does not hold
        // this gate either, and 16's binding cannot be judged.
        let liveness = |pid, _| match pid {
            12 => Liveness::Gone,
            15 => Liveness::Unknown(io::Error::other("no times")),
            _ => Liveness::Live,
        };
        let inside = |pid, _| match pid {
            14 | 15 => Binding::Outside,
            16 => Binding::Unjudged("denied".to_owned()),
            _ => Binding::Inside,
        };
        assert_eq!(
            walked(&markers, Slot::Heavy, &[], liveness, inside),
            Some(found(11, 200, Slot::Heavy)),
            "the earliest started; a binding that cannot be judged binds nothing"
        );
        assert_eq!(
            walked(&markers, Slot::Heavy, &[(11, 200), (16, 20)], liveness, inside),
            Some(found(10, 300, Slot::Heavy)),
            "never a gate skipped, as the gate itself"
        );
        let unknown = |_, _| Liveness::Unknown(io::Error::other("no times"));
        assert_eq!(
            walked(&[covered_marker(15, 30, heavy)], Slot::Heavy, &[], unknown, |_, _| Binding::Inside),
            Some(found(15, 30, Slot::Heavy)),
            "a gate whose start cannot be read counts as running"
        );
        assert_eq!(walked(&[], Slot::Heavy, &[], liveness, inside), None, "no covered gate runs");
    }

    #[test]
    fn a_candidate_whose_job_holds_this_gate_is_taken_without_a_walk_and_the_walk_follows_only_when_none_does() {
        let markers = [covered_marker(10, 300, Some("heavy")), covered_marker(11, 200, Some("heavy"))];
        let walks = std::cell::Cell::new(0);
        let inside = |_, _| {
            walks.set(walks.get() + 1);
            Binding::Inside
        };
        let found_by = |member: fn(u32, u64) -> bool| enclosing(candidates(&markers, &[]), Slot::Heavy, judged_live, member, inside);
        assert_eq!(
            found_by(|pid, _| pid == 10),
            Some(found(10, 300, Slot::Heavy)),
            "the member, before an outer gate that only the walk binds"
        );
        assert_eq!(walks.get(), 0, "no walk while a candidate's job holds this gate");
        assert_eq!(found_by(no_member), Some(found(11, 200, Slot::Heavy)), "the outermost the walk binds");
        assert_eq!(walks.get(), 1, "the walk asks only until one binds");
    }

    #[test]
    fn a_heavy_or_light_gate_stands_in_and_one_whose_own_kind_covers_this_gate_comes_first() {
        use Slot::{Critical, Heavy, Light, None as NoSlot};

        let stand_in = |own: Option<&str>, kind| walked(&[covered_marker(10, 300, own)], kind, &[], judged_live, bound);
        let table = [
            (Some("heavy"), Heavy, Some(found(10, 300, Heavy))),
            (Some("heavy"), Light, Some(found(10, 300, Heavy))),
            (Some("heavy"), NoSlot, Some(found(10, 300, Heavy))),
            (Some("heavy"), Critical, Some(found(10, 300, Heavy))),
            (Some("light"), Heavy, Some(found(10, 300, Light))),
            (Some("light"), Light, Some(found(10, 300, Light))),
            (Some("light"), Critical, Some(found(10, 300, Light))),
            (Some("none"), Light, None),
            (Some("critical"), Light, None),
            (Some("critical"), Heavy, None),
            (Some("medium"), Light, None),
            (None, Heavy, None),
            (None, Light, None),
        ];
        for (own, kind, expected) in table {
            assert_eq!(stand_in(own, kind), expected, "a {own:?} stand-in for a {kind:?} gate");
        }
        let outer_light_inner_heavy = [covered_marker(10, 100, Some("light")), covered_marker(11, 200, Some("heavy"))];
        assert_eq!(
            walked(&outer_light_inner_heavy, Heavy, &[], judged_live, bound),
            Some(found(11, 200, Heavy)),
            "the inner heavy gate, whose slot covers a heavy gate, before the outer light one"
        );
        assert_eq!(
            walked(&outer_light_inner_heavy, Light, &[], judged_live, bound),
            Some(found(10, 100, Light)),
            "the outermost, when both cover"
        );
    }

    #[test]
    fn a_critical_gate_under_a_gate_standing_in_for_its_ended_parent_is_refused_as_under_a_live_one() {
        for held in [Slot::Heavy, Slot::Light] {
            let parent = Parent::standing_in(marker(Slot::Critical), found(77, 88, held));
            let nesting = Nesting::under(parent.clone(), Slot::Critical);
            assert_eq!(nesting, Nesting::Refused(parent), "under a {held:?} stand-in");
            assert_eq!(
                nesting.line(Slot::Critical, Spelling::Cli),
                Some(format!(
                    "gate: --slot critical cannot run under the {} slot of gate 77: it would wait for that slot for good; run the \
                     critical command outside that gate",
                    held.as_str()
                ))
            );
        }
    }

    #[test]
    fn a_gate_turned_heavy_looks_for_its_stand_in_again_and_a_live_parent_is_only_judged_again() {
        let light = Parent::standing_in(marker(Slot::Heavy), found(10, 100, Slot::Light));
        let heavy = Parent::standing_in(marker(Slot::Heavy), found(11, 200, Slot::Heavy));
        assert_eq!(
            Nesting::Covered(light.clone()).as_heavy(|| Some(found(11, 200, Slot::Heavy))),
            Nesting::Covered(heavy),
            "the heavy gate the search finds covers it"
        );
        assert_eq!(
            Nesting::Covered(light.clone()).as_heavy(|| None),
            Nesting::Under(light),
            "the stand-in found first, when the search finds none"
        );
        let live = parent(Slot::Light);
        assert_eq!(
            Nesting::Covered(live.clone()).as_heavy(|| unreachable!("a live parent is not looked for again")),
            Nesting::Under(live)
        );
    }

    /// Every gate runs.
    fn judged_live(_: u32, _: u64) -> Liveness {
        Liveness::Live
    }

    #[test]
    fn a_gate_standing_in_for_an_ended_parent_names_its_own_slot_kind_and_says_so() {
        let parent = Parent::standing_in(
            marker(Slot::Critical),
            Enclosing {
                pid: 77,
                start_ms: 88,
                held: Slot::Light,
            },
        );
        assert_eq!(
            parent,
            Parent {
                held: Slot::Light,
                pid: 77,
                start_ms: 88,
                marker: marker(Slot::Critical),
                unjudged: None,
                stand_in: true,
                commit: false,
            },
            "the stand-in's own kind, not the ended parent's"
        );
        assert_eq!(
            parent.taken_up_line(PID),
            "gate: the parent gate 4242 ended; running under the light slot of gate 77"
        );
        let nesting = Nesting::under(parent.clone(), Slot::Light);
        assert_eq!(nesting, Nesting::Covered(parent), "nested under the stand-in's slot");
        assert_eq!(nesting.runs_as(Slot::Light), Slot::Light, "no longer as critical");
    }

    #[test]
    fn a_binding_that_cannot_be_judged_keeps_the_marker_and_says_so_first() {
        let unjudged = |kind| {
            Nesting::decide(Some(OsStr::new(&marker(Slot::Heavy))), kind, Some(HASH), judged, |_, _| {
                Binding::Unjudged("the query failed".to_owned())
            })
        };
        let parent = Parent {
            unjudged: Some("the query failed".to_owned()),
            ..parent(Slot::Heavy)
        };
        let covered = unjudged(Slot::Light);
        assert_eq!(covered, Nesting::Covered(parent.clone()), "covered as a bound gate is");
        assert_eq!(unjudged(Slot::Critical), Nesting::Refused(parent), "refused as a bound gate is");
        assert_eq!(
            covered.unjudged_line().as_deref(),
            Some(
                "gate: could not judge GATE_SLOT_HELD='heavy:4242:1759439760571:0a1b2c3d' (the query failed); running under the heavy \
                 slot of gate 4242"
            )
        );
        assert_eq!(
            covered.line(Slot::Light, Spelling::Cli).as_deref(),
            Some("gate: running under the heavy slot of gate 4242"),
            "the decision's own line follows"
        );
    }

    /// The job a box run names for its command, as these cases give it.
    const RUN_JOB: &str = r"Local\gate-remote-k-ab12cd34ef56";
    /// The line a gate inside its box job says.
    const IN_BOX: &str = "gate: inside a routed run; running without a slot on this builder";
    /// The line a gate says of a box job that does not hold it.
    const NOT_HELD: &str = "gate: ignored GATE_BOX_JOB: this gate does not run inside its job";

    /// What a gate whose environment names [`RUN_JOB`] makes of it, `binding` whether that job holds it.
    fn in_job(binding: Binding) -> BoxJob {
        box_job(Some(OsStr::new(RUN_JOB)), |name| {
            assert_eq!(name, RUN_JOB, "the job is opened by the name given");
            binding
        })
    }

    /// The environment of a gate given the marker `marker` and the box job `job`, each when given.
    fn environment(marker: Option<&str>, job: Option<&str>) -> Environment {
        Environment {
            state_dir: None,
            local_app_data: None,
            git_index_file: None,
            slot_held: marker.map(OsString::from),
            box_job: job.map(OsString::from),
            commit_slot: None,
        }
    }

    /// The nesting [`Nesting::read_with`] gives a heavy gate of `environment`, `binding` whether a job holds it, and
    /// the lines it said.
    fn read(environment: &Environment, binding: Binding) -> (Nesting, Vec<String>) {
        let path = r"C:\Users\someone\AppData\Local\gate\v1";
        let root = ResolvedRoot {
            path: PathBuf::from(path),
            root: StateRoot::new(path, Some(path)),
        };
        let mut console = Console::new();
        let nesting = Nesting::read_with(environment, Slot::Heavy, &root, true, |_| binding, &mut console);
        (nesting, console.take_notes())
    }

    #[test]
    fn a_gate_inside_its_box_job_takes_no_slot() {
        assert_eq!(in_job(Binding::Inside), BoxJob::Inside);
        assert_eq!(BoxJob::Inside.line(), None, "a trusted box job is said by the nesting's own line");
        let nesting = Nesting::InBox;
        for kind in [Slot::Heavy, Slot::Light, Slot::None] {
            assert_eq!(nesting.takes(kind), Slot::None, "a {kind:?} gate");
            assert_eq!(nesting.requested(kind), kind, "a {kind:?} gate runs as the kind it asked for");
            assert_eq!(nesting.line(kind, Spelling::Cli).as_deref(), Some(IN_BOX), "a {kind:?} gate");
        }
        assert_eq!(
            (nesting.covering(), nesting.uncovering(), nesting.unjudged_line()),
            (None, None, None),
            "no parent gate's slot and no marker"
        );
        assert_eq!(
            nesting.as_heavy(|| unreachable!("no parent is looked for")),
            Nesting::InBox,
            "a gate turned heavy still takes no slot"
        );
        assert_eq!(box_job(None, |_| unreachable!("no job is opened")), BoxJob::Unset, "an unset name");
        assert_eq!(
            box_job(Some(OsStr::new("")), |_| unreachable!("no job is opened")),
            BoxJob::Unset,
            "an empty name"
        );
        assert_eq!(BoxJob::Unset.line(), None);
    }

    #[test]
    fn a_box_job_that_does_not_hold_the_gate_is_ignored_with_a_line() {
        assert_eq!(in_job(Binding::Outside), BoxJob::NotHeld);
        assert_eq!(BoxJob::NotHeld.line().as_deref(), Some(NOT_HELD));
    }

    #[test]
    fn a_box_job_that_cannot_be_opened_is_ignored_with_a_line() {
        let reason = "Access is denied. (os error 5)";
        assert_eq!(
            in_job(Binding::Unjudged(reason.to_owned())),
            BoxJob::Unjudged(reason.to_owned()),
            "an unjudged box job never skips slots"
        );
        let (nesting, said) = read(&environment(None, Some(RUN_JOB)), Binding::Unjudged(reason.to_owned()));
        assert_eq!(nesting, Nesting::Own, "the gate runs as if no box job were named");
        assert_eq!(said, [format!("gate: ignored GATE_BOX_JOB: cannot judge its job: {reason}")]);
    }

    #[test]
    fn a_critical_gate_inside_a_box_job_takes_no_slot() {
        assert_eq!(in_job(Binding::Inside), BoxJob::Inside);
        let nesting = Nesting::InBox;
        assert_eq!(nesting.takes(Slot::Critical), Slot::None, "no machine taken");
        assert_eq!(nesting.requested(Slot::Critical), Slot::Heavy, "never raised to High on a builder");
        assert_eq!(nesting.line(Slot::Critical, Spelling::Cli).as_deref(), Some(IN_BOX), "never refused");
        assert_eq!(
            (Nesting::Own.requested(Slot::Critical), Nesting::Own.takes(Slot::Critical)),
            (Slot::Critical, Slot::Critical),
            "a gate outside a box job runs as and takes its own kind"
        );
    }

    #[test]
    fn a_box_job_that_holds_the_gate_wins_over_its_marker() {
        let marker = format!("light:{PID}:{START}:{HASH}");
        let (nesting, said) = read(&environment(Some(&marker), Some(RUN_JOB)), Binding::Inside);
        assert_eq!(nesting, Nesting::InBox, "the marker is not read");
        assert_eq!(said, Vec::<String>::new(), "the nesting's own line comes later");
    }

    #[test]
    fn a_name_that_is_no_box_run_s_job_is_ignored_without_opening_it() {
        let names = [
            r"Local\gate-job-4242",
            r"Local\gate-remote--ab12cd34ef56",
            r"Local\gate-remote-k-ab12cd34ef5",
            r"Local\gate-remote-k-ab12cd34ef567",
            r"Local\gate-remote-k-ab12cd34ef5g",
            r"Session\1\gate-remote-k-ab12cd34ef56",
            r"gate-remote-k-ab12cd34ef56",
        ];
        for name in names {
            let judged = box_job(Some(OsStr::new(name)), |_| unreachable!("{name} is not opened"));
            assert_eq!(judged, BoxJob::NotHeld, "{name}");
        }
        let (nesting, said) = read(&environment(None, Some(r"Local\gate-job-4242")), Binding::Inside);
        assert_eq!((nesting, said), (Nesting::Own, vec![NOT_HELD.to_owned()]));
        for name in [RUN_JOB, r"Global\gate-remote-k-ab12cd34ef56", r"Local\gate-remote-a-b-0123456789AB"] {
            assert_eq!(box_job(Some(OsStr::new(name)), |_| Binding::Inside), BoxJob::Inside, "{name}");
        }
    }

    #[test]
    fn only_a_gate_covered_by_a_critical_parent_runs_as_critical() {
        assert_eq!(Nesting::Covered(parent(Slot::Critical)).runs_as(Slot::Light), Slot::Critical);
        assert_eq!(Nesting::Covered(parent(Slot::Heavy)).runs_as(Slot::Light), Slot::Light);
        assert_eq!(Nesting::Under(parent(Slot::Light)).runs_as(Slot::Heavy), Slot::Heavy);
        assert_eq!(Nesting::Own.runs_as(Slot::Critical), Slot::Critical);
        assert_eq!(
            Nesting::Refused(parent(Slot::Heavy)).covering(),
            None,
            "a refused gate runs under nothing"
        );
        assert_eq!(
            Nesting::Under(parent(Slot::Light)).covering(),
            None,
            "an uncovered gate runs under nothing"
        );
    }
}
