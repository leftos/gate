//! `gate desktop hold --builder <host> --holder "<text>" --ceiling <seconds> [--wait <seconds>]`: the options in any
//! order, each once, as `--name value` or `--name=value`. And the holder text a desktop hold names, judged by one rule
//! for `gate desktop hold` on the PC and `gate box desktop-hold` on the builder: the PC carries it to the box as one
//! double-quoted word of the ssh line, so it holds no double quote and none of the characters cmd reads (`^ & | < > %`).

use std::ffi::OsString;
use std::iter::Peekable;
use std::path::PathBuf;

use crate::problem::{Problem, Required};
use crate::request::{DesktopHoldRequest, DesktopHoldSeams};
use crate::run::next_value;
use crate::values::in_range;

/// The one verb `gate desktop` takes.
const HOLD: &str = "hold";
/// The command a PC's holder refusal names.
const PC_COMMAND: &str = "gate desktop hold";
/// The longest ceiling and wait a desktop hold takes, in seconds, on the PC and the box alike: a day.
pub(crate) const MOST_DESKTOP_HOLD_SECONDS: u32 = 86_400;
/// The seam that raises the console interrupt once the box's held line has come.
#[cfg(feature = "test-seams")]
const INTERRUPT_AFTER_HELD: &str = "test-desktop-hold-interrupt-after-held";

/// Reads the words after `gate desktop`: the verb `hold`, then its options.
pub(crate) fn parse(words: impl Iterator<Item = OsString>) -> Result<DesktopHoldRequest, Vec<Problem>> {
    let mut words = words.peekable();
    match words.next().map(|word| word.to_string_lossy().into_owned()) {
        Some(verb) if verb == HOLD => {}
        Some(other) => return Err(vec![Problem::UnknownSubcommand(format!("desktop {other}"))]),
        None => return Err(vec![Problem::UnknownSubcommand("desktop".to_owned())]),
    }
    let mut read = Read::default();
    while let Some(word) = words.next() {
        match word.into_string() {
            Err(word) => read.problems.push(Problem::Unreadable(word.to_string_lossy().into_owned())),
            Ok(text) if text.starts_with("--") && text != "--" => read.option(&text, &mut words),
            Ok(text) => read.problems.push(Problem::UnexpectedArgument(text)),
        }
    }
    read.finish()
}

/// What the words said so far: each option's value as given.
#[derive(Default)]
struct Read {
    builder: Option<String>,
    holder: Option<String>,
    ceiling: Option<String>,
    wait: Option<String>,
    remote_transport: Option<String>,
    remote_ssh: Option<String>,
    interrupt_after_held: bool,
    problems: Vec<Problem>,
}

impl Read {
    /// Reads the option `word`, `--name` or `--name=value`, taking a value from the next word when it needs one.
    fn option(&mut self, word: &str, words: &mut Peekable<impl Iterator<Item = OsString>>) {
        let rest = &word[2..];
        let (name, inline) = match rest.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (rest, None),
        };
        if self.switch(name, inline) {
            return;
        }
        if self.slot(name).is_none() {
            self.problems.push(Problem::UnknownOption(word.to_owned()));
            return;
        }
        let spelled = format!("--{name}");
        let Some(value) = self.value(name, inline, words) else {
            self.problems.push(Problem::NeedsValue(spelled));
            return;
        };
        match self.slot(name) {
            Some(slot) if slot.is_none() => *slot = Some(value),
            _ => self.problems.push(Problem::GivenTwice(spelled)),
        }
    }

    /// The value of the option `name`: `inline`, else the next word unless it is `--` or another option, which is left
    /// to be read as one; `None` when it is missing, or empty for any option but the holder, whose empty text the holder
    /// rule refuses. A next word that is not Unicode is its own problem.
    fn value(&mut self, name: &str, inline: Option<&str>, words: &mut Peekable<impl Iterator<Item = OsString>>) -> Option<String> {
        let value = match inline {
            Some(value) => Some(value.to_owned()),
            None => next_value(words, |next| next.starts_with("--")).unwrap_or_else(|problem| {
                self.problems.push(problem);
                None
            }),
        };
        value.filter(|value| name == "holder" || !value.is_empty())
    }

    /// The value slot of the option `name`; `None` for a name that takes no value here.
    fn slot(&mut self, name: &str) -> Option<&mut Option<String>> {
        match name {
            "builder" => Some(&mut self.builder),
            "holder" => Some(&mut self.holder),
            "ceiling" => Some(&mut self.ceiling),
            "wait" => Some(&mut self.wait),
            #[cfg(feature = "test-seams")]
            "test-remote-transport" => Some(&mut self.remote_transport),
            #[cfg(feature = "test-seams")]
            "test-remote-ssh" => Some(&mut self.remote_ssh),
            _ => None,
        }
    }

    /// Takes the switch `name`, given with no `inline` value; whether `name` is one.
    #[cfg(feature = "test-seams")]
    fn switch(&mut self, name: &str, inline: Option<&str>) -> bool {
        if name != INTERRUPT_AFTER_HELD || inline.is_some() {
            return false;
        }
        if self.interrupt_after_held {
            self.problems.push(Problem::GivenTwice(format!("--{name}")));
        }
        self.interrupt_after_held = true;
        true
    }

    /// No switch without the `test-seams` feature.
    #[cfg(not(feature = "test-seams"))]
    #[expect(
        clippy::unused_self,
        reason = "the seamed build's switch sets the read's own flag; this one has none to set"
    )]
    fn switch(&mut self, _name: &str, _inline: Option<&str>) -> bool {
        false
    }

    /// The request the words make; else every problem found, those of the words first, then each missing option,
    /// then each value refused.
    fn finish(mut self) -> Result<DesktopHoldRequest, Vec<Problem>> {
        let missing = [
            (self.builder.is_none(), Required::Builder),
            (self.holder.is_none(), Required::Holder),
            (self.ceiling.is_none(), Required::Ceiling),
        ];
        let missing = missing.into_iter().filter(|(gone, _)| *gone);
        self.problems
            .extend(missing.map(|(_, required)| Problem::Missing(required, crate::problem::Spelling::Cli)));
        let ceiling = self.seconds("--ceiling", self.ceiling.clone(), 1);
        let wait = self.seconds("--wait", self.wait.clone(), 0);
        if self.holder.as_deref().is_some_and(|holder| !holder_taken(holder)) {
            self.problems.push(Problem::BadDesktopHolder(PC_COMMAND));
        }
        match (self.builder, self.holder, ceiling) {
            (Some(builder), Some(holder), Some(ceiling)) if self.problems.is_empty() => Ok(DesktopHoldRequest {
                builder,
                holder,
                ceiling,
                wait: wait.unwrap_or(0),
                seams: DesktopHoldSeams {
                    remote_transport: self.remote_transport,
                    remote_ssh: self.remote_ssh.map(PathBuf::from),
                    interrupt_after_held: self.interrupt_after_held,
                },
            }),
            _ => Err(self.problems),
        }
    }

    /// The whole seconds `value` of the option `spelled` gives, from `least` to [`MOST_DESKTOP_HOLD_SECONDS`]; `None` for no value,
    /// and for one refused, its problem said.
    fn seconds(&mut self, spelled: &str, value: Option<String>, least: u32) -> Option<u64> {
        in_range(spelled, &value?, least, MOST_DESKTOP_HOLD_SECONDS)
            .map(u64::from)
            .map_err(|problem| self.problems.push(problem))
            .ok()
    }
}

/// The most characters a holder text takes.
pub(crate) const MOST_HOLDER_CHARS: usize = 80;

/// The marks a holder text may hold besides ASCII letters, digits and spaces.
pub(crate) const HOLDER_MARKS: [char; 10] = ['-', '_', '.', ':', '(', ')', '/', '@', '+', ','];

/// Whether `holder` is a holder text a desktop hold takes: 1 to [`MOST_HOLDER_CHARS`] characters, each an ASCII letter,
/// a digit, a space or one of [`HOLDER_MARKS`].
pub(crate) fn holder_taken(holder: &str) -> bool {
    (1..=MOST_HOLDER_CHARS).contains(&holder.chars().count())
        && holder
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == ' ' || HOLDER_MARKS.contains(&character))
}
