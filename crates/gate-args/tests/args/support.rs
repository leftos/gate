//! Building argv and reading the parse.

use std::ffi::OsString;

use gate_args::{Invocation, RunRequest, StopRequest, parse};

/// The words a new-CLI run needs, before `--`.
pub const RUN: [&str; 7] = ["run", "--slot", "heavy", "--timeout", "5", "--log", "out.log"];

pub fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// `words` with `extra` after them.
pub fn with(words: &[&str], extra: &[&str]) -> Vec<String> {
    words.iter().chain(extra).map(|word| (*word).to_owned()).collect()
}

pub fn refs(words: &[String]) -> Vec<&str> {
    words.iter().map(String::as_str).collect()
}

pub fn invocation(words: &[&str]) -> Invocation {
    parse(os(words)).expect("the command line parses")
}

pub fn run_request(words: &[&str]) -> RunRequest {
    run_of(os(words))
}

pub fn run_of(words: Vec<OsString>) -> RunRequest {
    let invocation = parse(words).expect("the command line parses");
    let request = match &invocation {
        Invocation::Run(request) => Some(request.clone()),
        _ => None,
    };
    assert!(request.is_some(), "a run, got {invocation:?}");
    request.expect("checked above")
}

pub fn stop_request(words: &[&str]) -> StopRequest {
    let invocation = invocation(words);
    let request = match &invocation {
        Invocation::StopTree(request) => Some(request.clone()),
        _ => None,
    };
    assert!(request.is_some(), "{words:?} is a stop-tree, got {invocation:?}");
    request.expect("checked above")
}

/// The problem lines a refused command line prints.
pub fn problems(words: &[&str]) -> Vec<String> {
    problems_of(os(words))
}

pub fn problems_of(words: Vec<OsString>) -> Vec<String> {
    parse(words)
        .expect_err("the command line is refused")
        .iter()
        .map(ToString::to_string)
        .collect()
}

const QUOTE: u16 = 0x22;
const BACKSLASH: u16 = 0x5C;
const SPACE: u16 = 0x20;
const TAB: u16 = 0x09;

/// The words of a command line as the Microsoft C runtime splits it ("Parsing C command-line arguments"), the program
/// name first; no words for an empty line. The reference the quoting tests read the written line back with.
///
/// The program name ends at the first space or tab outside quotes, and its quotes are dropped with no escape read.
/// After it, spaces and tabs outside quotes separate words; a quote opens or closes a quoted span; inside a span `""`
/// is one literal quote; a backslash run before a quote gives one backslash per pair and, when odd, a literal quote;
/// any other backslash is literal; a line ending inside a span ends its last word.
pub fn split_command_line(line: &[u16]) -> Vec<Vec<u16>> {
    if line.is_empty() {
        return Vec::new();
    }
    let (program, rest) = program_name(line);
    let mut words = vec![program];
    words.extend(arguments(rest));
    words
}

fn program_name(line: &[u16]) -> (Vec<u16>, &[u16]) {
    let mut name = Vec::new();
    let mut quoted = false;
    for (index, &unit) in line.iter().enumerate() {
        match unit {
            QUOTE => quoted = !quoted,
            SPACE | TAB if !quoted => return (name, skip_blanks(&line[index..])),
            _ => name.push(unit),
        }
    }
    (name, &[])
}

fn arguments(mut rest: &[u16]) -> Vec<Vec<u16>> {
    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut started = false;
    let mut quoted = false;
    while let Some((&unit, after)) = rest.split_first() {
        rest = after;
        if !quoted && matches!(unit, SPACE | TAB) {
            words.push(std::mem::take(&mut word));
            started = false;
            rest = skip_blanks(rest);
            continue;
        }
        started = true;
        match unit {
            BACKSLASH => rest = backslash_run(rest, &mut word),
            QUOTE if quoted => (rest, quoted) = quote_in_span(rest, &mut word),
            QUOTE => quoted = true,
            _ => word.push(unit),
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// Reads the backslash run that starts with the backslash just read, `rest` following it, into `word`.
fn backslash_run<'a>(rest: &'a [u16], word: &mut Vec<u16>) -> &'a [u16] {
    let more = rest.iter().take_while(|&&unit| unit == BACKSLASH).count();
    let count = more + 1;
    let after = &rest[more..];
    if after.first() != Some(&QUOTE) {
        word.extend(std::iter::repeat_n(BACKSLASH, count));
        return after;
    }
    word.extend(std::iter::repeat_n(BACKSLASH, count / 2));
    if count % 2 == 1 {
        word.push(QUOTE);
        return &after[1..];
    }
    after
}

/// Reads a quote inside a quoted span, `rest` following it: `""` is one literal quote and the span goes on; otherwise
/// the span closes.
fn quote_in_span<'a>(rest: &'a [u16], word: &mut Vec<u16>) -> (&'a [u16], bool) {
    match rest.split_first() {
        Some((&QUOTE, after)) => {
            word.push(QUOTE);
            (after, true)
        }
        _ => (rest, false),
    }
}

fn skip_blanks(rest: &[u16]) -> &[u16] {
    let blanks = rest.iter().take_while(|&&unit| matches!(unit, SPACE | TAB)).count();
    &rest[blanks..]
}

/// A word that is not valid Unicode.
#[cfg(windows)]
pub fn unreadable_word() -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(&[u16::from(b'a'), 0xD800])
}

/// A word that is not valid Unicode.
#[cfg(unix)]
pub fn unreadable_word() -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(vec![b'a', 0xFF])
}
