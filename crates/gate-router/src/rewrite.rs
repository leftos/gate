//! The in-worktree path rewrite: a routed command's words as the builder runs them in the gate's folder there.
//! Every path in a word that lies inside the worktree is made relative to the gate's folder; any other path, a UNC
//! path or a relative path climbing above the root marks the word as outside the worktree.
//!
//! A path starts at a drive letter, a colon and a separator not preceded by a letter, digit, `_` or `-`, anywhere in
//! the word (`--flag=X:\a`, `-p:Name=X:\a`, `"X:\a;X:\b"`, a script's text), each such start a path of its own, judged
//! on its own. It runs to the first of `, ; " ' ) | & < >`. In a word that holds one of those or a line break (a
//! script's text or a list) white space ends it too; in any other word, one argument, a path runs through its spaces.

use crate::like::{eq_ignoring_case, lower_char};

/// Characters that end a path in a word.
const PATH_ENDS: [char; 9] = [',', ';', '"', '\'', ')', '|', '&', '<', '>'];

/// A command's words rewritten for the builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    /// Each word with its in-worktree paths made relative to the gate's folder; outside paths stay as written.
    pub words: Vec<String>,
    /// The first word, as given, that names a path outside the worktree.
    pub outside: Option<String>,
}

/// `words` as the builder runs them in the gate's folder. `root` is the worktree root as git gives it, `prefix` the
/// gate's folder below it as `git rev-parse --show-prefix` gives it (`sub/dir/`, empty at the root), and `drives` the
/// subst table: a drive letter for the path its device stands for. A drive whose path is the root or a
/// separator-bounded prefix of it gives the root another spelling on that letter; any other adds none. The root
/// compares in any letter case and with either separator; a path naming the gate's folder becomes `.`, any other loses
/// the root and the separators after it, gaining one `..` a level of the gate's folder (written with `\` when the path
/// held one, else `/`).
#[must_use]
pub fn rewrite_words(root: &str, prefix: &str, words: &[String], drives: &[(char, String)]) -> Rewritten {
    let cwd: Vec<&str> = prefix.split(['\\', '/']).filter(|segment| !segment.is_empty()).collect();
    let bases = root_spellings(root, drives);
    let mut outside = None;
    let mut rewritten = Vec::with_capacity(words.len());
    for word in words {
        let (text, is_outside) = rewrite_word(&bases, &cwd, word);
        rewritten.push(text);
        if is_outside && outside.is_none() {
            outside = Some(word.clone());
        }
    }
    Rewritten { words: rewritten, outside }
}

/// The drive letters, in upper case and each once, that a path in `words` starts on, other than the root's own: the
/// letters whose subst target the caller looks up.
pub(crate) fn drive_letters(root: &str, words: &[String]) -> Vec<char> {
    let own = root_letter(root);
    let mut letters = Vec::new();
    for word in words {
        for index in drive_starts(word) {
            let letter = char::from(word.as_bytes()[index]).to_ascii_uppercase();
            if Some(letter) != own && !letters.contains(&letter) {
                letters.push(letter);
            }
        }
    }
    letters
}

fn root_letter(root: &str) -> Option<char> {
    let mut characters = root.chars();
    let letter = characters.next()?;
    (letter.is_ascii_alphabetic() && characters.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

/// The root as words are compared with it: `/` separators and no trailing one, then its spelling on each subst drive
/// whose path is the root or a separator-bounded prefix of it.
fn root_spellings(root: &str, drives: &[(char, String)]) -> Vec<String> {
    let base = root.replace('\\', "/").trim_end_matches('/').to_owned();
    let mut spellings = vec![base.clone()];
    for (letter, target) in drives {
        let target = target.replace('\\', "/");
        let target = target.trim_end_matches('/');
        if target.is_empty() {
            continue;
        }
        match strip_prefix_ignoring_case(&base, target) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => spellings.push(format!("{letter}:{rest}")),
            _ => {}
        }
    }
    spellings
}

fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let mut rest = text.chars();
    for expected in prefix.chars() {
        if lower_char(rest.next()?) != lower_char(expected) {
            return None;
        }
    }
    Some(rest.as_str())
}

/// One word with its in-worktree paths rewritten, and whether it names a path outside the worktree.
fn rewrite_word(bases: &[String], cwd: &[&str], word: &str) -> (String, bool) {
    if word.starts_with(r"\\") || word.starts_with("//") {
        return (word.to_owned(), true);
    }
    let starts = drive_starts(word);
    if starts.is_empty() {
        return (word.to_owned(), relative_escape(cwd, word));
    }
    let script = word.contains(PATH_ENDS) || word.contains(['\n', '\r']);
    let mut text = String::with_capacity(word.len());
    let mut outside = false;
    let mut at = 0;
    for index in starts {
        match path_edit(bases, cwd, word, index, script) {
            None => outside = true,
            Some(edit) if index >= at => {
                text.push_str(&word[at..index]);
                text.push_str(&edit.text);
                at = edit.end;
            }
            Some(_) => {}
        }
    }
    text.push_str(&word[at..]);
    (text, outside)
}

/// The byte indexes in `word` where a drive path starts: an ASCII letter, `:` and `\` or `/`, not preceded by a
/// letter, digit, `_` or `-`.
fn drive_starts(word: &str) -> Vec<usize> {
    let bytes = word.as_bytes();
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 2 < bytes.len() {
        let drive = bytes[index].is_ascii_alphabetic() && bytes[index + 1] == b':' && matches!(bytes[index + 2], b'\\' | b'/');
        if drive && !follows_word_character(word, index) {
            starts.push(index);
            index += 3;
        } else {
            index += 1;
        }
    }
    starts
}

fn follows_word_character(word: &str, index: usize) -> bool {
    word[..index]
        .chars()
        .next_back()
        .is_some_and(|before| before.is_alphanumeric() || before == '_' || before == '-')
}

/// The span `index..end` of a word replaced by `text`.
struct Edit {
    end: usize,
    text: String,
}

/// How the path at `index` is sent when it lies inside the worktree in one of the root's spellings; `None` when it
/// lies outside in every one, its `..` climbing out included.
fn path_edit(bases: &[String], cwd: &[&str], word: &str, index: usize, script: bool) -> Option<Edit> {
    let after = bases.iter().find_map(|base| root_end(base, word, index, script))?;
    let end = path_end(word, after, script);
    let target = resolve(&[], &word[after..end])?;
    if eq_ignoring_case(&target.join("/"), &cwd.join("/")) {
        return Some(Edit { end, text: ".".to_owned() });
    }
    if cwd.is_empty() {
        return Some(Edit {
            end: separator_end(word, after),
            text: String::new(),
        });
    }
    let separator = if word[index..end].contains('\\') { "\\" } else { "/" };
    Some(Edit {
        end: after,
        text: vec![".."; cwd.len()].join(separator),
    })
}

/// The byte index just past the root spelled `base` at `index` in `word`, compared in any letter case with `\` read as
/// `/`, when what follows it is a separator, the word's end or a character ending the path; so `X:/a/wt` and
/// `X:/a/w.old` are not inside `X:/a/w`.
fn root_end(base: &str, word: &str, index: usize, script: bool) -> Option<usize> {
    let mut rest = word[index..].chars();
    for expected in base.chars() {
        let actual = rest.next()?;
        let actual = if actual == '\\' { '/' } else { actual };
        if lower_char(actual) != lower_char(expected) {
            return None;
        }
    }
    let after = word.len() - rest.as_str().len();
    let bounded = separator_end(word, after) > after || path_end(word, after, script) == after;
    bounded.then_some(after)
}

fn separator_end(word: &str, start: usize) -> usize {
    start + word[start..].bytes().take_while(|byte| matches!(byte, b'\\' | b'/')).count()
}

/// Where the path running on from `start` ends: the first character ending a path, or the word's length.
fn path_end(word: &str, start: usize, script: bool) -> usize {
    word[start..]
        .char_indices()
        .find(|(_, character)| PATH_ENDS.contains(character) || (script && character.is_whitespace()))
        .map_or(word.len(), |(at, _)| start + at)
}

/// The segments of `path` below the folder whose segments are `base`, `.` and empty segments dropped and each `..`
/// taking one off; `None` when a `..` climbs above the folder `base` starts from.
fn resolve<'a>(base: &[&'a str], path: &'a str) -> Option<Vec<&'a str>> {
    let mut segments = base.to_vec();
    for segment in path.split(['\\', '/']) {
        if segment == ".." {
            segments.pop()?;
        } else if !segment.is_empty() && segment != "." {
            segments.push(segment);
        }
    }
    Some(segments)
}

/// Whether a word holding no drive path names a folder above the worktree root: the word itself, the text after its
/// last `=` (`--project=../x`) or the text after a leading switch prefix (`-p:../x`, `/p:../x`).
fn relative_escape(cwd: &[&str], word: &str) -> bool {
    let after_equals = word.rfind('=').map(|at| &word[at + 1..]);
    [Some(word), after_equals, switch_value(word)]
        .into_iter()
        .flatten()
        .any(|text| climbs(cwd, text))
}

/// The value of a `-x:value`, `--x:value` or `/x:value` word: a switch name of a letter then letters, digits, `_`,
/// `.` or `-`, a colon, and a value of one line.
fn switch_value(word: &str) -> Option<&str> {
    let rest = word
        .strip_prefix("--")
        .or_else(|| word.strip_prefix('-'))
        .or_else(|| word.strip_prefix('/'))?;
    if !rest.starts_with(|first: char| first.is_ascii_alphabetic()) {
        return None;
    }
    let (colon, mark) = rest
        .char_indices()
        .find(|(_, character)| !(character.is_alphanumeric() || matches!(character, '_' | '.' | '-')))?;
    if mark != ':' {
        return None;
    }
    let value = &rest[colon + 1..];
    let value = value.strip_suffix('\n').unwrap_or(value);
    (!value.is_empty() && !value.contains('\n')).then_some(value)
}

/// Whether `text`, read as a path relative to the gate's folder, climbs above the root. Only text starting with `..`
/// or holding a separator is read as a path, and never text starting with a separator: that is a switch (`/p:`,
/// `/c`) or a path on the current drive, none of the worktree's.
fn climbs(cwd: &[&str], text: &str) -> bool {
    if text.starts_with(['\\', '/']) || (!text.contains(['\\', '/']) && !text.starts_with("..")) {
        return false;
    }
    resolve(cwd, text).is_none()
}
