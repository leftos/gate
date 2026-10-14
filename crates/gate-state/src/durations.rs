//! The learned durations: the key a run is filed under, its normalised command, the estimate of how long a run of a key
//! takes, and `<root>\durations.json`, the store of each key's recent passing runs, failure count and newest release.
//!
//! The store is written by [`record_run`] inside the journal hold (through
//! [`append_line_then`](crate::append_line_then)) and read with no lock by [`read_store`]. When it is missing, stale or
//! unreadable, one gate at a time ([`begin_rebuild`]) rebuilds it from the journal and the archive ([`rebuild_store`])
//! outside the hold, and [`swap_rebuilt`] puts it in place under the journal lock after filing the runs released
//! meanwhile. Nothing here runs git: the caller supplies where a run's folder sits ([`KeyPlace`]), and a folder git
//! cannot place is placed by its path's shape ([`place_by_shape`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use flate2::read::GzDecoder;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::archive::{ARCHIVE_DIR, archive_stamp, release_lines};
use crate::counts::write_replacing;
use crate::journal::{ELIDED, FileIdentity, file_identity, is_worktrees_folder, nearest_rank, quote_word, split_words, worktree_path};
use crate::journal_writer::{JOURNAL_FILE, JOURNAL_WAIT, JournalLock, LockError, ROLLED_JOURNAL_FILE, RunFacts};
use crate::root::root_hash;
use crate::state::CRITICAL;
use crate::verdict::Verdict;

/// The store's file name under the state root.
pub const DURATIONS_FILE: &str = "durations.json";
/// The marker's file name under the state root while a gate rebuilds the store.
pub const REBUILDING_FILE: &str = "durations.rebuilding";
/// The rule set the keys are made by; a store written under another one is rebuilt.
pub const KEY_VERSION: u64 = 1;
/// The store's format version, its `v`.
const STORE_VERSION: u64 = 1;
/// How many of a key's newest passing runs the estimate looks at, at most.
const WINDOW_RUNS: usize = 30;
/// How old a passing run may be for the estimate to look at it, and a key's newest release for the store to keep it.
const WINDOW_AGE: SignedDuration = SignedDuration::from_hours(14 * 24);
/// How many passing runs in the window a key needs for an estimate.
const MIN_RUNS: usize = 5;
/// How many of the newest runs the usual time is the median of.
const RECENT_RUNS: usize = 5;
/// The characters a key keeps before its hash is appended.
const KEY_CAP: usize = 200;
/// The flags whose next word is a filter value, besides `-t` (not under `dotnet`), `-Filter` (any case) and `--filter*`.
const FILTER_FLAGS: [&str; 1] = ["-E"];
/// The flags whose following words, up to the next one starting with `-`, name files.
const FILES_FLAGS: [&str; 2] = ["--include", "--files"];
/// What a commit hash becomes.
const SHA: &str = "<sha>";
/// What the words naming files become.
const FILES: &str = "<files>";
/// What a filter value becomes in the family key.
const FILTER: &str = "<filter>";
/// The `kind` a `--slot none` run's journal lines carry.
const NO_SLOT: &str = "none";

/// The store's path under the state root `root`.
#[must_use]
pub fn durations_path(root: &Path) -> PathBuf {
    root.join(DURATIONS_FILE)
}

/// The rebuild marker's path under the state root `root`.
#[must_use]
pub fn rebuilding_path(root: &Path) -> PathBuf {
    root.join(REBUILDING_FILE)
}

/// Where a run's folder sits, as git places it; the caller asks git, this crate never does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyPlace {
    /// A folder inside a git worktree.
    InGit {
        /// The main checkout's folder name, shared by every worktree of the repo.
        repo: String,
        /// The folder relative to the worktree root, empty at the root; `\` reads as `/` and an outer `/` is dropped.
        sub: String,
        /// The worktree root's full path, which a command word under it loses.
        worktree_root: String,
    },
    /// A folder outside git, keyed on its full path with `/` separators, in ASCII lower case, without a closing `/`.
    OutsideGit {
        /// The folder's full path.
        cwd: String,
    },
}

impl KeyPlace {
    /// The key's folder part: `repo[/sub]`, or the full path outside git as [`KeyPlace::OutsideGit`] keys it.
    fn folder(&self) -> String {
        match self {
            KeyPlace::InGit { repo, sub, .. } => {
                let sub = sub.replace('\\', "/");
                let sub = sub.trim_matches('/');
                if sub.is_empty() { repo.clone() } else { format!("{repo}/{sub}") }
            }
            KeyPlace::OutsideGit { cwd } => {
                let cwd = cwd.replace('\\', "/").to_ascii_lowercase();
                let trimmed = cwd.trim_end_matches('/');
                if trimmed.is_empty() { cwd } else { trimmed.to_owned() }
            }
        }
    }

    fn worktree_root(&self) -> Option<&str> {
        match self {
            KeyPlace::InGit { worktree_root, .. } => Some(worktree_root),
            KeyPlace::OutsideGit { .. } => None,
        }
    }
}

/// Where a folder git cannot place sits, by its path's shape: a path with a component `{repo}.wt` (the extension in
/// any case) followed by a slug is in repo `{repo}`, at the components after the slug, under the worktree root that
/// ends at the slug; any other path is outside git.
#[must_use]
pub fn place_by_shape(cwd: &str) -> KeyPlace {
    let components: Vec<&str> = cwd.split(['/', '\\']).collect();
    let worktrees = components.iter().position(|component| is_worktrees_folder(component));
    if let Some(at) = worktrees
        && components.get(at + 1).is_some_and(|slug| !slug.is_empty())
    {
        let folder = components[at];
        return KeyPlace::InGit {
            repo: folder[..folder.len() - ".wt".len()].to_owned(),
            sub: components[at + 2..].join("/"),
            worktree_root: components[..=at + 1].join("/"),
        };
    }
    KeyPlace::OutsideGit { cwd: cwd.to_owned() }
}

/// What a run's key is made of.
#[derive(Clone, Copy, Debug)]
pub struct KeyInputs<'a> {
    /// Where the run's folder sits.
    pub folder: &'a KeyPlace,
    /// The build box a routed run ran on, as `user@host`; `None` for a run on this machine.
    pub remote_host: Option<&'a str>,
    /// True for a critical gate.
    pub critical: bool,
    /// The system temp root, whose paths a command word loses; `None` when there is none.
    pub temp_root: Option<&'a str>,
    /// The gated command as one line, as the journal holds it.
    pub command: &'a str,
}

/// A command's words with what varies between runs of one command taken out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedCommand {
    /// The command with its filter values kept.
    pub exact: String,
    /// The command with each filter value replaced by `<filter>`.
    pub family: String,
}

/// The keys a run is filed under: `{place}:{class}:{folder} | {normalised command}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurationKey {
    /// The key with the command's filter values kept.
    pub exact: String,
    /// The key with the command's filter values replaced; the same as `exact` for a command without filters.
    pub family: String,
}

/// The keys of a run: `place` is `local` or `remote:{host}`, `class` is `crit` for a critical gate or empty, `folder` is
/// `repo[/sub]` or the full path outside git, and the command is normalised by [`normalize_for_durations`]. A key over
/// 200 characters is cut to its first 200 followed by the first 8 hex digits of the whole key's SHA-256.
#[must_use]
pub fn duration_key(inputs: &KeyInputs<'_>) -> DurationKey {
    let command = normalize_for_durations(inputs.command, inputs.folder.worktree_root(), inputs.temp_root);
    let place = inputs.remote_host.map_or_else(|| "local".to_owned(), |host| format!("remote:{host}"));
    let class = if inputs.critical { "crit" } else { "" };
    let prefix = format!("{place}:{class}:{} | ", inputs.folder.folder());
    DurationKey {
        exact: capped(format!("{prefix}{}", command.exact)),
        family: capped(format!("{prefix}{}", command.family)),
    }
}

/// `key` cut to [`KEY_CAP`] characters plus 8 hex digits of its SHA-256 when it is longer.
fn capped(key: String) -> String {
    let Some((cut, _)) = key.char_indices().nth(KEY_CAP) else {
        return key;
    };
    format!("{}{}", &key[..cut], root_hash(&key))
}

/// The command as the durations key holds it. The words are split as [`normalize_command`](crate::normalize_command)
/// splits them, the rules below run on each unquoted word in order, and the words are written back quoted as it writes
/// them:
///
/// 1. the first word becomes its file name without `.exe`, `.cmd` or `.bat` (any case, as often as they end it);
/// 2. a word starting with `worktree_root` loses it, with `/` separators (`.` when nothing follows); any other absolute
///    path with a component ending `.wt` becomes what follows the worktree's folder; a word starting `-` holding `=` has
///    this rule applied to what follows its first `=`;
/// 3. a word starting `.tmp/` or `.tmp\`, or naming `temp_root` or a path under it, becomes `…`;
/// 4. a word of 7 to 40 hex digits holding a digit and a letter becomes `<sha>`;
/// 5. the words after `--include` or `--files`, up to the next word starting with `-`, become one `<files>`;
/// 6. the family form replaces the word after `-Filter` (any case), `-E`, a `--filter*` flag, or `-t` when the program
///    is not `dotnet`, with `<filter>`, unless that word starts with `-`; and the value of `--filter*=value` likewise.
///
/// Roots match without regard to ASCII case or to `/` against `\`, and only at a folder boundary.
#[must_use]
pub fn normalize_for_durations(command: &str, worktree_root: Option<&str>, temp_root: Option<&str>) -> NormalizedCommand {
    let words: Vec<String> = split_words(command)
        .into_iter()
        .enumerate()
        .map(|(index, word)| {
            let word = if index == 0 { program_name(&word) } else { word };
            rewrite(&word, worktree_root, temp_root)
        })
        .collect();
    let words = collapse_files(words);
    let family = family_words(&words);
    NormalizedCommand {
        exact: joined(&words),
        family: joined(&family),
    }
}

/// The words quoted and joined by single spaces.
fn joined(words: &[String]) -> String {
    words.iter().map(|word| quote_word(word)).collect::<Vec<_>>().join(" ")
}

/// Rule 1: the file name of a program word without its `.exe`, `.cmd` or `.bat`; a word naming no file is kept.
fn program_name(word: &str) -> String {
    let trimmed = word.trim_end_matches(['/', '\\']);
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    if base.is_empty() {
        return word.to_owned();
    }
    let mut name = base;
    while let Some(stem) = without_program_suffix(name) {
        name = stem;
    }
    name.to_owned()
}

/// `name` without a closing `.exe`, `.cmd` or `.bat` in any case, or `None` when it has none or is nothing more.
fn without_program_suffix(name: &str) -> Option<&str> {
    [".exe", ".cmd", ".bat"].iter().find_map(|suffix| {
        let cut = name.len().checked_sub(suffix.len())?;
        let (stem, tail) = (name.get(..cut)?, name.get(cut..)?);
        (tail.eq_ignore_ascii_case(suffix) && !stem.is_empty()).then_some(stem)
    })
}

/// Rules 2 to 4 on one word.
fn rewrite(word: &str, worktree_root: Option<&str>, temp_root: Option<&str>) -> String {
    let word = relative(word, worktree_root);
    let scratch = word.starts_with(".tmp/") || word.starts_with(r".tmp\");
    if scratch || temp_root.is_some_and(|root| under_root(&word, root).is_some()) {
        return ELIDED.to_owned();
    }
    if is_sha(&word) {
        return SHA.to_owned();
    }
    word
}

/// Rule 2: the word, or the value of a `-flag=value` word, relative to its worktree.
fn relative(word: &str, worktree_root: Option<&str>) -> String {
    if word.starts_with('-')
        && let Some((flag, value)) = word.split_once('=')
        && !value.is_empty()
    {
        return format!("{flag}={}", relative_path(value, worktree_root));
    }
    relative_path(word, worktree_root)
}

/// A path under `worktree_root` without it; else a path through a `.wt` worktrees folder relative to its worktree, as
/// `{repo}:{rest}` unless that worktree is the run's own spelled another way; else the word as it is.
fn relative_path(word: &str, worktree_root: Option<&str>) -> String {
    if let Some(rest) = worktree_root.and_then(|root| under_root(word, root)) {
        return rest;
    }
    let Some(path) = worktree_path(word) else {
        return word.to_owned();
    };
    if worktree_root.is_some_and(|root| is_own_worktree(root, path.repo, path.slug)) {
        path.rest
    } else {
        format!("{}:{}", path.repo, path.rest)
    }
}

/// Whether the worktree root `root` ends in `{repo}.wt/{slug}` (ASCII case ignored), so a path through those folders
/// from another drive or link is in the run's own worktree.
fn is_own_worktree(root: &str, repo: &str, slug: &str) -> bool {
    let root = root.replace('\\', "/");
    let mut components = root.trim_end_matches('/').rsplit('/');
    let (Some(last), Some(folder)) = (components.next(), components.next()) else {
        return false;
    };
    last.eq_ignore_ascii_case(slug) && is_worktrees_folder(folder) && folder[..folder.len() - ".wt".len()].eq_ignore_ascii_case(repo)
}

/// What follows `root` in `word` with `/` separators, `.` when nothing does, or `None` when `word` does not start with
/// `root` at a folder boundary. Case (ASCII) and `/` against `\` are ignored.
fn under_root(word: &str, root: &str) -> Option<String> {
    let root = root.replace('\\', "/");
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return None;
    }
    let word = word.replace('\\', "/");
    if !word.get(..root.len())?.eq_ignore_ascii_case(root) {
        return None;
    }
    let rest = &word[root.len()..];
    let rest = if rest.is_empty() { rest } else { rest.strip_prefix('/')? };
    Some(if rest.is_empty() { ".".to_owned() } else { rest.to_owned() })
}

/// Whether `word` is 7 to 40 hex digits holding a digit and a letter.
fn is_sha(word: &str) -> bool {
    let bytes = word.as_bytes();
    (7..=40).contains(&bytes.len())
        && bytes.iter().all(u8::is_ascii_hexdigit)
        && bytes.iter().any(u8::is_ascii_digit)
        && bytes.iter().any(u8::is_ascii_alphabetic)
}

/// Rule 5: each run of words after a files flag, up to the next word starting with `-`, as one `<files>`.
fn collapse_files(words: Vec<String>) -> Vec<String> {
    let mut kept = Vec::with_capacity(words.len());
    let mut in_files = false;
    let mut wrote = false;
    for word in words {
        if in_files && !word.starts_with('-') {
            if !wrote {
                kept.push(FILES.to_owned());
                wrote = true;
            }
            continue;
        }
        in_files = FILES_FLAGS.contains(&word.as_str());
        wrote = false;
        kept.push(word);
    }
    kept
}

/// Rule 6: the words with each filter value replaced by `<filter>`.
fn family_words(words: &[String]) -> Vec<String> {
    let dotnet = words.first().is_some_and(|program| program.eq_ignore_ascii_case("dotnet"));
    let mut family = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        index += 1;
        if let Some((name, _)) = word.split_once('=')
            && name.starts_with("--filter")
        {
            family.push(format!("{name}={FILTER}"));
            continue;
        }
        family.push(word.clone());
        if is_filter_flag(word, dotnet) && words.get(index).is_some_and(|value| !value.starts_with('-')) {
            family.push(FILTER.to_owned());
            index += 1;
        }
    }
    family
}

/// Whether `word` is a flag whose next word is a filter value; `-t` is `--list-tests` under `dotnet`.
fn is_filter_flag(word: &str, dotnet: bool) -> bool {
    (word == "-t" && !dotnet) || FILTER_FLAGS.contains(&word) || word.eq_ignore_ascii_case("-Filter") || word.starts_with("--filter")
}

/// One passing run as the store keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    /// When the run was released.
    #[serde(with = "iso_time")]
    pub time: Timestamp,
    /// The command's wall time in seconds.
    pub wall: f64,
    /// The load-adjusted seconds, or `None` when there are none.
    pub adjusted: Option<f64>,
    /// The command's average busy cores, or `None` when no sample measured them.
    pub avg_cores: Option<f64>,
    /// The command's peak busy cores, or `None` when no sample measured them.
    pub peak_cores: Option<f64>,
}

/// What the store keeps of one key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyHistory {
    /// When the key's newest release was, passing or failing; a key with none in 14 days is dropped.
    #[serde(with = "iso_time")]
    pub last: Timestamp,
    /// How many runs of the key failed; they are counted, never timed.
    pub failed: u64,
    /// The newest passing runs, oldest first: at most 30, none more than 14 days older than the newest record.
    pub samples: Vec<Sample>,
}

impl KeyHistory {
    fn new(time: Timestamp) -> Self {
        KeyHistory {
            last: time,
            failed: 0,
            samples: Vec::new(),
        }
    }

    /// Drops the samples more than 14 days before `now`, then all but the newest 30, leaving them oldest first.
    fn trim(&mut self, now: Timestamp) {
        let start = window_start(now);
        self.samples.retain(|sample| sample.time >= start);
        self.samples.sort_by_key(|sample| sample.time);
        let excess = self.samples.len().saturating_sub(WINDOW_RUNS);
        self.samples.drain(..excess);
    }
}

/// How long runs of a key take, from its passing runs in the window.
#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
    /// The median wall seconds of the 5 newest runs.
    pub p50: f64,
    /// The 90th percentile of the wall seconds by nearest rank, raised to `p50` when below it.
    pub p90: f64,
    /// The 99th percentile of the wall seconds by nearest rank, raised to `p50` when below it.
    pub p99: f64,
    /// The 99th percentile of the adjusted seconds by nearest rank, never raised to the wall `p50`; `None` unless at
    /// least 5 of the window's runs have them.
    pub adjusted_p99: Option<f64>,
    /// The mean wall seconds of the window's runs.
    pub mean: f64,
    /// How many passing runs the window holds.
    pub runs: usize,
    /// The key the figures are of.
    pub key: String,
    /// True when the figures are the family key's, the exact key having too few runs.
    pub family: bool,
}

/// The durations store: each key's history.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DurationStore {
    keys: BTreeMap<String, KeyHistory>,
}

/// How a run counts.
enum Outcome {
    /// A timed passing run.
    Passed(Sample),
    /// A run that failed, counted.
    Failed,
}

impl Outcome {
    /// How a run with `verdict` counts: `None` for an unknown verdict or a transfer that timed out, or a passing run
    /// with no finite wall time.
    fn new(verdict: Verdict, sample: Option<Sample>) -> Option<Outcome> {
        match verdict {
            Verdict::Unknown | Verdict::TransferTimedOut => None,
            Verdict::Passed => sample.map(Outcome::Passed),
            _ => Some(Outcome::Failed),
        }
    }
}

/// A sample at `time`, or `None` when `wall` is missing or not finite; a value that is not finite is left out.
fn sample(time: Timestamp, wall: Option<f64>, adjusted: Option<f64>, cores: (Option<f64>, Option<f64>)) -> Option<Sample> {
    let finite = |value: Option<f64>| value.filter(|value| value.is_finite());
    Some(Sample {
        time,
        wall: finite(wall)?,
        adjusted: finite(adjusted),
        avg_cores: finite(cores.0),
        peak_cores: finite(cores.1),
    })
}

impl DurationStore {
    /// An empty store under this build's key version.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Files a run released at `time` under the key's exact and family keys: a passing run as a sample, then each key
    /// trimmed to the window ending at `time`; any other known verdict as a failure. Either way `time` becomes the key's
    /// newest release when it is newer. A run with an unknown verdict, or a passing run with no finite wall time, is not
    /// filed.
    pub fn record(&mut self, key: &DurationKey, time: Timestamp, facts: &RunFacts) {
        let timed = sample(
            time,
            Some(facts.wall_seconds),
            facts.adjusted_seconds,
            (facts.avg_cores, facts.peak_cores),
        );
        if let Some(outcome) = Outcome::new(facts.verdict, timed) {
            self.file(key, time, &outcome);
        }
    }

    fn file(&mut self, key: &DurationKey, time: Timestamp, outcome: &Outcome) {
        let names: &[&String] = if key.family == key.exact {
            &[&key.exact]
        } else {
            &[&key.exact, &key.family]
        };
        for name in names {
            let history = self.keys.entry((*name).clone()).or_insert_with(|| KeyHistory::new(time));
            history.last = history.last.max(time);
            match outcome {
                Outcome::Failed => history.failed += 1,
                Outcome::Passed(sample) => {
                    history.samples.push(sample.clone());
                    history.trim(time);
                }
            }
        }
    }

    /// Drops the keys whose newest release is more than 14 days before `now`.
    fn prune(&mut self, now: Timestamp) {
        let start = window_start(now);
        self.keys.retain(|_, history| history.last >= start);
    }

    /// The history of the key `key`, as the store holds it.
    #[must_use]
    pub fn history(&self, key: &str) -> Option<&KeyHistory> {
        self.keys.get(key)
    }

    /// The keys the store holds, in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.keys.keys().map(String::as_str)
    }

    /// The estimate for `key` at `now`, from the exact key when its window holds 5 passing runs, else from the family
    /// key, else none. The window is the passing runs within 14 days before `now`, the newest 30 at most.
    #[must_use]
    pub fn estimate(&self, key: &DurationKey, now: Timestamp) -> Option<Estimate> {
        let of = |name: &String, family: bool| self.keys.get(name).and_then(|history| estimate_of(history, now, name, family));
        of(&key.exact, false).or_else(|| if key.family == key.exact { None } else { of(&key.family, true) })
    }

    fn text(&self) -> serde_json::Result<String> {
        serde_json::to_string(&StoreFileOut {
            v: STORE_VERSION,
            key_version: KEY_VERSION,
            keys: &self.keys,
        })
    }
}

/// The earliest moment the window ending at `now` holds.
fn window_start(now: Timestamp) -> Timestamp {
    now.checked_sub(WINDOW_AGE).unwrap_or(Timestamp::MIN)
}

fn estimate_of(history: &KeyHistory, now: Timestamp, key: &str, family: bool) -> Option<Estimate> {
    let start = window_start(now);
    let mut window: Vec<&Sample> = history.samples.iter().filter(|sample| sample.time >= start).collect();
    window.sort_by_key(|sample| sample.time);
    let window = &window[window.len().saturating_sub(WINDOW_RUNS)..];
    if window.len() < MIN_RUNS {
        return None;
    }
    let sorted = |values: Vec<f64>| {
        let mut values = values;
        values.sort_by(f64::total_cmp);
        values
    };
    let recent = sorted(window[window.len() - RECENT_RUNS..].iter().map(|sample| sample.wall).collect());
    let p50 = nearest_rank(&recent, 50)?;
    let walls = sorted(window.iter().map(|sample| sample.wall).collect());
    let adjusted = sorted(window.iter().filter_map(|sample| sample.adjusted).collect());
    #[expect(clippy::cast_precision_loss, reason = "a window holds at most 30 runs")]
    let mean = walls.iter().sum::<f64>() / walls.len() as f64;
    Some(Estimate {
        p50,
        p90: nearest_rank(&walls, 90)?.max(p50),
        p99: nearest_rank(&walls, 99)?.max(p50),
        adjusted_p99: if adjusted.len() >= MIN_RUNS { nearest_rank(&adjusted, 99) } else { None },
        mean,
        runs: window.len(),
        key: key.to_owned(),
        family,
    })
}

/// The store file as written: `{"v": 1, "keyVersion": 1, "keys": {key: {"last", "failed", "samples"}}}`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoreFileOut<'a> {
    v: u64,
    key_version: u64,
    keys: &'a BTreeMap<String, KeyHistory>,
}

/// The keys of a store file read back once its versions are checked.
#[derive(Deserialize)]
struct StoreFileIn {
    keys: BTreeMap<String, KeyHistory>,
}

/// What reading the store found.
#[derive(Clone, Debug, PartialEq)]
pub enum StoreRead {
    /// A store under this build's key version.
    Current(DurationStore),
    /// No usable store: no history until it is rebuilt.
    NeedsRebuild(RebuildReason),
    /// The store is there but cannot be read now, such as while another process holds it: no history for this run, no
    /// rebuild, the reason said once.
    Unavailable(StoreReadError),
    /// A store a newer build wrote under a higher key version: no history for this build, which neither files into it
    /// nor rebuilds it, and says nothing of it.
    Newer,
}

/// Why the store needs a rebuild.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebuildReason {
    /// There is no store: nothing to say.
    Missing,
    /// The store was written under another key version.
    Stale,
    /// The store reads but does not parse, or has a format version other than 1: said once, with the reason.
    Invalid(StoreReadError),
}

/// Why the store could not be read; it then reads as no history, to be rebuilt.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cannot read {path} ({reason})", path = .path.display())]
pub struct StoreReadError {
    /// The store's path.
    pub path: PathBuf,
    /// Why, such as the parse error of a torn file.
    pub reason: String,
}

/// Why the store could not be written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cannot write {path} ({reason})", path = .path.display())]
pub struct StoreWriteError {
    /// The store's path.
    pub path: PathBuf,
    /// Why.
    pub reason: String,
}

/// Why a store update or swap did not happen.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The store could not be written.
    #[error(transparent)]
    Write(#[from] StoreWriteError),
    /// The journal lock was not taken.
    #[error(transparent)]
    Lock(#[from] LockError),
    /// The rebuild marker could not be made, read or removed.
    #[error("cannot mark the rebuild in {path} ({reason})", path = .path.display())]
    Marker {
        /// The marker's path.
        path: PathBuf,
        /// Why.
        reason: String,
    },
}

/// Reads `root`'s store, taking no lock: a store under this key version, why it needs a rebuild, or why it cannot be read
/// now.
#[must_use]
pub fn read_store(root: &Path) -> StoreRead {
    let path = durations_path(root);
    let parsed = match fs::read_to_string(&path) {
        Ok(text) => parse_store(&text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return StoreRead::NeedsRebuild(RebuildReason::Missing),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Err(error.to_string()),
        Err(error) => {
            return StoreRead::Unavailable(StoreReadError {
                path,
                reason: error.to_string(),
            });
        }
    };
    parsed.unwrap_or_else(|reason| StoreRead::NeedsRebuild(RebuildReason::Invalid(StoreReadError { path, reason })))
}

fn parse_store(text: &str) -> Result<StoreRead, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    match value.get("v").and_then(Value::as_u64) {
        Some(STORE_VERSION) => {}
        Some(other) => return Err(format!("format version {other} is not {STORE_VERSION}")),
        None => return Err("no format version".to_owned()),
    }
    match value.get("keyVersion").and_then(Value::as_u64) {
        Some(KEY_VERSION) => {}
        Some(newer) if newer > KEY_VERSION => return Ok(StoreRead::Newer),
        _ => return Ok(StoreRead::NeedsRebuild(RebuildReason::Stale)),
    }
    let file: StoreFileIn = serde_json::from_value(value).map_err(|error| error.to_string())?;
    Ok(StoreRead::Current(DurationStore { keys: file.keys }))
}

/// Writes `store` to `root`'s store through a temp file beside it moved over it, so a reader never sees it torn.
///
/// # Errors
///
/// The write or the move failed.
pub fn write_store(root: &Path, store: &DurationStore) -> Result<(), StoreWriteError> {
    let path = durations_path(root);
    let written = store.text().map_err(io::Error::other).and_then(|text| write_replacing(&path, &text));
    written.map_err(|error| StoreWriteError {
        path,
        reason: error.to_string(),
    })
}

/// What [`record_run`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// The run is in the store.
    Written,
    /// The store needs a rebuild, and nothing was written.
    NeedsRebuild(RebuildReason),
    /// The store cannot be read now: this run is not recorded, and nothing was written.
    Unavailable(StoreReadError),
    /// A newer build's store ([`StoreRead::Newer`]): this run is not recorded, and nothing was written.
    Newer,
}

/// Files a run released at `time` in `root`'s store, drops the keys with no release in the 14 days before `time`, and
/// writes it back: the step a gate runs inside the journal hold once its `release` line is written. A store that needs
/// a rebuild is left for [`rebuild_store`].
///
/// # Errors
///
/// The store could not be written.
pub fn record_run(root: &Path, key: &DurationKey, time: Timestamp, facts: &RunFacts) -> Result<Recorded, StoreError> {
    match read_store(root) {
        StoreRead::Current(mut store) => {
            store.record(key, time, facts);
            store.prune(time);
            write_store(root, &store)?;
            Ok(Recorded::Written)
        }
        StoreRead::NeedsRebuild(reason) => Ok(Recorded::NeedsRebuild(reason)),
        StoreRead::Unavailable(error) => Ok(Recorded::Unavailable(error)),
        StoreRead::Newer => Ok(Recorded::Newer),
    }
}

/// Who rebuilds the store, as the marker holds it.
#[derive(Serialize, Deserialize)]
struct Rebuilder {
    pid: u32,
    start: u64,
}

/// The gate that rebuilds the store: its pid and its process's start in milliseconds since the Unix epoch, as v1's
/// tickets name a gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebuildOwner {
    /// The gate's process id.
    pub pid: u32,
    /// The gate process's start, in milliseconds since the Unix epoch.
    pub start_ms: u64,
}

/// Claims the store's rebuild for `owner` by putting the marker `durations.rebuilding` in place whole: written to a temp
/// file beside it and hard-linked to its name, which fails when a marker is already there, so no gate reads one half
/// written. Returns whether this gate rebuilds. A marker already there is judged under the journal lock, waiting
/// [`JOURNAL_WAIT`] for it, so two gates never take one over together: one whose holder `holder_live` finds running
/// means another gate rebuilds; one whose holder is gone, or that does not read, is replaced by this gate's, unless
/// another process holds it open so it cannot be removed, which also means this gate does not rebuild.
/// [`swap_rebuilt`] removes it, or [`abandon_rebuild`] for a rebuild that ends without a swap.
///
/// # Errors
///
/// The marker could not be made, or a dead one removed for a reason other than another process holding it open, or the
/// journal lock was not taken to judge one already there.
pub fn begin_rebuild<L: JournalLock>(
    root: &Path,
    lock: &L,
    owner: RebuildOwner,
    holder_live: impl FnOnce(u32, u64) -> bool,
) -> Result<bool, StoreError> {
    let path = rebuilding_path(root);
    let failed = |error: io::Error| StoreError::Marker {
        path: path.clone(),
        reason: error.to_string(),
    };
    if link_marker(&path, owner).map_err(failed)? {
        return Ok(true);
    }
    let _held = lock.acquire(JOURNAL_WAIT)?;
    match read_marker(&path) {
        MarkerRead::Missing => {}
        MarkerRead::Holder(holder) if holder_live(holder.pid, holder.start) => return Ok(false),
        MarkerRead::Holder(_) | MarkerRead::Unreadable => match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) if held_open(&error) => return Ok(false),
            Err(error) => return Err(failed(error)),
        },
    }
    link_marker(&path, owner).map_err(failed)
}

/// `ERROR_SHARING_VIOLATION`: another process has the file open without sharing its delete.
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Whether `error`, met removing the marker, says another process holds it open: a sharing violation, or access denied,
/// as Windows says it of a file another handle keeps pending delete.
fn held_open(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied || (cfg!(windows) && error.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
}

/// Ends `owner`'s rebuild without a swap, as a gate interrupted while it rebuilds does: removes the marker when it is
/// still `owner`'s, so the next gate claims the rebuild at once. Why the marker could not be removed, when it could not.
#[must_use]
pub fn abandon_rebuild(root: &Path, owner: RebuildOwner) -> Option<String> {
    remove_own_marker(root, owner)
}

/// What the marker holds.
enum MarkerRead {
    /// There is no marker.
    Missing,
    /// The gate that holds it.
    Holder(Rebuilder),
    /// A marker that cannot be read or does not parse.
    Unreadable,
}

fn read_marker(path: &Path) -> MarkerRead {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_or(MarkerRead::Unreadable, MarkerRead::Holder),
        Err(error) if error.kind() == io::ErrorKind::NotFound => MarkerRead::Missing,
        Err(_) => MarkerRead::Unreadable,
    }
}

/// Puts `owner`'s marker at `path` whole: written to `<marker>.<pid>.tmp` beside it, then hard-linked to its name;
/// `false` when a marker is already there.
fn link_marker(path: &Path, owner: RebuildOwner) -> io::Result<bool> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    let temp = path.with_file_name(format!("{REBUILDING_FILE}.{}.tmp", owner.pid));
    let holder = Rebuilder {
        pid: owner.pid,
        start: owner.start_ms,
    };
    fs::write(&temp, serde_json::to_string(&holder).map_err(io::Error::other)?)?;
    // A hard link fails when the name exists, on Windows and Linux alike, so a marker already there is never replaced.
    let linked = fs::hard_link(&temp, path);
    // The temp file is only the marker's staging copy, rewritten whole by this gate's next claim; the link's own
    // outcome is the one to report.
    let _ = fs::remove_file(&temp);
    match linked {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// Removes the marker when it is still `owner`'s; another gate's, taken over meanwhile, is left. Returns why one could
/// not be removed.
fn remove_own_marker(root: &Path, owner: RebuildOwner) -> Option<String> {
    let path = rebuilding_path(root);
    let MarkerRead::Holder(holder) = read_marker(&path) else {
        return None;
    };
    if (holder.pid, holder.start) != (owner.pid, owner.start_ms) {
        return None;
    }
    match fs::remove_file(&path) {
        Ok(()) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => Some(format!("could not remove {}: {error}", path.display())),
    }
}

/// How far a rebuild read the live journal: its creation time, the byte past its last complete line, and the rolled
/// journal's identity, both taken before the read, so a roll after them shows at the swap; and when it read it, so the
/// swap finds the archives rolled since.
#[derive(Clone, Debug, PartialEq)]
struct JournalMark {
    created: Option<SystemTime>,
    offset: usize,
    rolled: Option<FileIdentity>,
    read_at: Timestamp,
}

/// How long before a rebuild's read of the journal an archive may be stamped and still hold lines appended after it:
/// the slack between the rebuild's clock and the clock the rolling gate stamped the archive with.
const ROLL_SLACK: SignedDuration = SignedDuration::from_hours(1);

/// A store rebuilt from the journal and the archive, what the rebuild has to say, and what [`swap_rebuilt`] needs to
/// file the runs released after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Rebuilt {
    /// The rebuilt store.
    pub store: DurationStore,
    /// Each file that could not be read, and each file's count of release lines that do not parse.
    pub notes: Vec<String>,
    mark: JournalMark,
    seen: HashSet<(Timestamp, Option<u64>)>,
    places: HashMap<String, KeyPlace>,
    temp_root: Option<String>,
}

/// The fields of a release line the rebuild reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseLine {
    #[serde(deserialize_with = "iso_time::deserialize")]
    time: Timestamp,
    pid: Option<u64>,
    #[serde(default)]
    kind: String,
    cwd: String,
    command: String,
    #[serde(default)]
    remote: bool,
    host: Option<String>,
    wall_seconds: Option<f64>,
    adjusted_seconds: Option<f64>,
    avg_cores: Option<f64>,
    peak_cores: Option<f64>,
    verdict: Option<String>,
}

impl ReleaseLine {
    fn outcome(&self) -> Option<Outcome> {
        let verdict = self.verdict.as_deref().and_then(Verdict::named)?;
        let cores = (self.avg_cores, self.peak_cores);
        Outcome::new(verdict, sample(self.time, self.wall_seconds, self.adjusted_seconds, cores))
    }
}

/// The release lines read so far, each counted once by its `time` and `pid`, and what reading them had to say.
#[derive(Default)]
struct Releases {
    seen: HashSet<(Timestamp, Option<u64>)>,
    runs: Vec<ReleaseLine>,
    notes: Vec<String>,
}

impl Releases {
    /// Reads the whole file at `path`, ungzipped for an archive; a missing file reads as empty.
    fn read(&mut self, path: &Path) {
        if let Some(bytes) = read_source(path, &mut self.notes) {
            self.collect(&bytes, path);
        }
    }

    /// Reads the archives of `root` stamped at or after `since`, newest first.
    fn read_archives(&mut self, root: &Path, since: Timestamp) {
        let dir = root.join(ARCHIVE_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.notes.push(format!("could not list the archive folder {}: {error}", dir.display()));
                return;
            }
        };
        let mut archives = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let stamp = archive_stamp(&entry.file_name().to_string_lossy());
                    if stamp.is_some_and(|stamp| stamp >= since) {
                        archives.push(entry.path());
                    }
                }
                Err(error) => self.notes.push(format!("could not list the archive folder {}: {error}", dir.display())),
            }
        }
        archives.sort_unstable_by(|a, b| b.cmp(a));
        for archive in archives {
            self.read(&archive);
        }
    }

    /// Adds the release lines of `bytes`, read from `source`, not seen before.
    fn collect(&mut self, bytes: &[u8], source: &Path) {
        let (lines, mut skipped) = release_lines(bytes);
        for line in lines {
            match serde_json::from_slice::<ReleaseLine>(line) {
                Ok(run) => {
                    if self.seen.insert((run.time, run.pid)) {
                        self.runs.push(run);
                    }
                }
                Err(_) => skipped += 1,
            }
        }
        if skipped > 0 {
            self.notes
                .push(format!("skipped {skipped} release line(s) of {} that do not parse", source.display()));
        }
    }

    /// Files the runs oldest first under the keys their folders give, placed by `place_of`. A `--slot none` run took no
    /// slot, and is left out as a live run leaves it out.
    fn file_into(&mut self, store: &mut DurationStore, temp_root: Option<&str>, mut place_of: impl FnMut(&str) -> KeyPlace) {
        let mut runs = std::mem::take(&mut self.runs);
        runs.sort_by_key(|run| run.time);
        for run in &runs {
            if run.kind == NO_SLOT {
                continue;
            }
            let Some(outcome) = run.outcome() else {
                continue;
            };
            let place = place_of(&run.cwd);
            let key = duration_key(&KeyInputs {
                folder: &place,
                remote_host: run.host.as_deref().filter(|_| run.remote),
                critical: run.kind == CRITICAL,
                temp_root,
                command: &run.command,
            });
            store.file(&key, run.time, &outcome);
        }
    }
}

/// A rebuild's reads, newest file first, so a roll between two reads leaves lines read twice, never missed: the live
/// journal ([`RebuildReads::journal`]), then the rolled journal ([`RebuildReads::rolled`]), then the archives
/// ([`RebuildReads::archives`]). [`rebuild_store`] runs them in that order.
pub struct RebuildReads {
    root: PathBuf,
    mark: JournalMark,
    releases: Releases,
}

impl RebuildReads {
    /// Reads the release lines of `root`'s `events.jsonl` up to its last complete line, first taking its creation time
    /// and the rolled journal's identity, and marks the read as made at `now`.
    #[must_use]
    pub fn journal(root: &Path, now: Timestamp) -> Self {
        let mut releases = Releases::default();
        let journal = root.join(JOURNAL_FILE);
        let rolled = rolled_identity(root, &mut releases.notes);
        let created = fs::metadata(&journal).and_then(|metadata| metadata.created()).ok();
        let bytes = read_source(&journal, &mut releases.notes).unwrap_or_default();
        let offset = bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at + 1);
        releases.collect(&bytes[..offset], &journal);
        RebuildReads {
            root: root.to_path_buf(),
            mark: JournalMark {
                created,
                offset,
                rolled,
                read_at: now,
            },
            releases,
        }
    }

    /// Reads the release lines of `events.1.jsonl`.
    pub fn rolled(&mut self) {
        self.releases.read(&self.root.join(ROLLED_JOURNAL_FILE));
    }

    /// Reads the release lines of the archives stamped within the 14 days before `now`, newest first; older archives
    /// hold only runs past the window.
    pub fn archives(&mut self, now: Timestamp) {
        self.releases.read_archives(&self.root, window_start(now));
    }

    /// The store of the runs read, filed oldest first by their `wallSeconds`, then the keys with no release in the 14
    /// days before `now` dropped. `key_place` places each folder, asked once per folder; a folder it cannot place is
    /// placed by its path's shape ([`place_by_shape`]).
    #[must_use]
    pub fn finish(mut self, temp_root: Option<&str>, now: Timestamp, key_place: impl Fn(&Path) -> Option<KeyPlace>) -> Rebuilt {
        let mut store = DurationStore::new();
        let mut places: HashMap<String, KeyPlace> = HashMap::new();
        self.releases.file_into(&mut store, temp_root, |cwd| {
            places
                .entry(cwd.to_owned())
                .or_insert_with(|| key_place(Path::new(cwd)).unwrap_or_else(|| place_by_shape(cwd)))
                .clone()
        });
        store.prune(now);
        Rebuilt {
            store,
            notes: self.releases.notes,
            mark: self.mark,
            seen: self.releases.seen,
            places,
            temp_root: temp_root.map(str::to_owned),
        }
    }
}

/// Rebuilds the store from the release lines of `root`'s `events.jsonl`, `events.1.jsonl` and the archives stamped in
/// the 14 days before `now`, newest first, taking no lock ([`RebuildReads`]): a line found twice counts once, by its
/// `time` and `pid`. A file that is missing reads as empty.
#[must_use]
pub fn rebuild_store(root: &Path, temp_root: Option<&str>, now: Timestamp, key_place: impl Fn(&Path) -> Option<KeyPlace>) -> Rebuilt {
    let mut reads = RebuildReads::journal(root, now);
    reads.rolled();
    reads.archives(now);
    reads.finish(temp_root, now, key_place)
}

/// The rolled journal's identity, or `None` (noted) when it cannot be told.
fn rolled_identity(root: &Path, notes: &mut Vec<String>) -> Option<FileIdentity> {
    let rolled = root.join(ROLLED_JOURNAL_FILE);
    file_identity(&rolled).unwrap_or_else(|error| {
        notes.push(format!("could not read {}: {error}", rolled.display()));
        None
    })
}

/// The bytes of a journal file, ungzipped for an archive, or `None` when it is missing or cannot be read (noted).
fn read_source(path: &Path, notes: &mut Vec<String>) -> Option<Vec<u8>> {
    let read = if path.extension().is_some_and(|extension| extension == "gz") {
        read_gzip(path)
    } else {
        fs::read(path)
    };
    match read {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            notes.push(format!("could not read {}: {error}", path.display()));
            None
        }
    }
}

fn read_gzip(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    GzDecoder::new(File::open(path)?).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Whether the rebuilt store went in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swapped {
    /// The rebuilt store, with the runs released since its reads, is in place.
    Replaced,
    /// A current store, or a newer build's, was written while the rebuild ran, and is kept.
    Kept,
}

/// What [`swap_rebuilt`] did, and what it has to say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Swap {
    /// Whether the rebuilt store went in.
    pub outcome: Swapped,
    /// Each journal file that could not be read, its count of release lines that do not parse, and a marker that could
    /// not be removed.
    pub notes: Vec<String>,
}

/// Puts a rebuilt store in place under the journal lock, waiting [`JOURNAL_WAIT`] for it, then removes the rebuild
/// marker when it is still `owner`'s, on every path: when the lock is not taken and when the write fails too. A store
/// that is current by then is kept (the swap that made it current filed the runs the same way), as is a newer build's
/// ([`StoreRead::Newer`]); any other is replaced
/// by the rebuilt store with every release line written after its reads filed: the rest of the journal it read, or
/// after a roll the whole rolled journal, the whole journal and the archives stamped from an hour before its read on,
/// each line counted once. A folder the rebuild did not place is placed under the longest worktree root it did place
/// that holds it, else by its path's shape, so no git runs under the lock. The keys with no release in the 14 days
/// before `now` are dropped.
///
/// # Errors
///
/// The lock was not taken, or the store could not be written.
pub fn swap_rebuilt<L: JournalLock>(root: &Path, lock: &L, rebuilt: &Rebuilt, now: Timestamp, owner: RebuildOwner) -> Result<Swap, StoreError> {
    let swapped = swap_holding(root, lock, rebuilt, now);
    let unremoved = remove_own_marker(root, owner);
    let mut swap = swapped?;
    swap.notes.extend(unremoved);
    Ok(swap)
}

/// [`swap_rebuilt`] up to the marker's removal.
fn swap_holding<L: JournalLock>(root: &Path, lock: &L, rebuilt: &Rebuilt, now: Timestamp) -> Result<Swap, StoreError> {
    let _held = lock.acquire(JOURNAL_WAIT)?;
    if matches!(read_store(root), StoreRead::Current(_) | StoreRead::Newer) {
        return Ok(Swap {
            outcome: Swapped::Kept,
            notes: Vec::new(),
        });
    }
    let mut releases = Releases {
        seen: rebuilt.seen.clone(),
        ..Releases::default()
    };
    read_after(root, &rebuilt.mark, &mut releases);
    let mut store = rebuilt.store.clone();
    releases.file_into(&mut store, rebuilt.temp_root.as_deref(), |cwd| {
        rebuilt
            .places
            .get(cwd)
            .cloned()
            .or_else(|| place_under_known_root(&rebuilt.places, cwd))
            .unwrap_or_else(|| place_by_shape(cwd))
    });
    store.prune(now);
    write_store(root, &store)?;
    Ok(Swap {
        outcome: Swapped::Replaced,
        notes: releases.notes,
    })
}

/// `cwd` placed under the longest worktree root among `places` that holds it at a folder boundary (ASCII case and `/`
/// against `\` ignored), or `None` when none does.
fn place_under_known_root(places: &HashMap<String, KeyPlace>, cwd: &str) -> Option<KeyPlace> {
    places
        .values()
        .filter_map(|place| match place {
            KeyPlace::InGit { repo, worktree_root, .. } => under_root(cwd, worktree_root).map(|rest| (repo, worktree_root, rest)),
            KeyPlace::OutsideGit { .. } => None,
        })
        .max_by_key(|(_, worktree_root, _)| worktree_root.len())
        .map(|(repo, worktree_root, rest)| KeyPlace::InGit {
            repo: repo.clone(),
            sub: if rest == "." { String::new() } else { rest },
            worktree_root: worktree_root.clone(),
        })
}

/// Reads the release lines written after `mark`: the rest of the journal when it was not rolled since; else the whole
/// rolled journal, the whole journal, and the archives stamped from [`ROLL_SLACK`] before the mark's read on, since a
/// second roll leaves the journal the rebuild read only in an archive.
fn read_after(root: &Path, mark: &JournalMark, releases: &mut Releases) {
    let journal = root.join(JOURNAL_FILE);
    let rolled = rolled_identity(root, &mut releases.notes);
    let created = fs::metadata(&journal).and_then(|metadata| metadata.created()).ok();
    let bytes = read_source(&journal, &mut releases.notes).unwrap_or_default();
    if rolled == mark.rolled && created == mark.created && bytes.len() >= mark.offset {
        releases.collect(&bytes[mark.offset..], &journal);
        return;
    }
    releases.read(&root.join(ROLLED_JOURNAL_FILE));
    releases.collect(&bytes, &journal);
    releases.read_archives(root, mark.read_at.checked_sub(ROLL_SLACK).unwrap_or(Timestamp::MIN));
}

/// A timestamp as the journal writes it, `2026-10-03T12:00:00.0000000Z`.
mod iso_time {
    use jiff::Timestamp;
    use serde::{Deserialize, Deserializer, Serializer, de};

    use crate::state::iso_utc;

    pub(super) fn serialize<S: Serializer>(time: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&iso_utc(*time))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}
