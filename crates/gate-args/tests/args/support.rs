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
