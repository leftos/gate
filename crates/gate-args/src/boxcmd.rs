//! The hidden `gate box <verb> --stdin`: the builder side of a routed run. Only the words are read here; the request
//! is stdin's first line, which the binary reads and decodes. `gate box register-desktop` takes no `--stdin`: it reads
//! no request; nor does `gate box prune --root <folder> [--keys <key>...]`, whose words are its root and the keys it
//! removes, nor `gate box hold --root <root> --holder <text> --ceiling <seconds> [--wait <seconds>] -- <command>
//! [args...]`, whose words are all it takes. A `test-seams` build also reads the hidden `--test-box-hold-poll-ms <n>`,
//! `--test-heartbeat-ms <n>`, `--test-box-console fail-before`, `--test-box-token refused`, `--test-box-park-launch <path>`,
//! `--test-outputs-budget-bytes <n>`, `--test-box-failure-cap-bytes <n>` (the Windows box's alone: the Unix box always
//! holds failure outputs to the built-in caps), `--test-inputs-budget-bytes <n>`, `--test-lfs-budget-bytes <n>`,
//! `--test-dotnet-limit-ms <n>`,
//! `--test-box-sample-ms <n>`, `--test-box-sampler-fail true|false`, `--test-box-free-percent <1-100>`,
//! `--test-box-drain-lease-ms <n>`, `--test-box-drain-lapse-ms <n>`, `--test-box-now <unix s>`,
//! `--test-box-drain-readable-only true|false`,
//! `--test-box-kill-leaves <n>`, `--test-probe-limit-ms <n>`, `--test-probe-drain-ms <n>`, `--test-box-desktop <fault>`,
//! `--test-box-desktop-session <state>`, `--test-box-desktop-mutex <name>`, `--test-box-desktop-ms <n>`,
//! `--test-box-desktop-message-ms <n>` and `--test-box-register ok|fail`, `--test-box-root-walk-stop <folder>`,
//! `--test-box-git-limit <step>:<ms>`, `--test-box-git-armed-by <path>`, `--test-box-git-long-ms <ms>`, `--test-box-gone-reached <path>`,
//! `--test-box-channel-ports <first>-<last>`, `--test-box-channel-accept-ms <n>`, `--test-box-channel-proof-ms <n>`,
//! `--test-box-channel-idle-ms <n>` and `--test-box-channel-send-buffer-bytes <n>` (each also as `--<seam>=<value>`)
//! before the verb; without the feature each is no verb, and so the box usage.

use std::ffi::{OsStr, OsString};
use std::iter::Peekable;
use std::path::PathBuf;

use crate::problem::Problem;
use crate::request::{BoxRequest, BoxSeams, BoxVerb, HoldRequest};

/// The one word after the verb: the request is stdin's first line, never a word.
const STDIN: &str = "--stdin";
/// The word after `prune`, before its root.
const ROOT: &str = "--root";
/// The word after a keyed `prune`'s root, before its keys.
const KEYS: &str = "--keys";
/// The most keys one keyed `prune` takes.
const MOST_KEYS: usize = 50;
/// The word before a `hold`'s holder text.
const HOLDER: &str = "--holder";
/// The word before a `hold`'s ceiling.
const CEILING: &str = "--ceiling";
/// The word before a `hold`'s wait for a slot.
const WAIT: &str = "--wait";
/// The word after a `hold`'s options, before its command.
const COMMAND: &str = "--";
/// The longest ceiling and wait a `hold` takes, in seconds: seven days.
const MOST_HOLD_SECONDS: u64 = 604_800;

/// The values `--test-box-register` takes: the registration succeeds, or fails as the seam refusing.
#[cfg(feature = "test-seams")]
const REGISTER_RESULTS: &[&str] = &["ok", "fail"];

/// The largest send buffer `--test-box-channel-send-buffer-bytes` takes: the 16 MiB a data channel's connection gets
/// without it.
#[cfg(feature = "test-seams")]
const CHANNEL_SEND_BUFFER_MOST: u32 = 16 * 1024 * 1024;

/// Reads the words after `gate box`: the seams, then a verb, then exactly `--stdin`, and nothing after it; or, for
/// `register-desktop`, which reads no request, nothing after the verb at all; or, for `prune`, exactly `--root` and
/// one word, its root, then nothing or `--keys` and its keys ([`prune_keys`]). A seam refused is its own problem, as
/// `gate run` reports it; any other shape is the one box usage problem, after those.
pub(crate) fn parse(words: impl Iterator<Item = OsString>) -> Result<BoxRequest, Vec<Problem>> {
    let mut words = words.peekable();
    let mut problems = Vec::new();
    let seams = seams(&mut words, &mut problems);
    let verb = words.next().as_deref().and_then(OsStr::to_str).and_then(verb);
    let (closed, root, keys, hold) = match verb {
        Some(BoxVerb::Prune) => match (words.next(), words.next()) {
            (Some(flag), Some(root)) if flag == ROOT => match prune_keys(&mut words) {
                Some(keys) => (true, Some(PathBuf::from(root)), keys, None),
                None => (false, None, Vec::new(), None),
            },
            _ => (false, None, Vec::new(), None),
        },
        Some(BoxVerb::Hold) => match hold_words(&mut words) {
            Some((root, hold)) => (true, Some(root), Vec::new(), Some(hold)),
            None => (false, None, Vec::new(), None),
        },
        // A verb that reads no request must have no word after it, which stands where `--stdin` stands for the others.
        Some(verb) if !verb.takes_request() => (words.peek().is_none(), None, Vec::new(), None),
        _ => (words.next().is_some_and(|word| word == STDIN), None, Vec::new(), None),
    };
    match (verb, closed, words.next()) {
        (Some(verb), true, None) if problems.is_empty() => Ok(BoxRequest {
            verb,
            seams,
            root,
            keys,
            hold,
        }),
        (Some(_), true, None) => Err(problems),
        _ => {
            problems.push(Problem::BoxUsage);
            Err(problems)
        }
    }
}

/// The keys after a `prune`'s root: none when no word follows the root; else, after exactly `--keys`, every word left,
/// 1 to [`MOST_KEYS`] words, each a key ([`is_key_word`]), a repeat in any case dropped. `None` for any other words, which is
/// the box usage.
fn prune_keys(words: &mut Peekable<impl Iterator<Item = OsString>>) -> Option<Vec<String>> {
    let Some(flag) = words.next() else {
        return Some(Vec::new());
    };
    if flag != KEYS {
        return None;
    }
    let mut keys: Vec<String> = Vec::new();
    for (given, word) in words.by_ref().enumerate() {
        let key = word.into_string().ok().filter(|key| is_key_word(key) && given < MOST_KEYS)?;
        // The builder's file system knows no case, so a key in another case names the same work folder.
        if !keys.iter().any(|kept| kept.eq_ignore_ascii_case(&key)) {
            keys.push(key);
        }
    }
    (!keys.is_empty()).then_some(keys)
}

/// Whether `word` is a key `--keys` takes: what `gate_wire::is_key` accepts, ASCII letters of either case, digits,
/// `-` and `_`, at least one, so no word can name a path; and not starting with `-`, so a flag is never read as a key.
fn is_key_word(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with('-')
        && word
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

/// The words of a `hold` after its verb: `--root`, `--holder` and `--ceiling`, each with its value, and `--wait` with
/// its own, in any order and each once, then `--` and its command, at least one word, every word after `--` kept as
/// given. The root is a word that is not empty, the holder any Unicode word (the box judges it), the ceiling whole
/// seconds above 0 and the wait whole seconds, 0 when not given, both at most [`MOST_HOLD_SECONDS`]. `None` for any
/// other words, which is the box usage.
fn hold_words(words: &mut impl Iterator<Item = OsString>) -> Option<(PathBuf, HoldRequest)> {
    let (mut root, mut holder, mut ceiling, mut wait) = (None, None, None, None);
    loop {
        let flag = words.next()?;
        if flag == COMMAND {
            break;
        }
        let value = words.next()?;
        let first = match flag.to_str()? {
            ROOT if !value.is_empty() => root.replace(PathBuf::from(value)).is_none(),
            HOLDER => holder.replace(value.into_string().ok()?).is_none(),
            CEILING => ceiling.replace(seconds(&value).filter(|seconds| *seconds > 0)?).is_none(),
            WAIT => wait.replace(seconds(&value)?).is_none(),
            _ => return None,
        };
        if !first {
            return None;
        }
    }
    let command: Vec<OsString> = words.collect();
    if command.is_empty() {
        return None;
    }
    let hold = HoldRequest {
        holder: holder?,
        ceiling: ceiling?,
        wait: wait.unwrap_or(0),
        command,
    };
    Some((root?, hold))
}

/// The whole seconds `word` gives: ASCII digits alone, at least one, at most [`MOST_HOLD_SECONDS`].
fn seconds(word: &OsStr) -> Option<u64> {
    let text = word.to_str()?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|seconds| *seconds <= MOST_HOLD_SECONDS)
}

fn verb(text: &str) -> Option<BoxVerb> {
    match text {
        "preflight" => Some(BoxVerb::Preflight),
        "run" => Some(BoxVerb::Run),
        "stop" => Some(BoxVerb::Stop),
        "boot" => Some(BoxVerb::Boot),
        "outputs" => Some(BoxVerb::Outputs),
        "inflight" => Some(BoxVerb::Inflight),
        "inputs" => Some(BoxVerb::Inputs),
        "lfs" => Some(BoxVerb::Lfs),
        "register-desktop" => Some(BoxVerb::RegisterDesktop),
        "prune" => Some(BoxVerb::Prune),
        "drain" => Some(BoxVerb::Drain),
        "hold" => Some(BoxVerb::Hold),
        "channel-check" => Some(BoxVerb::ChannelCheck),
        _ => None,
    }
}

/// A seam `gate box` reads before the verb.
#[cfg(feature = "test-seams")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seam {
    /// `--test-heartbeat-ms <n>`: the heartbeat's period.
    HeartbeatMs,
    /// `--test-box-console fail-before`: the command's UTF-8 console fails before the command starts.
    Console,
    /// `--test-box-token refused`: the command's start with the restricted token is refused as access denied.
    Token,
    /// `--test-box-park-launch <path>`: the command's launch waits until the path exists.
    ParkLaunch,
    /// `--test-outputs-budget-bytes <n>`: the output folder's outputs size budget, in bytes.
    OutputsBudgetBytes,
    /// `--test-box-failure-cap-bytes <n>`: the per-file cap on a failed run's failure outputs, in bytes, the total
    /// cap four times it. Only the Windows box takes it; the Unix box always uses the built-in caps.
    FailureCapBytes,
    /// `--test-inputs-budget-bytes <n>`: the most bytes one inputs put unpacks, decompressed.
    InputsBudgetBytes,
    /// `--test-lfs-budget-bytes <n>`: the most bytes one LFS put unpacks.
    LfsBudgetBytes,
    /// `--test-dotnet-limit-ms <n>`: how long the preflight's `dotnet --list-sdks` may take.
    DotnetLimitMs,
    /// `--test-box-sample-ms <n>`: the period of the run's samples of its machine.
    SampleMs,
    /// `--test-box-sampler-fail true|false`: every sample of the machine fails.
    SamplerFail,
    /// `--test-box-free-percent <1-100>`: the share of the machine the samples find free.
    FreePercent,
    /// `--test-box-drain-lease-ms <n>`: how long a drain ticket and a drain session stay live after their last poll or
    /// call.
    DrainLeaseMs,
    /// `--test-box-drain-lapse-ms <n>`: how long a drain barrier stands after its last raise.
    DrainLapseMs,
    /// `--test-box-now <unix s>`: the fixed time the drain judges its barrier, tickets and sessions at.
    Now,
    /// `--test-box-drain-readable-only true|false`: the drained test counts only processes whose command line it reads.
    DrainReadableOnly,
    /// `--test-box-kill-leaves <n>`: how many processes the ending of a job the ceiling killed reports left.
    KillLeaves,
    /// `--test-probe-limit-ms <n>`: how long the preflight's tool probes may take together.
    ProbeLimitMs,
    /// `--test-probe-drain-ms <n>`: how long the readers of each tool probe, and of dotnet's SDK list, get to end once the
    /// program has exited.
    ProbeDrainMs,
    /// `--test-netfx-key <path>`: the key under `HKEY_CURRENT_USER` the netfx-sdk probe reads.
    NetfxKey,
    /// `--test-box-desktop <fault>`: the fault a desktop run takes, in place of the real task start and helper.
    Desktop,
    /// `--test-box-desktop-session <state>`: the state a desktop run finds the console's session in.
    DesktopSession,
    /// `--test-box-desktop-mutex <name>`: the unique part of the private namespace and mutex a desktop run takes.
    DesktopMutex,
    /// `--test-box-desktop-ms <n>`: how long a desktop run's waits may take, in milliseconds.
    DesktopMs,
    /// `--test-box-desktop-message-ms <n>`: how long a desktop run waits for each message of the helper, in
    /// milliseconds.
    DesktopMessageMs,
    /// `--test-box-register ok|fail`: what `register-desktop` does in place of the Task Scheduler's work.
    Register,
    /// `--test-box-root-walk-stop <folder>`: the preflight's walk above the work root ends at this folder, unjudged.
    RootWalkStop,
    /// `--test-box-git-limit <step>:<ms>`: how long the box's git step `step` may take, in milliseconds.
    GitLimit,
    /// `--test-box-git-armed-by <path>`: the bound `--test-box-git-limit` gives its step starts only once this file
    /// exists.
    GitArmedBy,
    /// `--test-box-gone-reached <path>`: the file the box makes the first time it sees its client gone.
    GoneReached,
    /// `--test-box-git-long-ms <ms>`: how long each of the box's long git steps may take, in milliseconds.
    GitLongMs,
    /// `--test-box-hold-poll-ms <n>`: how long `hold` waits between its looks for a free slot, in milliseconds.
    HoldPollMs,
    /// `--test-box-channel-ports <first>-<last>`: the ports a data channel may listen on.
    ChannelPorts,
    /// `--test-box-channel-accept-ms <n>`: how long a data channel waits for the PC's connection.
    ChannelAcceptMs,
    /// `--test-box-channel-proof-ms <n>`: how long a data channel waits for the PC's whole proof once it connected.
    ChannelProofMs,
    /// `--test-box-channel-idle-ms <n>`: how long each read and write of a data channel may wait.
    ChannelIdleMs,
    /// `--test-box-channel-send-buffer-bytes <n>`: the send buffer a data channel's connection gets.
    ChannelSendBufferBytes,
}

#[cfg(feature = "test-seams")]
impl Seam {
    /// The seam `word` names, as `--<seam>` or `--<seam>=<value>`.
    fn named(word: &OsStr) -> Option<Seam> {
        let text = word.to_str()?;
        match text.split_once('=').map_or(text, |(name, _)| name) {
            "--test-heartbeat-ms" => Some(Seam::HeartbeatMs),
            "--test-box-console" => Some(Seam::Console),
            "--test-box-token" => Some(Seam::Token),
            "--test-box-park-launch" => Some(Seam::ParkLaunch),
            "--test-outputs-budget-bytes" => Some(Seam::OutputsBudgetBytes),
            "--test-box-failure-cap-bytes" => Some(Seam::FailureCapBytes),
            "--test-inputs-budget-bytes" => Some(Seam::InputsBudgetBytes),
            "--test-lfs-budget-bytes" => Some(Seam::LfsBudgetBytes),
            "--test-dotnet-limit-ms" => Some(Seam::DotnetLimitMs),
            "--test-box-sample-ms" => Some(Seam::SampleMs),
            "--test-box-sampler-fail" => Some(Seam::SamplerFail),
            "--test-box-free-percent" => Some(Seam::FreePercent),
            "--test-box-drain-lease-ms" => Some(Seam::DrainLeaseMs),
            "--test-box-drain-lapse-ms" => Some(Seam::DrainLapseMs),
            "--test-box-now" => Some(Seam::Now),
            "--test-box-drain-readable-only" => Some(Seam::DrainReadableOnly),
            "--test-box-kill-leaves" => Some(Seam::KillLeaves),
            "--test-probe-limit-ms" => Some(Seam::ProbeLimitMs),
            "--test-probe-drain-ms" => Some(Seam::ProbeDrainMs),
            "--test-netfx-key" => Some(Seam::NetfxKey),
            "--test-box-desktop" => Some(Seam::Desktop),
            "--test-box-desktop-session" => Some(Seam::DesktopSession),
            "--test-box-desktop-mutex" => Some(Seam::DesktopMutex),
            "--test-box-desktop-ms" => Some(Seam::DesktopMs),
            "--test-box-desktop-message-ms" => Some(Seam::DesktopMessageMs),
            "--test-box-register" => Some(Seam::Register),
            "--test-box-root-walk-stop" => Some(Seam::RootWalkStop),
            "--test-box-git-limit" => Some(Seam::GitLimit),
            "--test-box-git-armed-by" => Some(Seam::GitArmedBy),
            "--test-box-gone-reached" => Some(Seam::GoneReached),
            "--test-box-git-long-ms" => Some(Seam::GitLongMs),
            "--test-box-hold-poll-ms" => Some(Seam::HoldPollMs),
            "--test-box-channel-ports" => Some(Seam::ChannelPorts),
            "--test-box-channel-accept-ms" => Some(Seam::ChannelAcceptMs),
            "--test-box-channel-proof-ms" => Some(Seam::ChannelProofMs),
            "--test-box-channel-idle-ms" => Some(Seam::ChannelIdleMs),
            "--test-box-channel-send-buffer-bytes" => Some(Seam::ChannelSendBufferBytes),
            _ => None,
        }
    }

    /// Its option word.
    fn name(self) -> &'static str {
        match self {
            Seam::HeartbeatMs => "--test-heartbeat-ms",
            Seam::Console => "--test-box-console",
            Seam::Token => "--test-box-token",
            Seam::ParkLaunch => "--test-box-park-launch",
            Seam::OutputsBudgetBytes => "--test-outputs-budget-bytes",
            Seam::FailureCapBytes => "--test-box-failure-cap-bytes",
            Seam::InputsBudgetBytes => "--test-inputs-budget-bytes",
            Seam::LfsBudgetBytes => "--test-lfs-budget-bytes",
            Seam::DotnetLimitMs => "--test-dotnet-limit-ms",
            Seam::SampleMs => "--test-box-sample-ms",
            Seam::SamplerFail => "--test-box-sampler-fail",
            Seam::FreePercent => "--test-box-free-percent",
            Seam::DrainLeaseMs => "--test-box-drain-lease-ms",
            Seam::DrainLapseMs => "--test-box-drain-lapse-ms",
            Seam::Now => "--test-box-now",
            Seam::DrainReadableOnly => "--test-box-drain-readable-only",
            Seam::KillLeaves => "--test-box-kill-leaves",
            Seam::ProbeLimitMs => "--test-probe-limit-ms",
            Seam::ProbeDrainMs => "--test-probe-drain-ms",
            Seam::NetfxKey => "--test-netfx-key",
            Seam::Desktop => "--test-box-desktop",
            Seam::DesktopSession => "--test-box-desktop-session",
            Seam::DesktopMutex => "--test-box-desktop-mutex",
            Seam::DesktopMs => "--test-box-desktop-ms",
            Seam::DesktopMessageMs => "--test-box-desktop-message-ms",
            Seam::Register => "--test-box-register",
            Seam::RootWalkStop => "--test-box-root-walk-stop",
            Seam::GitLimit => "--test-box-git-limit",
            Seam::GitArmedBy => "--test-box-git-armed-by",
            Seam::GoneReached => "--test-box-gone-reached",
            Seam::GitLongMs => "--test-box-git-long-ms",
            Seam::HoldPollMs => "--test-box-hold-poll-ms",
            Seam::ChannelPorts => "--test-box-channel-ports",
            Seam::ChannelAcceptMs => "--test-box-channel-accept-ms",
            Seam::ChannelProofMs => "--test-box-channel-proof-ms",
            Seam::ChannelIdleMs => "--test-box-channel-idle-ms",
            Seam::ChannelSendBufferBytes => "--test-box-channel-send-buffer-bytes",
        }
    }
}

/// Reads the seams before the verb, in any order, each problem in `problems`: a value from the word itself after `=`
/// or from the next word unless that word is another `--` option, each seam given once.
#[cfg(feature = "test-seams")]
fn seams(words: &mut Peekable<impl Iterator<Item = OsString>>, problems: &mut Vec<Problem>) -> BoxSeams {
    let mut seams = BoxSeams::default();
    let mut given: Vec<Seam> = Vec::new();
    // Each word is named once: peeked, and taken only when it names a seam.
    while let Some((seam, word)) = words
        .peek()
        .and_then(|word| Seam::named(word))
        .and_then(|seam| words.next().map(|word| (seam, word)))
    {
        let value = match word.to_string_lossy().split_once('=') {
            Some((_, value)) => Ok(Some(value.to_owned())),
            None => crate::run::next_value(words, |next| next.starts_with("--")),
        };
        if given.contains(&seam) {
            problems.push(Problem::GivenTwice(seam.name().to_owned()));
            continue;
        }
        given.push(seam);
        if let Err(problem) = set_seam(&mut seams, seam, value) {
            problems.push(problem);
        }
    }
    // A file that arms a bound is the bound's, so one given without `--test-box-git-limit` has nothing to arm; a limit
    // given with a value it refuses is that value's problem alone.
    if seams.git_armed_by.is_some() && !given.contains(&Seam::GitLimit) {
        problems.push(Problem::GitArmedByNeedsLimit);
    }
    seams
}

/// Sets `seam` from `value`: whole milliseconds above 0 for the heartbeat, the dotnet limit, the sample period, the
/// probe limit, the probe drain, the desktop waits, the desktop message limit and the long git steps' bound, a step and
/// whole milliseconds above 0 after its last `:` for the git limit, `fail-before` for the console, `refused` for the
/// token, any path for the park, the git bound's arm and the gone mark, whole bytes above 0 for the outputs, the inputs
/// and the LFS budgets, `true` or `false` for the sampler's failure, a whole percent from 1 to 100 for the free share, a
/// whole count above 0 for the processes a kill leaves, one of the desktop faults for the desktop, one of the session
/// states for the desktop session, 1 to 64 letters, digits and `-` for the
/// desktop mutex, `ok` or `fail` for the registration, any key path for the netfx key and any path for the root walk's
/// stop. An empty or missing value is no value.
#[cfg(feature = "test-seams")]
fn set_seam(seams: &mut BoxSeams, seam: Seam, value: Result<Option<String>, Problem>) -> Result<(), Problem> {
    let name = seam.name();
    let value = match value? {
        Some(value) if !value.is_empty() => value,
        _ => return Err(Problem::NeedsValue(name.to_owned())),
    };
    match seam {
        Seam::HeartbeatMs => seams.heartbeat_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::OutputsBudgetBytes => seams.outputs_budget_bytes = Some(crate::values::above_zero(name, &value)?),
        Seam::FailureCapBytes => seams.failure_cap_bytes = Some(crate::values::above_zero(name, &value)?),
        Seam::InputsBudgetBytes => seams.inputs_budget_bytes = Some(crate::values::above_zero(name, &value)?),
        Seam::LfsBudgetBytes => seams.lfs_budget_bytes = Some(crate::values::above_zero(name, &value)?),
        Seam::DotnetLimitMs => seams.dotnet_limit_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::SampleMs => seams.sample_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::SamplerFail => seams.sampler_fail = crate::values::true_or_false(name, &value)?,
        Seam::FreePercent => seams.free_percent = Some(crate::values::in_range(name, &value, 1, 100)?),
        Seam::DrainLeaseMs => seams.drain_lease_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::DrainLapseMs => seams.drain_lapse_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::HoldPollMs => seams.hold_poll_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::Now => seams.now = Some(crate::values::above_zero(name, &value)?),
        Seam::DrainReadableOnly => {
            seams.drain_readable_only = if crate::values::true_or_false(name, &value)? {
                crate::request::ProcessScan::ReadableOnly
            } else {
                crate::request::ProcessScan::All
            };
        }
        Seam::KillLeaves => seams.kill_leaves = Some(crate::values::above_zero(name, &value)?),
        Seam::ProbeLimitMs => seams.probe_limit_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::ProbeDrainMs => seams.probe_drain_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::NetfxKey => seams.netfx_key = Some(value),
        Seam::Desktop => seams.desktop = Some(crate::values::desktop_mode(name, &value)?),
        Seam::DesktopSession => seams.desktop_session = Some(crate::values::desktop_session(name, &value)?),
        Seam::DesktopMutex => seams.desktop_mutex = Some(crate::values::desktop_mutex(name, &value)?),
        Seam::DesktopMs => seams.desktop_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::DesktopMessageMs => seams.desktop_message_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::GitLimit => seams.git_limit = Some(crate::values::git_limit(name, &value)?),
        Seam::GitArmedBy => seams.git_armed_by = Some(PathBuf::from(value)),
        Seam::GoneReached => seams.gone_reached = Some(PathBuf::from(value)),
        Seam::GitLongMs => seams.git_long_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::ChannelPorts => seams.channel_ports = Some(crate::values::port_range(name, &value)?),
        Seam::ChannelAcceptMs => seams.channel_accept_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::ChannelProofMs => seams.channel_proof_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::ChannelIdleMs => seams.channel_idle_ms = Some(crate::values::above_zero(name, &value)?),
        Seam::ChannelSendBufferBytes => {
            seams.channel_send_buffer_bytes = Some(crate::values::in_range(name, &value, 1, CHANNEL_SEND_BUFFER_MOST)?);
        }
        Seam::Register => seams.register_ok = Some(crate::values::remote_call(name, &value, REGISTER_RESULTS)? == "ok"),
        Seam::Console => seams.fail_console = the_word(name, value, "fail-before", |option, value| Problem::BadConsoleFailure { option, value })?,
        Seam::Token => seams.refuse_token = the_word(name, value, "refused", |option, value| Problem::BadTokenRefusal { option, value })?,
        Seam::ParkLaunch => seams.park_launch = Some(std::path::PathBuf::from(value)),
        Seam::RootWalkStop => seams.root_walk_stop = Some(std::path::PathBuf::from(value)),
    }
    Ok(())
}

/// `true` when `value`, given to the seam `name`, is `word`, the one value it takes; else the problem `bad` makes of
/// the seam's name and the value.
#[cfg(feature = "test-seams")]
fn the_word(name: &str, value: String, word: &str, bad: impl FnOnce(String, String) -> Problem) -> Result<bool, Problem> {
    if value == word { Ok(true) } else { Err(bad(name.to_owned(), value)) }
}

/// No seams without the `test-seams` feature: a `--test-*` word is read as the verb, and refused as one.
#[cfg(not(feature = "test-seams"))]
fn seams(_words: &mut Peekable<impl Iterator<Item = OsString>>, _problems: &mut Vec<Problem>) -> BoxSeams {
    BoxSeams::default()
}
