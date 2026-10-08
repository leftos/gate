//! The gate journal: the `events.jsonl` that `gate.ps1` appends a line to as each gate queues, takes its slot and ends,
//! read into runs, the newest runs and, per command, how long the runs that passed held their slot over the last week.
//!
//! A run is keyed by the gate's `pid` and `processStart`, since the journal carries no run id: its `queue` line, its
//! `take` once it holds a slot, then `release` as it lets the slot go or `abandon` when it ended still waiting. A run
//! routed to the remote pool carries `remote` and `host` on those lines, and a `fallback` line when it was sent back to
//! the local queue, whose local `take` follows. `covered`
//! and `parent` lines (a gate running on its parent's slot) and every line of the gate's own self-test (a `cwd` holding
//! `gate-selftest`) are ignored, and a line that does not parse is skipped. The folder, the prefix, the clock and the
//! process liveness check all come from the caller.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How many of the newest runs the history lists.
const RECENT_RUNS: usize = 50;
/// How far back a run's take may be for the summary to count it.
const SUMMARY_WINDOW: SignedDuration = SignedDuration::from_hours(7 * 24);
/// What every working directory of the gate's self-test holds.
const SELFTEST_MARKER: &str = "gate-selftest";
/// The flags whose next word names the tree a command runs in.
const TREE_FLAGS: [&str; 4] = ["--manifest-path", "--directory", "--dir", "-C"];
/// What a scratch file under `.tmp` becomes.
const ELIDED: &str = "\u{2026}";

/// How a run stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    /// Released with status 0.
    Ok,
    /// Released with a non-zero status, or with none when the gate failed before its command ran.
    Failed,
    /// Queued, then ended without a slot.
    Abandoned,
    /// Not ended, and its gate is still running.
    Running,
    /// Not ended, and its gate is gone: it was killed, or its end was never written.
    Unrecorded,
}

/// One run of a gate, in the JSON shape `GET /api/gate/history` serves.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRun {
    /// The gated command, as [`normalize_command`] gives it.
    pub command: String,
    /// The folder the gate ran in, as its first journal line read for the run wrote it; `None` when no line carried one.
    pub cwd: Option<String>,
    /// The repository the run's folder belongs to, as the caller's repo function names it; `None` without a folder or
    /// when the function names none.
    pub repo: Option<String>,
    /// The gate's `-Slot`: `heavy`, `light` or `critical`; a string because `critical` is not a `SlotKind`.
    pub kind: String,
    /// The index of the first slot the run took, or `None` when it took none.
    pub slot: Option<u32>,
    /// True for a run routed to the remote pool: its latest queue, take or release line says `remote` and no `fallback`
    /// line came after it. It stays true once the run has released its remote slot, and the summary counts the run's
    /// held time on the command's remote row.
    pub remote: bool,
    /// The build box a remote run waits for or runs on, its ssh destination; `None` for a local run.
    pub host: Option<String>,
    /// Why the run, routed to the remote pool, was sent back to the local queue (`unreachable`, `busy`, `rebooted`,
    /// `parity`, or `unknown` when the line names none); `None` when it was not.
    pub fallback: Option<String>,
    /// How the run stands.
    pub state: RunState,
    /// When the gate queued, as the journal wrote it (ISO 8601 UTC, seven fractional digits); `None` when its queue
    /// line is in neither journal file.
    pub queued: Option<String>,
    /// When the gate took its slot, as the journal wrote it, or `None` when it took none.
    pub started: Option<String>,
    /// How long the gate waited for its slot, from its take or its abandon.
    pub waited_seconds: Option<f64>,
    /// How long the gate held its slot, from its release.
    pub held_seconds: Option<f64>,
    /// The gate's exit status, from its release; `None` without a release or when the gate failed before its command
    /// ran.
    pub status: Option<i64>,
}

/// How long the runs of one command in one repository that passed held their slot, the remote runs apart from the local
/// ones.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSummary {
    /// The command, as [`normalize_command`] gives it.
    pub command: String,
    /// The repository the runs' folders belong to, as [`HistoryRun::repo`] names it.
    pub repo: Option<String>,
    /// True for the runs that held a remote slot, as [`HistoryRun::remote`] has it.
    pub remote: bool,
    /// The runs counted.
    pub count: usize,
    /// The median held time, by nearest rank.
    pub p50_seconds: f64,
    /// The 90th percentile held time, by nearest rank.
    pub p90_seconds: f64,
    /// The longest held time.
    pub max_seconds: f64,
}

/// The journal as the Gates tab shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct History {
    /// The 50 newest runs by the time they queued, newest first, in every state.
    pub runs: Vec<HistoryRun>,
    /// One row per repository, command and remote flag over the runs taken in the last 7 days that passed, the most run
    /// first, then by command, then by repository (none first), then local before remote.
    pub summary: Vec<CommandSummary>,
}

/// The journal's runs kept across reads, so each read parses only the complete lines appended since the last.
#[derive(Debug, Default)]
pub struct JournalCache {
    /// The journal last read; another one starts the cache again.
    journal: Option<PathBuf>,
    /// The journal's creation time as of the last read.
    created: Option<SystemTime>,
    /// The rolled journal's identity as of the last read: `None` when it was absent.
    rolled: Option<FileIdentity>,
    /// The byte just past the last complete line read from the journal.
    offset: u64,
    /// The runs read so far.
    runs: HashMap<RunKey, Run>,
}

/// A run's key: its gate's pid and that process's start time as the journal wrote it.
type RunKey = (u32, String);

/// A file's length and last write time, compared between reads to see that it changed.
type FileIdentity = (u64, Option<SystemTime>);

/// The identity of the file at `path`, or `None` when there is none.
fn file_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some((metadata.len(), metadata.modified().ok()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// A journal time: the moment, and the text the journal wrote for it.
#[derive(Clone, Debug)]
struct Stamp {
    at: Timestamp,
    text: String,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Released,
    Abandoned,
}

/// One run as read so far.
#[derive(Clone, Debug)]
struct Run {
    command: String,
    /// The `cwd` of the first line read for the run; `None` when it was empty.
    cwd: Option<String>,
    kind: String,
    /// The time of the first line read for the run, its queue line when that was read: what the runs are ordered by.
    first: Timestamp,
    queued: Option<Stamp>,
    started: Option<Stamp>,
    slot: Option<u32>,
    remote: bool,
    host: Option<String>,
    fallback: Option<String>,
    waited_seconds: Option<f64>,
    held_seconds: Option<f64>,
    status: Option<i64>,
    end: Option<End>,
}

/// The fields of a journal line the history reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    time: String,
    event: String,
    pid: u32,
    process_start: String,
    kind: String,
    cwd: String,
    command: String,
    #[serde(default)]
    slots: Value,
    #[serde(default)]
    waited_seconds: Option<f64>,
    #[serde(default)]
    held_seconds: Option<f64>,
    #[serde(default)]
    status: Option<i64>,
    #[serde(default)]
    remote: bool,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

impl JournalCache {
    /// The history in `dir`'s `<prefix>events.jsonl` and `<prefix>events.1.jsonl` as of `now`, an open run being
    /// [`RunState::Running`] when `is_live(pid, processStart)` says its gate still runs, and a run's repository being
    /// `repo_of(cwd)`, the caller's rule for naming the repository a folder belongs to.
    ///
    /// The first read, and a read after the journal was rolled (it is shorter than the bytes read so far, or its
    /// creation time changed, or `events.1.jsonl` appeared, went or changed its length or write time) or after `dir` or
    /// `prefix` changed, reads `events.1.jsonl` and then the journal from its start; any other read parses only the journal's complete lines appended since the last, a trailing fragment
    /// without its line feed being left for the next. Runs older than 7 days are then dropped unless they are among the
    /// 50 newest. A missing file reads as an empty one.
    ///
    /// # Errors
    ///
    /// Fails when a journal file exists but cannot be read; the cache then starts again at the next read.
    pub fn read(
        &mut self,
        dir: &Path,
        prefix: &str,
        now: Timestamp,
        is_live: impl Fn(u32, &str) -> bool,
        repo_of: impl Fn(&str) -> Option<String>,
    ) -> io::Result<History> {
        if let Err(error) = self.refresh(dir, prefix) {
            *self = Self::default();
            return Err(error);
        }
        self.prune(now);
        Ok(self.history(now, &is_live, &repo_of))
    }

    fn refresh(&mut self, dir: &Path, prefix: &str) -> io::Result<()> {
        let journal = dir.join(format!("{prefix}events.jsonl"));
        let (len, created) = match fs::metadata(&journal) {
            Ok(metadata) => (metadata.len(), metadata.created().ok()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (0, None),
            Err(error) => return Err(error),
        };
        // A roll moves the journal over the rolled file and begins a new one at once, so NTFS's tunnelling can give the new
        // journal the old one's creation time: a change to the rolled file is the signal that does not depend on it.
        let rolled = dir.join(format!("{prefix}events.1.jsonl"));
        let rolled_identity = file_identity(&rolled)?;
        let same_journal = self.journal.as_deref() == Some(journal.as_path());
        if !same_journal || len < self.offset || created != self.created || rolled_identity != self.rolled {
            *self = Self {
                journal: Some(journal.clone()),
                ..Self::default()
            };
            self.read_rolled(&rolled)?;
        }
        self.created = created;
        self.rolled = rolled_identity;
        if len > self.offset {
            self.read_appended(&journal)?;
        }
        Ok(())
    }

    /// Reads every line of the rolled journal, its last line included: nothing appends to it any more.
    fn read_rolled(&mut self, path: &Path) -> io::Result<()> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for line in bytes.split(|byte| *byte == b'\n') {
            self.apply(line);
        }
        Ok(())
    }

    /// Reads the journal's complete lines past [`Self::offset`], moving it past each.
    fn read_appended(&mut self, path: &Path) -> io::Result<()> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 || line.last() != Some(&b'\n') {
                return Ok(());
            }
            self.apply(&line);
            self.offset += u64::try_from(line.len()).unwrap_or(u64::MAX);
        }
    }

    /// Adds one journal line to its run; a line that does not parse, the self-test's, and every event but the five a
    /// run is made of are skipped. A run's first `queue` line sets its queued time; a later one (after a `fallback`)
    /// does not. A `fallback` line sends a remote run back to the local queue: the run is no longer remote, and keeps the
    /// reason.
    fn apply(&mut self, bytes: &[u8]) {
        let Ok(line) = serde_json::from_slice::<Line>(bytes.trim_ascii()) else {
            return;
        };
        let Ok(at) = line.time.parse::<Timestamp>() else {
            return;
        };
        if line.cwd.contains(SELFTEST_MARKER) || !matches!(line.event.as_str(), "queue" | "take" | "release" | "abandon" | "fallback") {
            return;
        }
        let stamp = Stamp { at, text: line.time };
        let run = self.runs.entry((line.pid, line.process_start)).or_insert_with(|| Run {
            command: normalize_command(&line.command),
            cwd: Some(line.cwd).filter(|cwd| !cwd.is_empty()),
            kind: line.kind,
            first: at,
            queued: None,
            started: None,
            slot: None,
            remote: false,
            host: None,
            fallback: None,
            waited_seconds: None,
            held_seconds: None,
            status: None,
            end: None,
        });
        match line.event.as_str() {
            "queue" => {
                run.queued.get_or_insert(stamp);
                run.route(line.remote, line.host);
            }
            "take" => {
                run.started = Some(stamp);
                run.slot = first_slot(&line.slots);
                run.waited_seconds = line.waited_seconds;
                run.route(line.remote, line.host);
            }
            "release" => {
                run.held_seconds = line.held_seconds;
                run.status = line.status;
                run.end = Some(End::Released);
                // A release line that says `remote` marks a run whose queue and take rolled out of the journal; one that
                // does not leaves the run as its take left it.
                if line.remote {
                    run.route(true, line.host);
                }
            }
            "fallback" => {
                // The run holds nothing until a local take: a remote take's slot, start and wait no longer stand.
                run.route(false, None);
                run.slot = None;
                run.started = None;
                run.waited_seconds = None;
                run.fallback = Some(line.reason.unwrap_or_else(|| "unknown".to_owned()));
            }
            _ => {
                run.waited_seconds = line.waited_seconds;
                run.end = Some(End::Abandoned);
            }
        }
    }

    /// Drops the runs that started more than 7 days before `now`, unless they are among the 50 newest.
    fn prune(&mut self, now: Timestamp) {
        let cutoff = now - SUMMARY_WINDOW;
        let newest: HashSet<RunKey> = newest_first(&self.runs)
            .into_iter()
            .take(RECENT_RUNS)
            .map(|(key, _)| key.clone())
            .collect();
        self.runs
            .retain(|key, run| run.started.as_ref().map_or(run.first, |started| started.at) >= cutoff || newest.contains(key));
    }

    fn history(&self, now: Timestamp, is_live: &impl Fn(u32, &str) -> bool, repo_of: &impl Fn(&str) -> Option<String>) -> History {
        let runs = newest_first(&self.runs)
            .into_iter()
            .take(RECENT_RUNS)
            .map(|(key, run)| run.row(run.state(key, is_live), run.repo(repo_of)))
            .collect();
        History {
            runs,
            summary: summarize(self.runs.values(), now - SUMMARY_WINDOW, repo_of),
        }
    }
}

impl Run {
    /// Marks the run remote on `host`, or local. A remote line after a `fallback` makes the run a plain remote one again.
    fn route(&mut self, remote: bool, host: Option<String>) {
        self.remote = remote;
        self.host = host.filter(|_| remote);
        if remote {
            self.fallback = None;
        }
    }

    fn state(&self, (pid, process_start): &RunKey, is_live: &impl Fn(u32, &str) -> bool) -> RunState {
        match self.end {
            Some(End::Released) if self.status == Some(0) => RunState::Ok,
            Some(End::Released) => RunState::Failed,
            Some(End::Abandoned) => RunState::Abandoned,
            None if is_live(*pid, process_start) => RunState::Running,
            None => RunState::Unrecorded,
        }
    }

    /// The repository the run's folder belongs to, by `repo_of`; `None` without a folder.
    fn repo(&self, repo_of: &impl Fn(&str) -> Option<String>) -> Option<String> {
        self.cwd.as_deref().and_then(repo_of)
    }

    fn row(&self, state: RunState, repo: Option<String>) -> HistoryRun {
        HistoryRun {
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            repo,
            kind: self.kind.clone(),
            slot: self.slot,
            remote: self.remote,
            host: self.host.clone(),
            fallback: self.fallback.clone(),
            state,
            queued: self.queued.as_ref().map(|stamp| stamp.text.clone()),
            started: self.started.as_ref().map(|stamp| stamp.text.clone()),
            waited_seconds: self.waited_seconds,
            held_seconds: self.held_seconds,
            status: self.status,
        }
    }

    /// The held time of a run that passed and took its slot at or after `cutoff`.
    fn counted_held_seconds(&self, cutoff: Timestamp) -> Option<f64> {
        let passed = self.end == Some(End::Released) && self.status == Some(0);
        let recent = self.started.as_ref().is_some_and(|started| started.at >= cutoff);
        if passed && recent { self.held_seconds } else { None }
    }
}

/// The runs, newest first by their first line's time, then by pid and process start for runs that share it.
fn newest_first(runs: &HashMap<RunKey, Run>) -> Vec<(&RunKey, &Run)> {
    let mut ordered: Vec<(&RunKey, &Run)> = runs.iter().collect();
    ordered.sort_by(|(a_key, a_run), (b_key, b_run)| b_run.first.cmp(&a_run.first).then_with(|| b_key.cmp(a_key)));
    ordered
}

/// One row per repository, command and remote flag over the runs that passed and took their slot at or after `cutoff`, a
/// run's repository named by `repo_of` its folder: the most run first, then by command, then by repository (none
/// first), then local before remote.
fn summarize<'a>(runs: impl Iterator<Item = &'a Run>, cutoff: Timestamp, repo_of: &impl Fn(&str) -> Option<String>) -> Vec<CommandSummary> {
    let mut held: BTreeMap<(Option<String>, &str, bool), Vec<f64>> = BTreeMap::new();
    for run in runs {
        if let Some(seconds) = run.counted_held_seconds(cutoff) {
            held.entry((run.repo(repo_of), run.command.as_str(), run.remote))
                .or_default()
                .push(seconds);
        }
    }
    let mut summary: Vec<CommandSummary> = held
        .into_iter()
        .filter_map(|((repo, command, remote), mut seconds)| {
            seconds.sort_by(f64::total_cmp);
            Some(CommandSummary {
                command: command.to_owned(),
                repo,
                remote,
                count: seconds.len(),
                p50_seconds: nearest_rank(&seconds, 50)?,
                p90_seconds: nearest_rank(&seconds, 90)?,
                max_seconds: *seconds.last()?,
            })
        })
        .collect();
    summary.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.command.cmp(&b.command))
            .then_with(|| a.repo.cmp(&b.repo))
            .then_with(|| a.remote.cmp(&b.remote))
    });
    summary
}

/// The `percent`th percentile of the sorted `values` by nearest rank: the value at `ceil(percent / 100 * n) - 1`.
fn nearest_rank(sorted: &[f64], percent: usize) -> Option<f64> {
    let rank = (percent * sorted.len()).div_ceil(100);
    sorted.get(rank.checked_sub(1)?).copied()
}

/// The number at the end of the first slot name, such as 0 for `Local\gate-light-slot-0`.
fn first_slot(slots: &Value) -> Option<u32> {
    let name = slots.as_array()?.first()?.as_str()?;
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    name[name.len() - digits..].parse().ok()
}

/// The gated command with what names one tree or one scratch file taken out, so the runs of one command in different
/// worktrees count together. The command is split on whitespace, a double-quoted span staying one word, then:
///
/// 1. `--manifest-path`, `--directory`, `--dir` or `-C` followed by an absolute path is dropped with that path;
/// 2. a word starting `.tmp/` or `.tmp\` becomes `…`;
/// 3. any other absolute path with a component ending `.wt` (in any case) and one more after it (the worktree's) becomes what follows
///    that component, with `/` separators, or `.` when nothing does;
/// 4. the words are joined by single spaces.
///
/// An absolute path starts with a drive letter and `:\` or `:/`, or with `/`, `\\` or `~`.
#[must_use]
pub fn normalize_command(command: &str) -> String {
    let words = split_words(command);
    let mut kept: Vec<Cow<'_, str>> = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        if TREE_FLAGS.contains(&word) && words.get(index + 1).is_some_and(|next| is_absolute(next)) {
            index += 2;
            continue;
        }
        kept.push(rewrite_word(word));
        index += 1;
    }
    kept.join(" ")
}

/// The command's words: runs of non-whitespace, whitespace inside double quotes not splitting one.
fn split_words(command: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    let mut quoted = false;
    for (index, character) in command.char_indices() {
        if character == '"' {
            quoted = !quoted;
            start.get_or_insert(index);
        } else if character.is_whitespace() && !quoted {
            if let Some(begin) = start.take() {
                words.push(&command[begin..index]);
            }
        } else {
            start.get_or_insert(index);
        }
    }
    if let Some(begin) = start {
        words.push(&command[begin..]);
    }
    words
}

fn is_absolute(word: &str) -> bool {
    let bytes = word.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
    drive || word.starts_with('/') || word.starts_with(r"\\") || word.starts_with('~')
}

/// A scratch file elided, or a worktree path shortened; any other word as it is.
fn rewrite_word(word: &str) -> Cow<'_, str> {
    if word.starts_with(".tmp/") || word.starts_with(r".tmp\") {
        return Cow::Borrowed(ELIDED);
    }
    if !is_absolute(word) {
        return Cow::Borrowed(word);
    }
    let components: Vec<&str> = word.split(['/', '\\']).collect();
    let is_worktrees = |component: &&str| {
        Path::new(component)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wt"))
    };
    let Some(worktrees) = components.iter().position(is_worktrees) else {
        return Cow::Borrowed(word);
    };
    if worktrees + 1 >= components.len() {
        return Cow::Borrowed(word);
    }
    let rest = components[worktrees + 2..].join("/");
    if rest.is_empty() { Cow::Borrowed(".") } else { Cow::Owned(rest) }
}
