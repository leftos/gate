//! `gate remote add|remove <section> <repo> <pattern>... [--builder <host>] [--tag <word>] [--dry-run]`: the words in
//! any order, the options before, between or after the positionals; a `--` makes every later word a positional.
//! `gate remote match <repo> <text>` reads the same way and takes none of the options.

use std::ffi::OsString;
use std::fmt;
use std::iter::Peekable;

use crate::problem::Problem;
use crate::request::{Invocation, REMOTE_BUILDER_SECTIONS, REMOTE_SECTIONS, RemoteMatchRequest, RemoteRequest, RemoteVerb};
use crate::run::next_value;
use crate::values::escaped;

/// The most characters a `--tag` takes.
pub(crate) const MAX_TAG: usize = 40;
/// The verb that asks which pattern matches, in place of an edit.
const MATCH: &str = "match";

/// Reads the words after `gate remote`.
pub(crate) fn parse(words: impl Iterator<Item = OsString>) -> Result<Invocation, Vec<Problem>> {
    let mut words = words.peekable();
    let mut read = Read::default();
    let mut options_ended = false;
    while let Some(word) = words.next() {
        let Some(text) = readable(word, &mut read.problems) else {
            continue;
        };
        if options_ended || !text.starts_with("--") {
            read.positional.push(text);
        } else if text == "--" {
            options_ended = true;
        } else {
            read.option(&text, &mut words);
        }
    }
    read.finish()
}

/// What the words said so far.
#[derive(Default)]
struct Read {
    positional: Vec<String>,
    builder: Option<String>,
    tag: Option<String>,
    dry_run: bool,
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
        match (name, inline) {
            ("dry-run", None) => {
                if self.dry_run {
                    self.problems.push(Problem::GivenTwice("--dry-run".to_owned()));
                }
                self.dry_run = true;
            }
            ("builder", _) => {
                let value = self.value("--builder", inline, words);
                set(&mut self.builder, "--builder", value, &mut self.problems);
            }
            ("tag", _) => {
                let value = self.value("--tag", inline, words);
                set(&mut self.tag, "--tag", value, &mut self.problems);
            }
            _ => self.problems.push(Problem::UnknownOption(word.to_owned())),
        }
    }

    /// The value of the option `spelled`: `inline`, else the next word unless it is `--` or another option, which is
    /// left to be read as one; `None`, a problem said, when it is missing, empty or not valid Unicode.
    fn value(&mut self, spelled: &str, inline: Option<&str>, words: &mut Peekable<impl Iterator<Item = OsString>>) -> Option<String> {
        let value = match inline {
            Some(value) => Some(value.to_owned()),
            None => match next_value(words, |next| next.starts_with("--")) {
                Ok(value) => value,
                Err(problem) => {
                    self.problems.push(problem);
                    return None;
                }
            },
        };
        let value = value.filter(|value| !value.is_empty());
        if value.is_none() {
            self.problems.push(Problem::NeedsValue(spelled.to_owned()));
        }
        value
    }

    /// The invocation the words make; else every problem found.
    fn finish(self) -> Result<Invocation, Vec<Problem>> {
        if self.positional.first().is_some_and(|verb| verb == MATCH) {
            self.finish_match().map(Invocation::RemoteMatch)
        } else {
            self.finish_edit().map(Invocation::Remote)
        }
    }

    /// The `match` request the words make; else every problem found.
    fn finish_match(mut self) -> Result<RemoteMatchRequest, Vec<Problem>> {
        let mut operands = std::mem::take(&mut self.positional).into_iter().skip(1);
        let repo = operands.next().unwrap_or_default();
        let text = operands.next().unwrap_or_default();
        let missing = if repo.is_empty() {
            Some("the repo")
        } else if text.is_empty() {
            Some("the text")
        } else {
            None
        };
        let options = [
            (self.builder.is_some(), "--builder"),
            (self.tag.is_some(), "--tag"),
            (self.dry_run, "--dry-run"),
        ];
        let refused = options
            .into_iter()
            .filter(|(given, _)| *given)
            .map(|(_, option)| RemoteProblem::NotOnMatch(option));
        let found = missing
            .map(RemoteProblem::Missing)
            .into_iter()
            .chain(operands.next().map(RemoteProblem::ExtraOperand))
            .chain(refused);
        self.problems.extend(found.map(Problem::Remote));
        if self.problems.is_empty() {
            Ok(RemoteMatchRequest { repo, text })
        } else {
            Err(self.problems)
        }
    }

    /// The edit request the words make; else every problem found.
    fn finish_edit(mut self) -> Result<RemoteRequest, Vec<Problem>> {
        let mut positional = std::mem::take(&mut self.positional).into_iter();
        let verb = verb_of(positional.next().as_deref(), &mut self.problems);
        let section = positional.next().unwrap_or_default();
        let repo = positional.next().unwrap_or_default();
        let patterns: Vec<String> = positional.collect();
        let positional_problem = match first_missing(&section, &repo, &patterns) {
            Some(missing) => Some(RemoteProblem::Missing(missing)),
            None => section_problem(&section, self.builder.is_some()),
        };
        let found = positional_problem.into_iter().chain(tag_problem(self.tag.as_deref(), verb));
        self.problems.extend(found.map(Problem::Remote));
        match verb {
            Some(verb) if self.problems.is_empty() => Ok(RemoteRequest {
                verb,
                section,
                repo,
                patterns,
                builder: self.builder,
                tag: self.tag,
                dry_run: self.dry_run,
            }),
            _ => Err(self.problems),
        }
    }
}

/// `word` as text; `None`, a problem said, when it is not valid Unicode.
fn readable(word: OsString, problems: &mut Vec<Problem>) -> Option<String> {
    word.into_string()
        .map_err(|word| problems.push(Problem::Unreadable(word.to_string_lossy().into_owned())))
        .ok()
}

/// Sets the option `spelled`'s `slot` to `value`, a problem said when it was set already.
fn set(slot: &mut Option<String>, spelled: &str, value: Option<String>, problems: &mut Vec<Problem>) {
    let Some(value) = value else {
        return;
    };
    if slot.is_some() {
        problems.push(Problem::GivenTwice(spelled.to_owned()));
    } else {
        *slot = Some(value);
    }
}

/// A `gate remote` line it cannot take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteProblem {
    /// One of its words is missing: `the verb add, remove or match`, `the section`, `the repo`, `a pattern` or `the text`.
    Missing(&'static str),
    /// A verb other than `add`, `remove` or `match`, as given.
    UnknownVerb(String),
    /// A section other than the ones it edits, as given.
    UnknownSection(String),
    /// A `--builder` section a `builders` entry does not have, as given.
    NotABuilderSection(String),
    /// A `--tag` given to `remove`, which makes no backup it could name.
    TagOnRemove,
    /// A `--tag` value that is not 1 to 40 ASCII letters, digits, `_` and `-`, as given.
    BadTag(String),
    /// An option of an edit given to `match`, by its name.
    NotOnMatch(&'static str),
    /// A word after `match`'s repo and text, as given.
    ExtraOperand(String),
}

impl fmt::Display for RemoteProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteProblem::Missing(what) => write!(f, "missing {what}"),
            RemoteProblem::UnknownVerb(verb) => write!(f, "unknown verb {}; add, remove or match", escaped(verb)),
            RemoteProblem::UnknownSection(section) => write!(f, "unknown section {}; one of {}", escaped(section), REMOTE_SECTIONS.join(", ")),
            RemoteProblem::NotABuilderSection(section) => write!(
                f,
                "a builder has no section {}; with --builder, one of {}",
                escaped(section),
                REMOTE_BUILDER_SECTIONS.join(", ")
            ),
            RemoteProblem::TagOnRemove => f.write_str("--tag is for add only; remove makes no backup to name"),
            RemoteProblem::BadTag(tag) => write!(
                f,
                "--tag must be 1 to {MAX_TAG} ASCII letters, digits, '_' and '-', got '{}'",
                escaped(tag)
            ),
            RemoteProblem::NotOnMatch(option) => write!(f, "{option} is for add and remove only; match changes nothing"),
            RemoteProblem::ExtraOperand(word) => write!(f, "match takes a repo and one text, and not {}", escaped(word)),
        }
    }
}

/// The verb the first positional names; `None`, a problem said, when it is missing or unknown.
fn verb_of(word: Option<&str>, problems: &mut Vec<Problem>) -> Option<RemoteVerb> {
    match word {
        Some("add") => Some(RemoteVerb::Add),
        Some("remove") => Some(RemoteVerb::Remove),
        Some(other) => {
            problems.push(Problem::Remote(RemoteProblem::UnknownVerb(other.to_owned())));
            None
        }
        None => {
            problems.push(Problem::Remote(RemoteProblem::Missing("the verb add, remove or match")));
            None
        }
    }
}

/// The first positional after the verb that is missing or empty, as [`RemoteProblem::Missing`] names it.
fn first_missing(section: &str, repo: &str, patterns: &[String]) -> Option<&'static str> {
    if section.is_empty() {
        Some("the section")
    } else if repo.is_empty() {
        Some("the repo")
    } else if patterns.is_empty() {
        Some("a pattern")
    } else {
        None
    }
}

/// Why `section` cannot be edited, with a builder named when `builder`.
fn section_problem(section: &str, builder: bool) -> Option<RemoteProblem> {
    if !REMOTE_SECTIONS.contains(&section) {
        Some(RemoteProblem::UnknownSection(section.to_owned()))
    } else if builder && !REMOTE_BUILDER_SECTIONS.contains(&section) {
        Some(RemoteProblem::NotABuilderSection(section.to_owned()))
    } else {
        None
    }
}

/// Why `tag` cannot be taken by `verb`.
fn tag_problem(tag: Option<&str>, verb: Option<RemoteVerb>) -> Option<RemoteProblem> {
    let tag = tag?;
    if verb == Some(RemoteVerb::Remove) {
        return Some(RemoteProblem::TagOnRemove);
    }
    let valid = (1..=MAX_TAG).contains(&tag.len()) && tag.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    (!valid).then(|| RemoteProblem::BadTag(tag.to_owned()))
}
