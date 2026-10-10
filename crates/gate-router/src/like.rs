//! PowerShell's `-like` wildcard match, as `System.Management.Automation.WildcardPattern` reads a pattern with
//! `IgnoreCase`: `*` matches any run of characters, `?` one character, `[...]` one character of a set whose `a-z`
//! entries are ranges, and a backtick makes the next character literal. The match covers the whole text and ignores
//! letter case. Text and pattern are compared as UTF-16 units, as .NET compares them.
//!
//! Inside a `[...]` set a unit matches by simple case folding: the unit itself, its lower and its upper case form. .NET's
//! regex, which `-like` builds a set into, uses its own table of case-insensitive equivalents instead, so a non-ASCII
//! set member whose equivalents differ there (the Kelvin sign U+212A against `k`) can match in pwsh and not here.

use std::fmt;

const BACKTICK: u16 = 0x60;
const STAR: u16 = 0x2A;
const QUESTION: u16 = 0x3F;
const OPEN: u16 = 0x5B;
const CLOSE: u16 = 0x5D;
const DASH: u16 = 0x2D;

/// A wildcard pattern as `-like` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    text: String,
    elements: Vec<Element>,
}

/// Why a pattern cannot be used: `-like` throws on it, as on an unclosed `[` or a range whose start is above its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPattern(pub String);

impl fmt::Display for InvalidPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "The specified wildcard character pattern is not valid: {}", self.0)
    }
}

impl std::error::Error for InvalidPattern {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Element {
    Literal(u16),
    One,
    Any,
    Set(Vec<(u16, u16)>),
}

impl Pattern {
    /// Reads `text` as `-like` does.
    ///
    /// # Errors
    ///
    /// [`InvalidPattern`] when `-like` would throw on it: a `[` that is never closed, or a set range such as `[c-a]`
    /// whose start is above its end.
    pub fn parse(text: &str) -> Result<Self, InvalidPattern> {
        let invalid = || InvalidPattern(text.to_owned());
        let mut parser = Parser::default();
        for unit in text.encode_utf16() {
            parser.push(unit).ok_or_else(invalid)?;
        }
        // A pattern of one backtick reads as the empty pattern, as it did in Windows PowerShell.
        let elements = parser.finish(text == "`").ok_or_else(invalid)?;
        Ok(Self {
            text: text.to_owned(),
            elements,
        })
    }

    /// The pattern as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether the whole of `text` matches, ignoring letter case.
    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        let count = self.elements.len();
        let mut states = vec![false; count + 1];
        states[0] = true;
        self.follow_stars(&mut states);
        for unit in text.encode_utf16() {
            let mut next = vec![false; count + 1];
            for (index, element) in self.elements.iter().enumerate() {
                if !states[index] {
                    continue;
                }
                match element {
                    Element::Any => next[index] = true,
                    _ if element.takes(unit) => next[index + 1] = true,
                    _ => {}
                }
            }
            self.follow_stars(&mut next);
            states = next;
        }
        states[count]
    }

    /// Marks the state after each `*` reached, since a `*` also matches no character.
    fn follow_stars(&self, states: &mut [bool]) {
        for (index, element) in self.elements.iter().enumerate() {
            if states[index] && *element == Element::Any {
                states[index + 1] = true;
            }
        }
    }
}

impl Element {
    /// Whether this element, other than `*`, matches the one unit `unit`.
    fn takes(&self, unit: u16) -> bool {
        match self {
            Self::Literal(literal) => lower_unit(*literal) == lower_unit(unit),
            Self::One => true,
            Self::Any => false,
            Self::Set(ranges) => {
                let forms = [unit, lower_unit(unit), upper_unit(unit)];
                ranges.iter().any(|(low, high)| forms.iter().any(|form| (*low..=*high).contains(form)))
            }
        }
    }
}

/// The pattern read so far: `WildcardPatternParser.Parse`, one UTF-16 unit at a time.
#[derive(Default)]
struct Parser {
    elements: Vec<Element>,
    escaped: bool,
    bracket: Option<Bracket>,
}

/// An open `[...]` set: its units, which of them are an unescaped `-`, and whether the `[` came just before.
#[derive(Default)]
struct Bracket {
    units: Vec<u16>,
    operators: Vec<bool>,
    just_opened: bool,
}

impl Parser {
    /// Takes one unit; `None` when it makes the pattern invalid.
    fn push(&mut self, unit: u16) -> Option<()> {
        let escaped = self.escaped;
        match self.bracket.take() {
            Some(bracket) if unit == CLOSE && !bracket.just_opened && !escaped => self.elements.push(bracket.close()?),
            Some(mut bracket) => {
                if unit != BACKTICK || escaped {
                    bracket.units.push(unit);
                    bracket.operators.push(unit == DASH && !escaped);
                }
                bracket.just_opened = false;
                self.bracket = Some(bracket);
            }
            None => self.push_outside(unit, escaped),
        }
        self.escaped = unit == BACKTICK && !escaped;
        Some(())
    }

    fn push_outside(&mut self, unit: u16, escaped: bool) {
        if escaped {
            self.elements.push(Element::Literal(unit));
            return;
        }
        match unit {
            STAR => self.elements.push(Element::Any),
            QUESTION => self.elements.push(Element::One),
            OPEN => {
                self.bracket = Some(Bracket {
                    just_opened: true,
                    ..Bracket::default()
                });
            }
            BACKTICK => {}
            _ => self.elements.push(Element::Literal(unit)),
        }
    }

    /// The elements once the pattern ends: `None` with a set still open; a trailing lone backtick is literal.
    fn finish(mut self, lone_backtick: bool) -> Option<Vec<Element>> {
        if self.bracket.is_some() {
            return None;
        }
        if self.escaped && !lone_backtick {
            self.elements.push(Element::Literal(BACKTICK));
        }
        Some(self.elements)
    }
}

impl Bracket {
    /// The set: a unit, an unescaped `-` and a third unit make a range, any other unit stands for itself; `None` when a
    /// range's start is above its end.
    fn close(self) -> Option<Element> {
        let mut ranges = Vec::new();
        let mut index = 0;
        while index < self.units.len() {
            if index + 2 < self.units.len() && self.operators[index + 1] {
                let (low, high) = (self.units[index], self.units[index + 2]);
                if low > high {
                    return None;
                }
                ranges.push((low, high));
                index += 3;
            } else {
                ranges.push((self.units[index], self.units[index]));
                index += 1;
            }
        }
        Some(Element::Set(ranges))
    }
}

/// A character's simple lower-case form, as .NET's `char.ToLowerInvariant` gives it: one character for one, and a
/// character whose lower case is longer (`İ`) kept as it is.
pub(crate) fn lower_char(character: char) -> char {
    single(character.to_lowercase()).unwrap_or(character)
}

/// Whether two texts are equal ignoring letter case, comparing simple lower-case forms.
pub(crate) fn eq_ignoring_case(left: &str, right: &str) -> bool {
    left.chars().map(lower_char).eq(right.chars().map(lower_char))
}

fn upper_char(character: char) -> char {
    single(character.to_uppercase()).unwrap_or(character)
}

fn single(mut characters: impl Iterator<Item = char>) -> Option<char> {
    let first = characters.next()?;
    characters.next().is_none().then_some(first)
}

fn lower_unit(unit: u16) -> u16 {
    map_unit(unit, lower_char)
}

fn upper_unit(unit: u16) -> u16 {
    map_unit(unit, upper_char)
}

/// A UTF-16 unit through a character mapping; a surrogate half, or a mapping that leaves the basic plane, is kept.
fn map_unit(unit: u16, map: fn(char) -> char) -> u16 {
    let Some(character) = char::from_u32(u32::from(unit)) else {
        return unit;
    };
    u16::try_from(u32::from(map(character))).unwrap_or(unit)
}
