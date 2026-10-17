//! The builder warm-up, over bytes and plain values: the repo's `.gate/warmup.toml` read ([`read_warmup`]), the hash
//! that ties a warm-up record to those bytes ([`warmup_hash`]), the judge of whether a command that would route goes to
//! the builder, runs here or starts a warm-up ([`judge`]), and the lines the gate says for each case that runs here.
//!
//! `.gate/warmup.toml` holds `commands`, an array of 1 to [`MOST_COMMANDS`] commands, each a non-empty array of
//! non-empty strings, and the optional `timeout`, whole seconds from 1 to [`MOST_TIMEOUT_SECONDS`],
//! [`DEFAULT_TIMEOUT_SECONDS`] when missing. Any other key is a problem, and any problem means no warm-up.

use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use toml::{Table, Value};

use crate::escape::escaped;

/// The most commands a warm-up file may declare.
pub const MOST_COMMANDS: usize = 8;
/// The warm-up's ceiling when the file sets none, in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
/// The longest ceiling a warm-up file may set, in seconds.
pub const MOST_TIMEOUT_SECONDS: u64 = 86_400;
/// How long after a warm-up whose commands failed no other starts.
pub const COMMANDS_BACKOFF: Duration = Duration::from_mins(30);
/// How long after a warm-up refused before any command ran no other starts.
pub const REFUSED_BACKOFF: Duration = Duration::from_secs(60);

/// The problem of a file that declares no command.
const NO_COMMANDS: &str = "declares no commands";
/// The problem of a timeout out of its range or not a whole number.
const BAD_TIMEOUT: &str = "timeout is not a whole number of seconds from 1 to 86400";

/// What a `.gate/warmup.toml` declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmupFile {
    /// The commands, in file order, each its words.
    pub commands: Vec<Vec<String>>,
    /// The ceiling of the whole warm-up, in seconds.
    pub timeout_seconds: u64,
}

/// The warm-up file whose bytes are `bytes`, a leading byte order mark skipped.
///
/// # Errors
///
/// Each problem, in file order, such as `commands[1] is empty`: one when the bytes are not UTF-8 or not TOML.
pub fn read_warmup(bytes: &[u8]) -> Result<WarmupFile, Vec<String>> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|error| vec![format!("not valid UTF-8 ({error})")])?;
    let table = text.parse::<Table>().map_err(|error| vec![parse_failure(text, &error)])?;
    let mut problems = Vec::new();
    let mut commands = None;
    let mut timeout_seconds = DEFAULT_TIMEOUT_SECONDS;
    for (key, value) in table {
        match key.as_str() {
            "commands" => commands = Some(read_commands(value, &mut problems)),
            "timeout" => match read_timeout(&value) {
                Some(seconds) => timeout_seconds = seconds,
                None => problems.push(BAD_TIMEOUT.to_owned()),
            },
            _ => problems.push(format!("{} is not a key (commands, timeout)", spelled(&key))),
        }
    }
    let Some(commands) = commands else {
        problems.push(NO_COMMANDS.to_owned());
        return Err(problems);
    };
    if problems.is_empty() {
        Ok(WarmupFile { commands, timeout_seconds })
    } else {
        Err(problems)
    }
}

/// The commands `value` declares, each problem pushed to `problems`: a value that is not an array, an empty array, and
/// each bad command; at the command past [`MOST_COMMANDS`] it says so and reads no further.
fn read_commands(value: Value, problems: &mut Vec<String>) -> Vec<Vec<String>> {
    let entries = match value {
        Value::Array(entries) => entries,
        other => {
            problems.push(format!("commands is not an array (TOML {})", other.type_str()));
            return Vec::new();
        }
    };
    if entries.is_empty() {
        problems.push(NO_COMMANDS.to_owned());
    }
    let mut commands = Vec::new();
    for (index, entry) in entries.into_iter().enumerate() {
        if index == MOST_COMMANDS {
            problems.push(format!("has more than {MOST_COMMANDS} commands"));
            break;
        }
        match command_of(index, entry) {
            Ok(words) => commands.push(words),
            Err(mut found) => problems.append(&mut found),
        }
    }
    commands
}

/// The words of the command `entry` at `index`; `Err` with its problems when it is not an array, is empty, or holds a
/// word that is not a string or is empty.
fn command_of(index: usize, entry: Value) -> Result<Vec<String>, Vec<String>> {
    let words = match entry {
        Value::Array(words) => words,
        other => return Err(vec![format!("commands[{index}] is not an array (TOML {})", other.type_str())]),
    };
    if words.is_empty() {
        return Err(vec![format!("commands[{index}] is empty")]);
    }
    let mut problems = Vec::new();
    let mut read = Vec::with_capacity(words.len());
    for (at, word) in words.into_iter().enumerate() {
        match word {
            Value::String(word) if word.is_empty() => problems.push(format!("commands[{index}][{at}] is empty")),
            Value::String(word) => read.push(word),
            other => problems.push(format!("commands[{index}][{at}] is not a string (TOML {})", other.type_str())),
        }
    }
    if problems.is_empty() { Ok(read) } else { Err(problems) }
}

/// The `timeout` `value` as whole seconds from 1 to [`MOST_TIMEOUT_SECONDS`]; `None` for anything else.
fn read_timeout(value: &Value) -> Option<u64> {
    let Value::Integer(seconds) = value else {
        return None;
    };
    u64::try_from(*seconds)
        .ok()
        .filter(|seconds| (1..=MOST_TIMEOUT_SECONDS).contains(seconds))
}

/// The key `key` as TOML spells it: bare when it is one or more ASCII letters, digits, `_` and `-`, else quoted, its
/// line ends and other control characters escaped, so it is always one line.
#[must_use]
pub fn spelled(key: &str) -> String {
    let bare = !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if bare { key.to_owned() } else { format!("{key:?}") }
}

/// `error`, a failure to parse `text`, as the line and column it starts at and its message; a leading byte order mark
/// takes no column.
#[must_use]
pub fn parse_failure(text: &str, error: &toml::de::Error) -> String {
    let message = error.message().trim_end();
    let Some(span) = error.span() else {
        return message.to_owned();
    };
    let before = text.get(..span.start).unwrap_or(text);
    let before = before.strip_prefix('\u{feff}').unwrap_or(before);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or(before).chars().count() + 1;
    format!("line {line}, column {column}: {message}")
}

/// The lower-case hex SHA-256 of a warm-up file's raw bytes, a byte order mark included: the hash a warm-up record
/// holds and the route compares.
#[must_use]
pub fn warmup_hash(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    hex.extend(Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")));
    hex
}

/// The worktree's `.gate/warmup.toml` as the caller read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRead {
    /// There is no such file.
    Absent,
    /// It cannot be read or has a problem: the first, on one line.
    Problem(String),
    /// It read, with the [`warmup_hash`] of its bytes.
    Parsed { file: WarmupFile, hash: String },
}

/// How a warm-up failed, which sets its backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The commands failed or ran out of time: [`COMMANDS_BACKOFF`].
    Commands,
    /// Refused before any command ran: [`REFUSED_BACKOFF`].
    Refused,
}

impl FailureClass {
    /// How long after such a failure no other warm-up starts.
    #[must_use]
    pub fn backoff(self) -> Duration {
        match self {
            FailureClass::Commands => COMMANDS_BACKOFF,
            FailureClass::Refused => REFUSED_BACKOFF,
        }
    }
}

/// Where a work folder's warm-up stands, as its record says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordState {
    Warming,
    Warm,
    Failed { class: FailureClass, reason: String },
}

/// A warm-up record as the judge reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The builder the warm-up ran on.
    pub host: String,
    pub state: RecordState,
    /// When the record was written.
    pub at: SystemTime,
    /// The [`warmup_hash`] of the file the warm-up ran.
    pub hash: String,
    /// The build id of the PC gate that wrote it.
    pub gate: String,
}

/// The work folder's warm-up record as the caller read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordRead {
    /// There is no record.
    Absent,
    /// The record at `path` exists but cannot be read, for `error`.
    Unreadable { path: String, error: String },
    /// The record read.
    Parsed(Record),
}

/// What the judge weighs for a command that would otherwise route.
#[derive(Debug, Clone, Copy)]
pub struct Situation<'a> {
    /// Whether `remote.json`'s `warmup` names the repo.
    pub consented: bool,
    pub file: &'a FileRead,
    pub record: &'a RecordRead,
    /// Whether the work folder's warm-up lock is held, as the caller's probe found it: a held lock means a warm-up is
    /// running, whatever the record says.
    pub lock_held: bool,
    /// `remote.json`'s current host.
    pub host: &'a str,
    /// The PC gate's build id.
    pub gate: &'a str,
    pub now: SystemTime,
}

/// Where a command that would otherwise route goes, given its work folder's warm-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    /// No consent or no warm-up file: route as a run without a warm-up does.
    Route,
    /// The work folder is warm for this host and the file on disk: route.
    Ready,
    /// A warm-up is running: run here and start nothing. Holds the host whose warm-up it is, for the line.
    Warming { host: String },
    /// Run here and start a warm-up: no record; a `warming` record whose lock is free to take; a record of another host
    /// or file hash; a failed record of another gate build; a failure dated in the future; or one past its backoff.
    Start,
    /// A warm-up failed within its backoff: run here and start nothing.
    Failed { minutes_ago: u64, reason: String },
    /// The warm-up file has a problem, or the record cannot be read: run here and start nothing. Holds the line to say.
    Problem(String),
}

/// Where a command that would otherwise route goes. In order: no consent or no warm-up file routes; a file problem runs
/// here, saying so; the work folder's lock being held means a warm-up is running, whatever the record says, so the run
/// stays here; else an unreadable record runs here, saying so, no record starts a warm-up, a `warm` record of this host
/// and the file's hash routes, a `warming` record starts a warm-up with its lock free, and a `failed` record of this
/// host, file hash and gate build runs here until its backoff has passed, then starts a warm-up.
#[must_use]
pub fn judge(situation: &Situation<'_>) -> Judgement {
    if !situation.consented {
        return Judgement::Route;
    }
    let hash = match situation.file {
        FileRead::Absent => return Judgement::Route,
        FileRead::Problem(problem) => return Judgement::Problem(file_problem_line(problem)),
        FileRead::Parsed { hash, .. } => hash,
    };
    if situation.lock_held {
        return Judgement::Warming {
            host: warming_host(situation),
        };
    }
    match situation.record {
        RecordRead::Absent => Judgement::Start,
        RecordRead::Unreadable { path, error } => Judgement::Problem(record_problem_line(path, error)),
        RecordRead::Parsed(record) => judge_record(record, hash, situation),
    }
}

/// The host a warm-up is running on: the one the record names, whatever its state, when one read, else `remote.json`'s
/// current host.
fn warming_host(situation: &Situation<'_>) -> String {
    match situation.record {
        RecordRead::Parsed(record) => record.host.clone(),
        RecordRead::Absent | RecordRead::Unreadable { .. } => situation.host.to_owned(),
    }
}

/// The judgement of a record that read, the work folder's lock free, for the warm-up file whose hash is `hash`: a `warm`
/// record of this host and file routes whatever gate build wrote it, and a failure of another host, file or build has no
/// backoff, and one written in the future, past a clock jump back, has none either.
fn judge_record(record: &Record, hash: &str, situation: &Situation<'_>) -> Judgement {
    let at_this_host_and_file = record.host == situation.host && record.hash == hash;
    match &record.state {
        RecordState::Warm if at_this_host_and_file => Judgement::Ready,
        RecordState::Warm | RecordState::Warming => Judgement::Start,
        RecordState::Failed { class, reason } => {
            if !at_this_host_and_file || record.gate != situation.gate {
                return Judgement::Start;
            }
            let Ok(elapsed) = situation.now.duration_since(record.at) else {
                return Judgement::Start;
            };
            if elapsed >= class.backoff() {
                Judgement::Start
            } else {
                Judgement::Failed {
                    minutes_ago: elapsed.as_secs() / 60,
                    reason: reason.clone(),
                }
            }
        }
    }
}

/// The line of a run kept here while `host` warms its work folder.
#[must_use]
pub fn warming_line(host: &str) -> String {
    format!("gate: not routed: {} for this worktree", warming_text(host))
}

/// That `host` is warming the work folder, as the route's line and `gate check`'s item both say it.
#[must_use]
pub fn warming_text(host: &str) -> String {
    format!("{} is warming the work folder", shown_value(host))
}

/// The line of a run kept here as it starts a warm-up on `host`. The route says it once it starts a warm-up of its own,
/// which the detached launch brings; until then it says [`unwarmed_line`].
#[must_use]
pub fn starting_line(host: &str) -> String {
    format!(
        "gate: not routed: starting a warm-up of the work folder for this worktree on {}",
        escaped(host)
    )
}

/// The line of a run kept here because the work folder for this worktree on `host` is not warm, and no warm-up is
/// running or held back by a failure.
#[must_use]
pub fn unwarmed_line(host: &str) -> String {
    format!(
        "gate: not routed: the work folder for this worktree on {} is not warm; gate warm warms it",
        escaped(host)
    )
}

/// The line a routed run says after it fell back `cold` from `host`, below the fallback's own line.
#[must_use]
pub fn warm_hint_line(host: &str) -> String {
    format!("gate: gate warm warms the work folder for this worktree on {}", escaped(host))
}

/// The line of a run kept here after a warm-up failed `minutes_ago` whole minutes ago, for `reason`.
#[must_use]
pub fn failed_line(minutes_ago: u64, reason: &str) -> String {
    let when = if minutes_ago == 0 {
        "less than a minute ago".to_owned()
    } else {
        format!("{minutes_ago} min ago")
    };
    format!("gate: not routed: the warm-up failed {when}: {}", shown_value(reason))
}

/// The line of a run kept here for a problem of `.gate/warmup.toml`.
#[must_use]
pub fn file_problem_line(problem: &str) -> String {
    format!("gate: not routed: .gate/warmup.toml: {}", escaped(problem))
}

/// The line of a run kept here because the warm-up record at `path` cannot be read, for `error`.
#[must_use]
pub fn record_problem_line(path: &str, error: &str) -> String {
    format!("gate: not routed: {}", record_problem_text(path, error))
}

/// That the warm-up record at `path` cannot be read, for `error`, as the route's line and `gate check`'s item both say
/// it.
#[must_use]
pub fn record_problem_text(path: &str, error: &str) -> String {
    format!("the warm-up record {} cannot be read: {}", escaped(path), shown_value(error))
}

/// The line of a run kept here because the warm-up lock at `path` cannot be probed, for `error`.
#[must_use]
pub fn lock_problem_line(path: &str, error: &str) -> String {
    format!("gate: not routed: {}", lock_problem_text(path, error))
}

/// That the warm-up lock at `path` cannot be probed, for `error`, as the route's line and `gate check`'s item both say
/// it.
#[must_use]
pub fn lock_problem_text(path: &str, error: &str) -> String {
    format!("the warm-up lock {} cannot be read: {}", escaped(path), escaped(error))
}

/// A value read from a warm-up record, such as its host or a failure's reason, as a line shows it: [`shortened`], then
/// escaped, so it is always one line.
#[must_use]
pub fn shown_value(text: &str) -> String {
    escaped(&shortened(text))
}

/// The most characters of a value read from a warm-up record that a line shows.
const LONGEST_SHOWN: usize = 200;

/// `text`, read from a warm-up record, as a line shows it: its first [`LONGEST_SHOWN`] characters, with `…` after them
/// when there are more, as the gate shows a name. Shortened before it is escaped.
fn shortened(text: &str) -> String {
    match text.char_indices().nth(LONGEST_SHOWN) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}
