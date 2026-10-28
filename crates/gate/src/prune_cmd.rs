//! `gate prune --builders`: the PC's sweep of its worktree records ([`v1::worktree_record_path`]). A record whose
//! worktree is gone is marked so (`goneSince`) by the first sweep that finds it gone; once it has been gone an hour, its
//! key is pruned on every builder `remote.json` lists, each through the box's keyed `gate box prune --root <root> --keys
//! <key>...`, and each builder's answer is said with its alias. A record whose worktree is there again has its mark
//! cleared. The sweep never runs `git worktree prune`. What the PC keeps of a key is removed as each builder's answer
//! allows ([`removal`]).

mod removal;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_args::{BoxVerb, PruneRequest, PruneSeams};
use gate_router::{Builder, Config, escaped};
use gate_state::v1;
use gate_win32::PipedConsole;
use gate_wire::stamp;
use jiff::{SignedDuration, Timestamp};

use crate::answer::FAILED_STATUS;
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::git::{self, Git, GitError, ProgramCall, ProgramOutput, ProgramPlace};
use crate::remote::launch;
use crate::remote::transport::ssh::Ssh;
use crate::remote::transport::{Program, Transport, Wire};
use crate::remote::{SSH_FAILED, no_box};
use crate::resolve::{self, Environment};
use crate::route;
use crate::worktree_record;

/// How long a worktree must have been found gone before its builders are called.
const LEAST_AGE: Duration = Duration::from_hours(1);
/// How long one keyed prune call to a builder may take: ssh's connection, and a few git calls for each key.
const BOUND: Duration = Duration::from_secs(300);
/// How long `git worktree list` may take.
const GIT_BOUND: Duration = Duration::from_secs(30);
/// The most keys one keyed prune call carries, as the box takes them.
const MOST_KEYS: usize = 50;
/// What a line the box refuses a keyed prune's words with starts with, once its stamp is taken off.
const BOX_REFUSAL: &str = "gate: box prune ";
/// What every line of the box's prune starts with, once its stamp is taken off.
const BOX_LINE: &str = "gate: remote ";

/// What one builder did with one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The box said it pruned the key.
    Removed,
    /// The box kept the key and that is no failure: a run holds it, the builder is drained, or its box cannot prune by
    /// key.
    Held,
    /// The key was not pruned and that is a failure: the box could not prune it or said nothing of it, the builder
    /// could not be reached, or its root cannot be given on a line.
    Failed,
}

/// What each builder did with each key the sweep sent: by key, each builder's id ([`Builder::id`], `None` for `host`)
/// and its outcome, in the order the builders were called.
pub(crate) type Outcomes = BTreeMap<String, Vec<(Option<String>, Outcome)>>;

/// Runs `gate prune --builders` and returns its exit status: 0 when nothing failed, 1 when anything did.
pub(crate) fn prune(request: &PruneRequest, environment: &Environment, console: &mut Console) -> u32 {
    prune_through(request, environment, || transport(&request.seams), console)
}

/// Runs the sweep as [`prune`] does, its builders reached through the transport `transport` makes, asked for only once a
/// record is a candidate; else the line saying why there is none. Returns the sweep's exit status.
pub(crate) fn prune_through(
    request: &PruneRequest,
    environment: &Environment,
    transport: impl FnOnce() -> Result<Box<dyn Transport>, String>,
    console: &mut Console,
) -> u32 {
    let state = match resolve::root(None, environment) {
        Ok(resolved) => resolved,
        Err(line) => return failed_with(&line, console),
    };
    let records = match scanned(&state.path, console) {
        Ok(Some(records)) => records,
        Ok(None) => {
            if !request.quiet {
                console.err("gate: prune: no worktree records");
            }
            return 0;
        }
        Err(line) => return failed_with(&line, console),
    };
    let place = match ProgramPlace::new(Variables::from_process()) {
        Ok(place) => place,
        Err(why) => return failed_with(&format!("gate: prune: {why}"), console),
    };
    let git = match Git::find(&state.path) {
        Ok(git) => git,
        Err(why) => return failed_with(&format!("gate: prune: {why}"), console),
    };
    let least = request.seams.gone_age_ms.map_or(LEAST_AGE, Duration::from_millis);
    let now = Timestamp::now();
    let mut failed = false;
    let mut candidates = Vec::new();
    for scanned in &records {
        match judged(scanned, &git, least, now, console) {
            Judged::Candidate => candidates.push(scanned),
            Judged::Failed => failed = true,
            Judged::Kept => {}
        }
    }
    if candidates.is_empty() {
        return status(failed);
    }
    let (config, builders) = match listed(environment.local_app_data.as_deref()) {
        Ok(listed) => listed,
        Err(line) => return failed_with(&line, console),
    };
    let transport = match transport() {
        Ok(transport) => transport,
        Err(line) => return failed_with(&line, console),
    };
    let keys: Vec<String> = candidates.iter().map(|scanned| scanned.key.clone()).collect();
    let outcomes = swept(&*transport, &builders, &keys, &place, console);
    failed |= outcomes.values().flatten().any(|(_, outcome)| *outcome == Outcome::Failed);
    let removal = removal::Removal {
        state: &state,
        config: &config,
        git: &git,
    };
    failed |= removal::removed(&removal, &candidates, &outcomes, console);
    status(failed)
}

/// Says `line`; the status of a failure.
fn failed_with(line: &str, console: &mut Console) -> u32 {
    console.err(line);
    u32::from(FAILED_STATUS)
}

/// The exit status of a sweep that `failed`, or did not.
fn status(failed: bool) -> u32 {
    if failed { u32::from(FAILED_STATUS) } else { 0 }
}

/// A worktree record the sweep read: its key, the file's stem, its path and what it says.
struct Scanned {
    key: String,
    path: PathBuf,
    record: v1::WorktreeRecord,
}

/// The worktree records of the state root `state`, every regular file `remote\*.worktree` in name order, a file that
/// cannot be read as one, or whose stem is no key, skipped with a line; `None` when there is no such file. Else the line
/// saying the folder cannot be listed.
fn scanned(state: &Path, console: &mut Console) -> Result<Option<Vec<Scanned>>, String> {
    let folder = state.join("remote");
    let cannot = |error: &std::io::Error| format!("gate: prune: cannot list {}: {error}", folder.display());
    let entries = match fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(cannot(&error)),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| cannot(&error))?;
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_file()) && path.extension().is_some_and(|extension| extension == "worktree") {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return Ok(None);
    }
    paths.sort();
    let mut records = Vec::new();
    for path in paths {
        let key = path.file_stem().and_then(OsStr::to_str).filter(|key| box_key(key));
        match (key, worktree_record::read_worktree_record(&path)) {
            (Some(key), Some(record)) => records.push(Scanned {
                key: key.to_owned(),
                path,
                record,
            }),
            _ => console.err(&format!(
                "gate: prune: skipped {}: it cannot be read as a worktree record",
                path.display()
            )),
        }
    }
    Ok(Some(records))
}

/// Whether `stem` is a key the box's `--keys` takes: a key ([`gate_wire::is_key`]) that does not start with `-`, so
/// the box never reads it as a flag.
fn box_key(stem: &str) -> bool {
    gate_wire::is_key(stem) && !stem.starts_with('-')
}

/// What the sweep makes of one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Judged {
    /// Its builders are not called this sweep.
    Kept,
    /// Its builders are not called, and its record could not be written.
    Failed,
    /// Its worktree has been gone at least the least age: its builders are called.
    Candidate,
}

/// What the sweep at `now` makes of the record `scanned`, its worktree found as [`found`] finds it with git run in
/// `place`, a worktree gone `least` or longer being a candidate: a live worktree's mark is cleared, silently; a gone
/// one's is set on the first sweep that finds it gone; each else says why its builders are not called yet.
fn judged(scanned: &Scanned, git: &Git, least: Duration, now: Timestamp, console: &mut Console) -> Judged {
    let Scanned { key, path, record } = scanned;
    let root = escaped(&record.root);
    match found(record, git) {
        Found::Live if record.gone_since.is_none() => Judged::Kept,
        Found::Live => {
            let cleared = v1::WorktreeRecord {
                gone_since: None,
                ..record.clone()
            };
            rewritten(path, record, &cleared, console).unwrap_or(Judged::Kept)
        }
        Found::Unknown(why) => {
            console.err(&format!("gate: prune: kept {key}: cannot tell whether {root} is gone: {why}"));
            Judged::Kept
        }
        Found::Gone => match record.gone_since {
            None => {
                let judged = rewritten(path, record, &v1::mark_gone(record.clone(), now), console);
                if judged == Some(Judged::Kept) {
                    console.err(&format!(
                        "gate: prune: {key}: {root} is gone; its builders are pruned on a sweep an hour from now"
                    ));
                }
                judged.unwrap_or(Judged::Kept)
            }
            Some(since) if younger(since, now, least) => {
                console.err(&format!(
                    "gate: prune: {key}: {root} gone since {since:.0}; its builders are pruned once it has been gone an hour"
                ));
                Judged::Kept
            }
            Some(_) => Judged::Candidate,
        },
    }
}

/// Whether a worktree found gone at `since` has been gone less than `least` at `now`; one found gone after `now`, by a
/// clock that went back, has.
fn younger(since: Timestamp, now: Timestamp, least: Duration) -> bool {
    now.duration_since(since) < SignedDuration::try_from(least).unwrap_or(SignedDuration::MAX)
}

/// Writes `record` at `path` over `judged`, the record the gone test judged, when the record there still is it:
/// [`Judged::Kept`], or [`Judged::Failed`] with the line saying why it could not be written; `None`, writing nothing and
/// saying nothing, when the record there changed since, which the next sweep judges.
fn rewritten(path: &Path, judged: &v1::WorktreeRecord, record: &v1::WorktreeRecord, console: &mut Console) -> Option<Judged> {
    if worktree_record::read_worktree_record(path).as_ref() != Some(judged) {
        return None;
    }
    Some(match v1::write_worktree_record(path, record) {
        Ok(()) => Judged::Kept,
        Err(error) => {
            console.err(&format!("gate: cannot write {}: {error}", path.display()));
            Judged::Failed
        }
    })
}

/// How a record's worktree is found.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Found {
    /// Something is at its root.
    Live,
    /// Nothing is at its root, its drive is there, and git does not list it, lists it prunable, or its common folder is
    /// gone.
    Gone,
    /// It cannot be told whether it is gone: why.
    Unknown(String),
}

/// How the worktree of `record` is found, `git worktree list` run in `place`: live when anything is at its root; gone
/// when nothing is, the root's drive as the record spells it is there, and its common folder is gone or git does not
/// list the root, or lists it prunable; unknown otherwise.
fn found(record: &v1::WorktreeRecord, git: &Git) -> Found {
    let looked = fs::symlink_metadata(&record.root);
    if looked.is_ok() {
        return Found::Live;
    }
    let Some(drive) = drive_root(&record.root) else {
        return Found::Unknown("its root is not on a drive".to_owned());
    };
    if fs::metadata(&drive).is_err() {
        return Found::Unknown(format!("the drive {drive} is not there"));
    }
    if let Err(error) = &looked
        && error.kind() != ErrorKind::NotFound
    {
        return Found::Unknown(format!("its root cannot be looked at: {error}"));
    }
    match fs::symlink_metadata(&record.common_dir) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Found::Gone,
        Err(error) => {
            return Found::Unknown(format!(
                "its git common folder {} cannot be looked at: {error}",
                escaped(&record.common_dir)
            ));
        }
        Ok(_) => {}
    }
    match worktree_list(&record.common_dir, git) {
        Ok(porcelain) => match listing(&porcelain, &record.root) {
            Listing::Absent | Listing::Prunable => Found::Gone,
            Listing::Listed => Found::Unknown("git still lists it as a worktree".to_owned()),
        },
        Err(why) => Found::Unknown(why),
    }
}

/// The drive root of `root` as it spells it, `X:\`; `None` for a root that does not start with a drive letter.
fn drive_root(root: &str) -> Option<String> {
    let mut characters = root.chars();
    match (characters.next(), characters.next()) {
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => Some(format!(r"{letter}:\")),
        _ => None,
    }
}

/// What `git --git-dir=<common> worktree list --porcelain` said on stdout, asked about `common`, run as [`git_run`]
/// runs git; else why it said nothing.
fn worktree_list(common: &str, git: &Git) -> Result<String, String> {
    let git_dir = format!("--git-dir={common}");
    let args = [git_dir.as_str(), "worktree", "list", "--porcelain"];
    let at = GitAt {
        folder: Path::new(common),
        set: &[],
    };
    git_run(git, "git worktree list", &at, &args, None).map_err(|fault| match fault {
        GitFault::Said(line) => format!("git worktree list failed: {line}"),
        GitFault::Ended(why) => why,
    })
}

/// Where one of the sweep's git calls runs: the folder git is asked about with `-C`, and the variables it is given
/// beside the ones every call is given.
struct GitAt<'a> {
    folder: &'a Path,
    set: &'a [(&'a str, &'a OsStr)],
}

/// The git call of the sweep at `at` with `args`, and `stdin` on its standard input, bounded at [`GIT_BOUND`]: run by
/// [`Git`], so it is given none of the parent's git location or config variables and runs no hook.
fn sweep_call<'a>(at: &GitAt<'a>, args: &'a [&'a str], stdin: Option<&'a [u8]>) -> git::Call<'a> {
    git::Call {
        set: at.set,
        stdin,
        ..git::Call::plain(at.folder, args, GIT_BOUND)
    }
}

/// Why a git call the sweep made did not pass.
#[derive(Clone, Debug, PartialEq, Eq)]
enum GitFault {
    /// It failed, and this is the last line it said on stderr, shown [`escaped`].
    Said(String),
    /// It failed saying nothing, could not run or was ended: the whole line saying so.
    Ended(String),
}

/// What `git` said on stdout when run as [`sweep_call`] makes the call at `at` with `args` and `stdin`; else why it
/// did not pass, the call named as `named` (`git worktree list`).
fn git_run<'a>(git: &Git, named: &str, at: &GitAt<'a>, args: &'a [&'a str], stdin: Option<&'a [u8]>) -> Result<String, GitFault> {
    match git.status_within(&sweep_call(at, args, stdin), git::OUTPUT_CAP) {
        Ok((status, stdout, _)) if status.success() => Ok(String::from_utf8_lossy(&stdout).into_owned()),
        Ok((status, _, stderr)) => {
            let stderr = String::from_utf8_lossy(&stderr);
            Err(match stderr.lines().map(str::trim).rfind(|line| !line.is_empty()) {
                Some(line) => GitFault::Said(escaped(line)),
                None => GitFault::Ended(format!("{named} ended with {status}")),
            })
        }
        Err(GitError::Interrupted) => Err(GitFault::Ended(format!("{named} was interrupted"))),
        Err(GitError::Failed(why) | GitError::OverCap(why)) => Err(GitFault::Ended(escaped(&why))),
    }
}

/// How `git worktree list --porcelain` lists a root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Listing {
    Absent,
    Listed,
    /// Listed with a `prunable` line: git knows its folder is gone.
    Prunable,
}

/// How the `porcelain` text lists the worktree `root`, roots compared as [`compared`] spells them.
fn listing(porcelain: &str, root: &str) -> Listing {
    let wanted = compared(root);
    let (mut listing, mut current) = (Listing::Absent, false);
    for line in porcelain.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = compared(path) == wanted;
            if current && listing == Listing::Absent {
                listing = Listing::Listed;
            }
        } else if line.is_empty() {
            current = false;
        } else if current && (line == "prunable" || line.starts_with("prunable ")) {
            listing = Listing::Prunable;
        }
    }
    listing
}

/// `root` as roots are compared: lower-cased, each `/` a `\`, trailing separators left out.
fn compared(root: &str) -> String {
    root.replace('/', r"\").trim_end_matches('\\').to_lowercase()
}

/// The routing file `remote.json` under `local_app_data`, and every builder it lists, routed or not: `host`, each
/// `hosts` entry, then each `builders` entry no `hosts` alias names in any letter case. Else the line saying the file
/// is missing or why it is ignored.
fn listed(local_app_data: Option<&OsStr>) -> Result<(Config, Vec<Builder>), String> {
    let path =
        route::remote_json_path(local_app_data).ok_or_else(|| "gate: prune: no remote.json: LOCALAPPDATA is not an absolute folder".to_owned())?;
    let ignored = |reason: &str| format!("gate: prune: ignored {}: {reason}", path.display());
    let config = match route::read_remote_json(&path) {
        Ok(Some(config)) => config,
        Ok(None) => return Err(format!("gate: prune: no remote.json at {}", path.display())),
        Err(problem) => return Err(ignored(&problem.reason)),
    };
    let hosts = config.builders().map_err(ignored)?;
    let mut builders = Vec::new();
    for alias in hosts {
        builders.extend(config.named_builder(alias).map_err(ignored)?);
    }
    let entries = config.builder_entries();
    builders.extend(
        entries
            .filter(|entry| !hosts.iter().any(|host| host.eq_ignore_ascii_case(&entry.alias)))
            .cloned(),
    );
    Ok((config, builders))
}

/// The way to the builders: the local transport `--test-remote-transport` names, else ssh found along the gate's own
/// `PATH`. Else the line saying why the local transport cannot be used, or why ssh cannot be found.
#[cfg(feature = "test-seams")]
fn transport(seams: &PruneSeams) -> Result<Box<dyn Transport>, String> {
    match &seams.remote_transport {
        Some(named) => crate::remote::local_transport(named, seams.remote_ssh.as_deref()).map(|local| Box::new(local) as Box<dyn Transport>),
        None => ssh(),
    }
}

/// The way to the builders: ssh found along the gate's own `PATH`; else the line saying why it cannot be found.
#[cfg(not(feature = "test-seams"))]
fn transport(_seams: &PruneSeams) -> Result<Box<dyn Transport>, String> {
    ssh()
}

/// ssh found along the gate's own `PATH`; else the line saying why it cannot be found.
fn ssh() -> Result<Box<dyn Transport>, String> {
    Ssh::find()
        .map(|ssh| Box::new(ssh) as Box<dyn Transport>)
        .map_err(|why| format!("gate: prune: {why}"))
}

/// Sends `keys` to each of `builders` in turn through `transport`, the calls run in `place`, saying each answer; what
/// each builder did with each key.
fn swept(transport: &dyn Transport, builders: &[Builder], keys: &[String], place: &ProgramPlace, console: &mut Console) -> Outcomes {
    let mut outcomes = Outcomes::new();
    for builder in builders {
        let answered = builder_swept(transport, builder, keys, place, console);
        merged(&mut outcomes, builder.id.as_deref(), answered);
    }
    outcomes
}

/// Adds what the builder whose id is `id` did with each key, `answered`, to `outcomes`.
fn merged(outcomes: &mut Outcomes, id: Option<&str>, answered: Vec<(String, Outcome)>) {
    for (key, outcome) in answered {
        outcomes.entry(key).or_default().push((id.map(str::to_owned), outcome));
    }
}

/// How a builder's later calls go once one call stopped it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stopped {
    /// Its box cannot prune by key: each key is said kept, no failure.
    TooOld,
    /// It could not be reached: each key is kept, a failure said once.
    Unreachable,
}

/// Sends `keys` to `builder` through `transport`, at most [`MOST_KEYS`] a call ([`calls_of`]), saying each answer
/// with its alias; what it did with each key. A builder whose root has a space is not called.
fn builder_swept(
    transport: &dyn Transport,
    builder: &Builder,
    keys: &[String],
    place: &ProgramPlace,
    console: &mut Console,
) -> Vec<(String, Outcome)> {
    let host = escaped(&builder.alias);
    let profile = transport.profile(builder);
    if profile.root().contains(' ') {
        console.err(&format!(
            "gate: remote {host} cannot prune: its root {} has a space",
            escaped(profile.root())
        ));
        return keys.iter().map(|key| (key.clone(), Outcome::Failed)).collect();
    }
    let mut answered = Vec::new();
    let mut stopped = None;
    for (chunk, line) in calls_of(&profile.line(BoxVerb::Prune), profile.root(), keys) {
        if let Some(stopped) = stopped {
            answered.extend(kept_all(&host, chunk, stopped, console));
            continue;
        }
        let program = transport.call(&builder.alias, &line, Wire::Plain);
        match called(&program, place).map(|output| answer_of(&host, chunk, &output)) {
            Ok(Answer::Said { lines, keyed }) => {
                for line in lines {
                    console.err(&line);
                }
                answered.extend(keyed);
            }
            Ok(Answer::Refused(why)) => {
                console.err(&format!("gate: remote {host} could not prune: {why}"));
                answered.extend(chunk.iter().map(|key| (key.clone(), Outcome::Failed)));
            }
            Ok(Answer::TooOld) => {
                stopped = Some(Stopped::TooOld);
                answered.extend(kept_all(&host, chunk, Stopped::TooOld, console));
            }
            Ok(Answer::Unreachable(why)) | Err(why) => {
                console.err(&format!("gate: remote {host} could not be reached: {why}"));
                stopped = Some(Stopped::Unreachable);
                answered.extend(kept_all(&host, chunk, Stopped::Unreachable, console));
            }
        }
    }
    answered
}

/// Each key of `chunk` as a builder `host` that `stopped` keeps it, said when its box cannot prune by key.
fn kept_all(host: &str, chunk: &[String], stopped: Stopped, console: &mut Console) -> Vec<(String, Outcome)> {
    let outcome = match stopped {
        Stopped::TooOld => Outcome::Held,
        Stopped::Unreachable => Outcome::Failed,
    };
    if stopped == Stopped::TooOld {
        for key in chunk {
            console.err(&format!("gate: remote {host} kept {key}: its box cannot prune by key"));
        }
    }
    chunk.iter().map(|key| (key.clone(), outcome)).collect()
}

/// The keyed prune calls of `keys` to a builder whose box line is `line` and whose root is `root`: each [`MOST_KEYS`]
/// keys, in order, with the line that sends them, the words not quoted.
fn calls_of<'k>(line: &str, root: &str, keys: &'k [String]) -> Vec<(&'k [String], String)> {
    keys.chunks(MOST_KEYS)
        .map(|chunk| (chunk, format!("{line} --root {root} --keys {}", chunk.join(" "))))
        .collect()
}

/// Makes the one call `program`, with nothing on its standard input, bounded at [`BOUND`], in `place`, as the
/// client's programs other than git run ([`git::program_output`]); what it said, else why it said nothing.
fn called(program: &Program, place: &ProgramPlace) -> Result<ProgramOutput, String> {
    let shown = program.program.to_string_lossy();
    let words: Vec<String> = program.args.iter().map(|word| word.to_string_lossy().into_owned()).collect();
    let args: Vec<&str> = words.iter().map(String::as_str).collect();
    let call = ProgramCall {
        program: &shown,
        args: &args,
        limit: BOUND,
        drain: launch::DRAIN,
        interrupt: None,
        worktrees: &[],
        console: PipedConsole::Attached,
        stdin: None,
        launch_guard: None,
    };
    let late = call.late_line();
    let limit = git::spelled_limit(BOUND);
    // A line never carries ssh's options: the whole call's words are shown as the program alone.
    let named = late.strip_suffix(&format!(" did not answer in {limit}")).unwrap_or(&late);
    match git::program_output(&call, place) {
        Ok(Some(output)) => Ok(output),
        Ok(None) => Err(format!("cannot run {shown}: it is not found")),
        Err(GitError::Interrupted) => Err("the call was interrupted".to_owned()),
        Err(GitError::Failed(why)) if why == late => Err(format!("no answer in {limit}")),
        Err(GitError::Failed(why) | GitError::OverCap(why)) => Err(escaped(&why.replace(named, &shown))),
    }
}

/// What one keyed prune call came to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
    /// The builder's box does not take the keyed prune.
    TooOld,
    /// The builder could not be reached: why.
    Unreachable(String),
    /// The box refused the call with its own `gate: box prune` line: that line without its `gate: ` head.
    Refused(String),
    /// The box answered: the lines saying it, and what it did with each key.
    Said { lines: Vec<String>, keyed: Vec<(String, Outcome)> },
}

/// What the call to the builder `host` of the keys `keys` that ended in `output` came to: ssh's failure, a box that
/// does not take the keyed prune ([`no_box`]) or refused its words with a line of its own, or the box's lines on stdout then stderr ([`said_of`]), whatever its
/// status.
fn answer_of(host: &str, keys: &[String], output: &ProgramOutput) -> Answer {
    let lines: Vec<String> = String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect();
    let errors: Vec<String> = String::from_utf8_lossy(&output.stderr).lines().map(str::to_owned).collect();
    let Some(code) = output.status.code() else {
        return Answer::Unreachable("the call ended with no status".to_owned());
    };
    if code == SSH_FAILED {
        let last = errors
            .iter()
            .map(|line| stamp::unstamped(line).text.trim())
            .rfind(|line| !line.is_empty());
        return Answer::Unreachable(last.map_or_else(|| format!("ssh exited {SSH_FAILED} with no message"), escaped));
    }
    if no_box(code, &lines, errors.iter().map(String::as_str)) {
        let refusal = errors
            .iter()
            .map(|line| stamp::unstamped(line).text)
            .find(|text| text.starts_with(BOX_REFUSAL));
        return match refusal {
            Some(text) => Answer::Refused(escaped(text.strip_prefix("gate: ").unwrap_or(text))),
            None => Answer::TooOld,
        };
    }
    let (lines, keyed) = said_of(host, keys, lines.iter().chain(&errors).map(String::as_str));
    Answer::Said { lines, keyed }
}

/// The lines saying what the box of the builder `host` said in `said` of the keys `keys`, and what it did with each:
/// each line without its stamp, a line of a key of `keys` ([`keyed`]) said with the alias after `gate: remote `, any
/// other with the alias and a colon, then a line for each key it said nothing of. A key with a line saying it could not
/// be pruned failed whatever else is said of it.
fn said_of<'l>(host: &str, keys: &[String], said: impl IntoIterator<Item = &'l str>) -> (Vec<String>, Vec<(String, Outcome)>) {
    let mut outcomes: Vec<Option<Outcome>> = vec![None; keys.len()];
    let mut lines = Vec::new();
    for line in said {
        let text = stamp::unstamped(line).text;
        if text.trim().is_empty() {
            continue;
        }
        let rest = text.strip_prefix(BOX_LINE);
        let named = rest
            .and_then(keyed)
            .and_then(|(key, outcome)| keys.iter().position(|wanted| wanted.eq_ignore_ascii_case(key)).map(|at| (at, outcome)));
        match (named, rest) {
            (Some((at, outcome)), Some(rest)) => {
                lines.push(format!("{BOX_LINE}{host} {}", escaped(rest)));
                if let Some(slot) = outcomes.get_mut(at) {
                    *slot = Some(if *slot == Some(Outcome::Failed) { Outcome::Failed } else { outcome });
                }
            }
            _ => lines.push(format!("{BOX_LINE}{host}: {}", escaped(rest.unwrap_or(text)))),
        }
    }
    let mut keyed = Vec::new();
    for (key, outcome) in keys.iter().zip(outcomes) {
        if outcome.is_none() {
            lines.push(format!("{BOX_LINE}{host} kept {key}: its box said nothing of it"));
        }
        keyed.push((key.clone(), outcome.unwrap_or(Outcome::Failed)));
    }
    (lines, keyed)
}

/// The key the box's line `rest`, after `gate: remote `, says of and what became of it: `pruned {key} (worktree
/// gone)`, `kept {key}: in use`, `kept {key}: the builder is drained` or `could not prune {key}: {why}`; `None` for any
/// other line.
fn keyed(rest: &str) -> Option<(&str, Outcome)> {
    let word = |key: &str| !key.is_empty() && !key.contains(' ');
    if let Some(key) = rest.strip_prefix("pruned ").and_then(|tail| tail.strip_suffix(" (worktree gone)")) {
        return word(key).then_some((key, Outcome::Removed));
    }
    if let Some((key, why)) = rest.strip_prefix("kept ").and_then(|tail| tail.split_once(": ")) {
        return (word(key) && matches!(why, "in use" | "the builder is drained")).then_some((key, Outcome::Held));
    }
    let (key, _) = rest.strip_prefix("could not prune ")?.split_once(": ")?;
    word(key).then_some((key, Outcome::Failed))
}

#[cfg(test)]
mod tests {
    use std::os::windows::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::{
        Answer, GitAt, Listing, MOST_KEYS, Outcome, Outcomes, answer_of, box_key, calls_of, compared, drive_root, listing, merged, said_of,
        sweep_call,
    };
    use crate::remote::git::ProgramOutput;

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn each_box_line_of_a_key_is_said_with_the_alias_and_read_as_its_outcome() {
        let wanted = keys(&["a-1", "b-2", "c-3", "d-4"]);
        let (lines, keyed) = said_of(
            "builder-1",
            &wanted,
            [
                "2026-10-27T00:00:00.000000+00:00 out gate: remote pruned A-1 (worktree gone)",
                "gate: remote kept b-2: in use",
                "gate: remote kept c-3: the builder is drained",
                "2026-10-27T00:00:00.000000+00:00 err gate: remote could not prune d-4: its refs could not be removed: \u{1b}[2K",
            ],
        );
        assert_eq!(
            lines,
            [
                "gate: remote builder-1 pruned A-1 (worktree gone)",
                "gate: remote builder-1 kept b-2: in use",
                "gate: remote builder-1 kept c-3: the builder is drained",
                r"gate: remote builder-1 could not prune d-4: its refs could not be removed: \u{1b}[2K",
            ],
            "a stamped line is read without its stamp, and a key in another case is the key"
        );
        assert_eq!(
            keyed,
            [
                ("a-1".to_owned(), Outcome::Removed),
                ("b-2".to_owned(), Outcome::Held),
                ("c-3".to_owned(), Outcome::Held),
                ("d-4".to_owned(), Outcome::Failed),
            ]
        );
    }

    #[test]
    fn another_line_is_passed_through_and_a_key_said_nothing_of_fails() {
        let wanted = keys(&["a-1", "b-2"]);
        let (lines, keyed) = said_of(
            "builder-2",
            &wanted,
            [
                r"gate: remote could not remove the pruned folder D:\gate-remote\pruned\a-1.7: in use",
                "gate: remote pruned a-1 (worktree gone)",
                "gate: remote kept b-2: for no reason it knows",
                "gate: remote pruned z-9 (worktree gone)",
                "Warning: Permanently added 'builder-2'",
                "",
            ],
        );
        assert_eq!(
            lines,
            [
                r"gate: remote builder-2: could not remove the pruned folder D:\gate-remote\pruned\a-1.7: in use",
                "gate: remote builder-2 pruned a-1 (worktree gone)",
                "gate: remote builder-2: kept b-2: for no reason it knows",
                "gate: remote builder-2: pruned z-9 (worktree gone)",
                "gate: remote builder-2: Warning: Permanently added 'builder-2'",
                "gate: remote builder-2 kept b-2: its box said nothing of it",
            ]
        );
        assert_eq!(keyed, [("a-1".to_owned(), Outcome::Removed), ("b-2".to_owned(), Outcome::Failed)]);
    }

    #[test]
    fn a_key_that_could_not_be_pruned_fails_whatever_else_is_said() {
        let wanted = keys(&["a-1"]);
        let (_, keyed) = said_of(
            "b",
            &wanted,
            ["gate: remote could not prune a-1: busy", "gate: remote pruned a-1 (worktree gone)"],
        );
        assert_eq!(keyed, [("a-1".to_owned(), Outcome::Failed)]);
    }

    #[test]
    fn roots_are_compared_lower_cased_with_backslashes_and_no_trailing_separator() {
        assert_eq!(compared("X:/dev/a/"), compared(r"x:\dev\a"));
        assert_eq!(compared(r"X:\dev\a\\"), r"x:\dev\a");
        assert_ne!(compared("X:/dev/a"), compared("X:/dev/ab"));
        assert_eq!(drive_root("x:/dev/a").as_deref(), Some(r"x:\"));
        assert_eq!(drive_root(r"\\server\share\a"), None);
    }

    #[test]
    fn the_porcelain_list_says_a_root_listed_prunable_or_absent() {
        let porcelain = "worktree X:/dev/app\nHEAD 0123\nbranch refs/heads/main\n\n\
                         worktree X:/dev/app-wt\nHEAD 4567\ndetached\n\n\
                         worktree X:/dev/app-gone\nHEAD 89ab\nbranch refs/heads/gone\nprunable gitdir file points to non-existent location\n";
        assert_eq!(listing(porcelain, r"x:\dev\app-wt\"), Listing::Listed);
        assert_eq!(listing(porcelain, r"X:\dev\app-gone"), Listing::Prunable);
        assert_eq!(listing(porcelain, "X:/dev/app-other"), Listing::Absent);
        assert_eq!(listing("", "X:/dev/app"), Listing::Absent);
    }

    #[test]
    fn keys_go_at_most_fifty_a_call() {
        let many: Vec<String> = (0..=MOST_KEYS).map(|at| format!("k-{at}")).collect();
        let calls = calls_of(r#""C:\gate.exe" box prune"#, r"D:\root", &many);
        let lines: Vec<&String> = calls.iter().map(|(_, line)| line).collect();
        let sizes: Vec<usize> = calls.iter().map(|(chunk, _)| chunk.len()).collect();
        assert_eq!(sizes, [50, 1], "51 keys are two calls, each carrying its own keys");
        let first = lines.first().map(|line| line.split(' ').count());
        assert_eq!(first, Some(6 + 50), "the exe, box, prune, --root, the root and --keys, then 50 keys");
        assert_eq!(
            lines.get(1).map(|line| line.as_str()),
            Some(r#""C:\gate.exe" box prune --root D:\root --keys k-50"#)
        );
    }

    #[test]
    fn the_outcomes_hold_each_builder_s_answer_for_each_key() {
        let wanted = keys(&["a-1", "b-2"]);
        let mut outcomes = Outcomes::new();
        let (_, host) = said_of(
            "builder-1",
            &wanted,
            ["gate: remote pruned a-1 (worktree gone)", "gate: remote kept b-2: in use"],
        );
        merged(&mut outcomes, None, host);
        let (_, other) = said_of("builder-2", &wanted, ["gate: remote could not prune a-1: busy"]);
        merged(&mut outcomes, Some("1a2b3c4d"), other);

        let other_id = Some("1a2b3c4d".to_owned());
        assert_eq!(
            outcomes.get("a-1"),
            Some(&vec![(None, Outcome::Removed), (other_id.clone(), Outcome::Failed)])
        );
        assert_eq!(outcomes.get("b-2"), Some(&vec![(None, Outcome::Held), (other_id, Outcome::Failed)]));
        assert_eq!(outcomes.len(), 2);
    }

    /// The output of a call that exited `code`, writing `stderr` on its standard error and nothing on its standard
    /// output.
    fn output(code: u32, stderr: &str) -> ProgramOutput {
        ProgramOutput {
            status: ExitStatus::from_raw(code),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn ssh_s_last_line_says_why_a_builder_could_not_be_reached() {
        let said = "Warning: Permanently added 'builder-1'\nssh: connect to host builder-1 port 22: Connection refused\n";
        assert_eq!(
            answer_of("builder-1", &keys(&["a-1"]), &output(255, said)),
            Answer::Unreachable("ssh: connect to host builder-1 port 22: Connection refused".to_owned())
        );
        assert_eq!(
            answer_of("builder-1", &keys(&["a-1"]), &output(255, "")),
            Answer::Unreachable("ssh exited 255 with no message".to_owned())
        );
    }

    #[test]
    fn a_status_2_box_refusal_is_no_old_box() {
        let refused = "2026-10-27T00:00:00.000000+00:00 err gate: box prune --root is not an absolute path on a drive: \"x\"\n";
        assert_eq!(
            answer_of("builder-1", &keys(&["a-1"]), &output(2, refused)),
            Answer::Refused(r#"box prune --root is not an absolute path on a drive: "x""#.to_owned())
        );
        let usage = "usage: gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs|drain --stdin\n";
        assert_eq!(answer_of("builder-1", &keys(&["a-1"]), &output(2, usage)), Answer::TooOld);
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test with no git must fail loudly")]
    fn the_sweep_s_git_is_given_no_git_location_of_its_parent_and_runs_no_hook() {
        let folder = crate::test_support::temp_dir();
        let git = crate::remote::git::Git::find(folder).expect("git is found");
        let at = GitAt { folder, set: &[] };

        let command = git.prepared(&sweep_call(&at, &["for-each-ref"], None));

        let unset: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for name in ["GIT_DIR", "GIT_COMMON_DIR"] {
            assert!(unset.iter().any(|unset| unset == name), "{name} is not passed on: {unset:?}");
        }
        let args: Vec<String> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert!(args.iter().any(|arg| arg.starts_with("core.hooksPath=")), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("for-each-ref"), "{args:?}");
    }

    #[test]
    fn a_stem_the_box_would_read_as_a_flag_is_no_key() {
        assert!(box_key("app-1a2b3c4d"));
        assert!(!box_key("-app-1a2b3c4d"), "a word starting with - is a flag to the box");
        assert!(!box_key("app.1a2b"));
        assert!(!box_key(""));
    }
}
