//! The learned durations as a run uses them: the key its runs are filed under, the expected-time line it says as it
//! takes a slot, its release filed in `durations.json` in the hold of its `release` line, and the store rebuilt from
//! the journal once the run has released when its reads found the store missing, stale or torn.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_args::{Ceiling, MAX_WHOLE_NUMBER, TestSeams};
use gate_router::Place;
use gate_state::{
    DurationKey, DurationStore, Estimate, JournalLock, KeyInputs, KeyPlace, RebuildOwner, RebuildReason, Rebuilt, Recorded, RunFacts, StoreError,
    StoreRead, StoreReadError, abandon_rebuild, begin_rebuild, duration_key, durations_path, place_by_shape, read_store, rebuild_store, record_run,
    swap_rebuilt,
};
use gate_win32::Interrupt;
use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::console::Console;
use crate::remote::git::{self, Git};
use crate::route::{self, Placed};
use crate::slots::{self, Liveness};

/// The `about` figure, in seconds, from which the expected-time line is in minutes.
const MINUTES_FROM: f64 = 90.0;

/// The line a gate says as it takes its slot for a key with history: `gate: expected about {mean} (usual {p50}, p90
/// {p90}, {runs} runs)`, every figure in whole seconds when the mean is under 90 s, else in whole minutes, each rounded to
/// the nearest and never below 1. An estimate of the family key ends `runs of similar commands)`.
pub(crate) fn expected_line(estimate: &Estimate) -> String {
    let in_minutes = in_minutes(estimate.mean);
    let shown = |seconds: f64| in_unit(seconds, in_minutes);
    let of = if estimate.family { " of similar commands" } else { "" };
    format!(
        "gate: expected about {} (usual {}, p90 {}, {} runs{of})",
        shown(estimate.mean),
        shown(estimate.p50),
        shown(estimate.p90),
        estimate.runs
    )
}

/// A span of `seconds` alone, by the expected-time line's rule: whole seconds below 90 s, whole minutes from 90 s,
/// rounded to the nearest and never below 1.
pub(crate) fn span_text(seconds: f64) -> String {
    in_unit(seconds, in_minutes(seconds))
}

/// Whether a span of `seconds` is said in minutes: from 90 s once rounded to the whole second, so no span reads `90 s`.
fn in_minutes(seconds: f64) -> bool {
    seconds.round() >= MINUTES_FROM
}

/// A span of `seconds` in whole minutes when `in_minutes`, else in whole seconds, rounded to the nearest and never
/// below 1; the minutes are those of the whole seconds, so 89.6 s, which is 90 s, reads as 90 s does.
fn in_unit(seconds: f64, in_minutes: bool) -> String {
    let whole_seconds = seconds.round();
    if in_minutes {
        format!("{} min", (whole_seconds / 60.0).round().max(1.0))
    } else {
        format!("{} s", whole_seconds.max(1.0))
    }
}

/// A run's ceiling as a take resolved it: whole seconds, and the adjusted p99 that raised it above an auto ceiling's
/// floor with that floor, `None` when nothing raised it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Resolved {
    pub seconds: u64,
    pub raised: Option<(f64, u64)>,
}

/// The usual p90 of the slot a run took, which its hang hint reads, and whether it is a family key's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Usual {
    pub p90: f64,
    pub family: bool,
}

impl Usual {
    /// The usual p90 of `estimate`; `None` for one too wide to judge by ([`too_wide`]).
    pub(crate) fn of(estimate: &Estimate) -> Option<Usual> {
        (!too_wide(estimate)).then_some(Usual {
            p90: estimate.p90,
            family: estimate.family,
        })
    }
}

/// Whether `estimate` is too wide to judge a run by: a family key's whose p90 is above 2x its p50. It raises no auto
/// ceiling and gives neither the ceiling hint nor the hang hint.
fn too_wide(estimate: &Estimate) -> bool {
    estimate.family && estimate.p90 > FAMILY_SPREAD * estimate.p50
}

/// The shortest usual p90 the hang hint is said for, in seconds.
const HANG_HINT_FLOOR: f64 = 1.0;

/// Whose figures a line names: this command's, or, for a family key's estimate, similar commands'.
fn whose(family: bool) -> &'static str {
    if family { "similar commands'" } else { "this command's" }
}

/// How many times its p99 an auto ceiling allows a command, and how many times its p90 a run goes before its hang hint.
const USUAL_TIMES: f64 = 3.0;

/// How many times its p50 a family key's p90 may be before its estimate is too wide to raise an auto ceiling.
const FAMILY_SPREAD: f64 = 2.0;

/// `ceiling` resolved from a take's `estimate`: a given ceiling as given; an auto ceiling at
/// `max(n, ceil(3 x adjusted p99))`, capped at [`MAX_WHOLE_NUMBER`], and at its floor `n` without an adjusted p99 or
/// when the estimate is a family key's whose p90 is above 2x its p50.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the CLI bounds the floor to 2^31 - 1, which an f64 holds exactly, and the learned figure is capped there before the cast"
)]
pub(crate) fn resolve_ceiling(ceiling: Ceiling, estimate: Option<&Estimate>) -> Resolved {
    let floor = match ceiling {
        Ceiling::Given(seconds) => return Resolved { seconds, raised: None },
        Ceiling::Auto(floor) => floor,
    };
    let p99 = estimate.filter(|estimate| !too_wide(estimate)).and_then(|estimate| estimate.adjusted_p99);
    match p99 {
        Some(p99) => {
            let learned = (USUAL_TIMES * p99).ceil().min(MAX_WHOLE_NUMBER as f64);
            if learned > floor as f64 {
                Resolved {
                    seconds: learned as u64,
                    raised: Some((p99, floor)),
                }
            } else {
                Resolved {
                    seconds: floor,
                    raised: None,
                }
            }
        }
        None => Resolved {
            seconds: floor,
            raised: None,
        },
    }
}

/// The line a take says after its expected-time line when history raised an auto ceiling above its `floor`: `gate:
/// ceiling {resolved} (3x this command's p99 of {p99}, above the given {floor})`, each a span by [`span_text`], and
/// "similar commands'" for a `family` key's p99.
#[expect(clippy::cast_precision_loss, reason = "the CLI bounds a ceiling to 2^31 - 1, which an f64 holds exactly")]
pub(crate) fn raised_ceiling_line(resolved: u64, p99: f64, floor: u64, family: bool) -> String {
    format!(
        "gate: ceiling {} (3x {} p99 of {}, above the given {})",
        span_text(resolved as f64),
        whose(family),
        span_text(p99),
        span_text(floor as f64)
    )
}

/// The hint a take says when its `resolved` ceiling is below the load-adjusted p90 of its `estimate`: `gate: the
/// ceiling {resolved} is below this command's load-adjusted p90 of {p90}`, each a span by [`span_text`], and "similar
/// commands'" for a family key's estimate; `None` at or above the p90, when the estimate has no adjusted p90, and for
/// an estimate too wide to judge by ([`too_wide`]).
#[expect(clippy::cast_precision_loss, reason = "the CLI bounds a ceiling to 2^31 - 1, which an f64 holds exactly")]
pub(crate) fn ceiling_hint(resolved: u64, estimate: &Estimate) -> Option<String> {
    let ceiling = resolved as f64;
    let p90 = estimate.adjusted_p90?;
    (ceiling < p90 && !too_wide(estimate)).then(|| {
        format!(
            "gate: the ceiling {} is below {} load-adjusted p90 of {}",
            span_text(ceiling),
            whose(estimate.family),
            span_text(p90)
        )
    })
}

/// The hint a run says once while it runs, when it has been running `running_seconds` and that is 3x or more its
/// `usual` p90: `gate: still running after {running}, {k}x its usual p90 of {p90}; {phase} for {phase_seconds}`, each
/// time a span by [`span_text`] and `k` the whole times the p90 it has run, and "similar commands' usual p90" for a
/// family key's; `None` below 3x, and for a p90 under 1 s. `phase` names what the run is doing now (`running here`,
/// `running on {builder}`, `pulling outputs from {builder}`), and `phase_seconds` is how long it has been doing it.
pub(crate) fn hang_hint(running_seconds: f64, usual: Usual, phase: &str, phase_seconds: f64) -> Option<String> {
    let Usual { p90, family } = usual;
    if p90 < HANG_HINT_FLOOR || running_seconds < USUAL_TIMES * p90 {
        return None;
    }
    let whose = if family { "similar commands'" } else { "its" };
    Some(format!(
        "gate: still running after {}, {}x {whose} usual p90 of {}; {phase} for {}",
        span_text(running_seconds),
        (running_seconds / p90).floor(),
        span_text(p90),
        span_text(phase_seconds)
    ))
}

/// The usual p90 a routed run's hang hint compares with: `usual`, or none for a `cold` run, whose work tree on the
/// builder is new and whose ceiling was raised, as a usual p90 learnt from warm runs would call it hung.
pub(crate) fn routed_usual(usual: Option<Usual>, cold: bool) -> Option<Usual> {
    usual.filter(|_| !cold)
}

/// The `expected` field of a claim and a ticket for `estimate`: its usual and p90 seconds to three decimals, a midpoint
/// to the even digit as the journal writes seconds, its run count and the key it was read under.
pub(crate) fn expected_field(estimate: &Estimate) -> Map<String, Value> {
    let seconds = |value: f64| Value::from((value * 1000.0).round_ties_even() / 1000.0);
    let mut field = Map::new();
    field.insert("p50".to_owned(), seconds(estimate.p50));
    field.insert("p90".to_owned(), seconds(estimate.p90));
    field.insert("n".to_owned(), Value::from(estimate.runs));
    field.insert("key".to_owned(), Value::from(estimate.key.clone()));
    field
}

/// How long each git call that places a folder may take, and how long its readers get once git has exited.
#[derive(Clone, Copy)]
struct GitLimits {
    limit: Duration,
    drain: Duration,
}

impl GitLimits {
    fn of(seams: &TestSeams) -> GitLimits {
        GitLimits {
            limit: git::limit(seams),
            drain: git::drain(seams),
        }
    }

    /// Where the folder `cwd` sits, placed by one git call, or by its path's shape when git does not place it.
    fn place_or_shape(self, cwd: &str) -> KeyPlace {
        let folder = Path::new(cwd);
        let placed = Git::find(folder).ok().and_then(|git| self.place(&git, folder, None));
        placed.unwrap_or_else(|| place_by_shape(cwd))
    }

    /// Where git places `folder`, asked as the routing decision asks it ([`route::read_place`]), the call ended when
    /// `interrupt` is raised: in its worktree, under the main checkout's name. `None` when git fails, says the folder
    /// is in no worktree, does not answer in time, or writes what does not read as a place.
    fn place(self, git: &Git, folder: &Path, interrupt: Option<Interrupt>) -> Option<KeyPlace> {
        let call = self.place_call(folder, interrupt);
        route::read_place(git, &call).ok().flatten().as_ref().map(from_route)
    }

    /// The call that places `folder`, as every placement call is built ([`git::Call::placing`]).
    fn place_call(self, folder: &Path, interrupt: Option<Interrupt>) -> git::Call<'_> {
        git::Call::placing(folder, &route::PLACE_ARGS, self.limit, self.drain, interrupt)
    }
}

/// Where the folder `cwd` sits in a key, as a run whose routing decision asked no git places it: by one git call under
/// the git limit of `seams`, or by its path's shape when git does not place it.
pub(crate) fn folder_place(cwd: &str, seams: &TestSeams) -> KeyPlace {
    GitLimits::of(seams).place_or_shape(cwd)
}

/// The system temp root, whose paths a command word loses in a key; `None` when it is empty.
pub(crate) fn temp_root() -> Option<String> {
    Some(std::env::temp_dir().to_string_lossy().into_owned()).filter(|root| !root.is_empty())
}

/// The line that says the store cannot be read, so no expected time is said.
pub(crate) fn unreadable_line(error: &StoreReadError) -> String {
    format!(
        "gate: cannot read {} ({}); expected times are unavailable",
        error.path.display(),
        error.reason
    )
}

/// The key place of a folder the routing decision placed.
fn from_route(place: &Place) -> KeyPlace {
    KeyPlace::InGit {
        repo: place.checkout.clone(),
        sub: place.prefix.clone(),
        worktree_root: place.root.clone(),
    }
}

/// What a run that takes a slot knows of its durations: its key's parts, read once before it queues, and which of its
/// once-a-run lines it has said.
pub(crate) struct RunDurations {
    root: PathBuf,
    folder: KeyPlace,
    command: String,
    temp_root: Option<String>,
    critical: bool,
    owner: RebuildOwner,
    git: GitLimits,
    /// Whether a read of the store found it missing, stale or torn, so the run rebuilds it once it has released.
    rebuild: bool,
    /// The store as the run's one read found it.
    store: StoreSeen,
    said: Said,
}

/// The store as a run's one read of it found it.
enum StoreSeen {
    /// Not read yet.
    Unread,
    Current(DurationStore),
    /// Missing, stale, torn, unreadable or a newer build's: no estimates this run.
    NotCurrent,
}

/// Which of a run's once-a-run durations lines it has said.
#[derive(Default)]
struct Said {
    unreadable: bool,
    unwritten: bool,
}

/// Who a run is, for its key and its rebuild: the folder and command its journal lines name, whether it is a critical
/// gate, and the gate itself.
pub(crate) struct RunIdentity<'a> {
    pub cwd: &'a str,
    pub command: &'a str,
    pub critical: bool,
    pub owner: RebuildOwner,
}

impl RunDurations {
    /// The durations of a run in the state root `root`: its folder placed as the routing decision `placed` it when the
    /// decision asked git, else by one git call under the run's git limit, and by its path's shape when git does not
    /// place it.
    pub(crate) fn open(root: &Path, run: &RunIdentity<'_>, placed: &Placed, seams: &TestSeams) -> RunDurations {
        let limits = GitLimits::of(seams);
        let folder = match placed {
            Placed::Asked(Some(place)) => from_route(place),
            Placed::Asked(None) => place_by_shape(run.cwd),
            Placed::NotAsked => limits.place_or_shape(run.cwd),
        };
        RunDurations {
            root: root.to_path_buf(),
            folder,
            command: run.command.to_owned(),
            temp_root: temp_root(),
            critical: run.critical,
            owner: run.owner,
            git: limits,
            rebuild: false,
            store: StoreSeen::Unread,
            said: Said::default(),
        }
    }

    /// The run's key on the build box `remote_host` (`user@host`), or here when it is `None`.
    fn key(&self, remote_host: Option<&str>) -> DurationKey {
        duration_key(&KeyInputs {
            folder: &self.folder,
            remote_host,
            critical: self.critical,
            temp_root: self.temp_root.as_deref(),
            command: &self.command,
        })
    }

    /// The estimate of the run's key on `remote_host`, from the store as the run's first call read it with no lock:
    /// `None` for a key without history, a missing store or a newer build's, and a store that cannot be read, said at
    /// that read. Every take of the run, its ticket, claim and expected-time line, shares the one read.
    pub(crate) fn estimate(&mut self, remote_host: Option<&str>, console: &mut Console) -> Option<Estimate> {
        let key = self.key(remote_host);
        self.store(console)?.estimate(&key, Timestamp::now())
    }

    /// The store as the run read it once, read now on the first call; `None` when it was not current.
    fn store(&mut self, console: &mut Console) -> Option<&DurationStore> {
        if matches!(self.store, StoreSeen::Unread) {
            self.store = match read_store(&self.root) {
                StoreRead::Current(store) => StoreSeen::Current(store),
                StoreRead::NeedsRebuild(reason) => {
                    self.needs_rebuild(reason, console);
                    StoreSeen::NotCurrent
                }
                StoreRead::Unavailable(error) => {
                    self.say_unreadable(&error, console);
                    StoreSeen::NotCurrent
                }
                StoreRead::Newer => StoreSeen::NotCurrent,
            };
        }
        match &self.store {
            StoreSeen::Current(store) => Some(store),
            StoreSeen::Unread | StoreSeen::NotCurrent => None,
        }
    }

    /// What files the run's release on `remote_host` in the journal hold, or `None` when the run reached no verdict;
    /// `cold` files a passing cold run apart from the samples the estimate reads.
    pub(crate) fn filing(&self, remote_host: Option<&str>, facts: Option<RunFacts>, cold: bool) -> Option<Filing> {
        let facts = facts?;
        Some(Filing {
            root: self.root.clone(),
            key: self.key(remote_host),
            facts,
            cold,
        })
    }

    /// Says what filing the release found: a store that cannot be read or written, each once a run; a store that
    /// needs a rebuild is rebuilt once the run has released ([`RunDurations::rebuild`]), and one the release was filed
    /// in, made current since the run's read, needs none.
    pub(crate) fn filed(&mut self, filed: Result<Recorded, StoreError>, console: &mut Console) {
        match filed {
            Ok(Recorded::Written) => self.rebuild = false,
            Ok(Recorded::Newer) => {}
            Ok(Recorded::NeedsRebuild(reason)) => self.needs_rebuild(reason, console),
            Ok(Recorded::Unavailable(error)) => self.say_unreadable(&error, console),
            Err(error) => self.say_unwritten(&error, console),
        }
    }

    /// Rebuilds the store from the journal and the archive when a read of this run asked for it, at most once a run,
    /// and no Ctrl+C raised `interrupt` ([`RunDurations::rebuild_unless`]), git placing each folder.
    pub(crate) fn rebuild(&mut self, lock: &impl JournalLock, interrupt: Option<Interrupt>, console: &mut Console) {
        let git = Git::find(&self.root).ok();
        let limits = self.git;
        let place = |folder: &Path| limits.place(git.as_ref()?, folder, interrupt);
        self.rebuild_unless(lock, || interrupt.is_some_and(Interrupt::is_raised), place, console);
    }

    /// Claims the rebuild ([`RunDurations::claim_rebuild`]), reads the journal outside its hold with `place` placing each
    /// folder that still exists, then swaps the store in under `lock` ([`RunDurations::swap_in`]). Once `stopped` says
    /// so, no folder is placed any more, nothing is swapped in and the claim is given up.
    fn rebuild_unless(
        &mut self,
        lock: &impl JournalLock,
        stopped: impl Fn() -> bool,
        place: impl Fn(&Path) -> Option<KeyPlace>,
        console: &mut Console,
    ) {
        if stopped() || !self.claim_rebuild(lock, console) {
            return;
        }
        let key_place = |folder: &Path| (!stopped() && folder.is_dir()).then(|| place(folder)).flatten();
        let rebuilt = rebuild_store(&self.root, self.temp_root.as_deref(), Timestamp::now(), key_place);
        if stopped() {
            if let Some(note) = abandon_rebuild(&self.root, self.owner) {
                console.err(&format!("gate: {note}"));
            }
            return;
        }
        self.swap_in(lock, &rebuilt, console);
    }

    /// Whether this run rebuilds the store: a read of it asked for a rebuild and none was claimed yet, the store is
    /// still not current nor a newer build's, read again with no lock, and the claim is this run's. Another gate's live
    /// claim, or a marker another process holds open, is a quiet no; a claim that fails otherwise is said.
    fn claim_rebuild(&mut self, lock: &impl JournalLock, console: &mut Console) -> bool {
        if !std::mem::take(&mut self.rebuild) || matches!(read_store(&self.root), StoreRead::Current(_) | StoreRead::Newer) {
            return false;
        }
        // A holder whose liveness cannot be read is taken for live: its rebuild is never taken over under it.
        let holder_live = |pid, start_ms| !matches!(slots::gate_liveness(pid, start_ms), Liveness::Gone);
        match begin_rebuild(&self.root, lock, self.owner, holder_live) {
            Ok(claimed) => claimed,
            Err(error) => {
                console.err(&format!(
                    "gate: cannot rebuild {} ({error}); expected times are unavailable",
                    durations_path(&self.root).display()
                ));
                false
            }
        }
    }

    /// Says what the rebuild's reads had to say, then swaps `rebuilt` in under `lock`, saying what the swap had to say
    /// and a store it could not write; a swap that cannot take `lock` leaves the rebuild to a later run.
    fn swap_in(&mut self, lock: &impl JournalLock, rebuilt: &Rebuilt, console: &mut Console) {
        for note in &rebuilt.notes {
            console.err(&format!("gate: {note}"));
        }
        match swap_rebuilt(&self.root, lock, rebuilt, Timestamp::now(), self.owner) {
            Ok(swap) => {
                for note in &swap.notes {
                    console.err(&format!("gate: {note}"));
                }
            }
            // A journal mutex held elsewhere leaves the rebuild to a later run, as a claim another gate holds does.
            Err(StoreError::Lock(_)) => {}
            Err(error) => self.say_unwritten(&error, console),
        }
    }

    fn needs_rebuild(&mut self, reason: RebuildReason, console: &mut Console) {
        self.rebuild = true;
        if let RebuildReason::Invalid(error) = reason {
            self.say_unreadable(&error, console);
        }
    }

    fn say_unreadable(&mut self, error: &StoreReadError, console: &mut Console) {
        if !std::mem::replace(&mut self.said.unreadable, true) {
            console.err(&unreadable_line(error));
        }
    }

    fn say_unwritten(&mut self, error: &StoreError, console: &mut Console) {
        if std::mem::replace(&mut self.said.unwritten, true) {
            return;
        }
        let (path, reason) = match error {
            StoreError::Write(error) => (error.path.clone(), error.reason.clone()),
            StoreError::Lock(_) | StoreError::Marker { .. } => (durations_path(&self.root), error.to_string()),
        };
        console.err(&format!("gate: could not write {} ({reason})", path.display()));
    }
}

/// A release to file in the store, inside the journal hold of its line.
pub(crate) struct Filing {
    root: PathBuf,
    key: DurationKey,
    facts: RunFacts,
    /// Whether the run was cold: its builder work tree was one its own preflight laid out.
    cold: bool,
}

impl Filing {
    /// Files the release, its line written at `time`: no git, no journal read.
    pub(crate) fn file(&self, time: Timestamp) -> Result<Recorded, StoreError> {
        record_run(&self.root, &self.key, time, &self.facts, self.cold)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use gate_state::{
        DurationStore, Estimate, Held, JournalLock, KeyPlace, LockError, RebuildOwner, durations_path, place_by_shape, rebuilding_path, write_store,
    };
    use jiff::Timestamp;
    use serde_json::json;

    use gate_args::Ceiling;

    use super::{
        GitLimits, Resolved, RunDurations, Said, StoreSeen, Usual, ceiling_hint, expected_line, hang_hint, in_unit, raised_ceiling_line,
        resolve_ceiling, routed_usual, span_text,
    };
    use crate::console::Console;

    fn estimate(mean: f64, p50: f64, p90: f64, runs: usize, family: bool) -> Estimate {
        Estimate {
            p50,
            p90,
            p99: p90,
            adjusted_p90: None,
            adjusted_p99: None,
            mean,
            runs,
            key: "local:: | cargo build".to_owned(),
            family,
        }
    }

    #[test]
    fn a_placement_call_gets_the_whole_limit_once_git_exits_by_itself_and_the_drain_otherwise() {
        let folder = Path::new("C:/work");
        let starved = GitLimits {
            limit: Duration::from_mins(15),
            drain: Duration::from_secs(2),
        };
        let call = starved.place_call(folder, None);
        assert_eq!(
            call.drain_once_exited(),
            Duration::from_mins(15),
            "a starved gate's readers get the whole git limit once git exits by itself"
        );
        assert_eq!(
            call.drain,
            Duration::from_secs(2),
            "a call its interrupt or limit ends keeps the ordinary drain"
        );
        let longer = GitLimits {
            limit: Duration::from_secs(2),
            drain: Duration::from_secs(30),
        };
        assert_eq!(
            longer.place_call(folder, None).drain_once_exited(),
            Duration::from_secs(30),
            "a drain above the limit still wins"
        );
    }

    #[test]
    fn under_90_s_the_line_is_in_whole_seconds() {
        assert_eq!(
            expected_line(&estimate(19.4, 18.0, 31.2, 12, false)),
            "gate: expected about 19 s (usual 18 s, p90 31 s, 12 runs)"
        );
        assert_eq!(
            expected_line(&estimate(89.6, 80.0, 120.0, 7, false)),
            "gate: expected about 2 min (usual 1 min, p90 2 min, 7 runs)",
            "a mean of 89.6 s is 90 s once rounded, so the line is in minutes"
        );
        assert_eq!(
            expected_line(&estimate(89.4, 80.0, 120.0, 7, false)),
            "gate: expected about 89 s (usual 80 s, p90 120 s, 7 runs)",
            "every figure in the unit of the about figure"
        );
    }

    #[test]
    fn a_sub_second_command_says_about_1_s() {
        assert_eq!(
            expected_line(&estimate(0.3, 0.2, 0.6, 5, false)),
            "gate: expected about 1 s (usual 1 s, p90 1 s, 5 runs)",
            "no figure is ever 0 s"
        );
    }

    #[test]
    fn from_90_s_the_line_is_in_whole_minutes_never_below_1() {
        assert_eq!(
            expected_line(&estimate(90.0, 20.0, 100.0, 5, false)),
            "gate: expected about 2 min (usual 1 min, p90 2 min, 5 runs)",
            "90 s rounds to 2 min, and 20 s is never 0 min"
        );
        assert_eq!(
            expected_line(&estimate(240.0, 230.0, 380.0, 30, false)),
            "gate: expected about 4 min (usual 4 min, p90 6 min, 30 runs)"
        );
    }

    #[test]
    fn a_span_alone_is_in_seconds_below_90_s_and_in_whole_minutes_from_90_s_never_below_1() {
        assert_eq!(span_text(89.0), "89 s");
        assert_eq!(span_text(89.4), "89 s", "still below 90 s");
        assert_eq!(span_text(89.6), "2 min", "rounds to 90 s, so it is in minutes, never 90 s");
        assert_eq!(span_text(90.0), "2 min", "90 s is 1.5 min, rounded to 2");
        assert_eq!(span_text(240.0), "4 min");
        assert_eq!(span_text(0.2), "1 s", "never 0 s");
        assert_eq!(in_unit(20.0, true), "1 min", "never 0 min in the unit of a longer figure");
    }

    #[test]
    fn a_family_estimate_says_it_is_of_similar_commands() {
        assert_eq!(
            expected_line(&estimate(20.0, 18.0, 31.0, 41, true)),
            "gate: expected about 20 s (usual 18 s, p90 31 s, 41 runs of similar commands)"
        );
    }

    /// An estimate of a usual 10 s with a p90 of 15 s, whose adjusted p99 is `adjusted_p99`.
    fn adjusted(adjusted_p99: Option<f64>) -> Estimate {
        Estimate {
            adjusted_p99,
            ..estimate(12.0, 10.0, 15.0, 8, false)
        }
    }

    #[test]
    fn an_auto_ceiling_keeps_a_floor_above_3x_the_p99() {
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), Some(&adjusted(Some(200.0)))),
            Resolved { seconds: 900, raised: None },
            "3x 200 s is 600 s, below the floor"
        );
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), Some(&adjusted(Some(300.0)))),
            Resolved { seconds: 900, raised: None },
            "3x 300 s is the floor itself, which raises nothing"
        );
    }

    #[test]
    fn an_auto_ceiling_rises_to_3x_the_adjusted_p99_rounded_up() {
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), Some(&adjusted(Some(840.0)))),
            Resolved {
                seconds: 2520,
                raised: Some((840.0, 900))
            }
        );
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), Some(&adjusted(Some(840.1)))),
            Resolved {
                seconds: 2521,
                raised: Some((840.1, 900))
            },
            "3x 840.1 s is 2520.3 s, rounded up to the whole second"
        );
    }

    #[test]
    fn an_auto_ceiling_without_an_adjusted_p99_is_its_floor() {
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), Some(&adjusted(None))),
            Resolved { seconds: 900, raised: None },
            "the wall p99 never raises it"
        );
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), None),
            Resolved { seconds: 900, raised: None },
            "no history"
        );
    }

    #[test]
    fn a_wide_family_estimate_does_not_raise_an_auto_ceiling() {
        let family = |p90: f64| Estimate {
            adjusted_p99: Some(1000.0),
            ..estimate(12.0, 10.0, p90, 8, true)
        };
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), Some(&family(20.1))),
            Resolved { seconds: 900, raised: None },
            "a family p90 above 2x its p50"
        );
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), Some(&family(20.0))).seconds,
            3000,
            "a family p90 of exactly 2x its p50 raises it"
        );
        let exact = Estimate {
            family: false,
            ..family(50.0)
        };
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), Some(&exact)).seconds,
            3000,
            "a wide exact key raises it"
        );
    }

    #[test]
    fn an_auto_ceiling_is_capped_at_the_largest_whole_number() {
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(900), Some(&adjusted(Some(1.0e12)))),
            Resolved {
                seconds: 2_147_483_647,
                raised: Some((1.0e12, 900))
            }
        );
        assert_eq!(
            resolve_ceiling(Ceiling::Auto(2_147_483_647), Some(&adjusted(Some(1.0e12)))),
            Resolved {
                seconds: 2_147_483_647,
                raised: None
            },
            "a floor at the cap is not raised"
        );
    }

    #[test]
    fn a_given_ceiling_is_never_resolved() {
        assert_eq!(
            resolve_ceiling(Ceiling::Given(60), Some(&adjusted(Some(1000.0)))),
            Resolved { seconds: 60, raised: None }
        );
        assert_eq!(resolve_ceiling(Ceiling::Given(60), None), Resolved { seconds: 60, raised: None });
    }

    #[test]
    fn the_raised_ceiling_line_says_minutes_from_90_s() {
        assert_eq!(
            raised_ceiling_line(2520, 840.0, 900, false),
            "gate: ceiling 42 min (3x this command's p99 of 14 min, above the given 15 min)"
        );
        assert_eq!(
            raised_ceiling_line(60, 20.0, 30, false),
            "gate: ceiling 60 s (3x this command's p99 of 20 s, above the given 30 s)"
        );
    }

    #[test]
    fn the_ceiling_hint_says_the_owner_s_line() {
        let usual = |p90: f64| Estimate {
            adjusted_p90: Some(p90),
            ..estimate(p90, p90, p90, 5, false)
        };
        assert_eq!(
            ceiling_hint(600, &usual(840.0)).as_deref(),
            Some("gate: the ceiling 10 min is below this command's load-adjusted p90 of 14 min")
        );
        assert_eq!(
            ceiling_hint(5, &usual(60.0)).as_deref(),
            Some("gate: the ceiling 5 s is below this command's load-adjusted p90 of 60 s")
        );
        assert_eq!(ceiling_hint(840, &usual(840.0)), None, "at the load-adjusted p90");
        assert_eq!(ceiling_hint(900, &usual(840.0)), None, "above the load-adjusted p90");
    }

    /// This command's usual p90 of `p90` seconds.
    fn usual(p90: f64) -> Usual {
        Usual { p90, family: false }
    }

    /// The phase a local command's hang hint names.
    const HERE: &str = "running here";

    #[test]
    fn the_hang_hint_fires_from_3x_the_p90_with_a_whole_k() {
        assert_eq!(hang_hint(2.99 * 840.0, usual(840.0), HERE, 2.99 * 840.0), None, "below 3x the p90");
        assert_eq!(
            hang_hint(2520.0, usual(840.0), HERE, 2520.0).as_deref(),
            Some("gate: still running after 42 min, 3x its usual p90 of 14 min; running here for 42 min")
        );
        assert_eq!(
            hang_hint(3.7 * 840.0, usual(840.0), HERE, 3.7 * 840.0).as_deref(),
            Some("gate: still running after 52 min, 3x its usual p90 of 14 min; running here for 52 min"),
            "3.7x says 3x"
        );
        assert_eq!(
            hang_hint(41.0, usual(10.0), HERE, 41.0).as_deref(),
            Some("gate: still running after 41 s, 4x its usual p90 of 10 s; running here for 41 s")
        );
    }

    #[test]
    fn a_hang_hint_names_its_phase_and_how_long_it_ran() {
        assert_eq!(
            hang_hint(2520.0, usual(840.0), "pulling outputs from mac-1", 125.0).as_deref(),
            Some("gate: still running after 42 min, 3x its usual p90 of 14 min; pulling outputs from mac-1 for 2 min"),
            "the phase's own time, spelt as the run's is"
        );
        assert_eq!(
            hang_hint(41.0, usual(10.0), "running on builder", 41.0).as_deref(),
            Some("gate: still running after 41 s, 4x its usual p90 of 10 s; running on builder for 41 s")
        );
        assert_eq!(
            hang_hint(29.0, usual(10.0), "pulling outputs from mac-1", 29.0),
            None,
            "the phase does not move the 3x threshold"
        );
    }

    #[test]
    fn a_cold_routed_run_gets_no_hang_hint() {
        let compared = routed_usual(Some(usual(840.0)), true);
        assert_eq!(compared, None, "a cold run compares with no p90");
        assert_eq!(
            compared.and_then(|usual| hang_hint(10.0 * 840.0, usual, HERE, 10.0 * 840.0)),
            None,
            "even at 10x the p90"
        );
        assert_eq!(routed_usual(Some(usual(840.0)), false), Some(usual(840.0)), "a warm run keeps its p90");
        assert_eq!(routed_usual(None, false), None, "no history");
    }

    #[test]
    fn no_hang_hint_below_a_1_s_p90() {
        assert_eq!(hang_hint(5.0, usual(0.0), HERE, 5.0), None, "a p90 of 0 s");
        assert_eq!(hang_hint(60.0, usual(0.999), HERE, 60.0), None, "a p90 just under 1 s");
        assert_eq!(
            hang_hint(3.0, usual(1.0), HERE, 3.0).as_deref(),
            Some("gate: still running after 3 s, 3x its usual p90 of 1 s; running here for 3 s"),
            "a p90 of 1 s"
        );
    }

    #[test]
    fn the_lines_say_similar_commands_for_a_family_estimate() {
        assert_eq!(
            raised_ceiling_line(2520, 840.0, 900, true),
            "gate: ceiling 42 min (3x similar commands' p99 of 14 min, above the given 15 min)"
        );
        let family = Estimate {
            adjusted_p90: Some(840.0),
            ..estimate(840.0, 840.0, 840.0, 5, true)
        };
        assert_eq!(
            ceiling_hint(600, &family).as_deref(),
            Some("gate: the ceiling 10 min is below similar commands' load-adjusted p90 of 14 min")
        );
        assert_eq!(
            hang_hint(2520.0, Usual { p90: 840.0, family: true }, HERE, 2520.0).as_deref(),
            Some("gate: still running after 42 min, 3x similar commands' usual p90 of 14 min; running here for 42 min")
        );
    }

    #[test]
    fn a_wide_family_estimate_gives_no_hints() {
        // A family key's p90 above 2x its p50 raises no auto ceiling, and says neither hint.
        let wide = Estimate {
            adjusted_p90: Some(840.0),
            ..estimate(840.0, 400.0, 840.0, 5, true)
        };
        assert_eq!(ceiling_hint(600, &wide), None, "no ceiling hint");
        assert_eq!(Usual::of(&wide), None, "no p90 for the hang hint");

        let at_twice = Estimate {
            adjusted_p90: Some(840.0),
            ..estimate(840.0, 420.0, 840.0, 5, true)
        };
        assert_eq!(
            ceiling_hint(600, &at_twice).as_deref(),
            Some("gate: the ceiling 10 min is below similar commands' load-adjusted p90 of 14 min"),
            "a family p90 of exactly 2x its p50 still hints"
        );
        assert_eq!(Usual::of(&at_twice), Some(Usual { p90: 840.0, family: true }));

        let own = Estimate {
            adjusted_p90: Some(840.0),
            ..estimate(840.0, 400.0, 840.0, 5, false)
        };
        assert_eq!(
            ceiling_hint(600, &own).as_deref(),
            Some("gate: the ceiling 10 min is below this command's load-adjusted p90 of 14 min"),
            "the rule is a family key's only"
        );
        assert_eq!(Usual::of(&own), Some(Usual { p90: 840.0, family: false }));
    }

    #[test]
    fn a_wall_p90_above_the_ceiling_says_nothing_when_the_load_adjusted_p90_is_at_or_below_it() {
        let at = Estimate {
            adjusted_p90: Some(600.0),
            ..estimate(840.0, 840.0, 840.0, 5, false)
        };
        let below = Estimate {
            adjusted_p90: Some(60.0),
            ..estimate(840.0, 840.0, 840.0, 5, false)
        };
        assert_eq!(ceiling_hint(600, &at), None, "at the load-adjusted p90");
        assert_eq!(ceiling_hint(600, &below), None, "below the load-adjusted p90");
    }

    #[test]
    fn an_estimate_without_a_load_adjusted_p90_says_no_ceiling_hint() {
        let high_wall = estimate(840.0, 840.0, 840.0, 5, false);
        assert_eq!(high_wall.p90, 840.0, "a wall p90 well above the ceiling");
        assert_eq!(
            ceiling_hint(600, &high_wall),
            None,
            "no load-adjusted p90 to compare, however high the wall p90"
        );
    }

    /// A journal lock no other gate holds.
    struct FreeLock;

    impl JournalLock for FreeLock {
        type Guard<'a> = ();

        fn name(&self) -> &'static str {
            "test-journal"
        }

        fn acquire(&self, _timeout: Duration) -> Result<Held<()>, LockError> {
            Ok(Held { guard: (), abandoned: false })
        }
    }

    /// A state root of its own in the unit tests' temp folder, holding a journal of one passing release in each of two
    /// folders that exist, its own `first` and `second`; removed when dropped.
    struct Root(PathBuf);

    impl Root {
        #[expect(clippy::expect_used, reason = "a test that cannot build its state root must fail loudly")]
        fn new(test: &str) -> Root {
            let root = crate::test_support::temp_dir().join(format!("gate-durations-{}-{test}", std::process::id()));
            // A folder left by an earlier run of the same pid is cleared first.
            let _ = fs::remove_dir_all(&root);
            let lines: Vec<String> = ["first", "second"]
                .iter()
                .zip(1..)
                .map(|(name, pid)| {
                    let folder = root.join(name);
                    fs::create_dir_all(&folder).expect("the folder can be made");
                    let release = json!({
                        "event": "release", "time": Timestamp::now().to_string(), "pid": pid, "kind": "light",
                        "cwd": folder, "command": "cargo build", "wallSeconds": 1.0, "verdict": "passed",
                    });
                    format!("{release}\n")
                })
                .collect();
            fs::write(root.join("events.jsonl"), lines.concat()).expect("the journal can be written");
            Root(root)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            // A folder left in the unit tests' temp folder harms no later test, and a panic in drop would abort the run.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The durations of a run in `root` whose read of the store asked for a rebuild.
    fn rebuilding(root: &Path) -> RunDurations {
        RunDurations {
            root: root.to_path_buf(),
            folder: place_by_shape("C:/work"),
            command: "cargo build".to_owned(),
            temp_root: None,
            critical: false,
            owner: RebuildOwner {
                pid: std::process::id(),
                start_ms: 1,
            },
            git: GitLimits {
                limit: Duration::ZERO,
                drain: Duration::ZERO,
            },
            rebuild: true,
            store: StoreSeen::Unread,
            said: Said::default(),
        }
    }

    #[test]
    fn a_ctrl_c_during_the_rebuild_places_no_more_folders_and_swaps_nothing() {
        let root = Root::new("interrupted");
        let raised = Cell::new(false);
        let asked = Cell::new(0);
        // The Ctrl+C comes while git places the first folder.
        let place = |_: &Path| -> Option<KeyPlace> {
            asked.set(asked.get() + 1);
            raised.set(true);
            None
        };
        let mut console = Console::new();

        rebuilding(&root.0).rebuild_unless(&FreeLock, || raised.get(), place, &mut console);

        assert_eq!(asked.get(), 1, "no folder is placed once the Ctrl+C came");
        assert!(!durations_path(&root.0).exists(), "nothing is swapped in");
        assert!(!rebuilding_path(&root.0).exists(), "the claim is given up");
        assert_eq!(console.take_notes(), Vec::<String>::new(), "an interrupted rebuild says nothing");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot write its store must fail loudly")]
    fn a_store_made_current_since_the_runs_read_is_not_rebuilt() {
        let root = Root::new("current-since");
        write_store(&root.0, &DurationStore::new()).expect("another gate's store can be written");
        let asked = Cell::new(0);
        let place = |_: &Path| -> Option<KeyPlace> {
            asked.set(asked.get() + 1);
            None
        };
        let mut console = Console::new();

        rebuilding(&root.0).rebuild_unless(&FreeLock, || false, place, &mut console);

        assert_eq!(asked.get(), 0, "the journal is not read again");
        assert!(!rebuilding_path(&root.0).exists(), "no rebuild is claimed");
        assert_eq!(console.take_notes(), Vec::<String>::new(), "nothing is said");
    }
}
