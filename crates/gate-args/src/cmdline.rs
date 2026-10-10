//! The command's words written into one `CreateProcess` command line for the Microsoft C runtime to read back (C4), and
//! the journal's and the claim's text of the command.
//!
//! Words are UTF-16, as `CreateProcessW` takes them, so a word holding a lone surrogate is written unchanged.

use std::ffi::OsString;
use std::iter;

pub(crate) const QUOTE: u16 = 0x22;
const BACKSLASH: u16 = 0x5C;
pub(crate) const SPACE: u16 = 0x20;

/// One word as the C runtime reads it back as one argument, as the PowerShell gate's `Format-Argument` writes it.
///
/// A non-empty word holding no whitespace (Unicode's, as .NET's `\s`) and no `"` is written as it is. Any other word is
/// wrapped in quotes, `"` inside becomes `\"`, and a backslash run before a quote or at the end is doubled: only there
/// does the C runtime read backslashes as escapes.
#[must_use]
pub fn quote_arg(word: &[u16]) -> Vec<u16> {
    if is_plain(word, &[]) {
        word.to_vec()
    } else {
        wrapped(word, &[BACKSLASH, QUOTE])
    }
}

/// [`quote_arg`] over a string.
#[must_use]
pub fn quote_arg_str(word: &str) -> String {
    // Quoting adds only ASCII to valid UTF-16, so the lossy decode loses nothing.
    String::from_utf16_lossy(&quote_arg(&wide(word)))
}

/// The words, the program first, as one command line: each through [`quote_arg`], joined by single spaces. A word
/// holding U+0000 is written as it is; the launch refuses such a line.
#[must_use]
pub fn command_line<W: AsRef<[u16]>>(words: &[W]) -> Vec<u16> {
    let quoted: Vec<Vec<u16>> = words.iter().map(|word| quote_arg(word.as_ref())).collect();
    quoted.join(&SPACE)
}

/// The command as the journal and the claim carry it, as the PowerShell gate's `Format-CommandText` writes it: the
/// words as given (the program unresolved), each through [`quote_arg_str`] even for a batch target, joined by single
/// spaces. A word that is not valid Unicode is written with U+FFFD in place of what cannot be read, as the files are
/// UTF-8.
#[must_use]
pub fn command_text(words: &[OsString]) -> String {
    let quoted: Vec<String> = words.iter().map(|word| quote_arg_str(&word.to_string_lossy())).collect();
    quoted.join(" ")
}

pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// Whether `word` is non-empty and holds no whitespace, no `"` and none of `specials`. An unpaired surrogate is none
/// of them.
pub(crate) fn is_plain(word: &[u16], specials: &[char]) -> bool {
    !word.is_empty()
        && char::decode_utf16(word.iter().copied())
            .filter_map(Result::ok)
            .all(|character| !character.is_whitespace() && character != '"' && !specials.contains(&character))
}

/// `word` wrapped in quotes with each `"` written as `escaped_quote`, and the backslash run before each quote and at
/// the end doubled.
pub(crate) fn wrapped(word: &[u16], escaped_quote: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(word.len() + 2);
    out.push(QUOTE);
    let mut backslashes = 0;
    for &unit in word {
        if unit == BACKSLASH {
            backslashes += 1;
            continue;
        }
        if unit == QUOTE {
            out.extend(iter::repeat_n(BACKSLASH, backslashes * 2));
            out.extend_from_slice(escaped_quote);
        } else {
            out.extend(iter::repeat_n(BACKSLASH, backslashes));
            out.push(unit);
        }
        backslashes = 0;
    }
    out.extend(iter::repeat_n(BACKSLASH, backslashes * 2));
    out.push(QUOTE);
    out
}
