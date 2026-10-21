//! Value validation, shared by the CLI and the compatibility layer.

use std::ffi::OsString;

use crate::problem::{MAX_WHOLE_NUMBER, Problem};
use crate::request::{Ceiling, Slot};
#[cfg(feature = "test-seams")]
use crate::request::{DESKTOP_MODES, DESKTOP_SESSIONS, DesktopMode, DesktopSession};

/// How a value reads as a whole number.
enum Whole {
    /// Not a run of ASCII digits: a sign, a space, a fraction, a word or nothing.
    Invalid,
    /// Digits for a number above [`MAX_WHOLE_NUMBER`], however many.
    TooLarge,
    Number(u64),
}

fn whole(value: &str) -> Whole {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Whole::Invalid;
    }
    match value.parse::<u64>() {
        Ok(number) if number <= MAX_WHOLE_NUMBER => Whole::Number(number),
        _ => Whole::TooLarge,
    }
}

fn too_large(option: &str, value: &str) -> Problem {
    Problem::TooLarge {
        option: option.to_owned(),
        value: value.to_owned(),
    }
}

/// The longest `gate burn` may run: the owner's 120 s burn cap plus 60 s of margin.
pub(crate) const MAX_BURN_SECONDS: u32 = 180;

pub(crate) fn above_zero(option: &str, value: &str) -> Result<u64, Problem> {
    above_zero_shown(option, value, value)
}

/// The whole number above 0 that `digits` spell, a problem naming the value as given, `shown`.
fn above_zero_shown(option: &str, digits: &str, shown: &str) -> Result<u64, Problem> {
    match whole(digits) {
        Whole::Number(number) if number > 0 => Ok(number),
        Whole::TooLarge => Err(too_large(option, shown)),
        Whole::Number(_) | Whole::Invalid => Err(Problem::NotAboveZero {
            option: option.to_owned(),
            value: shown.to_owned(),
        }),
    }
}

/// The prefix of an auto ceiling's value, read in any ASCII case.
const AUTO_PREFIX: &str = "auto:";

/// The subcommand CLI's `--timeout`: a whole number above 0, or `auto:<n>` for a floor `n` read the same way, `auto` in
/// any ASCII case. A problem names the whole value as given; a bare `auto` needs its floor.
pub(crate) fn ceiling(option: &str, value: &str) -> Result<Ceiling, Problem> {
    if value.eq_ignore_ascii_case("auto") {
        return Err(Problem::AutoNeedsFloor(option.to_owned()));
    }
    match value.get(..AUTO_PREFIX.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(AUTO_PREFIX) => above_zero_shown(option, &value[AUTO_PREFIX.len()..], value).map(Ceiling::Auto),
        _ => above_zero(option, value).map(Ceiling::Given),
    }
}

pub(crate) fn zero_or_above(option: &str, value: &str) -> Result<u64, Problem> {
    match whole(value) {
        Whole::Number(number) => Ok(number),
        Whole::TooLarge => Err(too_large(option, value)),
        Whole::Invalid => Err(Problem::NotZeroOrAbove {
            option: option.to_owned(),
            value: value.to_owned(),
        }),
    }
}

/// A whole number from `low` to `high`, for an option bounded at both ends such as `gate burn`'s `--seconds`. Every
/// value outside the range takes the one line, one above the whole-number ceiling included.
pub(crate) fn in_range(option: &str, value: &str, low: u32, high: u32) -> Result<u32, Problem> {
    let out_of_range = || Problem::NotInRange {
        option: option.to_owned(),
        value: value.to_owned(),
        low,
        high,
    };
    match whole(value) {
        Whole::Number(number) if u64::from(low) <= number && number <= u64::from(high) => u32::try_from(number).map_err(|_| out_of_range()),
        Whole::Number(_) | Whole::TooLarge | Whole::Invalid => Err(out_of_range()),
    }
}

/// A whole number above 0 as a `u32`, [`above_zero`] for a count no option may set as high as a `u32` holds.
pub(crate) fn above_zero_u32(option: &str, value: &str) -> Result<u32, Problem> {
    let number = above_zero(option, value)?;
    // `above_zero` refuses anything above `MAX_WHOLE_NUMBER`, which is below `u32::MAX`.
    u32::try_from(number).map_err(|_| Problem::NotAboveZero {
        option: option.to_owned(),
        value: value.to_owned(),
    })
}

/// The changes `--test-remote-tamper` may name, each its point and its step.
#[cfg(feature = "test-seams")]
pub(crate) const TAMPERS: &[&str] = &[
    "before-hold:case",
    "before-hold:replace",
    "before-hold:replace-root",
    "after-first-snapshot:worktree",
];
/// The calls `--test-remote-interrupt-at` may name.
#[cfg(feature = "test-seams")]
pub(crate) const INTERRUPTED_CALLS: &[&str] = &[
    "preflight",
    "inputs",
    "lfs",
    "run",
    "outputs",
    "install",
    "sdks",
    "stop",
    "manifest",
    "apply",
];
/// The calls `--test-remote-watch-call` may name.
#[cfg(feature = "test-seams")]
pub(crate) const WATCHED_CALLS: &[&str] = &[
    "preflight",
    "push",
    "inputs-diff",
    "inputs",
    "lfs-diff",
    "lfs",
    "run",
    "ls-remote",
    "fetch",
    "outputs",
];

/// A call of the remote client a seam names, one of `calls`, as written.
#[cfg(feature = "test-seams")]
pub(crate) fn remote_call(option: &str, value: &str, calls: &'static [&'static str]) -> Result<String, Problem> {
    if calls.contains(&value) {
        return Ok(value.to_owned());
    }
    Err(Problem::BadRemoteCall {
        option: option.to_owned(),
        value: value.to_owned(),
        calls,
    })
}

/// The step and the whole milliseconds above 0 that `--test-box-git-limit`'s `<step>:<ms>` names, split at its last
/// `:`, so a step whose name holds one is named whole; else the problem naming the value as given: no `:` or no step
/// before it, or milliseconds [`above_zero`] refuses.
#[cfg(feature = "test-seams")]
pub(crate) fn git_limit(option: &str, value: &str) -> Result<(String, u64), Problem> {
    match value.rsplit_once(':') {
        Some((step, ms)) if !step.is_empty() => Ok((step.to_owned(), above_zero_shown(option, ms, value)?)),
        _ => Err(Problem::BadGitLimit {
            option: option.to_owned(),
            value: value.to_owned(),
        }),
    }
}

/// The fault `--test-box-desktop` names, one of [`DESKTOP_MODES`]; else the problem naming the value.
#[cfg(feature = "test-seams")]
pub(crate) fn desktop_mode(option: &str, value: &str) -> Result<DesktopMode, Problem> {
    DESKTOP_MODES
        .iter()
        .find_map(|(word, mode)| (*word == value).then_some(*mode))
        .ok_or_else(|| Problem::BadDesktopMode {
            option: option.to_owned(),
            value: value.to_owned(),
        })
}

/// The state `--test-box-desktop-session` names, one of [`DESKTOP_SESSIONS`]; else the problem naming the value.
#[cfg(feature = "test-seams")]
pub(crate) fn desktop_session(option: &str, value: &str) -> Result<DesktopSession, Problem> {
    DESKTOP_SESSIONS
        .iter()
        .find_map(|(word, state)| (*word == value).then_some(*state))
        .ok_or_else(|| Problem::BadDesktopSession {
            option: option.to_owned(),
            value: value.to_owned(),
        })
}

/// The longest unique part `--test-box-desktop-mutex` takes.
#[cfg(feature = "test-seams")]
pub(crate) const MAX_MUTEX_NAME: usize = 64;

/// The unique part `--test-box-desktop-mutex` names: 1 to [`MAX_MUTEX_NAME`] ASCII letters, digits and `-`, the rule a
/// private namespace's unique part is held to; else the problem naming the value.
#[cfg(feature = "test-seams")]
pub(crate) fn desktop_mutex(option: &str, value: &str) -> Result<String, Problem> {
    let named = !value.is_empty() && value.len() <= MAX_MUTEX_NAME && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if named {
        Ok(value.to_owned())
    } else {
        Err(Problem::BadDesktopMutex {
            option: option.to_owned(),
            value: value.to_owned(),
        })
    }
}

pub(crate) fn slot(option: &str, value: &str) -> Result<Slot, Problem> {
    [Slot::Heavy, Slot::Light, Slot::Critical, Slot::None]
        .into_iter()
        .find(|slot| slot.as_str().eq_ignore_ascii_case(value))
        .ok_or_else(|| Problem::BadSlot {
            option: option.to_owned(),
            value: value.to_owned(),
        })
}

/// A switch's colon value: `true`, `$true`, `false` or `$false`, in any ASCII case.
pub(crate) fn true_or_false(option: &str, value: &str) -> Result<bool, Problem> {
    let bare = value.strip_prefix('$').unwrap_or(value);
    if bare.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if bare.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        Err(Problem::NotTrueOrFalse {
            option: option.to_owned(),
            value: value.to_owned(),
        })
    }
}

/// A process id: a run of ASCII digits for a number from 1 to `u32::MAX`.
pub(crate) fn pid(value: &str) -> Result<u32, Problem> {
    let digits = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    digits
        .then(|| value.parse::<u32>().ok())
        .flatten()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| Problem::BadPid(value.to_owned()))
}

/// The names the gate sets on every command it starts, over the environment it starts it in: the slot marker, a commit
/// run's own marker and the state root, the build-server settings of the run, the `GIT_CONFIG_COUNT` entry the box gives git, and the Event Log
/// logging provider turned off. A caller may set none of them; the gate sets each after the caller's own pairs.
pub const GATE_SET_NAMES: &[&str] = &[
    "GATE_SLOT_HELD",
    "GATE_COMMIT_SLOT",
    "GATE_STATE_DIR",
    "SharedCompilationId",
    "MSBUILDNODEHANDSHAKESALT",
    "MSBUILDUSESERVER",
    "DOTNET_CLI_USE_MSBUILD_SERVER",
    "UseSharedCompilation",
    "MSBUILDDISABLENODEREUSE",
    "GIT_CONFIG_COUNT",
    "Logging__EventLog__LogLevel__Default",
    "Logging:EventLog:LogLevel:Default",
];

/// The names git itself reads over the config entries the gate sets ([`GATE_SET_NAMES`], [`GATE_SET_PREFIXES`]): a value
/// the caller gave one of them would be read after the gate's own entries, undoing the `safe.directory` the box gives the
/// git a command runs. The gate sets none of them, so they refuse for their own reason ([`EnvRefusal::GitReads`]).
pub const GIT_READ_NAMES: &[&str] = &["GIT_CONFIG_PARAMETERS", "GIT_CONFIG"];

/// The names the gate sets on a command with a number after them: the `safe.directory` entry the box appends to git's
/// config takes the index `GIT_CONFIG_COUNT` holds, as `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>`.
pub const GATE_SET_PREFIXES: &[&str] = &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

/// The prefix of the gate's own names, none of which a caller may set.
pub const GATE_NAME_PREFIX: &str = "GATE_";

/// The names of machine paths the gate keeps for the command: the builder's own, which a caller's value would move out
/// from under it, `GODOT_PATH`, which a routed run of a repo pinning one Godot sets to the install it resolved, and
/// `UE_ROOT`, which a routed run of a repo pinning one Unreal Engine sets to the engine it resolved.
pub const MACHINE_PATH_NAMES: &[&str] = &[
    "PATH",
    "PATHEXT",
    "TEMP",
    "TMP",
    "SystemRoot",
    "windir",
    "ComSpec",
    "USERPROFILE",
    "HOME",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "GODOT_PATH",
    "UE_ROOT",
];

/// The most names a caller may give for the command's environment.
pub const MOST_ENV_NAMES: usize = 64;
/// The most the names and values of a caller's environment may take together, in bytes.
pub const MOST_ENV_BYTES: usize = 32 * 1024;

/// Why a name and value the caller gives for the command's environment cannot be set; `None` when the gate can set it.
///
/// A name holding a character outside the ASCII set a name may hold ([`name_characters`]), and a value holding a NUL,
/// cannot be carried by an environment block. A name the gate sets itself ([`GATE_SET_NAMES`], [`GATE_SET_PREFIXES`]), a
/// name git would read over the gate's own git settings ([`GIT_READ_NAMES`]), a `GATE_` name and a machine path
/// ([`MACHINE_PATH_NAMES`]) are the gate's own: refusing them is what makes one `--env` mean the same thing on the PC
/// and on a builder. Names are compared without regard to ASCII case.
#[must_use]
pub fn env_refusal(name: &str, value: &str) -> Option<EnvRefusal> {
    if let Some(refusal) = malformed(name, value) {
        return Some(refusal);
    }
    if gate_set(name) {
        return Some(EnvRefusal::GateSets);
    }
    if named(name, GIT_READ_NAMES) {
        return Some(EnvRefusal::GitReads);
    }
    if starts_with_ignoring_case(name, GATE_NAME_PREFIX) {
        return Some(EnvRefusal::GateName);
    }
    named(name, MACHINE_PATH_NAMES).then_some(EnvRefusal::MachinePath)
}

/// Why the pair is malformed: a name holding a character no name may hold ([`name_characters`]), or a value holding a
/// NUL.
fn malformed(name: &str, value: &str) -> Option<EnvRefusal> {
    if !name_characters(name) {
        return Some(EnvRefusal::MalformedName);
    }
    value.contains('\0').then_some(EnvRefusal::MalformedValue)
}

/// Whether `name` holds at least one character and none but the printable ASCII characters, `=` excepted: U+0021 to
/// U+007E. A space, every control character and every character outside ASCII are refused.
///
/// The set is as wide as it is because a name is whatever the tool that reads the variable calls it: npm names a
/// per-registry token `npm_config_//registry.npmjs.org/:_authToken`, and `/`, `@` and the rest of the printable
/// punctuation appear in other tools' names. The colon is .NET's colon-form setting key and the parentheses are
/// `ProgramFiles(x86)`. What the set buys is that a name is ASCII: the ASCII-case check of the tables below is then the
/// same check Windows makes when it reads a name, so a name that is not ASCII can never pass a table check written in
/// ASCII and then be read, under Windows' own comparison of names, as one the gate protects. Do not widen this to
/// non-ASCII.
fn name_characters(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_graphic() && byte != b'=')
}

/// Whether `name` is one the gate sets itself: a name of [`GATE_SET_NAMES`], a numbered name of
/// [`GATE_SET_PREFIXES`], or one of .NET's Event Log logging settings in any spelling ([`event_log_name`]).
fn gate_set(name: &str) -> bool {
    named(name, GATE_SET_NAMES) || GATE_SET_PREFIXES.iter().any(|prefix| numbered(name, prefix)) || event_log_name(name)
}

/// Whether `name` is one of .NET's Event Log logging settings, in any of the spellings .NET reads: an optional leading
/// `DOTNET_` or `ASPNETCORE_` removed, then, read in ASCII lower case with every `__` taken as the `:` of a key, a name
/// under `logging:eventlog:`, or one under the full type name of the provider that setting belongs to
/// (`logging:microsoft.extensions.logging.eventlog.eventlogloggerprovider:`). So `Logging__EventLog__LogLevel__Default`,
/// its colon spelling, a level below the default, either host prefix and the provider's own type name all refuse.
fn event_log_name(name: &str) -> bool {
    let bare = ["DOTNET_", "ASPNETCORE_"]
        .iter()
        .find_map(|prefix| name.get(prefix.len()..).filter(|_| starts_with_ignoring_case(name, prefix)))
        .unwrap_or(name);
    let folded = bare.to_ascii_lowercase().replace("__", ":");
    [
        "logging:eventlog:",
        "logging:microsoft.extensions.logging.eventlog.eventlogloggerprovider:",
    ]
    .iter()
    .any(|prefix| folded.starts_with(prefix))
}

/// Whether any name of `names` is `name`, compared without regard to ASCII case.
fn named(name: &str, names: &[&str]) -> bool {
    names.iter().any(|set| set.eq_ignore_ascii_case(name))
}

/// The first name of `names` an earlier one equals without regard to ASCII case, as the later spelling gives it; `None`
/// when every name is set once.
#[must_use]
pub fn repeated_name<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let mut seen: Vec<&str> = Vec::new();
    for name in names {
        if seen.iter().any(|set| set.eq_ignore_ascii_case(name)) {
            return Some(name);
        }
        seen.push(name);
    }
    None
}

/// Whether a caller's environment is over a cap: more than [`MOST_ENV_NAMES`] names, or more than [`MOST_ENV_BYTES`]
/// bytes of names and values together. Read from the request before any pair of it is built or checked, so a request
/// holding a hundred thousand names is refused without one of them being built.
#[must_use]
pub fn over_cap(names: usize, bytes: usize) -> bool {
    names > MOST_ENV_NAMES || bytes > MOST_ENV_BYTES
}

/// The most characters of a name a refusal line shows, before [`shown_name`] shortens it.
const LONGEST_SHOWN_NAME: usize = 200;

/// `name` as a refusal line shows it: its first [`LONGEST_SHOWN_NAME`] characters, with `…` after them when there are
/// more, so a name of any length says which name it is without filling the line. Shortened before it is escaped.
#[must_use]
pub fn shown_name(name: &str) -> String {
    match name.char_indices().nth(LONGEST_SHOWN_NAME) {
        Some((end, _)) => format!("{}…", &name[..end]),
        None => name.to_owned(),
    }
}

/// Why a name or value the caller gives for the command's environment cannot be set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvRefusal {
    /// The name holds a character outside the ASCII set a name may hold ([`name_characters`]).
    MalformedName,
    /// The value holds a NUL.
    MalformedValue,
    /// A name the gate sets on every command it starts.
    GateSets,
    /// A name git would read over the gate's own git settings, so a caller's value could undo them.
    GitReads,
    /// A `GATE_` name no table lists, the gate's own.
    GateName,
    /// A machine path the gate keeps for the command.
    MachinePath,
    /// A name given twice, in any letter case.
    Twice,
    /// More pairs or more bytes than a request may carry ([`MOST_ENV_NAMES`], [`MOST_ENV_BYTES`]).
    OverCap,
}

impl EnvRefusal {
    /// The line the box says for this refusal, `name` being the pair's name shortened ([`shown_name`]) and escaped as
    /// the box escapes one. The cap refusal names no name and ignores it.
    #[must_use]
    pub fn box_line(&self, name: &str) -> String {
        match self {
            EnvRefusal::MalformedName => format!("gate: box payload Env has a malformed name '{name}'"),
            EnvRefusal::MalformedValue => format!("gate: box payload Env gives {name} a value holding NUL"),
            EnvRefusal::GateSets => format!("gate: box payload Env sets {name}, which the gate sets itself"),
            EnvRefusal::GitReads => {
                format!("gate: box payload Env sets {name}, which git would read over the gate's own git settings")
            }
            EnvRefusal::GateName => format!("gate: box payload Env sets {name}: GATE_ names are the gate's"),
            EnvRefusal::MachinePath => format!("gate: box payload Env sets {name}, a machine path the builder keeps"),
            EnvRefusal::Twice => format!("gate: box payload Env names {name} twice"),
            EnvRefusal::OverCap => format!(
                "gate: box payload Env is over its cap of {MOST_ENV_NAMES} names or {} KiB",
                MOST_ENV_BYTES / 1024
            ),
        }
    }

    /// The usage line the command line says for this refusal of the `--env` word at `position`, `name` being the pair's
    /// name shortened ([`shown_name`]) and escaped ([`escaped`]). A name the gate cannot carry is not said at all, so a
    /// word pasted without its `NAME=` cannot reach the line; the cap refusal names no name and ignores the position.
    #[must_use]
    pub fn usage_line(&self, name: &str, position: usize) -> String {
        match self {
            EnvRefusal::MalformedName => format!("gate: --env word {position} has a malformed name"),
            EnvRefusal::MalformedValue => format!("gate: --env gives {name} a value holding NUL"),
            EnvRefusal::GateSets => format!("gate: --env sets {name}, which the gate sets itself"),
            EnvRefusal::GitReads => format!("gate: --env sets {name}, which git would read over the gate's own git settings"),
            EnvRefusal::GateName => format!("gate: --env sets {name}: GATE_ names are the gate's"),
            EnvRefusal::MachinePath => format!("gate: --env sets {name}, a machine path the gate keeps for the command"),
            EnvRefusal::Twice => format!("gate: --env names {name} twice"),
            EnvRefusal::OverCap => format!("gate: --env is over its cap of {MOST_ENV_NAMES} names or {} KiB", MOST_ENV_BYTES / 1024),
        }
    }
}

/// A `--env` word as a pair: `NAME=value` split at its first `=`, an empty value allowed; or a bare `NAME`, the
/// `position`-th word among the run's `--env` words, whose value `forward` gives from the gate's own environment. The
/// pair is then refused as any pair is ([`env_refusal`]).
pub(crate) fn env_pair(word: &str, position: usize, forward: impl Fn(&str) -> Option<OsString>) -> Result<(String, String), Problem> {
    let (name, value) = match word.split_once('=') {
        Some((name, value)) => (name.to_owned(), value.to_owned()),
        None => forwarded(word, position, forward)?,
    };
    match env_refusal(&name, &value) {
        Some(refusal) => Err(Problem::EnvRefused { refusal, name, position }),
        None => Ok((name, value)),
    }
}

/// The pair of a bare `--env` word: the word as the name and the value `forward` gives it. A word `forward` does not set
/// is refused as the `position`-th word of the run, and so is one whose value is not Unicode.
fn forwarded(word: &str, position: usize, forward: impl Fn(&str) -> Option<OsString>) -> Result<(String, String), Problem> {
    match forward(word) {
        None => Err(Problem::EnvForwardNotSet(position)),
        Some(value) => match value.into_string() {
            Ok(value) => Ok((word.to_owned(), value)),
            Err(_) => Err(Problem::EnvForwardNotUnicode(position)),
        },
    }
}

/// `text` with each control character escaped, and each mark that reorders text or separates lines, so a word cannot
/// break or forge a usage line. The same rule as gate-router's `escaped`, which gate-args cannot depend on.
pub(crate) fn escaped(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                character.escape_debug().to_string()
            } else if is_reordering_mark(character) {
                format!("\\u{{{:x}}}", u32::from(character))
            } else {
                character.to_string()
            }
        })
        .collect()
}

/// Whether `character` is a bidirectional mark, embedding, override or isolate, or the line or paragraph separator:
/// U+061C, U+200E, U+200F, U+202A to U+202E, U+2066 to U+2069, U+2028 and U+2029.
fn is_reordering_mark(character: char) -> bool {
    matches!(
        character,
        '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{2028}' | '\u{2029}'
    )
}

/// Whether `name` is `prefix` followed by one or more ASCII digits, `prefix` compared without regard to ASCII case.
fn numbered(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.get(prefix.len()..) else {
        return false;
    };
    starts_with_ignoring_case(name, prefix) && !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
}

/// Whether `name` starts with `prefix`, compared without regard to ASCII case.
fn starts_with_ignoring_case(name: &str, prefix: &str) -> bool {
    name.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

#[cfg(test)]
mod tests {
    use super::{EnvRefusal, LONGEST_SHOWN_NAME, MOST_ENV_BYTES, MOST_ENV_NAMES, env_pair, env_refusal, over_cap, repeated_name, shown_name};
    use crate::problem::Problem;
    use std::ffi::OsString;

    #[test]
    fn a_name_or_value_no_environment_block_carries_is_malformed() {
        for bad in ["", "A=B", "A\0B"] {
            assert_eq!(env_refusal(bad, "v"), Some(EnvRefusal::MalformedName), "{bad:?}");
        }
        assert_eq!(env_refusal("A", "v\0"), Some(EnvRefusal::MalformedValue));
        assert_eq!(env_refusal("A", ""), None, "an empty value is allowed");
        assert_eq!(env_refusal("A", "v"), None);
    }

    #[test]
    fn the_names_the_gate_sets_are_refused_in_any_letter_case() {
        for name in [
            "GATE_SLOT_HELD",
            "gate_state_dir",
            "SharedCompilationId",
            "sharedcompilationid",
            "MSBUILDNODEHANDSHAKESALT",
            "msbuilduseserver",
            "DOTNET_CLI_USE_MSBUILD_SERVER",
            "dotnet_cli_use_msbuild_server",
            "MSBUILDDISABLENODEREUSE",
            "GIT_CONFIG_COUNT",
            "git_config_count",
            "Logging__EventLog__LogLevel__Default",
            "logging:eventlog:loglevel:default",
            "GIT_CONFIG_KEY_12",
            "git_config_value_0",
        ] {
            assert_eq!(env_refusal(name, "x"), Some(EnvRefusal::GateSets), "{name}");
        }
    }

    #[test]
    fn the_names_git_would_read_over_the_gates_own_are_refused_as_such() {
        for name in ["GIT_CONFIG", "git_config", "GIT_CONFIG_PARAMETERS", "git_config_parameters"] {
            assert_eq!(
                env_refusal(name, "x").map(|refusal| refusal.box_line(name)),
                Some(format!(
                    "gate: box payload Env sets {name}, which git would read over the gate's own git settings"
                )),
                "{name}"
            );
        }
        for name in ["GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"] {
            assert_eq!(
                env_refusal(name, "x").map(|refusal| refusal.box_line(name)),
                Some(format!("gate: box payload Env sets {name}, which the gate sets itself")),
                "{name}"
            );
        }
    }

    #[test]
    fn every_spelling_of_the_event_log_setting_is_refused() {
        for name in [
            "Logging__EventLog__LogLevel__Default",
            "Logging:EventLog:LogLevel:Default",
            "Logging__EventLog:LogLevel__Default",
            "Logging__EventLog__LogLevel__Microsoft",
            "DOTNET_Logging__EventLog__LogLevel__Default",
            "dotnet_logging:eventlog:loglevel:default",
            "ASPNETCORE_Logging__EventLog__LogLevel__Default",
            "aspnetcore_logging__eventlog__loglevel__default",
            "Logging__Microsoft.Extensions.Logging.EventLog.EventLogLoggerProvider__LogLevel__Microsoft",
        ] {
            assert_eq!(env_refusal(name, "x"), Some(EnvRefusal::GateSets), "{name}");
        }
        for name in [
            "Logging__Console__LogLevel__Default",
            "DOTNET_Logging__Console__LogLevel__Default",
            "Logging__EventLogs__LogLevel__Default",
        ] {
            assert_eq!(env_refusal(name, "x"), None, "{name}");
        }
    }

    #[test]
    fn a_git_config_name_needs_its_digits() {
        assert_eq!(env_refusal("GIT_CONFIG_KEYS", "x"), None, "not a name the gate sets");
        assert_eq!(env_refusal("git_config_key_", "x"), None, "no digits after the prefix");
        assert_eq!(env_refusal("GIT_CONFIG_KEY_1x", "x"), None, "not all digits");
        assert_eq!(env_refusal("GIT_CONFIG_KEY_12", "x"), Some(EnvRefusal::GateSets));
    }

    #[test]
    fn a_gate_name_no_table_lists_is_refused_as_the_gates() {
        assert_eq!(env_refusal("GATE_ANYTHING", "x"), Some(EnvRefusal::GateName));
        assert_eq!(env_refusal("gate_anything", "x"), Some(EnvRefusal::GateName));
        assert_eq!(env_refusal("AGATE_X", "x"), None, "the prefix starts the name");
    }

    #[test]
    fn a_machine_path_is_refused_in_any_letter_case() {
        for name in [
            "PATH",
            "path",
            "Path",
            "PATHEXT",
            "TEMP",
            "tmp",
            "SystemRoot",
            "windir",
            "ComSpec",
            "userprofile",
            "HOME",
            "appdata",
            "LOCALAPPDATA",
            "programdata",
            "GODOT_PATH",
            "godot_path",
        ] {
            assert_eq!(env_refusal(name, "x"), Some(EnvRefusal::MachinePath), "{name}");
        }
        for name in ["PATHS", "HOMEDRIVE", "TMPDIR"] {
            assert_eq!(env_refusal(name, "x"), None, "{name}");
        }
    }

    #[test]
    fn ue_root_is_refused_as_a_machine_path_in_any_letter_case() {
        for name in ["UE_ROOT", "ue_root", "Ue_Root"] {
            assert_eq!(env_refusal(name, r"C:\UE_5.8"), Some(EnvRefusal::MachinePath), "{name}");
        }
        assert_eq!(env_refusal("UE_ROOTS", "x"), None);
    }

    #[test]
    fn a_name_may_hold_only_printable_ascii_and_never_an_equals_sign() {
        for bad in ["\u{17f}ystemRoot", "G\u{131}T_CONFIG_PARAMETERS", "A\nPATH", "A B", "A\u{200b}", "A=B"] {
            assert_eq!(env_refusal(bad, "v"), Some(EnvRefusal::MalformedName), "{bad:?}");
        }
        for good in [
            "npm_config_//registry.npmjs.org/:_authToken",
            "a@b",
            "ProgramFiles(x86)",
            "Logging:LogLevel:Default",
            "a.b-c_1",
        ] {
            assert_eq!(env_refusal(good, "v"), None, "{good:?}");
        }
    }

    #[test]
    fn a_value_holding_a_nul_is_malformed_and_a_multiline_one_is_not() {
        assert_eq!(env_refusal("A", "x\0y"), Some(EnvRefusal::MalformedValue));
        for good in ["x\ny", "x\ry", "x\r\ny", "x\ty", ""] {
            assert_eq!(env_refusal("A", good), None, "{good:?}");
        }
    }

    #[test]
    fn a_name_given_twice_is_named_as_its_later_spelling() {
        assert_eq!(repeated_name(["NuGetAudit", "NUGETAUDIT"]), Some("NUGETAUDIT"));
        assert_eq!(repeated_name(["NuGetAudit", "Other", "nugetaudit"]), Some("nugetaudit"));
        assert_eq!(repeated_name(["NuGetAudit", "Other"]), None);
        assert_eq!(repeated_name(std::iter::empty::<&str>()), None);
    }

    #[test]
    fn a_list_over_its_cap_is_refused() {
        assert!(
            !over_cap(MOST_ENV_NAMES, MOST_ENV_BYTES),
            "{MOST_ENV_NAMES} names of {MOST_ENV_BYTES} bytes are allowed"
        );
        assert!(!over_cap(MOST_ENV_NAMES, 0), "both caps are separate");
        assert!(over_cap(MOST_ENV_NAMES + 1, 0), "one more name is not");
        assert!(over_cap(0, MOST_ENV_BYTES + 1), "one byte more is not");
    }

    #[test]
    fn a_name_a_line_shows_is_shortened_to_its_first_two_hundred_characters() {
        let longest = "x".repeat(LONGEST_SHOWN_NAME);
        assert_eq!(shown_name("A"), "A", "a short name is shown as it is");
        assert_eq!(shown_name(&longest), longest, "a name of the whole limit is shown as it is");
        assert_eq!(
            shown_name(&format!("{longest}x")),
            format!("{longest}…"),
            "one more character shortens it"
        );
        assert_eq!(
            shown_name(&"é".repeat(LONGEST_SHOWN_NAME + 1)),
            format!("{}…", "é".repeat(LONGEST_SHOWN_NAME)),
            "characters, not bytes"
        );
    }

    #[test]
    fn each_refusal_says_its_line() {
        assert_eq!(
            EnvRefusal::MalformedName.box_line("A=B"),
            "gate: box payload Env has a malformed name 'A=B'"
        );
        assert_eq!(EnvRefusal::MalformedName.box_line(""), "gate: box payload Env has a malformed name ''");
        assert_eq!(
            EnvRefusal::MalformedValue.box_line("A"),
            "gate: box payload Env gives A a value holding NUL"
        );
        assert_eq!(
            EnvRefusal::GateSets.box_line("MSBUILDUSESERVER"),
            "gate: box payload Env sets MSBUILDUSESERVER, which the gate sets itself"
        );
        assert_eq!(
            EnvRefusal::GitReads.box_line("GIT_CONFIG"),
            "gate: box payload Env sets GIT_CONFIG, which git would read over the gate's own git settings"
        );
        assert_eq!(
            EnvRefusal::GateName.box_line("GATE_X"),
            "gate: box payload Env sets GATE_X: GATE_ names are the gate's"
        );
        assert_eq!(
            EnvRefusal::MachinePath.box_line("PATH"),
            "gate: box payload Env sets PATH, a machine path the builder keeps"
        );
        assert_eq!(EnvRefusal::Twice.box_line("NuGetAudit"), "gate: box payload Env names NuGetAudit twice");
        assert_eq!(
            EnvRefusal::OverCap.box_line(""),
            "gate: box payload Env is over its cap of 64 names or 32 KiB"
        );
    }

    #[test]
    fn an_env_word_splits_at_its_first_equals_sign() {
        assert_eq!(env_pair("A=b=c", 1, nothing), Ok(("A".to_owned(), "b=c".to_owned())));
        assert_eq!(
            env_pair("A=", 1, nothing),
            Ok(("A".to_owned(), String::new())),
            "an empty value is allowed"
        );
        assert_eq!(env_pair("A", 2, nothing), Err(Problem::EnvForwardNotSet(2)));
        assert_eq!(
            env_pair("PATH=x", 1, nothing),
            Err(Problem::EnvRefused {
                refusal: EnvRefusal::MachinePath,
                name: "PATH".to_owned(),
                position: 1
            })
        );
    }

    /// A lookup that sets no variable.
    fn nothing(_: &str) -> Option<OsString> {
        None
    }

    /// A lookup that gives every name the value `value`.
    fn holding(value: &'static str) -> impl Fn(&str) -> Option<OsString> {
        move |_| Some(OsString::from(value))
    }

    #[test]
    fn a_forwarded_name_takes_the_value_the_lookup_gives() {
        assert_eq!(
            env_pair("FORWARDED", 1, holding("v")),
            Ok(("FORWARDED".to_owned(), "v".to_owned())),
            "a set name forwards its value"
        );
        assert_eq!(
            env_pair("EMPTY", 2, holding("")),
            Ok(("EMPTY".to_owned(), String::new())),
            "a name set to nothing forwards nothing"
        );
        assert_eq!(env_pair("MISSING", 3, nothing), Err(Problem::EnvForwardNotSet(3)));
    }

    #[test]
    fn a_forwarded_pair_is_refused_as_the_written_one_is() {
        for (word, value) in [("GATE_X", "v"), ("PATH", "C:\\bin"), ("GATE_SLOT_HELD", "forged")] {
            let forwarded = env_pair(word, 1, holding(value));
            let written = env_pair(&format!("{word}={value}"), 1, nothing);
            assert_eq!(forwarded, written, "{word}");
            assert!(forwarded.is_err(), "{word} is refused");
        }
        assert_eq!(
            env_pair("HOLDS", 1, holding("x\0y")),
            Err(Problem::EnvRefused {
                refusal: EnvRefusal::MalformedValue,
                name: "HOLDS".to_owned(),
                position: 1
            }),
            "a forwarded value holding a NUL"
        );
    }

    #[test]
    #[cfg(windows)]
    fn a_forwarded_value_that_is_not_unicode_is_refused() {
        use std::os::windows::ffi::OsStringExt as _;

        let odd = |_: &str| Some(OsString::from_wide(&[0xD800]));
        assert_eq!(env_pair("ODD", 3, odd), Err(Problem::EnvForwardNotUnicode(3)));
    }

    #[test]
    fn an_env_usage_line_shows_its_name_escaped_and_shortened() {
        let refused = |name: &str| {
            Problem::EnvRefused {
                refusal: EnvRefusal::GateName,
                name: name.to_owned(),
                position: 1,
            }
            .to_string()
        };
        assert_eq!(
            refused("GATE_\u{1b}X\u{202e}"),
            r"gate: --env sets GATE_\u{1b}X\u{202e}: GATE_ names are the gate's"
        );
        let long = format!("GATE_{}", "x".repeat(LONGEST_SHOWN_NAME));
        assert_eq!(
            refused(&long),
            format!("gate: --env sets {}…: GATE_ names are the gate's", &long[..LONGEST_SHOWN_NAME])
        );
        assert_eq!(
            Problem::EnvForwardNotSet(2).to_string(),
            "gate: --env word 2 names a variable to forward, and it is not set"
        );
        assert_eq!(
            Problem::EnvForwardNotUnicode(3).to_string(),
            "gate: --env word 3 names a variable whose value is not Unicode"
        );
        assert_eq!(
            EnvRefusal::MalformedName.usage_line("A=B", 2),
            "gate: --env word 2 has a malformed name",
            "a name the gate cannot carry is not said"
        );
        assert_eq!(EnvRefusal::MalformedValue.usage_line("A", 1), "gate: --env gives A a value holding NUL");
        assert_eq!(EnvRefusal::OverCap.usage_line("", 1), "gate: --env is over its cap of 64 names or 32 KiB");
        assert_eq!(EnvRefusal::Twice.usage_line("A", 1), "gate: --env names A twice");
    }
}
