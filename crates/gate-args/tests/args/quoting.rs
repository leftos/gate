//! C-runtime quoting (C4): each word as the command line writes it, the command line and the journal's command text,
//! the C runtime's splitting rules, and the round trip from words to a line and back.

use std::ffi::OsString;

use gate_args::{command_line, command_text, quote_arg, quote_arg_str};
use proptest::prelude::*;

use crate::support::split_command_line;

const QUOTE: u16 = 0x22;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn wide_words(words: &[&str]) -> Vec<Vec<u16>> {
    words.iter().map(|word| wide(word)).collect()
}

/// Each word and how `Format-Argument` in the PowerShell gate writes it.
#[test]
fn quote_arg_writes_each_word_as_format_argument_does() {
    for (word, written) in [
        ("", r#""""#),
        ("a b", r#""a b""#),
        ("a\tb", "\"a\tb\""),
        (r#"x"y"#, r#""x\"y""#),
        ("\"", r#""\"""#),
        (r"c:\dir with space\", r#""c:\dir with space\\""#),
        (r"c:\dir\", r"c:\dir\"),
        (r#"back\\"quote"#, r#""back\\\\\"quote""#),
        (r#"end\""#, r#""end\\\"""#),
        (r"a\b", r"a\b"),
        ("héllo wörld", "\"héllo wörld\""),
        ("日本語", "日本語"),
        ("😀", "😀"),
        ("a\u{3000}b", "\"a\u{3000}b\""),
        ("a\u{a0}b", "\"a\u{a0}b\""),
        ("--", "--"),
        ("-p:Name=Value", "-p:Name=Value"),
    ] {
        assert_eq!(quote_arg_str(word), written, "word {word:?}");
        assert_eq!(quote_arg(&wide(word)), wide(written), "word {word:?} as UTF-16");
    }
}

#[test]
fn quote_arg_keeps_a_lone_surrogate() {
    let lone = [u16::from(b'a'), 0xD800];
    assert_eq!(quote_arg(&lone), lone);
    let spaced = [u16::from(b'a'), u16::from(b' '), 0xDC00];
    assert_eq!(quote_arg(&spaced), [QUOTE, u16::from(b'a'), u16::from(b' '), 0xDC00, QUOTE]);
}

#[test]
fn command_line_joins_the_quoted_words_with_spaces() {
    let words = wide_words(&[r"C:\Program Files\tool.exe", "a b", "", "plain"]);
    assert_eq!(command_line(&words), wide(r#""C:\Program Files\tool.exe" "a b" "" plain"#));
    assert_eq!(command_line::<Vec<u16>>(&[]), Vec::<u16>::new());
}

#[test]
fn command_text_is_the_raw_words_quoted_as_format_command_text_does() {
    let words: Vec<OsString> = [
        "cargo",
        "test",
        "--manifest-path",
        "X:/dev/ops.wt/ops-61/Cargo.toml",
        "--",
        "--exact",
        "a b",
        "a\u{a0}b",
        ".tmp/out.txt",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    assert_eq!(
        command_text(&words),
        "cargo test --manifest-path X:/dev/ops.wt/ops-61/Cargo.toml -- --exact \"a b\" \"a\u{a0}b\" .tmp/out.txt"
    );
    let batch: Vec<OsString> = ["run.cmd", "a&b", r#"x"y"#].into_iter().map(OsString::from).collect();
    assert_eq!(command_text(&batch), r#"run.cmd a&b "x\"y""#, "C4 even for a batch target");
}

/// The examples of Microsoft's "Parsing C command-line arguments", each after a program name.
#[test]
fn split_command_line_reads_the_documented_examples() {
    for (line, words) in [
        (r#"prog "a b c" d e"#, vec!["prog", "a b c", "d", "e"]),
        (r#"prog "ab\"c" "\\" d"#, vec!["prog", r#"ab"c"#, r"\", "d"]),
        (r#"prog a\\\b d"e f"g h"#, vec!["prog", r"a\\\b", "de fg", "h"]),
        (r#"prog a\\\"b c d"#, vec!["prog", r#"a\"b"#, "c", "d"]),
        (r#"prog a\\\\"b c" d e"#, vec!["prog", r"a\\b c", "d", "e"]),
        (r#"prog a"b"" c d"#, vec!["prog", r#"ab" c d"#]),
    ] {
        assert_eq!(split_command_line(&wide(line)), wide_words(&words), "line {line}");
    }
}

/// The program name takes quotes but no backslash escape; whitespace runs separate; an empty quoted word is a word.
#[test]
fn split_command_line_edges() {
    for (line, words) in [
        ("", vec![]),
        (r#""C:\Program Files\tool.exe" a"#, vec![r"C:\Program Files\tool.exe", "a"]),
        (r#"C:\a\"b c" d"#, vec![r"C:\a\b c", "d"]),
        ("prog \t a  \t", vec!["prog", "a"]),
        (r#"prog """#, vec!["prog", ""]),
        (r#"prog "" """#, vec!["prog", "", ""]),
        (r#"prog "unclosed  x"#, vec!["prog", "unclosed  x"]),
        (r"prog a\", vec!["prog", r"a\"]),
    ] {
        assert_eq!(split_command_line(&wide(line)), wide_words(&words), "line {line:?}");
    }
}

fn unit() -> impl Strategy<Value = u16> {
    prop_oneof![
        Just(u16::from(b'\\')),
        Just(QUOTE),
        Just(u16::from(b' ')),
        Just(u16::from(b'\t')),
        Just(u16::from(b'a')),
        Just(0xD800_u16),
        Just(0x3000_u16),
        1_u16..=u16::MAX,
    ]
}

fn word() -> impl Strategy<Value = Vec<u16>> {
    prop::collection::vec(unit(), 0..8)
}

/// A program word: the C runtime reads the program name without backslash escapes, so a name holding a quote, or one
/// ending in a backslash, cannot be written for it to read back; no path is either.
fn program() -> impl Strategy<Value = Vec<u16>> {
    word().prop_filter("a program name holds no quote and does not end in a backslash", |word| {
        !word.contains(&QUOTE) && word.last() != Some(&u16::from(b'\\'))
    })
}

proptest! {
    #[test]
    fn splitting_the_command_line_gives_back_the_words(program in program(), arguments in prop::collection::vec(word(), 0..6)) {
        let mut words = vec![program];
        words.extend(arguments);
        prop_assert_eq!(split_command_line(&command_line(&words)), words);
    }
}
