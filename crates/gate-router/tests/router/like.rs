//! PowerShell's `-like`. Every expected value in these tables was confirmed by running `'text' -like 'pattern'` in
//! `pwsh -NoProfile` (PowerShell 7), the invalid patterns being the ones on which it threw.

use gate_router::{InvalidPattern, Pattern};

/// Text, pattern, and what `-like` gives.
const CASES: &[(&str, &str, bool)] = &[
    ("", "", true),
    ("a", "", false),
    ("", "*", true),
    ("abc", "*", true),
    ("abc", "a*", true),
    ("abc", "*c", true),
    ("abc", "a*c", true),
    ("ac", "a*c", true),
    ("abc", "a?c", true),
    ("ac", "a?c", false),
    ("ABC", "abc", true),
    ("abc", "A*C", true),
    ("a*c", "a`*c", true),
    ("abc", "a`*c", false),
    ("a?c", "a`?c", true),
    ("abc", "a`?c", false),
    ("a", "`", false),
    ("", "`", true),
    ("a`", "a`", true),
    ("a", "a`", false),
    ("a`", "a``", true),
    ("b", "[abc]", true),
    ("d", "[abc]", false),
    ("B", "[a-c]", true),
    ("b", "[A-C]", true),
    ("-", "[a-]", true),
    ("-", "[-a]", true),
    ("]", "[]]", true),
    ("]", "[`]]", true),
    ("^", "[^a]", true),
    ("b", "[^a]", false),
    ("!", "[!a]", true),
    ("[", "[[]", true),
    ("[", "`[", true),
    ("-", "[a`-c]", true),
    ("b", "[a`-c]", false),
    ("dotnet build x", "dotnet build *", true),
    ("dotnet build", "dotnet build *", false),
    ("dotnet test", "dotnet test*", true),
    ("ab", "[a-c][a-c]", true),
    ("x", "[a-c-x]", true),
    ("-", "[a-c-x]", true),
    ("b", "[a-c-x]", true),
    ("aXb", "a[x]b", true),
    ("a]", "a]", true),
    ("a", "*a*", true),
    ("aaa", "*a", true),
    ("ba", "*a?", false),
    ("abcabd", "*abd", true),
    ("`", "[``]", true),
    ("a", "[a``]", true),
    ("x", "*`", false),
    ("x`", "*`", true),
];

/// Patterns `-like` throws on: an unclosed set (an escaped `]` does not close one) and a reversed range.
const INVALID: &[&str] = &["[", "[a", "[]", "[c-a]", "ab[c", "[`]"];

#[test]
fn like_matches_as_powershell_does() {
    let failures: Vec<String> = CASES
        .iter()
        .filter_map(|&(text, pattern, expected)| {
            let got = Pattern::parse(pattern).map(|parsed| parsed.is_match(text));
            (got != Ok(expected)).then(|| format!("'{text}' -like '{pattern}': expected {expected}, got {got:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn patterns_powershell_throws_on_are_invalid() {
    let accepted: Vec<&str> = INVALID.iter().copied().filter(|pattern| Pattern::parse(pattern).is_ok()).collect();
    assert!(accepted.is_empty(), "accepted: {accepted:?}");
}

#[test]
fn an_invalid_pattern_says_which() {
    let error = Pattern::parse("[x").expect_err("an unclosed set is invalid");
    assert_eq!(error, InvalidPattern("[x".to_owned()));
    assert_eq!(error.to_string(), "The specified wildcard character pattern is not valid: [x");
}

#[test]
fn a_pattern_keeps_its_text() {
    assert_eq!(Pattern::parse("dotnet `[x] *").expect("valid").as_str(), "dotnet `[x] *");
}

#[test]
fn letter_case_is_ignored_beyond_ascii() {
    let pattern = Pattern::parse("caf\u{e9}*").expect("valid");
    assert!(pattern.is_match("CAF\u{c9} build"));
}
