//! The usage errors, each one `gate: ` line.

use std::fmt;

/// The largest value a timeout, stall window, tail or test-seam value may take.
pub const MAX_WHOLE_NUMBER: u64 = 2_147_483_647;

/// Which form of the command line a problem is reported in, so a missing input is named in the caller's spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spelling {
    /// The subcommand CLI: `--slot`, `--timeout`, `--log`.
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
}

/// One thing wrong with the command line. `option` is the option as the caller spelled it, without its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    Missing(Required, Spelling),
    BadSlot {
        option: String,
        value: String,
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
    NotTrueOrFalse {
        option: String,
        value: String,
    },
    /// A value that is not an exit status, a whole number 0 to 255.
    NotAStatus {
        option: String,
        value: String,
    },
    /// The journal seam and the request printer, which both replace the run, given together.
    JournalRunWithPrintRequest,
    NeedsValue(String),
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
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Missing(required, spelling) => write!(f, "gate: missing {}", missing_text(*required, *spelling)),
            Problem::BadSlot { option, value } => write!(f, "gate: {option} must be heavy, light or critical, got '{value}'"),
            Problem::NotAboveZero { option, value } => write!(f, "gate: {option} must be a whole number above 0, got '{value}'"),
            Problem::NotZeroOrAbove { option, value } => write!(f, "gate: {option} must be a whole number 0 or above, got '{value}'"),
            Problem::TooLarge { option, value } => write!(f, "gate: {option} must be at most {MAX_WHOLE_NUMBER}, got '{value}'"),
            Problem::NotTrueOrFalse { option, value } => write!(f, "gate: {option} takes true or false, got '{value}'"),
            Problem::NotAStatus { option, value } => write!(f, "gate: {option} must be a whole number 0 to 255, got '{value}'"),
            Problem::JournalRunWithPrintRequest => f.write_str("gate: --test-journal-run and --test-print-request cannot be combined"),
            Problem::NeedsValue(option) => write!(f, "gate: {option} needs a value"),
            Problem::GivenTwice(option) => write!(f, "gate: {option} is given twice"),
            Problem::UnknownOption(word) => write!(f, "gate: unknown option {word}"),
            Problem::UnknownSubcommand(word) => write!(f, "gate: unknown subcommand {word}"),
            Problem::UnexpectedArgument(word) => write!(f, "gate: unexpected argument {word}"),
            Problem::StopTreeNotAlone => f.write_str("gate: -StopTree is used alone"),
            Problem::MissingPid => f.write_str("gate: missing the process id"),
            Problem::BadPid(value) => write!(f, "gate: the process id must be a whole number above 0, got '{value}'"),
            Problem::Unreadable(lossy) => write!(f, "gate: cannot read the argument {lossy}"),
        }
    }
}

fn missing_text(required: Required, spelling: Spelling) -> &'static str {
    match (required, spelling) {
        (Required::Slot, Spelling::Cli) => "--slot heavy|light|critical",
        (Required::Slot, Spelling::Compat) => "-Slot heavy|light|critical",
        (Required::Timeout, Spelling::Cli) => "--timeout <seconds>",
        (Required::Timeout, Spelling::Compat) => "-TimeoutSeconds <seconds>",
        (Required::Log, Spelling::Cli) => "--log <path>",
        (Required::Log, Spelling::Compat) => "-Log <path>",
        (Required::Command, _) => "the command after --",
    }
}
