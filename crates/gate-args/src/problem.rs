//! The usage errors, each one `gate: ` line, except the hidden `gate box`'s own `usage: gate box ...` line.

use std::fmt;

#[cfg(feature = "test-seams")]
use crate::request::{DESKTOP_MODES, DESKTOP_SESSIONS};
#[cfg(feature = "test-seams")]
use crate::values::MAX_MUTEX_NAME;
use crate::values::{EnvRefusal, escaped, shown_name};

/// The largest value a timeout, stall window, tail or test-seam value may take.
pub const MAX_WHOLE_NUMBER: u64 = 2_147_483_647;

/// Which form of the command line a problem is reported in, so a missing input is named in the caller's spelling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Spelling {
    /// The subcommand CLI: `--slot`, `--timeout`, `--log`; every subcommand's but the compatibility layer's.
    #[default]
    Cli,
    /// The compatibility layer: `-Slot`, `-TimeoutSeconds`, `-Log`.
    Compat,
}

/// An input `gate run` cannot do without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Required {
    Slot,
    Timeout,
    Log,
    Command,
    /// `gate burn`'s `--seconds`, which it cannot do without.
    Seconds,
}

/// One thing wrong with the command line. `option` is the option as the caller spelled it, without its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    Missing(Required, Spelling),
    BadSlot {
        option: String,
        value: String,
    },
    /// A `gate expect` slot other than heavy, light or critical: a none-slot run has no expected time.
    BadExpectSlot {
        option: String,
        value: String,
    },
    #[cfg(feature = "test-seams")]
    BadRemoteCall {
        option: String,
        value: String,
        calls: &'static [&'static str],
    },
    NotAboveZero {
        option: String,
        value: String,
    },
    NotZeroOrAbove {
        option: String,
        value: String,
    },
    /// A whole number above [`MAX_WHOLE_NUMBER`], one too large for a u64 included.
    TooLarge {
        option: String,
        value: String,
    },
    /// A whole number outside the range an option takes, such as `--seconds` outside 1 to 180.
    NotInRange {
        option: String,
        value: String,
        low: u32,
        high: u32,
    },
    NotTrueOrFalse {
        option: String,
        value: String,
    },
    /// A `--test-box-console` value other than `fail-before`.
    BadConsoleFailure {
        option: String,
        value: String,
    },
    /// A `--test-box-token` value other than `refused`.
    BadTokenRefusal {
        option: String,
        value: String,
    },
    /// A `--test-box-desktop` value other than the faults it names.
    #[cfg(feature = "test-seams")]
    BadDesktopMode {
        option: String,
        value: String,
    },
    /// A `--test-box-desktop-session` value other than the states it names.
    #[cfg(feature = "test-seams")]
    BadDesktopSession {
        option: String,
        value: String,
    },
    /// A `--test-box-desktop-mutex` value that is not 1 to [`MAX_MUTEX_NAME`] ASCII letters, digits and `-`.
    #[cfg(feature = "test-seams")]
    BadDesktopMutex {
        option: String,
        value: String,
    },
    /// A `--test-box-git-limit` value that is not a step, a `:` and whole milliseconds.
    #[cfg(feature = "test-seams")]
    BadGitLimit {
        option: String,
        value: String,
    },
    NeedsValue(String),
    /// A bare `--timeout auto`, which carries no floor.
    AutoNeedsFloor(String),
    GivenTwice(String),
    UnknownOption(String),
    UnknownSubcommand(String),
    /// A word in an option's place that is not an option, such as a command given without `--`.
    UnexpectedArgument(String),
    StopTreeNotAlone,
    MissingPid,
    BadPid(String),
    /// A gate word that is not valid Unicode, shown lossily.
    Unreadable(String),
    /// A `gate box` with no verb, an unknown verb, any word but `--stdin` after it, or an extra word: the box's own usage line, said alone.
    BoxUsage,
    /// A test seam given without the seam it works through.
    SeamNeeds {
        option: &'static str,
        needs: &'static str,
    },
    /// `--alone` given with a slot other than critical, in the caller's spelling.
    AloneNeedsCritical(Spelling),
    /// `--builder <host>` given on a run that can never route, for `why`: `--local`, or a slot other than heavy.
    BuilderCannotRoute {
        host: String,
        why: String,
    },
    /// A run that must state its reason (`local`, `slot critical`, or both) and gave no `--reason`, in the caller's
    /// spelling.
    ReasonNeeded {
        local: bool,
        critical: bool,
        spelling: Spelling,
    },
    /// A `--reason` whose text is empty or whitespace only, named as the caller spelled it.
    EmptyReason(String),
    /// Two test seams that each choose the remote client, given together.
    SeamsConflict {
        option: &'static str,
        other: &'static str,
    },
    /// A test seam the form `with` chooses does not read, given with it.
    #[cfg(feature = "test-seams")]
    SeamNotUsedWith {
        option: &'static str,
        with: &'static str,
    },
    /// A `--env` word the gate cannot set, `name` as given and `position` its 1-based place among the run's `--env`
    /// words; the cap refusal's `name` is empty.
    EnvRefused {
        refusal: EnvRefusal,
        name: String,
        position: usize,
    },
    /// A `--env` word that is not valid Unicode, `position` its place among the run's `--env` words. The word is not
    /// said: it may be a secret pasted without its name.
    EnvWordNotUnicode(usize),
    /// A `--env` word naming a variable to forward, `n` its 1-based position among the run's `--env` words, which the
    /// gate's own environment does not set. The word is not said: it may be a secret pasted without its name.
    EnvForwardNotSet(usize),
    /// A `--env` word naming a variable to forward whose value is not Unicode, `n` its position among the run's words.
    EnvForwardNotUnicode(usize),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Missing(required, spelling) => write!(f, "gate: missing {}", missing_text(*required, *spelling)),
            Problem::BadSlot { option, value } => write!(f, "gate: {option} must be heavy, light, critical or none, got '{value}'"),
            Problem::BadExpectSlot { option, value } => write!(f, "gate: {option} must be heavy, light or critical, got '{value}'"),
            #[cfg(feature = "test-seams")]
            Problem::BadRemoteCall { option, value, calls } => {
                write!(f, "gate: {option} must be {}, got '{value}'", one_of(calls.iter().copied()))
            }
            Problem::NotAboveZero { option, value } => write!(f, "gate: {option} must be a whole number above 0, got '{value}'"),
            Problem::NotZeroOrAbove { option, value } => write!(f, "gate: {option} must be a whole number 0 or above, got '{value}'"),
            Problem::TooLarge { option, value } => write!(f, "gate: {option} must be at most {MAX_WHOLE_NUMBER}, got '{value}'"),
            Problem::NotInRange { option, value, low, high } => {
                write!(f, "gate: {option} must be a whole number from {low} to {high}, got '{value}'")
            }
            Problem::NotTrueOrFalse { option, value } => write!(f, "gate: {option} takes true or false, got '{value}'"),
            Problem::BadConsoleFailure { option, value } => write!(f, "gate: {option} must be fail-before, got '{value}'"),
            Problem::BadTokenRefusal { option, value } => write!(f, "gate: {option} must be refused, got '{value}'"),
            #[cfg(feature = "test-seams")]
            Problem::BadDesktopMode { option, value } => {
                write!(
                    f,
                    "gate: {option} must be {}, got '{value}'",
                    one_of(DESKTOP_MODES.iter().map(|(word, _)| *word))
                )
            }
            #[cfg(feature = "test-seams")]
            Problem::BadDesktopSession { option, value } => {
                write!(
                    f,
                    "gate: {option} must be {}, got '{value}'",
                    one_of(DESKTOP_SESSIONS.iter().map(|(word, _)| *word))
                )
            }
            #[cfg(feature = "test-seams")]
            Problem::BadDesktopMutex { option, value } => {
                write!(
                    f,
                    "gate: {option} must be 1 to {MAX_MUTEX_NAME} ASCII letters, digits and '-', got '{value}'"
                )
            }
            #[cfg(feature = "test-seams")]
            Problem::BadGitLimit { option, value } => write!(f, "gate: {option} must be <step>:<ms>, got '{value}'"),
            Problem::NeedsValue(option) => write!(f, "gate: {option} needs a value"),
            Problem::AutoNeedsFloor(option) => write!(f, "gate: {option} auto needs a floor in seconds: auto:<n>"),
            Problem::GivenTwice(option) => write!(f, "gate: {option} is given twice"),
            Problem::UnknownOption(word) => write!(f, "gate: unknown option {word}"),
            Problem::UnknownSubcommand(word) => write!(f, "gate: unknown subcommand {word}"),
            Problem::UnexpectedArgument(word) => write!(f, "gate: unexpected argument {word}"),
            Problem::StopTreeNotAlone => f.write_str("gate: -StopTree is used alone"),
            Problem::MissingPid => f.write_str("gate: missing the process id"),
            Problem::BadPid(value) => write!(f, "gate: the process id must be a whole number above 0, got '{value}'"),
            Problem::Unreadable(lossy) => write!(f, "gate: cannot read the argument {lossy}"),
            Problem::BoxUsage => f.write_str(concat!(
                "usage: gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs --stdin (the base64 JSON request as stdin's first line)\n",
                "       gate box prune --root <folder> [--keys <key>...]",
            )),
            Problem::SeamNeeds { option, needs } => write!(f, "gate: {option} needs {needs}"),
            Problem::AloneNeedsCritical(Spelling::Cli) => f.write_str("gate: --alone needs --slot critical"),
            Problem::AloneNeedsCritical(Spelling::Compat) => f.write_str("gate: -Alone needs -Slot critical"),
            Problem::BuilderCannotRoute { host, why } => write!(f, "gate: --builder {} cannot route this run: {why}", escaped(host)),
            Problem::ReasonNeeded { local, critical, spelling } => f.write_str(match (local, critical, spelling) {
                (true, false, Spelling::Cli) => "gate: --local needs --reason \"<why>\"",
                (false, true, Spelling::Cli) => "gate: --slot critical needs --reason \"<why>\"",
                (true, true, Spelling::Cli) => "gate: --local and --slot critical need --reason \"<why>\"",
                (true, false, Spelling::Compat) => "gate: -Local needs -Reason \"<why>\"",
                (false, true, Spelling::Compat) => "gate: -Slot critical needs -Reason \"<why>\"",
                (true, true, Spelling::Compat) => "gate: -Local and -Slot critical need -Reason \"<why>\"",
                (false, false, _) => unreachable!("a run that needs no reason is given no ReasonNeeded problem"),
            }),
            Problem::EmptyReason(option) => write!(f, "gate: {option} must not be empty"),
            Problem::SeamsConflict { option, other } => write!(f, "gate: {option} cannot be given with {other}"),
            #[cfg(feature = "test-seams")]
            Problem::SeamNotUsedWith { option, with } => write!(f, "gate: {option} is not used with {with}"),
            Problem::EnvRefused { refusal, name, position } => f.write_str(&refusal.usage_line(&escaped(&shown_name(name)), *position)),
            Problem::EnvWordNotUnicode(n) => write!(f, "gate: --env word {n} is not valid Unicode"),
            Problem::EnvForwardNotSet(n) => write!(f, "gate: --env word {n} names a variable to forward, and it is not set"),
            Problem::EnvForwardNotUnicode(n) => write!(f, "gate: --env word {n} names a variable whose value is not Unicode"),
        }
    }
}

/// The choices a seam may name, as a choice: `a or b`, `a, b or c`.
#[cfg(feature = "test-seams")]
fn one_of<'a>(choices: impl IntoIterator<Item = &'a str>) -> String {
    let choices: Vec<&str> = choices.into_iter().collect();
    match choices.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

fn missing_text(required: Required, spelling: Spelling) -> &'static str {
    match (required, spelling) {
        (Required::Slot, Spelling::Cli) => "--slot heavy|light|critical|none",
        (Required::Slot, Spelling::Compat) => "-Slot heavy|light|critical|none",
        (Required::Timeout, Spelling::Cli) => "--timeout <seconds>",
        (Required::Timeout, Spelling::Compat) => "-TimeoutSeconds <seconds>",
        (Required::Log, Spelling::Cli) => "--log <path>",
        (Required::Log, Spelling::Compat) => "-Log <path>",
        (Required::Command, _) => "the command after --",
        (Required::Seconds, _) => "--seconds <seconds>",
    }
}
