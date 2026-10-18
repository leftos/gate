//! The check of this PC: the worktree root the gate's working folder is in, the repo's `.gate/toolchain.toml` and
//! `.gate/siblings.toml` read from disk through the guarded `.gate` read routing uses ([`route::read_gate_file`]), so
//! an uncommitted edit counts, the root `global.json`'s SDK pin, the manifest's tools probed as a builder probes them
//! ([`probe::probe`]) while this PC's SDK list is read, both with the worktree's own folders dropped from `PATH`, each
//! sibling looked for by routing's rule, and the warm-up: `.gate/warmup.toml`, `remote.json`'s consent and the work
//! folder's warm-up record, judged as the route judges it. It runs only version commands and installs nothing.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use gate_args::{CheckRequest, CheckSeams};
use gate_router::warm::{FileRead, Judgement, Record, RecordRead, RecordState, Situation, WarmupFile, judge, lock_problem_text, record_problem_text};
use gate_router::{Place, SiblingFolder, repo_name, sibling_root, work_folder_key};
use gate_state::v1;

use crate::answer::FAILED_STATUS;
use crate::boxside::probe;
use crate::check::report::{self, Checked, Found, Problem, Sdk, Sibling, Tool, Warmup};
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::toolchain::{self, Manifest, Pin, Reported, ToolNeed};
use crate::remote::{git, local_sdk_listing, outside_worktree, parity, siblings};
use crate::resolve::{self, Environment};
use crate::route::{self, PLACE_ARGS, read_place};
use crate::warm;
use gate_desktop::version::build_version;

/// The manifest's name in `.gate`.
const TOOLCHAIN: &str = "toolchain.toml";
/// The siblings file's name in `.gate`.
const SIBLINGS: &str = "siblings.toml";
/// Why an SDK pin is skipped when the root's `global.json` holds none that reads.
const NO_READABLE_PIN: &str = "global.json has no readable sdk pin";
/// Why a `.gate` that is not a folder leaves the manifest unreadable.
const GATE_NOT_A_FOLDER: &str = ".gate is not a folder";
/// How the siblings file's reader starts the problem of one entry of its array: `siblings[<index>] ...`.
const ENTRY_PROBLEM: &str = "siblings[";
/// Why a sibling named like the worktree's own folder is refused, as routing words it.
const OWN_FOLDER: &str = "is the worktree's own folder";

/// Checks this PC for the repo the working folder is in and says what it found through `console`; returns the exit
/// status: 0 when nothing is missing, 1 when something is or the check could not run.
pub(crate) fn check(request: &CheckRequest, console: &mut Console) -> u32 {
    let place = match placed() {
        Ok(Some(place)) => place,
        Ok(None) => {
            console.err("check: not in a git repository");
            return u32::from(FAILED_STATUS);
        }
        Err(why) => {
            console.err(&format!("check: {why}"));
            return u32::from(FAILED_STATUS);
        }
    };
    super::say(&report::said(report::HERE, &checked(&place, probe_limit(&request.seams))), console)
}

/// How long the tool probes may take together: `--test-probe-limit-ms` when given, else [`probe::PROBE_LIMIT`].
fn probe_limit(seams: &CheckSeams) -> Duration {
    seams.probe_limit_ms.map_or(probe::PROBE_LIMIT, Duration::from_millis)
}

/// The place in its git worktree of the gate's working folder; `None` when it is in none; else why git could not
/// tell.
pub(super) fn placed() -> Result<Option<Place>, String> {
    let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
    let git = git::Git::find(&folder)?;
    read_place(&git, &git::Call::plain(&folder, &PLACE_ARGS, git::GIT_LIMIT))
}

/// What the repo's own files in its worktree root declare, as both checks read them from disk: the manifest, its
/// tools and workloads, the root `global.json`'s SDK pin, and the problems of those files, in the order found.
pub(super) struct Declared {
    /// Whether the repo has a `.gate/toolchain.toml`, readable or not.
    pub declares_toolchain: bool,
    /// The manifest as it reads, empty when there is none or it cannot be read; its problems are in `problems`.
    pub manifest: Manifest,
    /// The exact Godot versions the manifest requires, as [`toolchain::godot_required`] reads them: empty for any
    /// version, `None` when it names no godot.
    pub godot: Option<Vec<Pin>>,
    pub pin: Option<parity::Pin>,
    pub problems: Vec<Problem>,
    /// Whether the manifest is there but cannot be read whole.
    pub toolchain_unreadable: bool,
    /// Whether the `global.json` is there but its pin cannot be read.
    pub pin_unreadable: bool,
}

impl Declared {
    /// The files that are there but cannot be read whole, in the order the summary names them: the manifest, its
    /// siblings when `siblings_unreadable`, the warm-up file when `warmup_unreadable`, the root `global.json`.
    pub(super) fn unreadable(&self, siblings_unreadable: bool, warmup_unreadable: bool) -> Vec<&'static str> {
        [
            self.toolchain_unreadable.then_some(report::TOOLCHAIN),
            siblings_unreadable.then_some(report::SIBLINGS),
            warmup_unreadable.then_some(report::WARMUP),
            self.pin_unreadable.then_some(report::GLOBAL_JSON),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// What the manifest and the `global.json` of the worktree `root` declare ([`read_manifest`], [`read_pin`]).
pub(super) fn declared(root: &Path) -> Declared {
    let mut problems = Vec::new();
    let manifest = read_manifest(root);
    let declares_toolchain = !matches!(manifest, Ok(None));
    let toolchain_unreadable = manifest.is_err();
    let mut manifest = match manifest {
        Ok(Some(manifest)) => manifest,
        Ok(None) => Manifest::default(),
        Err(why) => {
            problems.push(Problem {
                file: report::TOOLCHAIN,
                text: why,
            });
            Manifest::default()
        }
    };
    problems.extend(std::mem::take(&mut manifest.problems).into_iter().map(|text| Problem {
        file: report::TOOLCHAIN,
        text,
    }));
    let godot = toolchain::godot_required(Some(&Ok(manifest.clone())), &[]);
    let (pin, pin_problem) = read_pin(root);
    let pin_unreadable = pin_problem.is_some();
    if let Some(text) = pin_problem {
        problems.push(Problem {
            file: report::GLOBAL_JSON,
            text,
        });
    }
    Declared {
        declares_toolchain,
        manifest,
        godot,
        pin,
        problems,
        toolchain_unreadable,
        pin_unreadable,
    }
}

/// What a worktree root's `.gate/siblings.toml` declares, as both checks read it ([`read_siblings_file`]).
#[derive(Debug, Default)]
pub(super) struct SiblingsFile {
    /// The sibling names that read, in the file's order.
    pub names: Vec<String>,
    /// Whether it declares a sibling: an entry of its `siblings` array names one, as it reads or as one it skips as
    /// bad, or the file is there but cannot be read, so it may. A problem of the file that is no entry's, such as a
    /// stray key or a `siblings` that is not an array, declares none.
    pub declares: bool,
    /// Whether the file is there but cannot be read whole.
    pub unreadable: bool,
}

/// What the worktree `root`'s `.gate/siblings.toml` declares, read through routing's guarded read
/// ([`route::read_gate_file`]); every problem of the file is added to `problems`.
pub(super) fn read_siblings_file(root: &Path, problems: &mut Vec<Problem>) -> SiblingsFile {
    let declared = route::read_gate_file(root, SIBLINGS).and_then(|bytes| bytes.map(|bytes| siblings::read_bytes(&bytes)).transpose());
    match declared {
        Ok(None) => SiblingsFile::default(),
        Err(why) => {
            problems.push(Problem {
                file: report::SIBLINGS,
                text: why,
            });
            SiblingsFile {
                names: Vec::new(),
                declares: true,
                unreadable: true,
            }
        }
        Ok(Some(declared)) => {
            let declares = !declared.names.is_empty() || declared.problems.iter().any(|problem| problem.starts_with(ENTRY_PROBLEM));
            problems.extend(declared.problems.into_iter().map(|text| Problem {
                file: report::SIBLINGS,
                text,
            }));
            SiblingsFile {
                names: declared.names,
                declares,
                unreadable: false,
            }
        }
    }
}

/// What this PC has of what the repo at `place` needs, its tools probed within `limit`.
fn checked(place: &Place, limit: Duration) -> Checked {
    let root = Path::new(&place.root);
    let mut declared = declared(root);
    let needs = std::mem::take(&mut declared.manifest.tools);
    // The SDK list is read while the tools are probed, so the check waits for the longer of the two bounds.
    let (tools, sdk) = thread::scope(|scope| {
        let sdk = scope.spawn(|| sdk_of(declared.pin.as_ref(), || sdk_list(root)));
        let tools = probed(needs, limit, root);
        let sdk = sdk
            .join()
            .unwrap_or_else(|panic| Some(Sdk::Skipped(format!("cannot read this PC's SDK list: {}", probe::panic_reason(&*panic)))));
        (tools, sdk)
    });
    let repo = repo_name(&place.checkout);
    let hooks_problem = hooks_problem(&repo);
    let (allowed, consent_problem) = match allowed_siblings(&repo) {
        None => (None, None),
        Some(Ok(names)) => (Some(names), None),
        Some(Err(problem)) => (Some(Vec::new()), Some(problem)),
    };
    let (siblings, siblings_unreadable) = read_siblings(&place.root, allowed.as_deref(), &mut declared.problems);
    let consent_problem = consent_problem.filter(|_| !siblings.is_empty());
    let warmed = warmup(&place.root, WarmSources::from_process(&repo), None, &mut declared.problems);
    Checked {
        repo,
        root: place.root.clone(),
        declares_toolchain: declared.declares_toolchain,
        unreadable: declared.unreadable(siblings_unreadable, warmed.unreadable),
        problems: declared.problems,
        tools,
        godot: declared.godot,
        sdk,
        workloads: declared.manifest.workloads,
        consent_problem,
        hooks_problem,
        siblings,
        created: Vec::new(),
        siblings_skipped: false,
        warmup: warmed.items,
    }
}

/// How `remote.json` answers the repo's warm-up consent.
#[derive(Debug)]
pub(super) enum Consent {
    /// It consents, and routes the repo's runs to `host`.
    Given { host: String },
    /// No routing file that can be used, one whose `repos` does not list the repo, or one whose `warmup` does not
    /// name it.
    Refused,
    /// The repo is listed, but the `warmup` value cannot be used, for this reason.
    Problem(String),
}

/// What the warm-up's items are judged from besides the worktree: `remote.json`'s consent, and the state root that
/// holds the work folder's record and lock, or why it cannot be found.
pub(super) struct WarmSources {
    pub consent: Consent,
    pub state: Result<PathBuf, String>,
}

impl WarmSources {
    /// The sources of `repo` on this PC: its routing file's consent, and the state root found with no `--state-dir`
    /// (`GATE_STATE_DIR`, then `%LOCALAPPDATA%\gate\v1`).
    pub(super) fn from_process(repo: &str) -> WarmSources {
        let state = resolve::root(None, &Environment::from_process())
            .map(|state| state.path)
            .map_err(|why| why.strip_prefix("gate: ").map_or_else(|| why.clone(), str::to_owned));
        WarmSources {
            consent: warmup_consent(repo),
            state,
        }
    }
}

/// How this PC's routing file answers `repo`'s warm-up consent: a repo its `repos` does not list has none, whatever
/// the `warmup` value, as the siblings consent is asked only of a listed repo and the route never weighs a warm-up for
/// one it does not route.
fn warmup_consent(repo: &str) -> Consent {
    let Some(path) = route::remote_json_path(std::env::var_os("LOCALAPPDATA").as_deref()) else {
        return Consent::Refused;
    };
    let Ok(Some(config)) = route::read_remote_json(&path) else {
        return Consent::Refused;
    };
    if config.patterns(repo).is_none() {
        return Consent::Refused;
    }
    match config.warmup_consented(repo) {
        Err(problem) => Consent::Problem(problem.to_owned()),
        Ok(true) => Consent::Given {
            host: config.host().to_owned(),
        },
        Ok(false) => Consent::Refused,
    }
}

/// What the warm-up of a worktree root came to: its items, the hash of its warm-up file when the repo consents and the
/// file parsed, and whether the file is there but cannot be read whole.
pub(super) struct Warmed {
    pub items: Vec<Warmup>,
    pub hash: Option<String>,
    pub unreadable: bool,
}

/// The warm-up's items of the worktree root `root`, from `sources`; each problem of the file is added to `problems`,
/// consent or not, and a file that cannot be read whole is said as such whatever the consent. A repo with neither the
/// file nor consent, and a `.gate` that is not a folder, have no item. The record is judged for `remote.json`'s host,
/// or for the builder `builder` a builder check checks, and then only a record that names it, or cannot be read, is
/// said.
pub(super) fn warmup(root: &str, sources: WarmSources, builder: Option<&str>, problems: &mut Vec<Problem>) -> Warmed {
    let folder = Path::new(root);
    if route::gate_is_not_a_folder(folder) {
        return Warmed {
            items: Vec::new(),
            hash: None,
            unreadable: false,
        };
    }
    let file = warm::read_warmup_file_checked(folder);
    let unreadable = file.as_ref().is_err_and(|found| found.unreadable);
    if let Err(found) = &file {
        problems.extend(found.problems.iter().map(|text| Problem {
            file: report::WARMUP,
            text: text.clone(),
        }));
    }
    let (items, hash) = warmup_items(root, file.map_err(drop), sources, builder);
    Warmed { items, hash, unreadable }
}

/// The items of [`warmup`], from the warm-up file `file` as it read (`Err` when it did not), and the file's hash when
/// the repo consents and it parsed.
fn warmup_items(
    root: &str,
    file: Result<Option<(WarmupFile, String)>, ()>,
    sources: WarmSources,
    builder: Option<&str>,
) -> (Vec<Warmup>, Option<String>) {
    let declared = !matches!(file, Ok(None));
    let host = match sources.consent {
        Consent::Given { host } => host,
        Consent::Refused => return (declared.then_some(Warmup::Unconsented).into_iter().collect(), None),
        Consent::Problem(why) => return (declared.then_some(Warmup::ConsentProblem(why)).into_iter().collect(), None),
    };
    let (file, hash) = match file {
        Ok(Some(parsed)) => parsed,
        Ok(None) => return (vec![Warmup::NoFile], None),
        Err(()) => return (vec![Warmup::FileProblems], None),
    };
    let state = match sources.state {
        Ok(state) => state,
        Err(why) => return (vec![Warmup::NoStateRoot(why)], Some(hash)),
    };
    let file = FileRead::Parsed { file, hash: hash.clone() };
    let item = recorded(root, &state, &file, builder.unwrap_or(&host), builder.is_some());
    (item.into_iter().collect(), Some(hash))
}

/// The item of the warm-up record of the worktree root `root`'s work folder, in the state root `state`, for the
/// warm-up file `file` on `host`: the lock probed first, never taken, then the record read, in the route's order. A lock
/// that cannot be probed is said whatever the record, as the route gives it precedence. Else `None` when
/// `only_its_own` and the record is absent or names another host; one that cannot be read names none, so it is said,
/// as every routed run stays here for it.
fn recorded(root: &str, state: &Path, file: &FileRead, host: &str, only_its_own: bool) -> Option<Warmup> {
    let key = work_folder_key(root);
    let lock = v1::warm_lock_path(state, &key);
    let held = match warm::lock_held(&lock) {
        Ok(held) => held,
        Err(error) => return Some(Warmup::Unreadable(lock_problem_text(&lock.to_string_lossy(), &error.to_string()))),
    };
    let record = warm::read_warm_record(&v1::warm_record_path(state, &key));
    let names_another = match &record {
        RecordRead::Absent => true,
        RecordRead::Parsed(found) => found.host != host,
        RecordRead::Unreadable { .. } => false,
    };
    if only_its_own && names_another {
        return None;
    }
    Some(record_item(&record, file, held, host, SystemTime::now()))
}

/// The item of the warm-up record `record` for the warm-up file `file` that parsed, its lock held or not, on `host` at
/// `now`, as the route's judge ([`judge`]) weighs them: warm, warming, failed within its backoff, unreadable, or else
/// not warm, a `warming` record whose lock is free said as one that ended without a verdict.
pub(super) fn record_item(record: &RecordRead, file: &FileRead, lock_held: bool, host: &str, now: SystemTime) -> Warmup {
    let gate = build_version();
    let situation = Situation {
        consented: true,
        file,
        record,
        lock_held,
        host,
        gate: &gate,
        now,
    };
    let not_warm = |stale: bool| Warmup::NotWarm {
        host: host.to_owned(),
        stale,
    };
    match (judge(&situation), record) {
        (Judgement::Ready, _) => Warmup::Warm { host: host.to_owned() },
        (Judgement::Warming { host }, _) => Warmup::Warming { host },
        (Judgement::Problem(_), RecordRead::Unreadable { path, error }) => Warmup::Unreadable(record_problem_text(path, error)),
        (
            Judgement::Start,
            RecordRead::Parsed(Record {
                state: RecordState::Warming, ..
            }),
        ) => not_warm(true),
        (
            Judgement::Failed { reason, .. },
            RecordRead::Parsed(
                found @ Record {
                    state: RecordState::Failed { class, .. },
                    ..
                },
            ),
        ) => {
            let Ok(age) = now.duration_since(found.at) else {
                return not_warm(false);
            };
            Warmup::Failed {
                host: found.host.clone(),
                minutes_ago: minutes_ago(age),
                reason,
                minutes_left: minutes_up(class.backoff().saturating_sub(age)),
            }
        }
        // Start with any other record is not warm. Route (no consent or no file), and a failure or a problem without a
        // record of its kind, never come with a consented file that parsed. Each judgement is named, so a new one is a
        // compile error.
        (Judgement::Start | Judgement::Route | Judgement::Failed { .. } | Judgement::Problem(_), _) => not_warm(false),
    }
}

/// An age in whole minutes, rounded down and at least 1, so an age under a minute, zero included, is 1.
fn minutes_ago(age: Duration) -> u64 {
    (age.as_secs() / 60).max(1)
}

/// `duration` in whole minutes, rounded up.
fn minutes_up(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos().div_ceil(60_000_000_000)).unwrap_or(u64::MAX)
}

/// The manifest `<root>\.gate\toolchain.toml` holds as it stands on disk; `None` when there is none; else why it
/// cannot be read whole, a `.gate` that is not a folder among the reasons.
fn read_manifest(root: &Path) -> Result<Option<Manifest>, String> {
    match route::read_gate_file(root, TOOLCHAIN)? {
        Some(bytes) => toolchain::read_bytes(&bytes).map(Some),
        None if route::gate_is_not_a_folder(root) => Err(GATE_NOT_A_FOLDER.to_owned()),
        None => Ok(None),
    }
}

/// Each of `needs` with what its probe found within `limit`, in their order; an unknown tool runs nothing. The tools
/// are looked for as this PC's SDK list looks for dotnet: along `PATH` with the folders inside the worktree `root`
/// dropped, and one found that still lies inside it refused.
fn probed(needs: Vec<ToolNeed>, limit: Duration, root: &Path) -> Vec<Tool> {
    if needs.is_empty() {
        return Vec::new();
    }
    let names: Vec<String> = needs.iter().map(|need| need.name.clone()).collect();
    probe::probe(&names, limit, outside_worktree(Variables::from_process(), root), &[root])
        .into_iter()
        .zip(needs)
        .map(|((_, probed), need)| Tool {
            need,
            reported: Reported::from(&probed),
        })
        .collect()
}

/// The SDK pin of `<root>\global.json`, read without following a link and up to 64 KiB
/// ([`route::read_unfollowed`]), and why its pin cannot be read when the file names one that does not: the pin is
/// `None` when there is no such file or it names no `sdk.version` at all, which pins no SDK rather than failing.
fn read_pin(root: &Path) -> (Option<parity::Pin>, Option<String>) {
    let bytes = match route::read_unfollowed(&root.join("global.json")) {
        Ok(None) => return (None, None),
        Err(why) => return (Some(parity::Pin::unreadable()), Some(why)),
        Ok(Some(bytes)) => bytes,
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return (Some(parity::Pin::unreadable()), Some("is not UTF-8".to_owned()));
    };
    match parity::pin_in_text(text) {
        parity::PinRead::Pinned(pin) => (Some(pin), None),
        parity::PinRead::NoPin => (None, None),
        parity::PinRead::Unreadable(why) => (Some(parity::Pin::unreadable()), Some(why)),
    }
}

/// The check of `pin` against the SDKs this PC's list `sdks` gives ([`sdk_listed`]), one that cannot be read skipped
/// as `cannot read this PC's SDK list: <why>`.
fn sdk_of(pin: Option<&parity::Pin>, sdks: impl FnOnce() -> Result<Vec<String>, String>) -> Option<Sdk> {
    sdk_listed(pin, || sdks().map_err(|why| format!("cannot read this PC's SDK list: {why}")))
}

/// The check of `pin` against the SDKs `sdks` lists, read only for a pin that names a version, a list that cannot be
/// read skipped with the reason `sdks` gives; `None` with no pin.
pub(super) fn sdk_listed(pin: Option<&parity::Pin>, sdks: impl FnOnce() -> Result<Vec<String>, String>) -> Option<Sdk> {
    let pin = pin?;
    let Some(version) = pin.version.clone() else {
        return Some(Sdk::Skipped(NO_READABLE_PIN.to_owned()));
    };
    let have = match sdks() {
        Ok(have) => have,
        Err(why) => return Some(Sdk::Skipped(why)),
    };
    // With no SDK to choose from, a pin that names a version always needs it: its words are parity's need text.
    let need = parity::need(pin, &[], || 0).into_iter().next().map_or_else(String::new, |need| need.text);
    let served = parity::need(pin, &have, || 0).is_empty();
    Some(Sdk::Checked { version, need, have, served })
}

/// The SDKs this PC's `dotnet --list-sdks` lists, run with the folders inside the worktree `root` dropped from
/// `PATH`; none when no dotnet is found; else why the list cannot be read.
fn sdk_list(root: &Path) -> Result<Vec<String>, String> {
    let listing = local_sdk_listing(Variables::from_process(), &[root], git::DOTNET_LIMIT, None)?;
    Ok(parity::sdk_versions(&listing).into_iter().map(str::to_owned).collect())
}

/// Each sibling `<root>\.gate\siblings.toml` declares, checked in routing's order: one `allowed` does not name is not
/// allowed (all are when `allowed` is `None`), one named like the root's folder (ASCII letter case aside) is refused,
/// and only the rest are looked for beside the worktree root `root`; and whether that file is there but cannot be read
/// whole. Every problem of the file is added to `problems` ([`read_siblings_file`]), and the names that read are still
/// checked.
fn read_siblings(root: &str, allowed: Option<&[String]>, problems: &mut Vec<Problem>) -> (Vec<Sibling>, bool) {
    let file = read_siblings_file(Path::new(root), problems);
    let leaf = root_leaf(root);
    let siblings = file
        .names
        .into_iter()
        .map(|name| {
            if !allowed.is_none_or(|allowed| allowed.contains(&name)) {
                Sibling {
                    path: sibling_root(root, &name),
                    name,
                    found: Found::NotAllowed,
                }
            } else if name.eq_ignore_ascii_case(leaf) {
                Sibling {
                    path: sibling_root(root, &name),
                    name,
                    found: Found::Folder(Ok(SiblingFolder::Refused(OWN_FOLDER.to_owned()))),
                }
            } else {
                sibling(root, name)
            }
        })
        .collect();
    (siblings, file.unreadable)
}

/// The worktree root `root`'s folder name: its last segment, separators at its end ignored.
fn root_leaf(root: &str) -> &str {
    let trimmed = root.trim_end_matches(['\\', '/']);
    trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed)
}

/// The sibling `name` of the worktree root `root`: where it must be, and what the folder check routing makes found
/// there ([`route::sibling_folder`]).
fn sibling(root: &str, name: String) -> Sibling {
    let path = sibling_root(root, &name);
    let folder = route::sibling_folder(&path);
    Sibling {
        name,
        path,
        found: Found::Folder(folder),
    }
}

/// The sibling names this PC's routing file allows the repo `repo`, as routing reads them
/// ([`gate_router::Config::allowed_siblings`], an exact match), or why its `siblings` value cannot be used, which
/// allows none. `None` when routing would never route the repo, so asks no consent: no routing file, one routing
/// ignores, or one whose `repos` does not list it.
/// Why this PC's routing file's `hooks` value cannot be used, if it cannot: the reason the router's `hooks_allowed`
/// gives, whatever the repo. A routing file that cannot be read at all is no such problem, as its own line says it.
fn hooks_problem(repo: &str) -> Option<String> {
    let path = route::remote_json_path(std::env::var_os("LOCALAPPDATA").as_deref())?;
    let Ok(Some(config)) = route::read_remote_json(&path) else {
        return None;
    };
    config.hooks_allowed(repo).err().map(str::to_owned)
}

fn allowed_siblings(repo: &str) -> Option<Result<Vec<String>, String>> {
    let path = route::remote_json_path(std::env::var_os("LOCALAPPDATA").as_deref())?;
    let config = route::read_remote_json(&path).ok().flatten()?;
    config.patterns(repo)?;
    Some(config.allowed_siblings(repo).map(<[String]>::to_vec).map_err(str::to_owned))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use gate_router::SiblingFolder;

    use super::{Declared, read_manifest, read_pin, read_siblings, read_siblings_file, sdk_of, sibling};
    use crate::check::report::{Found, Sdk, Sibling};
    use crate::remote::parity::{Pin, pin_of};
    use crate::remote::toolchain::{Manifest, ToolNeed, Version};
    use crate::test_support::junction;

    /// A folder of its own under the unit tests' temp folder, removed when the test ends, failed or not.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let path = crate::test_support::temp_dir().join(format!("gate-check-{name}-{}", std::process::id()));
            if path.exists() {
                fs::remove_dir_all(&path).expect("a folder left by an earlier run can be removed");
            }
            fs::create_dir_all(&path).expect("the test folder can be made");
            Folder(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn text(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for Folder {
        #[expect(clippy::print_stderr, reason = "a case that cannot clean up says so without failing the test it ends")]
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                eprintln!("cannot remove the test folder {}: {error}", self.0.display());
            }
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the file's folder can be made");
        fs::write(path, text).expect("the file can be written");
    }

    #[test]
    fn the_manifest_is_read_from_the_file_on_disk_not_from_the_commit() {
        let root = Folder::new("manifest-on-disk");
        write(&root.path().join(".gate").join("toolchain.toml"), "[tools]\nnode = \"24\"\n");

        let manifest = read_manifest(root.path()).expect("the manifest reads").expect("there is a manifest");

        assert_eq!(
            manifest.tools,
            [ToolNeed {
                name: "node".to_owned(),
                min: Version::parse("24"),
                known: true,
            }]
        );
    }

    #[test]
    fn the_unreadable_files_are_named_in_the_summary_s_order() {
        let declared = Declared {
            declares_toolchain: true,
            manifest: Manifest::default(),
            godot: None,
            pin: None,
            problems: Vec::new(),
            toolchain_unreadable: true,
            pin_unreadable: true,
        };

        assert_eq!(
            declared.unreadable(true, true),
            [".gate/toolchain.toml", ".gate/siblings.toml", ".gate/warmup.toml", "global.json"]
        );
    }

    #[test]
    fn a_missing_manifest_declares_nothing() {
        let root = Folder::new("manifest-missing");
        assert_eq!(read_manifest(root.path()), Ok(None), "no .gate");
        fs::create_dir_all(root.path().join(".gate")).expect("the .gate folder can be made");
        assert_eq!(read_manifest(root.path()), Ok(None), "a .gate with no manifest");
    }

    #[test]
    fn a_gate_that_is_not_a_folder_leaves_the_manifest_unreadable() {
        let root = Folder::new("manifest-gate-file");
        write(&root.path().join(".gate"), "not a folder\n");

        assert_eq!(read_manifest(root.path()), Err(".gate is not a folder".to_owned()));
    }

    #[test]
    fn a_linked_or_oversized_manifest_is_refused() {
        let root = Folder::new("manifest-refused");
        let elsewhere = Folder::new("manifest-refused-elsewhere");
        let file = root.path().join(".gate").join("toolchain.toml");
        write(&file, &format!("[tools]\n#{}\n", "x".repeat(64 * 1024)));
        assert_eq!(read_manifest(root.path()), Err("is larger than 64 KiB".to_owned()));
        write(&file, "[tools\n");
        let why = read_manifest(root.path()).expect_err("a manifest that is no TOML is unreadable");
        assert!(why.starts_with("line 1, column "), "{why}");

        write(&elsewhere.path().join("toolchain.toml"), "[tools]\nnode = \"24\"\n");
        fs::remove_dir_all(root.path().join(".gate")).expect("the real .gate can be removed");
        junction(&root.path().join(".gate"), elsewhere.path());
        assert_eq!(read_manifest(root.path()), Err(".gate is a link or a junction".to_owned()));
        fs::remove_dir(root.path().join(".gate")).expect("the junction can be removed");
    }

    #[test]
    fn only_a_siblings_file_that_names_one_or_cannot_be_read_declares_siblings() {
        let root = Folder::new("declares-siblings");
        let file = root.path().join(".gate").join("siblings.toml");
        let declares_siblings = |root: &Path| read_siblings_file(root, &mut Vec::new()).declares;
        assert!(!declares_siblings(root.path()), "no file");
        write(&file, "siblings = []\n");
        assert!(!declares_siblings(root.path()), "an empty list");
        write(&file, "siblings = ['docs']\n");
        assert!(declares_siblings(root.path()), "a name");
        write(&file, "siblings = ['..']\n");
        assert!(declares_siblings(root.path()), "a name skipped as bad");
        write(&file, "siblings = []\nstray = 1\n");
        assert!(!declares_siblings(root.path()), "an empty list beside a stray key");
        write(&file, "other = ['docs']\n");
        assert!(!declares_siblings(root.path()), "no siblings key");
        write(&file, "siblings = 'docs'\n");
        assert!(!declares_siblings(root.path()), "a siblings that is not an array");
        write(&file, "siblings = [\n");
        assert!(declares_siblings(root.path()), "a file that cannot be read may name one");
    }

    #[test]
    fn a_global_json_pin_is_checked_against_the_sdk_list() {
        let pin = pin_of(Some(r#"{"sdk": {"version": "10.0.401", "rollForward": "latestFeature"}}"#));
        let checked = sdk_of(Some(&pin), || Ok(vec!["8.0.404".to_owned(), "10.0.500".to_owned()]));
        let Some(Sdk::Checked { version, need, have, served }) = checked else {
            unreachable!("a pin with a version is checked, got {checked:?}");
        };
        assert_eq!(version, "10.0.401");
        assert_eq!(need, ".NET SDK 10.0.401 (rollForward latestFeature)");
        assert_eq!(have, ["8.0.404", "10.0.500"]);
        assert!(served, "a later feature band serves latestFeature");

        assert!(
            sdk_of(None, || unreachable!("no pin reads no SDK list")).is_none(),
            "no global.json, no line"
        );
        let unreadable = sdk_of(Some(&pin_of(Some("{}"))), || unreachable!("a pin with no version reads no SDK list"));
        assert!(
            matches!(&unreadable, Some(Sdk::Skipped(why)) if why == "global.json has no readable sdk pin"),
            "{unreadable:?}"
        );
        let failed = sdk_of(Some(&pin), || Err("dotnet failed".to_owned()));
        assert!(
            matches!(&failed, Some(Sdk::Skipped(why)) if why == "cannot read this PC's SDK list: dotnet failed"),
            "{failed:?}"
        );
    }

    #[test]
    fn a_global_json_naming_no_sdk_version_pins_nothing() {
        let root = Folder::new("global-json-no-version");
        let global = root.path().join("global.json");
        assert_eq!(read_pin(root.path()), (None, None), "no global.json");
        for text in [
            "{\"msbuild-sdks\": {\"Godot.NET.Sdk\": \"4.5.0\"}}\n",
            "{}\n",
            "{\"sdk\": {\"rollForward\": \"latestFeature\"}}\n",
        ] {
            write(&global, text);
            assert_eq!(
                read_pin(root.path()),
                (None, None),
                "{text:?} names no SDK version, so there is no SDK line"
            );
        }
        for text in ["{\"sdk\": \n", "{\"sdk\": {\"version\": \"10.0\"}}\n", "{\"sdk\": {\"version\": 10}}\n"] {
            write(&global, text);
            let (pin, why) = read_pin(root.path());
            assert!(
                matches!(
                    &pin,
                    Some(Pin {
                        found: true,
                        version: None,
                        ..
                    })
                ),
                "{text:?} holds no readable pin: {pin:?}"
            );
            assert!(why.is_some(), "{text:?} says why the pin cannot be read: {why:?}");
        }
    }

    #[test]
    fn a_global_json_that_is_not_utf8_says_so() {
        let root = Folder::new("global-json-not-utf8");
        fs::write(root.path().join("global.json"), [0xff, 0xfe, b'{', b'}']).expect("the file can be written");

        let (pin, why) = read_pin(root.path());

        assert_eq!(pin, Some(Pin::unreadable()), "a file that is not UTF-8 holds no readable pin");
        assert_eq!(why, Some("is not UTF-8".to_owned()));
    }

    #[test]
    fn an_oversized_or_linked_global_json_holds_no_readable_pin() {
        let root = Folder::new("global-json-refused");
        let elsewhere = Folder::new("global-json-refused-elsewhere");
        let global = root.path().join("global.json");
        let pinned = "{\"sdk\": {\"version\": \"10.0.401\"}}";
        write(&global, &format!("{pinned}{}\n", " ".repeat(64 * 1024)));
        assert_eq!(read_pin(root.path()).0, Some(Pin::unreadable()), "over 64 KiB");
        assert_eq!(read_pin(root.path()).1, Some("is larger than 64 KiB".to_owned()), "and says why");
        write(&global, &format!("{pinned}{}\n", " ".repeat(64 * 1024 - pinned.len() - 1)));
        assert_eq!(
            read_pin(root.path()).0.and_then(|pin| pin.version),
            Some("10.0.401".to_owned()),
            "64 KiB reads"
        );

        fs::remove_file(&global).expect("the file can be removed");
        fs::create_dir_all(&global).expect("a folder named global.json can be made");
        assert_eq!(read_pin(root.path()).0, Some(Pin::unreadable()), "a folder");
        fs::remove_dir(&global).expect("the folder can be removed");
        junction(&global, elsewhere.path());
        assert_eq!(read_pin(root.path()).0, Some(Pin::unreadable()), "a junction is not followed");
        fs::remove_dir(&global).expect("the junction can be removed");
    }

    #[test]
    fn a_pin_nobody_here_has_is_missing() {
        let pin = pin_of(Some(r#"{"sdk": {"version": "10.0.401"}}"#));
        let checked = sdk_of(Some(&pin), || Ok(vec!["10.0.300".to_owned(), "10.0.500".to_owned()]));
        assert!(
            matches!(&checked, Some(Sdk::Checked { need, served: false, .. }) if need == ".NET SDK 10.0.401 (rollForward patch)"),
            "patch rolls only within the band: {checked:?}"
        );
        let none = sdk_of(Some(&pin), || Ok(Vec::new()));
        assert!(
            matches!(&none, Some(Sdk::Checked { served: false, have, .. }) if have.is_empty()),
            "{none:?}"
        );
    }

    #[test]
    fn a_sibling_is_present_when_its_git_exists_beside_the_root() {
        let parent = Folder::new("sibling-beside");
        let root = parent.path().join("app");
        fs::create_dir_all(root.join(".gate")).expect("the root can be made");
        write(&root.join(".gate").join("siblings.toml"), "siblings = ['here', 'gone', '..']\n");
        fs::create_dir_all(parent.path().join("here").join(".git")).expect("the sibling's git can be made");
        fs::create_dir_all(parent.path().join("gone")).expect("a folder with no git can be made");

        let mut problems = Vec::new();
        let (found, unreadable) = read_siblings(&root.to_string_lossy(), None, &mut problems);
        assert!(!unreadable, "the siblings file reads");

        let seen: Vec<(&str, bool)> = found.iter().map(|sibling| (sibling.name.as_str(), present(sibling))).collect();
        assert_eq!(seen, [("here", true), ("gone", false)]);
        assert_eq!(found[1].found, Found::Folder(Ok(SiblingFolder::Missing)));
        assert_eq!(found[0].path, format!("{}\\here", parent.text()));
        let texts: Vec<&str> = problems.iter().map(|problem| problem.text.as_str()).collect();
        assert_eq!(texts.len(), 1, "the bad name is a problem and the rest are looked for: {texts:?}");
        assert!(texts[0].starts_with("siblings[2] is not a folder name"), "{texts:?}");
    }

    #[test]
    fn siblings_beside_a_linked_worktree_use_the_worktree_root() {
        let dev = Folder::new("sibling-worktree");
        fs::create_dir_all(dev.path().join("app").join(".git")).expect("the main checkout can be made");
        fs::create_dir_all(dev.path().join("b").join(".git")).expect("a sibling beside the main checkout can be made");
        let worktree = dev.path().join("app.wt").join("feature");
        write(&worktree.join(".git"), "gitdir: elsewhere\n");

        let beside = sibling(&worktree.to_string_lossy().replace('\\', "/"), "b".to_owned());

        assert_eq!(beside.path, format!("{}/app.wt/b", dev.text().replace('\\', "/")));
        assert_eq!(
            beside.found,
            Found::Folder(Ok(SiblingFolder::Missing)),
            "the sibling beside the main checkout is not the worktree's"
        );
        fs::create_dir_all(dev.path().join("app.wt").join("b").join(".git")).expect("a sibling beside the worktree can be made");
        assert!(present(&sibling(&worktree.to_string_lossy(), "b".to_owned())));
    }

    #[test]
    fn only_a_sibling_the_routing_file_names_exactly_is_allowed() {
        let parent = Folder::new("sibling-allowed");
        let root = parent.path().join("app");
        fs::create_dir_all(root.join(".gate")).expect("the root can be made");
        write(&root.join(".gate").join("siblings.toml"), "siblings = ['here', 'Other', 'gone']\n");
        let root = root.to_string_lossy();
        let allowed = |list: Option<&[String]>| -> Vec<bool> {
            read_siblings(&root, list, &mut Vec::new())
                .0
                .iter()
                .map(|sibling| sibling.found != Found::NotAllowed)
                .collect()
        };

        assert_eq!(
            allowed(Some(&["here".to_owned(), "other".to_owned()])),
            [true, false, false],
            "an exact match"
        );
        assert_eq!(allowed(Some(&[])), [false, false, false], "an empty list");
        assert_eq!(allowed(None), [true, true, true], "no consent asked");
    }

    /// Whether the folder check found `sibling` present.
    fn present(sibling: &Sibling) -> bool {
        matches!(sibling.found, Found::Folder(Ok(SiblingFolder::Present(_))))
    }

    #[test]
    fn a_sibling_named_like_the_root_is_refused_without_a_look() {
        let parent = Folder::new("sibling-like-root");
        let root = parent.path().join("app");
        fs::create_dir_all(root.join(".gate")).expect("the root can be made");
        write(&root.join(".gate").join("siblings.toml"), "siblings = ['APP', 'here']\n");
        fs::create_dir_all(parent.path().join("here").join(".git")).expect("the sibling's git can be made");

        let (found, _) = read_siblings(&root.to_string_lossy(), None, &mut Vec::new());

        assert_eq!(
            found[0].found,
            Found::Folder(Ok(SiblingFolder::Refused("is the worktree's own folder".to_owned())))
        );
        assert!(present(&found[1]), "the other sibling is still looked for");
        let (trailing, _) = read_siblings(&format!("{}\\", root.to_string_lossy()), None, &mut Vec::new());
        assert_eq!(trailing[0].found, found[0].found, "a separator at the root's end changes nothing");
    }

    #[test]
    fn a_sibling_that_is_a_junction_is_refused_by_the_check() {
        let parent = Folder::new("sibling-junction");
        let root = parent.path().join("app");
        fs::create_dir_all(root.join(".gate")).expect("the root can be made");
        write(&root.join(".gate").join("siblings.toml"), "siblings = ['linked']\n");
        fs::create_dir_all(parent.path().join("real").join(".git")).expect("the junction's target can be made");
        junction(&parent.path().join("linked"), &parent.path().join("real"));

        let (found, _) = read_siblings(&root.to_string_lossy(), None, &mut Vec::new());

        assert_eq!(
            found[0].found,
            Found::Folder(Ok(SiblingFolder::Refused("is a link or a junction".to_owned())))
        );
    }
}
