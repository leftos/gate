//! The options both forms of the command line share, collected word by word into a request, with every problem kept.

use std::ffi::{OsStr, OsString};
use std::iter::Peekable;
use std::path::PathBuf;

use crate::problem::{Problem, Required, Spelling};
use crate::request::{DEFAULT_STALL_SECONDS, DEFAULT_TAIL_LINES, RunRequest, Slot, StopRequest, TestSeams};
use crate::values;

/// An option that takes a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueOption {
    Slot,
    Timeout,
    Log,
    Stall,
    Tail,
    StateDir,
    #[cfg(feature = "test-seams")]
    SampleMs,
    #[cfg(feature = "test-seams")]
    HeartbeatMs,
    #[cfg(feature = "test-seams")]
    PollMs,
    #[cfg(feature = "test-seams")]
    StallMs,
}

/// An option that is on or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchOption {
    NoMarkers,
    Local,
    #[cfg(feature = "test-seams")]
    PrintRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunOption {
    Value(ValueOption),
    Switch(SwitchOption),
}

/// The options read so far and the problems found so far.
#[derive(Default)]
pub(crate) struct RunBuilder {
    slot: Option<Slot>,
    timeout: Option<u64>,
    log: Option<PathBuf>,
    stall: Option<u64>,
    tail: Option<u64>,
    state_dir: Option<PathBuf>,
    no_markers: bool,
    local: bool,
    print_request: bool,
    test_seams: TestSeams,
    given: Vec<RunOption>,
    pub(crate) problems: Vec<Problem>,
}

impl RunBuilder {
    /// Whether any option other than the hidden `--test-print-request` was given, valid or not.
    pub(crate) fn has_options(&self) -> bool {
        self.given.iter().any(|option| !is_print_request(*option))
    }

    /// Records `option` as given, or the problem of its being given twice.
    fn first_time(&mut self, option: RunOption, spelled: &str) -> bool {
        if self.given.contains(&option) {
            self.problems.push(Problem::GivenTwice(spelled.to_owned()));
            return false;
        }
        self.given.push(option);
        true
    }

    /// Reads `option`'s value, spelled `spelled` by the caller; an empty value is no value.
    pub(crate) fn value(&mut self, option: ValueOption, spelled: &str, value: &str) {
        if !self.first_time(RunOption::Value(option), spelled) {
            return;
        }
        if value.is_empty() {
            self.problems.push(Problem::NeedsValue(spelled.to_owned()));
        } else if let Err(problem) = self.apply(option, spelled, value) {
            self.problems.push(problem);
        }
    }

    /// Reads `option`'s value from the next word, unless that word `is_option`. An unreadable value is reported once,
    /// as unreadable.
    pub(crate) fn value_from_next(
        &mut self,
        option: ValueOption,
        spelled: &str,
        words: &mut Peekable<impl Iterator<Item = OsString>>,
        is_option: fn(&str) -> bool,
    ) {
        match next_value(words, is_option) {
            Ok(Some(value)) => self.value(option, spelled, &value),
            Ok(None) => self.value(option, spelled, ""),
            Err(problem) => {
                if self.first_time(RunOption::Value(option), spelled) {
                    self.problems.push(problem);
                }
            }
        }
    }

    fn apply(&mut self, option: ValueOption, spelled: &str, value: &str) -> Result<(), Problem> {
        match option {
            ValueOption::Slot => self.slot = Some(values::slot(spelled, value)?),
            ValueOption::Timeout => self.timeout = Some(values::above_zero(spelled, value)?),
            ValueOption::Log => self.log = Some(PathBuf::from(value)),
            ValueOption::Stall => self.stall = Some(values::above_zero(spelled, value)?),
            ValueOption::Tail => self.tail = Some(values::zero_or_above(spelled, value)?),
            ValueOption::StateDir => self.state_dir = Some(PathBuf::from(value)),
            #[cfg(feature = "test-seams")]
            ValueOption::SampleMs => self.test_seams.sample_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::HeartbeatMs => self.test_seams.heartbeat_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::PollMs => self.test_seams.poll_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::StallMs => self.test_seams.stall_ms = Some(values::above_zero(spelled, value)?),
        }
        Ok(())
    }

    /// Sets or clears a switch.
    pub(crate) fn switch(&mut self, option: SwitchOption, spelled: &str, on: bool) {
        if !self.first_time(RunOption::Switch(option), spelled) {
            return;
        }
        match option {
            SwitchOption::NoMarkers => self.no_markers = on,
            SwitchOption::Local => self.local = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::PrintRequest => self.print_request = on,
        }
    }

    /// The run request, or every problem: those found while reading, then each missing input in the order slot,
    /// timeout, log, command.
    pub(crate) fn finish_run(mut self, command: Vec<OsString>, spelling: Spelling) -> Result<RunRequest, Vec<Problem>> {
        for (option, required) in [
            (ValueOption::Slot, Required::Slot),
            (ValueOption::Timeout, Required::Timeout),
            (ValueOption::Log, Required::Log),
        ] {
            if !self.given.contains(&RunOption::Value(option)) {
                self.problems.push(Problem::Missing(required, spelling));
            }
        }
        if command.is_empty() {
            self.problems.push(Problem::Missing(Required::Command, spelling));
        }
        match (self.slot, self.timeout, self.log) {
            (Some(slot), Some(timeout), Some(log)) if self.problems.is_empty() => Ok(RunRequest {
                slot,
                timeout,
                log,
                stall: self.stall.unwrap_or(DEFAULT_STALL_SECONDS),
                tail: self.tail.unwrap_or(DEFAULT_TAIL_LINES),
                no_markers: self.no_markers,
                local: self.local,
                state_dir: self.state_dir,
                command,
                test_seams: self.test_seams,
                print_request: self.print_request,
            }),
            _ => Err(self.problems),
        }
    }

    /// The stop request for the pid read, or every problem found, the pid's last.
    pub(crate) fn finish_stop(mut self, pid: Result<u32, Problem>) -> Result<StopRequest, Vec<Problem>> {
        match pid {
            Ok(pid) if self.problems.is_empty() => Ok(StopRequest {
                pid,
                state_dir: self.state_dir,
                print_request: self.print_request,
            }),
            Ok(_) => Err(self.problems),
            Err(problem) => {
                self.problems.push(problem);
                Err(self.problems)
            }
        }
    }
}

#[cfg(feature = "test-seams")]
fn is_print_request(option: RunOption) -> bool {
    option == RunOption::Switch(SwitchOption::PrintRequest)
}

#[cfg(not(feature = "test-seams"))]
fn is_print_request(_option: RunOption) -> bool {
    false
}

/// The next gate word that is valid Unicode, each unreadable word before it reported in `problems`.
pub(crate) fn next_text(words: &mut impl Iterator<Item = OsString>, problems: &mut Vec<Problem>) -> Option<String> {
    for word in words {
        match word.into_string() {
            Ok(text) => return Some(text),
            Err(word) => problems.push(unreadable(&word)),
        }
    }
    None
}

/// The next word as an option's value: `None` at the end of the words, before `--`, which is never a value, and
/// before a word that `is_option`, which is left to be read as an option.
pub(crate) fn next_value(words: &mut Peekable<impl Iterator<Item = OsString>>, is_option: fn(&str) -> bool) -> Result<Option<String>, Problem> {
    match words.next_if(|word| word.as_os_str() != "--" && !word.to_str().is_some_and(is_option)) {
        None => Ok(None),
        Some(word) => word.into_string().map(Some).map_err(|word| unreadable(&word)),
    }
}

/// The problem of a gate word that is not valid Unicode.
pub(crate) fn unreadable(word: &OsStr) -> Problem {
    Problem::Unreadable(word.to_string_lossy().into_owned())
}
