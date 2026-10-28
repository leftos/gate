//! `gate warm`: warms the builder's work folder of the worktree the gate is started in by running the commands of its
//! `.gate/warmup.toml` there, and keeps the outcome in the worktree's warm-up record.
//!
//! The warm-up is worded and laid out from the worktree root, as a routed run of the worktree is, whatever folder of it
//! the gate starts in. It holds, in order, the warm-up lock of the work folder's key for its whole life, then the key's
//! mutex and then the cap of one warm-up at a time on this PC, each wait bounded by the file's ceiling. The record says
//! `warming` from just after the warm-up lock is taken; once the client has run the warm-up it says `warm` or `failed`,
//! decided from how the client says the warm-up ended alone, or it is deleted by a Ctrl+C. Consent, the routing entry
//! and the file are read again just before the builder is called, and the commands, their hash and the record come from
//! that read; a recheck that finds one of them gone or changed withdraws the warm-up and deletes the record. The
//! warm-up's log is made new for each warm-up and held open, so nothing can put a link in its place while the client
//! writes it. A warm-up takes no slot and writes no journal event.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use gate_args::{DEFAULT_STALL_SECONDS, Spelling, TestSeams, WarmRequest};
use gate_router::warm::{FailureClass, FileRead, Record, RecordRead, RecordState, WarmupFile, read_warmup, warmup_hash};
use gate_router::{Config, Facts, Place, Target, builder_words, escaped, repo_name, stays_local, target};
use gate_state::v1::{self, WarmFailure, WarmRecord, WarmState};
use gate_win32::{Interrupt, OwnedMutexGuard};
use gate_wire::build_version;
use jiff::Timestamp;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::answer::{FAILED_STATUS, USAGE_STATUS, enter};
use crate::console::Console;
use crate::remote::{self, KeyWait, RemoteCall, RemoteClient, RemoteOutcome, WarmEnd, Warmth};
use crate::resolve::{self, Environment, ResolvedRoot};
use crate::route::{self, Machine, Origin, Source};
use crate::run::CONTROL_C_EXIT;
use crate::worktree_record;

/// `ERROR_SHARING_VIOLATION`: another handle holds the file without sharing it.
const SHARING_VIOLATION: i32 = 32;
/// `ERROR_LOCK_VIOLATION`: another handle has a region of the file locked.
const LOCK_VIOLATION: i32 = 33;
/// How long the warm-up lock's open is tried again while another handle holds it, to ride out a route's probe of it.
const LOCK_RETRY: Duration = Duration::from_millis(200);
/// The pause between two opens of a lock another handle holds.
const LOCK_STEP: Duration = Duration::from_millis(20);
/// The largest warm-up record the route reads: 64 KiB.
const RECORD_MOST_BYTES: u64 = 64 * 1024;
/// How long one slice of the cap's wait lasts, between two looks at the console interrupt.
const CAP_SLICE: Duration = Duration::from_millis(100);
/// The warm-up file, as its path below the worktree root is said.
const FILE_SAID: &str = r".gate\warmup.toml";
/// The warm-up file's name in `.gate` and in a declarations folder.
pub(crate) const WARMUP_NAME: &str = "warmup.toml";
/// Why a warm-up whose key another routed run held past the wait did not start.
const KEY_BUSY: &str = "another routed run held the work folder past the wait";
/// Why a warm-up that another warm-up kept from the cap past the wait did not start.
const CAP_BUSY: &str = "another warm-up held the warm-up cap past the wait";
/// Why a warm-up the client ended without saying how it ended, and why it could not start, failed.
const NOT_STARTED: &str = "the client did not start the warm-up";
/// What a warm-up the builder's drain refused says as it ends, with status 1 and no record.
const DRAINED: &str = "gate: warm: the builder is drained; this warm-up ends without warming";

/// Warms the worktree of `request`'s folder, saying its lines through `console`, and returns the exit status: 0 warm;
/// 2 a usage or precondition problem; 1 a warm-up that failed, was refused, met the builder's drain or is already running;
/// [`CONTROL_C_EXIT`] on Ctrl+C. With `--background`, no consent, no file and a warm-up already running say nothing
/// and exit 0.
pub(crate) fn warm(request: &WarmRequest, environment: &Environment, console: &mut Console) -> u32 {
    let started = Timestamp::now();
    match prepare(request, environment) {
        Err(end) => end.say(request.background, console),
        Ok(prepared) => warm_up(&prepared, request, started, console),
    }
}

/// How `gate warm` ends without a warm-up record of its own to write, and what it says.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// A usage or precondition problem: the line, status 2.
    Usage(String),
    /// No consent or no file: the line and status 2, or nothing and status 0 with `--background`.
    Quiet(String),
    /// Another warm-up of the key is running: the line and status 1, or nothing and status 0 with `--background`.
    Running(String),
    /// A failure with no record: the line, status 1.
    Failed(String),
}

impl End {
    /// Says the line unless `background` keeps it quiet, and returns the status.
    fn say(self, background: bool, console: &mut Console) -> u32 {
        let (line, status, quiet) = match self {
            End::Usage(line) => (line, USAGE_STATUS, false),
            End::Quiet(line) => (line, USAGE_STATUS, true),
            End::Running(line) => (line, FAILED_STATUS, true),
            End::Failed(line) => (line, FAILED_STATUS, false),
        };
        if quiet && background {
            return 0;
        }
        console.err(&line);
        u32::from(status)
    }

    /// The same end, kept quiet by `--background`: at the late recheck, a routing file that cannot be read is no
    /// consent.
    fn quiet(self) -> End {
        match self {
            End::Usage(line) | End::Failed(line) => End::Quiet(line),
            end => end,
        }
    }
}

/// What the warm-up is of: the state root, the facts of the worktree root, its target and the warm-up file as first
/// read.
struct Prepared<'e> {
    root: ResolvedRoot,
    environment: &'e Environment,
    machine: Machine<'e>,
    target: Target,
    first: Warmup,
    /// The warm-up's drain session, made once for every call it makes.
    session: String,
}

/// A read of the warm-up file: what it declares, the hash of its bytes and its commands worded for the builder.
struct Warmup {
    file: WarmupFile,
    hash: String,
    commands: Vec<Vec<String>>,
}

/// The state root, the worktree root entered, its repo's consent, its target and the file's first read; else how it
/// ends. Consent is checked before the target, on the repo name git gives, so a repo without consent ends as an
/// unconsented one does whatever its siblings.
fn prepare<'e>(request: &WarmRequest, environment: &'e Environment) -> Result<Prepared<'e>, End> {
    let root = resolve::root(request.state_dir.as_deref(), environment).map_err(End::Usage)?;
    enter(request.cd.as_deref(), Spelling::Cli).map_err(End::Usage)?;
    let (machine, place) = at_root(environment, &request.test_seams)?;
    let repo = repo_name(&place.checkout);
    let config = routing(environment)?;
    consented(&config, &repo)?;
    machine.hold_config(&config);
    let target = target(&config, place, &machine).map_err(|reason| End::Usage(reason.map_or_else(|| not_listed(&repo), |reason| reason.line())))?;
    let first = read_file(&target, &machine, &config)?;
    Ok(Prepared {
        root,
        environment,
        machine,
        target,
        first,
        session: remote::payload::run_nonce(),
    })
}

/// The facts of the worktree root and its place, the gate's working folder made the root when it was started below it:
/// a warm-up is worded and targeted from the root, whose key alone names the work folder.
fn at_root<'e>(environment: &'e Environment, seams: &TestSeams) -> Result<(Machine<'e>, Place), End> {
    let machine = Machine::new(environment, seams);
    let place = placed(&machine)?;
    if place.prefix.is_empty() {
        return Ok((machine, place));
    }
    std::env::set_current_dir(&place.root)
        .map_err(|error| End::Usage(format!("gate: warm: cannot enter the worktree root {}: {error}", escaped(&place.root))))?;
    let machine = Machine::new(environment, seams);
    let place = placed(&machine)?;
    Ok((machine, place))
}

/// The gate's place in its git worktree, as git gives it.
fn placed(machine: &Machine<'_>) -> Result<Place, End> {
    match machine.place() {
        Ok(Some(place)) => Ok(place),
        Ok(None) => Err(End::Usage("gate: warm: the folder is not in a git worktree".to_owned())),
        Err(why) => Err(End::Usage(format!("gate: warm: cannot find the git worktree: {}", escaped(&why)))),
    }
}

/// The routing file; else the line of one that is missing or cannot be used.
fn routing(environment: &Environment) -> Result<Config, End> {
    let Some(path) = route::remote_json_path(environment.local_app_data.as_deref()) else {
        return Err(End::Usage(
            "gate: warm: LOCALAPPDATA is unset or not absolute, so there is no remote.json".to_owned(),
        ));
    };
    match route::read_remote_json(&path) {
        Ok(Some(config)) => Ok(config),
        Ok(None) => Err(End::Usage(format!("gate: warm: {} does not exist", shown(&path)))),
        Err(problem) => Err(End::Usage(format!(
            "gate: warm: {} cannot be used: {}",
            shown(&problem.path),
            escaped(&problem.reason)
        ))),
    }
}

/// A path as a line says it, escaped.
fn shown(path: &Path) -> String {
    escaped(&path.to_string_lossy())
}

/// A client's line as a record's reason, which never starts with `gate: `.
fn reason_of(line: String) -> String {
    match line.strip_prefix("gate: ") {
        Some(rest) => rest.to_owned(),
        None => line,
    }
}

/// The line of a repo the routing file does not list.
fn not_listed(repo: &str) -> String {
    format!("gate: warm: {} is not listed in remote.json", escaped(repo))
}

/// Whether the routing file lets `repo` warm up; a `warmup` value that cannot be read is no consent.
fn consented(config: &Config, repo: &str) -> Result<(), End> {
    match config.warmup_consented(repo) {
        Ok(true) => Ok(()),
        Ok(false) => Err(End::Quiet(format!(
            "gate: warm: {} has not consented to warm-ups in remote.json",
            escaped(repo)
        ))),
        Err(reason) => Err(End::Quiet(format!(
            "gate: warm: remote.json's warmup cannot be read: {}",
            escaped(reason)
        ))),
    }
}

/// The warm-up file of `target`'s worktree read through the declarations reader, with the declarations folder `config`
/// names, and its commands worded for the builder; else how the warm-up ends: no file, a file problem, or a command
/// whose words name a path outside the worktree. A line about a file read from the declarations folder names its full
/// path.
fn read_file(target: &Target, machine: &Machine<'_>, config: &Config) -> Result<Warmup, End> {
    let (read, source) = read_warmup_file(Path::new(&target.place.root), &target.repo, Some(config));
    let said = match &source.origin {
        Origin::Repo => FILE_SAID.to_owned(),
        Origin::Declarations(path) => path.to_string_lossy().into_owned(),
    };
    let (file, hash) = match read {
        FileRead::Parsed { file, hash } => (file, hash),
        FileRead::Absent => {
            let root = target.place.root.replace('/', "\\");
            let unread = source.unusable.as_ref().map_or_else(String::new, |why| {
                format!("; declarations for {} not read: {}", escaped(&target.repo), escaped(why))
            });
            return Err(End::Quiet(format!("gate: warm: {}\\{FILE_SAID} does not exist{unread}", escaped(&root))));
        }
        FileRead::Problem(problem) => return Err(file_problem(&said, &problem)),
    };
    let commands = worded(target, &file, machine, &said)?;
    Ok(Warmup { file, hash, commands })
}

/// The warm-up file of the worktree at `root`, read through the declarations reader ([`route::read_declaration`]) with
/// the declarations folder `config` names for `repo`, parsed and hashed: absent, its first problem ([`first_problem`]),
/// or what it declares with the [`warmup_hash`] of its bytes, wherever they came from; and where the read went. `gate
/// warm` and the route read the file through this.
pub(crate) fn read_warmup_file(root: &Path, repo: &str, config: Option<&Config>) -> (FileRead, Source) {
    let (bytes, source) = route::read_declaration(root, repo, config, WARMUP_NAME);
    let read = match warmup_from(bytes) {
        Ok(None) => FileRead::Absent,
        Ok(Some((file, hash))) => FileRead::Parsed { file, hash },
        Err(found) => FileRead::Problem(first_problem(&found.problems)),
    };
    (read, source)
}

/// The problems of a `.gate\warmup.toml` that did not read, and whether it cannot be read whole: the `.gate` reader
/// refused it, or it is not UTF-8 or not TOML, rather than a TOML file with problems of its keys or commands.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WarmupProblems {
    /// Every problem, in file order.
    pub problems: Vec<String>,
    /// Whether the file cannot be read whole.
    pub unreadable: bool,
}

/// The warm-up file a read of it gave, `bytes` ([`route::read_declaration`]): `None` when it is absent, else what it
/// declares with the [`warmup_hash`] of its bytes. `gate check` and [`read_warmup_file`] read it through this, its
/// problems also saying whether it cannot be read whole.
///
/// # Errors
///
/// Every problem of the file, in file order, and whether it cannot be read whole: one when it cannot be read or is not
/// UTF-8 or not TOML.
pub(crate) fn warmup_from(bytes: Result<Option<Vec<u8>>, String>) -> Result<Option<(WarmupFile, String)>, WarmupProblems> {
    let bytes = match bytes {
        Ok(None) => return Ok(None),
        Ok(Some(bytes)) => bytes,
        Err(problem) => {
            return Err(WarmupProblems {
                problems: vec![problem],
                unreadable: true,
            });
        }
    };
    match read_warmup(&bytes) {
        Ok(file) => Ok(Some((file, warmup_hash(&bytes)))),
        Err(problems) => Err(WarmupProblems {
            problems,
            unreadable: !reads_as_toml(&bytes),
        }),
    }
}

/// Whether `bytes`, a leading byte order mark skipped as [`read_warmup`] skips it, are UTF-8 that parses as a TOML
/// table.
fn reads_as_toml(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    std::str::from_utf8(bytes).is_ok_and(|text| text.parse::<toml::Table>().is_ok())
}

/// The warm-up record at `path` as the route reads it: absent when nothing is there; else opened without following a
/// link, sharing everything so a writer's replace is never blocked, required to be a plain file, read up to
/// [`RECORD_MOST_BYTES`] and parsed strictly ([`v1::warm_record`]). A record that is there but fails any of these is
/// unreadable, with why.
pub(crate) fn read_warm_record(path: &Path) -> RecordRead {
    let unreadable = |error: String| RecordRead::Unreadable {
        path: path.to_string_lossy().into_owned(),
        error,
    };
    let opened = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    let file = match opened {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return RecordRead::Absent,
        Err(error) => return unreadable(error.to_string()),
        Ok(file) => file,
    };
    if let Err(error) = gate_win32::check_plain_file(&file) {
        return unreadable(error.to_string());
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.take(RECORD_MOST_BYTES + 1).read_to_end(&mut bytes) {
        return unreadable(error.to_string());
    }
    if bytes.len() as u64 > RECORD_MOST_BYTES {
        return unreadable(format!("is larger than {} KiB", RECORD_MOST_BYTES / 1024));
    }
    match v1::warm_record(&bytes) {
        Ok(record) => record_of(record).map_or_else(unreadable, RecordRead::Parsed),
        Err(error) => unreadable(error.to_string()),
    }
}

/// The router's [`Record`] of a record the strict reader passed; a failed record without its failure or reason, which
/// that reader refuses, is a problem all the same.
fn record_of(record: WarmRecord) -> Result<Record, String> {
    let state = match (record.state, record.failure, record.reason) {
        (WarmState::Warming, ..) => RecordState::Warming,
        (WarmState::Warm, ..) => RecordState::Warm,
        (WarmState::Failed, Some(failure), Some(reason)) => RecordState::Failed {
            class: match failure {
                WarmFailure::Commands => FailureClass::Commands,
                WarmFailure::Refused => FailureClass::Refused,
            },
            reason,
        },
        (WarmState::Failed, ..) => return Err("state failed without failure or reason".to_owned()),
    };
    Ok(Record {
        host: record.host,
        state,
        at: SystemTime::from(record.at),
        hash: record.hash,
        gate: record.gate,
    })
}

/// Whether the warm-up lock at `path` is held, from one open for reading that shares everything and does not follow a
/// link, closed at once: a sharing or lock violation is held, and a lock that is not there, or that opens as a plain file, is free. The
/// probe never takes the lock, and a warm-up whose own open meets it tries again ([`LOCK_RETRY`]).
///
/// # Errors
///
/// Why the lock cannot be opened, for any other failure, or why what opened is not a plain file (a link or a folder).
pub(crate) fn lock_held(path: &Path) -> io::Result<bool> {
    let opened = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    match opened {
        Ok(file) => gate_win32::check_plain_file(&file).map(|()| false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => Ok(true),
        Err(error) => Err(error),
    }
}

/// The line of a problem of the warm-up file, named as `said`.
fn file_problem(said: &str, problem: &str) -> End {
    End::Usage(format!("gate: warm: {} {}", escaped(said), escaped(problem)))
}

/// The first of the file's `problems`, with how many more there are when there are more.
fn first_problem(problems: &[String]) -> String {
    match problems {
        [] => "has a problem".to_owned(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} (and {} more)", rest.len()),
    }
}

/// The commands of `file` as the builder of `target` runs them, as a routed run's words are made; a command a routed
/// run keeps on this PC ([`stays_local`]) and a command whose words, as the rewrite leaves them, name a path outside
/// the worktree and its siblings are problems of the file. A first word naming a program by such a path is sent as the
/// program's name, as a routed run sends it, and is no problem unless that name is one this PC keeps too. A problem's
/// line names the file as `said`.
fn worded(target: &Target, file: &WarmupFile, machine: &Machine<'_>, said: &str) -> Result<Vec<Vec<String>>, End> {
    file.commands
        .iter()
        .enumerate()
        .map(|(at, command)| {
            let first = command.iter().find(|word| !word.is_empty()).map_or("", String::as_str);
            if stays_local(command) {
                return Err(never_sent(said, at, first));
            }
            let sent = builder_words(target, command, machine);
            if sent.program_named && stays_local(&sent.words) {
                return Err(never_sent(said, at, first));
            }
            match sent.outside {
                Some(word) => Err(End::Usage(format!(
                    "gate: warm: {} command {} names {}, outside the worktree",
                    escaped(said),
                    at + 1,
                    escaped(&word)
                ))),
                None => Ok(sent.words),
            }
        })
        .collect()
}

/// The end of a command of the warm-up file a routed run never sends: a command this PC keeps ([`stays_local`]), by its
/// first word as given or by the program's name the rewrite sends for it; the file named as `said`.
fn never_sent(said: &str, at: usize, first: &str) -> End {
    End::Usage(format!(
        "gate: warm: {} command {} starts with {}, which a routed run never sends",
        escaped(said),
        at + 1,
        escaped(first)
    ))
}

/// How a started warm-up ends.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    /// The builder confirmed the warm-up of the file of `hash`.
    Warm { hash: String },
    /// The warm-up of the file of `hash` failed, of the class `failure`, for `reason`.
    Failed { hash: String, failure: WarmFailure, reason: String },
    /// Ctrl+C ended it: the record is deleted.
    Deleted,
    /// The builder is drained and refused it: the record is deleted, and the end says so.
    Drained,
    /// The late recheck found no consent, no file, a file problem or another builder: the record is deleted, and the
    /// end says why.
    Withdrawn(End),
}

/// Takes the warm-up lock, writes the `warming` record, runs the warm-up and writes how it ended.
fn warm_up(prepared: &Prepared<'_>, request: &WarmRequest, started: Timestamp, console: &mut Console) -> u32 {
    let state = &prepared.root.path;
    let key = &prepared.target.key;
    let host = &prepared.target.builder.alias;
    if let Err(error) = fs::create_dir_all(v1::warm_dir(state)) {
        let line = format!("gate: warm: cannot make {}: {error}", shown(&v1::warm_dir(state)));
        return End::Failed(line).say(request.background, console);
    }
    let lock_path = v1::warm_lock_path(state, key);
    let _lock = match take_lock(&lock_path, LOCK_RETRY) {
        Lock::Taken(file) => file,
        Lock::Held => {
            let line = format!("gate: a warm-up of {} is already running", escaped(host));
            return End::Running(line).say(request.background, console);
        }
        Lock::Refused(why) => {
            let line = format!("gate: warm: cannot take {}: {}", shown(&lock_path), escaped(&why));
            return End::Failed(line).say(request.background, console);
        }
    };
    leave_worktree_record(prepared, console);
    let log_path = v1::warm_log_path(state, key);
    let _log = match fresh_log(&log_path).and_then(|log| noted(log, &log_path, &prepared.machine.unsaid_notes())) {
        Ok(log) => log,
        Err(line) => return End::Failed(line).say(request.background, console),
    };
    let record = Recorder {
        path: v1::warm_record_path(state, key),
        host,
        started,
    };
    if let Err(line) = record.write(WarmState::Warming, &prepared.first.hash, None) {
        return End::Failed(line).say(request.background, console);
    }
    let interrupt = Interrupt::install()
        .inspect_err(|error| {
            console.err(&format!(
                "gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: {error}"
            ));
        })
        .ok();
    let mut held = Held::default();
    let ending = attempt(prepared, &request.test_seams, interrupt, &mut held);
    let status = conclude(ending, &record, request.background, console);
    drop(held);
    status
}

/// Writes the worktree record of the warm-up's key ([`worktree_record::leave`]) under the key's mutex, the one a routed
/// run writes it under, taken without waiting and let go at once. A mutex another gate holds writes none and says
/// nothing: its routed run writes the record. A mutex that cannot be opened or taken is said, and writes none.
fn leave_worktree_record(prepared: &Prepared<'_>, console: &mut Console) {
    let target = &prepared.target;
    match worktree_record::try_hold_key(&prepared.root.root, &target.key) {
        Ok(Some(_key)) => worktree_record::leave(&prepared.root.path, &target.place.root, &target.repo, &target.key, console),
        Ok(None) => {}
        Err(line) => console.err(&format!("gate: {line}")),
    }
}

/// The key's mutex and the cap a warm-up holds until its record is written.
#[derive(Default)]
struct Held {
    key: Option<OwnedMutexGuard>,
    cap: Option<File>,
}

/// Runs the warm-up once its record says `warming`: the client chosen and ready, the key's mutex and the cap waited
/// for into `held`, consent, the routing entry and the file read again, then the client's warm call; how it ended.
fn attempt(prepared: &Prepared<'_>, seams: &TestSeams, interrupt: Option<Interrupt>, held: &mut Held) -> Ending {
    let first = &prepared.first;
    let mut client = match remote::chosen(seams) {
        Ok(client) => client,
        Err(line) => return refused(&first.hash, line),
    };
    if let Err(why) = client.ready() {
        return refused(&first.hash, why);
    }
    let wait = seams
        .warm_wait_ms
        .map_or_else(|| Duration::from_secs(prepared.first.file.timeout_seconds), Duration::from_millis);
    let log = v1::warm_log_path(&prepared.root.path, &prepared.target.key);
    let early = Warmth::WarmUp {
        commands: &first.commands,
        hash: &first.hash,
    };
    let key = call_of(prepared, &log, interrupt, first.file.timeout_seconds, early)
        .map_err(|why| refused(&first.hash, why))
        .and_then(|call| held_key(client.as_ref(), &call, wait, &first.hash));
    match key {
        Ok(guard) => held.key = guard,
        Err(ending) => return ending,
    }
    match held_cap(&v1::warm_cap_lock_path(&prepared.root.path), wait, interrupt, &first.hash) {
        Ok(cap) => held.cap = Some(cap),
        Err(ending) => return ending,
    }
    let late = match recheck(prepared) {
        Ok(late) => late,
        Err(end) => return Ending::Withdrawn(end),
    };
    let warmth = Warmth::WarmUp {
        commands: &late.commands,
        hash: &late.hash,
    };
    match call_of(prepared, &log, interrupt, late.file.timeout_seconds, warmth) {
        Ok(call) => ending_of(&client.run(&call), &late.hash),
        Err(why) => refused(&late.hash, why),
    }
}

/// The ending of a warm-up of the file of `hash` refused for `reason`, said as a record's reason.
fn refused(hash: &str, reason: String) -> Ending {
    Ending::Failed {
        hash: hash.to_owned(),
        failure: WarmFailure::Refused,
        reason: reason_of(reason),
    }
}

/// The cap of one warm-up at a time, waited for up to `wait`: its lock file; else the ending of a warm-up of the file
/// of `hash` that did not get it.
fn held_cap(path: &Path, wait: Duration, interrupt: Option<Interrupt>, hash: &str) -> Result<File, Ending> {
    match wait_for_cap(path, wait, interrupt) {
        Cap::Taken(file) => Ok(file),
        Cap::OutOfTime => Err(refused(hash, CAP_BUSY.to_owned())),
        Cap::Interrupted => Err(Ending::Deleted),
        Cap::Refused(why) => Err(refused(hash, why)),
    }
}

/// Makes the warm-up's log at `path` new: an earlier one removed, as a run's log is, then created as a plain file
/// without following a link, and held open sharing reading and writing but not deletion, so nothing can put a link in
/// its place while the client appends to it by its path; else the line saying why it cannot be.
fn fresh_log(path: &Path) -> Result<File, String> {
    let cannot = |what: &str, error: &dyn std::fmt::Display| format!("gate: warm: cannot {what} warm-up log {}: {error}", shown(path));
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(cannot("remove the earlier", &error)),
        _ => {}
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| cannot("create the", &error))?;
    gate_win32::check_plain_file(&file).map_err(|error| cannot("use the", &error))?;
    Ok(file)
}

/// The warm-up's new log `log`, at `path`, with `notes` written into it first, each a line, as a routed run's log gets
/// its decision's notes; else the line saying why they cannot be written.
fn noted(mut log: File, path: &Path, notes: &[String]) -> Result<File, String> {
    let mut text = String::new();
    for note in notes {
        text.push_str(note);
        text.push('\n');
    }
    log.write_all(text.as_bytes())
        .map_err(|error| format!("gate: warm: cannot write the warm-up log {}: {error}", shown(path)))?;
    Ok(log)
}

/// The key's mutex, waited for up to `wait`: the guard, `None` for a client that takes none; else the ending of a
/// warm-up of the file of `hash` that did not get it.
fn held_key(client: &dyn RemoteClient, call: &RemoteCall<'_>, wait: Duration, hash: &str) -> Result<Option<OwnedMutexGuard>, Ending> {
    match client.hold_key_waiting(call, wait) {
        Ok(KeyWait::Held(guard)) => Ok(guard),
        Ok(KeyWait::Busy) => Err(refused(hash, KEY_BUSY.to_owned())),
        Ok(KeyWait::Interrupted) => Err(Ending::Deleted),
        Err(line) => Err(refused(hash, line)),
    }
}

/// Consent, the routing entry and the file read again, just before the builder is called: the file's late read, whose
/// bytes alone give the commands, their hash and the record; else why the warm-up is withdrawn. Consent is checked
/// first, as it is before the first read; a target that cannot be read says its own line, as `prepare` says it, and a
/// sibling list that changed says so on a line of its own.
fn recheck(prepared: &Prepared<'_>) -> Result<Warmup, End> {
    let early = &prepared.target;
    let config = routing(prepared.environment).map_err(End::quiet)?;
    consented(&config, &early.repo)?;
    if config.host() != early.builder.alias {
        return Err(End::Quiet(format!(
            "gate: warm: remote.json's builder changed from {} to {}",
            escaped(&early.builder.alias),
            escaped(config.host())
        )));
    }
    let changed = || End::Quiet(format!("gate: warm: remote.json's entry for {} changed", escaped(&early.repo)));
    prepared.machine.hold_config(&config);
    let late = match target(&config, early.place.clone(), &prepared.machine) {
        Ok(late) => late,
        Err(Some(reason)) => return Err(End::Usage(reason.line())),
        Err(None) => return Err(changed()),
    };
    if late.siblings != early.siblings {
        return Err(End::Quiet(format!(
            "gate: warm: the siblings of {} changed while the warm-up waited",
            escaped(&early.repo)
        )));
    }
    if late.key != early.key || late.inputs != early.inputs {
        return Err(changed());
    }
    read_file(early, &prepared.machine, &config)
}

/// The client's warm call of `prepared`'s target, from the worktree root, writing `log`, under the ceiling `timeout`.
///
/// # Errors
///
/// Why a sibling the target carries fails its check again ([`remote::carried`]).
fn call_of<'a>(
    prepared: &'a Prepared<'_>,
    log: &'a Path,
    interrupt: Option<Interrupt>,
    timeout: u64,
    warmth: Warmth<'a>,
) -> Result<RemoteCall<'a>, String> {
    let target = &prepared.target;
    let siblings = remote::carried(&target.place.root, &target.siblings)?;
    Ok(RemoteCall {
        builder: &target.builder,
        key: &target.key,
        session: &prepared.session,
        root: &target.place.root,
        root_id: target.root_id,
        leaf: &target.leaf,
        siblings,
        repo: &target.repo,
        subfolder: "",
        words: &[],
        outputs: &[],
        outputs_withheld: false,
        failure_outputs: &[],
        dropped_no_build: false,
        send: None,
        desktop: false,
        inputs: &target.inputs,
        bring_tracked: &[],
        env: &[],
        log,
        timeout,
        usual: None,
        stall: DEFAULT_STALL_SECONDS,
        tail: 0,
        no_markers: false,
        interrupt,
        state: &prepared.root.root,
        state_dir: &prepared.root.path,
        warmth,
    })
}

/// How the warm-up of the file of `hash` ended, from how the client said it ended alone, never from its status: a call
/// that said nothing of it did not start, and is refused for the client's detail. A call the builder's drain refused
/// is drained, whatever else it said.
fn ending_of(outcome: &RemoteOutcome, hash: &str) -> Ending {
    if outcome.draining.is_some() {
        return Ending::Drained;
    }
    let hash = hash.to_owned();
    match &outcome.warm {
        Some(WarmEnd::Warm) => Ending::Warm { hash },
        Some(WarmEnd::Failed { failure, reason }) => Ending::Failed {
            hash,
            failure: *failure,
            reason: reason.clone(),
        },
        Some(WarmEnd::Deleted) => Ending::Deleted,
        None => Ending::Failed {
            hash,
            failure: WarmFailure::Refused,
            reason: outcome.detail.clone().unwrap_or_else(|| NOT_STARTED.to_owned()),
        },
    }
}

/// Writes or deletes the record as the warm-up ended, says its verdict and returns the status. A final write that
/// fails for a warm-up that ran deletes the `warming` record the warm-up wrote first, saying the write's line and, when
/// the delete fails too, its line as well, so a failed write leaves no record behind.
fn conclude(ending: Ending, record: &Recorder<'_>, background: bool, console: &mut Console) -> u32 {
    let host = escaped(record.host);
    let replaces = matches!(&ending, Ending::Warm { .. } | Ending::Failed { .. });
    let written = match ending {
        Ending::Warm { hash } => record.write(WarmState::Warm, &hash, None).map(|()| {
            console.err(&format!("gate: warm-up of {host} passed"));
            0
        }),
        Ending::Failed { hash, failure, reason } => record.write(WarmState::Failed, &hash, Some((failure, reason.clone()))).map(|()| {
            console.err(&format!("gate: warm-up of {host} failed: {}", escaped(&reason)));
            u32::from(FAILED_STATUS)
        }),
        Ending::Deleted => record.delete().map(|()| CONTROL_C_EXIT),
        Ending::Drained => record.delete().map(|()| End::Failed(DRAINED.to_owned()).say(background, console)),
        Ending::Withdrawn(end) => record.delete().map(|()| end.say(background, console)),
    };
    match written {
        Ok(status) => status,
        Err(line) => {
            console.err(&line);
            if replaces && let Err(why) = record.delete() {
                console.err(&why);
            }
            u32::from(FAILED_STATUS)
        }
    }
}

/// The warm-up record of the key: where it is, the builder it names and this process's start.
struct Recorder<'a> {
    path: PathBuf,
    host: &'a str,
    started: Timestamp,
}

impl Recorder<'_> {
    /// Writes the record in `state` for the file of `hash`, with the failure's class and reason when it failed; else
    /// the line saying why it cannot be written.
    fn write(&self, state: WarmState, hash: &str, failed: Option<(WarmFailure, String)>) -> Result<(), String> {
        let (failure, reason) = failed.map_or((None, None), |(failure, reason)| (Some(failure), Some(reason)));
        let record = WarmRecord {
            host: self.host.to_owned(),
            state,
            at: Timestamp::now(),
            hash: hash.to_owned(),
            gate: build_version(),
            pid: std::process::id(),
            started: self.started,
            failure,
            reason,
        };
        v1::write_warm_record(&self.path, &record)
            .map_err(|error| format!("gate: warm: cannot write the warm-up record {}: {error}", shown(&self.path)))
    }

    /// Deletes the record; one already gone is deleted.
    fn delete(&self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(format!("gate: warm: cannot delete the warm-up record {}: {error}", shown(&self.path)))
            }
            _ => Ok(()),
        }
    }
}

/// How an open of a lock file ended.
enum Lock {
    /// Taken: held while the file is open.
    Taken(File),
    /// Another handle holds it.
    Held,
    /// It cannot be opened, or is not a plain file: why.
    Refused(String),
}

/// Takes the lock file at `path`, made when it is missing: opened for reading and writing, sharing nothing, without
/// following a link, and required to be a plain file. An open another handle refuses is tried again every
/// [`LOCK_STEP`] until `retry` has passed.
fn take_lock(path: &Path, retry: Duration) -> Lock {
    let begun = Instant::now();
    loop {
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path);
        match opened {
            Ok(file) => {
                return match gate_win32::check_plain_file(&file) {
                    Ok(()) => Lock::Taken(file),
                    Err(error) => Lock::Refused(error.to_string()),
                };
            }
            Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => {
                if begun.elapsed() >= retry {
                    return Lock::Held;
                }
                thread::sleep(LOCK_STEP);
            }
            Err(error) => return Lock::Refused(error.to_string()),
        }
    }
}

/// How the wait for the warm-up cap ended.
enum Cap {
    Taken(File),
    /// Another warm-up still held it at the end of the wait.
    OutOfTime,
    /// The console interrupt was raised during the wait.
    Interrupted,
    /// It cannot be opened, or is not a plain file: why.
    Refused(String),
}

/// The cap of one warm-up at a time at `path`, waited for up to `wait` in slices of [`CAP_SLICE`], the interrupt
/// looked at between them.
fn wait_for_cap(path: &Path, wait: Duration, interrupt: Option<Interrupt>) -> Cap {
    let begun = Instant::now();
    loop {
        match take_lock(path, Duration::ZERO) {
            Lock::Taken(file) => return Cap::Taken(file),
            Lock::Refused(why) => return Cap::Refused(format!("cannot take {}: {why}", shown(path))),
            Lock::Held => {}
        }
        if interrupt.is_some_and(Interrupt::is_raised) {
            return Cap::Interrupted;
        }
        let left = wait.saturating_sub(begun.elapsed());
        if left.is_zero() {
            return Cap::OutOfTime;
        }
        thread::sleep(left.min(CAP_SLICE));
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use gate_router::warm::{FailureClass, FileRead, Record, RecordRead, RecordState};
    use gate_state::FallbackReason;
    use gate_state::v1::{self, WarmFailure, WarmRecord, WarmState};
    use jiff::Timestamp;

    use super::{
        Ending, Lock, NOT_STARTED, WARMUP_NAME, ending_of, first_problem, fresh_log, lock_held, read_warm_record, read_warmup_file, take_lock,
        warmup_from,
    };
    use crate::remote::{RemoteOutcome, WarmEnd};
    use crate::route;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A scratch folder for one test, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-warm-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("an earlier scratch folder can be removed");
        }
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
    }

    fn ended(warm: Option<WarmEnd>, status: i64, detail: Option<&str>) -> Ending {
        let outcome = RemoteOutcome {
            status,
            warm,
            detail: detail.map(str::to_owned),
            ..RemoteOutcome::default()
        };
        ending_of(&outcome, HASH)
    }

    fn failed(failure: WarmFailure, reason: &str) -> Ending {
        Ending::Failed {
            hash: HASH.to_owned(),
            failure,
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn each_warm_end_the_client_says_is_the_record() {
        assert_eq!(ended(Some(WarmEnd::Warm), 0, None), Ending::Warm { hash: HASH.to_owned() });
        let commands = WarmEnd::Failed {
            failure: WarmFailure::Commands,
            reason: "command 1 stopped with status 3".to_owned(),
        };
        assert_eq!(
            ended(Some(commands), 3, None),
            failed(WarmFailure::Commands, "command 1 stopped with status 3")
        );
        let refused = WarmEnd::Failed {
            failure: WarmFailure::Refused,
            reason: "the builder is busy".to_owned(),
        };
        assert_eq!(ended(Some(refused), 75, None), failed(WarmFailure::Refused, "the builder is busy"));
        assert_eq!(ended(Some(WarmEnd::Deleted), 0, None), Ending::Deleted);
    }

    #[test]
    fn a_call_that_says_no_warm_end_is_refused_for_its_detail_whatever_its_status() {
        assert_eq!(
            ended(None, 75, Some("cannot find ssh")),
            failed(WarmFailure::Refused, "cannot find ssh"),
            "a client that could not start"
        );
        assert_eq!(
            ended(None, 0, None),
            failed(WarmFailure::Refused, NOT_STARTED),
            "status 0 alone is not warm"
        );
        let refused = RemoteOutcome {
            status: 75,
            reason: Some(FallbackReason::Busy),
            ..RemoteOutcome::default()
        };
        assert_eq!(
            ending_of(&refused, HASH),
            failed(WarmFailure::Refused, NOT_STARTED),
            "a refusal with no warm end"
        );
    }

    #[test]
    fn a_free_lock_is_taken_and_held_against_the_next_open() {
        let lock = scratch("free").join("k.lock");
        let taken = take_lock(&lock, Duration::ZERO);
        assert!(matches!(taken, Lock::Taken(_)), "a missing lock is made and taken");
        assert!(
            matches!(take_lock(&lock, Duration::from_millis(40)), Lock::Held),
            "a taken lock is held against another open"
        );
        drop(taken);
        assert!(matches!(take_lock(&lock, Duration::ZERO), Lock::Taken(_)), "a lock let go is free again");
    }

    #[test]
    fn a_lock_another_handle_holds_is_held() {
        let lock = scratch("held").join("k.lock");
        let other = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(&lock)
            .expect("another handle takes the lock");
        assert!(matches!(take_lock(&lock, Duration::from_millis(40)), Lock::Held));
        drop(other);
    }

    #[test]
    fn a_junction_at_the_log_name_is_refused_and_never_written_through() {
        let folder = scratch("log-junction");
        let target = folder.join("target");
        fs::create_dir(&target).expect("the junction's target is made");
        let log = folder.join("k.log");
        crate::test_support::junction(&log, &target);
        assert!(fresh_log(&log).is_err(), "a junction is no log");
        assert_eq!(
            fs::read_dir(&target).expect("the target can be listed").count(),
            0,
            "nothing is written through it"
        );
    }

    #[test]
    fn a_hard_link_at_the_log_name_is_replaced_by_a_plain_file() {
        let folder = scratch("log-hard-link");
        let other = folder.join("other.txt");
        fs::write(&other, "keep").expect("the linked file is written");
        let log = folder.join("k.log");
        fs::hard_link(&other, &log).expect("a hard link is made");
        let held = fresh_log(&log).expect("the earlier log is removed and a new one made");
        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"line\n"))
            .expect("the client can append to the held log by its path");
        assert_eq!(
            fs::read_to_string(&other).expect("the linked file is read"),
            "keep",
            "never written through"
        );
        assert_eq!(fs::read_to_string(&log).expect("the log is read"), "line\n");
        assert!(fs::remove_file(&log).is_err(), "the held log cannot be removed or replaced");
        drop(held);
    }

    #[test]
    fn a_file_problem_is_the_first_and_a_count_of_the_rest() {
        let problems = |list: &[&str]| first_problem(&list.iter().map(|text| (*text).to_owned()).collect::<Vec<_>>());
        assert_eq!(problems(&["a"]), "a");
        assert_eq!(problems(&["a", "b", "c"]), "a (and 2 more)");
    }

    #[test]
    fn the_whole_read_of_a_file_with_two_problems_says_both_in_file_order() {
        let root = scratch("file-two-problems");
        fs::create_dir_all(root.join(".gate")).expect("the .gate folder is made");
        fs::write(root.join(".gate").join("warmup.toml"), "commands = [[]]\nextra = 1\n").expect("the file is written");
        let whole = |root: &Path| warmup_from(route::read_declaration(root, "g", None, WARMUP_NAME).0).map_err(|found| found.problems);
        assert_eq!(
            whole(&root),
            Err(vec![
                "commands[0] is empty".to_owned(),
                "extra is not a key (commands, timeout)".to_owned()
            ])
        );
        assert_eq!(
            read_warmup_file(&root, "g", None).0,
            FileRead::Problem("commands[0] is empty (and 1 more)".to_owned()),
            "the route's read keeps the first"
        );
        assert_eq!(whole(&scratch("file-absent")), Ok(None), "no file");
    }

    #[test]
    fn a_missing_record_is_absent() {
        let folder = scratch("record-missing");
        assert_eq!(read_warm_record(&folder.join("k.json")), RecordRead::Absent);
        assert_eq!(
            read_warm_record(&folder.join("gone").join("k.json")),
            RecordRead::Absent,
            "nor is its folder there"
        );
    }

    /// Whether `read` is an unreadable record at `path`.
    fn is_unreadable(read: &RecordRead, path: &Path) -> bool {
        matches!(read, RecordRead::Unreadable { path: said, .. } if *said == path.to_string_lossy())
    }

    #[test]
    fn a_record_that_is_a_junction_or_a_folder_is_a_read_problem() {
        let folder = scratch("record-not-a-file");
        let target = folder.join("target");
        fs::create_dir(&target).expect("the junction's target is made");
        let linked = folder.join("linked.json");
        crate::test_support::junction(&linked, &target);
        assert!(is_unreadable(&read_warm_record(&linked), &linked), "a junction");
        let made = folder.join("folder.json");
        fs::create_dir(&made).expect("a folder named like a record is made");
        assert!(is_unreadable(&read_warm_record(&made), &made), "a folder");
    }

    #[test]
    fn an_over_cap_or_malformed_record_is_a_read_problem() {
        let path = scratch("record-bad").join("k.json");
        fs::write(&path, " ".repeat(64 * 1024 + 1)).expect("an over-cap record is written");
        assert_eq!(
            read_warm_record(&path),
            RecordRead::Unreadable {
                path: path.to_string_lossy().into_owned(),
                error: "is larger than 64 KiB".to_owned(),
            }
        );
        for text in ["{", "{}", r#"{"v": 1, "host": "b"}"#, "\u{feff}{}"] {
            fs::write(&path, text).expect("a malformed record is written");
            assert!(is_unreadable(&read_warm_record(&path), &path), "{text}");
        }
    }

    #[test]
    fn a_written_record_reads_as_the_router_s_record() {
        let path = scratch("record-written").join("k.json");
        let at = Timestamp::from_second(1_700_000_000).expect("a time in range");
        let record = WarmRecord {
            host: "builder".to_owned(),
            state: WarmState::Failed,
            at,
            hash: HASH.to_owned(),
            gate: "1.2.3".to_owned(),
            pid: 7,
            started: at,
            failure: Some(WarmFailure::Refused),
            reason: Some("the builder is busy".to_owned()),
        };
        v1::write_warm_record(&path, &record).expect("the record is written");
        assert_eq!(
            read_warm_record(&path),
            RecordRead::Parsed(Record {
                host: "builder".to_owned(),
                state: RecordState::Failed {
                    class: FailureClass::Refused,
                    reason: "the builder is busy".to_owned(),
                },
                at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                hash: HASH.to_owned(),
                gate: "1.2.3".to_owned(),
            })
        );
    }

    #[test]
    fn a_lock_a_warm_up_holds_is_held_and_one_nobody_holds_is_free() {
        let folder = scratch("probe");
        let lock = folder.join("k.lock");
        assert!(!lock_held(&lock).expect("a missing lock can be probed"), "a missing lock is free");
        let taken = take_lock(&lock, Duration::ZERO);
        assert!(matches!(taken, Lock::Taken(_)), "the lock is taken as a warm-up takes it");
        assert!(lock_held(&lock).expect("a held lock can be probed"), "a taken lock is held");
        drop(taken);
        assert!(!lock_held(&lock).expect("a free lock can be probed"), "a lock let go is free");
        assert!(
            matches!(take_lock(&lock, Duration::ZERO), Lock::Taken(_)),
            "the probe left the lock free to take"
        );
    }

    #[test]
    fn a_lock_that_is_a_folder_or_a_junction_cannot_be_probed() {
        let folder = scratch("probe-not-a-file");
        let target = folder.join("target");
        fs::create_dir(&target).expect("the junction's target is made");
        let made = folder.join("folder.lock");
        fs::create_dir(&made).expect("a folder named like the lock is made");
        let linked = folder.join("linked.lock");
        crate::test_support::junction(&linked, &target);
        for lock in [made, linked] {
            assert!(lock_held(&lock).is_err(), "a folder or a junction is no lock: {}", lock.display());
        }
    }

    #[test]
    fn a_lock_that_is_a_junction_is_refused() {
        let folder = scratch("junction");
        let target = folder.join("target");
        fs::create_dir(&target).expect("the junction's target is made");
        let lock = folder.join("k.lock");
        crate::test_support::junction(&lock, &target);
        assert!(matches!(take_lock(&lock, Duration::ZERO), Lock::Refused(_)), "a link is not followed");
    }
}
