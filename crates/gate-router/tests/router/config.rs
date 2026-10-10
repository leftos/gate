//! The routing file. The reasons are the PowerShell gate's own (`remote.ps1`'s `Read-RemoteConfig`), each confirmed
//! by feeding the same text to it in `pwsh -NoProfile`, but for a host whose characters ssh or git would misread: that
//! refusal is this gate's own and was not confirmed against `pwsh`.

use std::path::Path;

use gate_router::{Config, DEFAULT_OUTPUTS, Pattern, ignored_line};

fn reason(text: &str) -> String {
    Config::parse(text).expect_err("the file is unusable")
}

#[test]
fn a_good_file_gives_host_slots_patterns_and_outputs() {
    let config = Config::parse(
        r#"{"host": "builder-1", "slots": 3,
            "repos": {"gate": ["cargo test *", "cargo build*"], "other": []},
            "outputs": {"gate": ["target/**"]}}"#,
    )
    .expect("usable");
    assert_eq!(config.host(), "builder-1");
    assert_eq!(config.slots(), 3);
    let patterns: Vec<&str> = config.patterns("gate").expect("listed").iter().map(Pattern::as_str).collect();
    assert_eq!(patterns, ["cargo test *", "cargo build*"]);
    assert_eq!(config.patterns("other").expect("listed").len(), 0);
    assert!(config.patterns("missing").is_none());
    assert_eq!(config.outputs("gate"), ["target/**"]);
    assert_eq!(config.outputs("other"), DEFAULT_OUTPUTS);
}

#[test]
fn keys_and_repo_names_match_in_any_letter_case() {
    let config = Config::parse(r#"{"HOST": "builder-1", "Slots": 2, "Repos": {"Gate": ["x"]}}"#).expect("usable");
    assert_eq!(config.host(), "builder-1");
    assert_eq!(config.slots(), 2);
    assert!(config.patterns("gate").is_some());
    assert!(config.patterns("GATE").is_some());
}

#[test]
fn unknown_keys_are_ignored() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"gate": []}, "inputs": {"gate": 5}, "other": [1]}"#).expect("usable");
    assert_eq!(config.host(), "builder-1");
}

#[test]
fn a_byte_order_mark_is_skipped() {
    assert!(Config::parse("\u{feff}{\"host\": \"builder-1\", \"repos\": {}}").is_ok());
}

#[test]
fn slots_default_to_one_and_take_whole_numbers_one_to_sixteen() {
    let slots = |value: &str| Config::parse(&format!(r#"{{"host": "builder-1", "slots": {value}, "repos": {{}}}}"#)).map(|config| config.slots());
    assert_eq!(Config::parse(r#"{"host": "builder-1", "repos": {}}"#).expect("usable").slots(), 1);
    assert_eq!(slots("1"), Ok(1));
    assert_eq!(slots("16"), Ok(16));
    assert_eq!(slots("2.0"), Ok(2));
    assert_eq!(slots("1e1"), Ok(10));
    let refused = Err("slots must be a whole number 1 to 16".to_owned());
    for value in ["0", "17", "2.5", "-1", "\"2\"", "null", "true", "[2]"] {
        assert_eq!(slots(value), refused, "slots {value}");
    }
}

#[test]
fn each_unusable_file_gives_the_powershell_reason() {
    let cases = [
        ("", "not a JSON object"),
        ("   ", "not a JSON object"),
        ("[]", "not a JSON object"),
        ("\"x\"", "not a JSON object"),
        ("null", "not a JSON object"),
        ("{}", "host is missing"),
        (r#"{"host": 1}"#, "host must be a non-empty string"),
        (r#"{"host": ""}"#, "host must be a non-empty string"),
        (r#"{"host": "b"}"#, "repos is missing"),
        (r#"{"host": "b", "repos": []}"#, "repos must be a JSON object"),
        (r#"{"host": "b", "repos": {"g": "x"}}"#, "repos.g is not an array of strings"),
        (r#"{"host": "b", "repos": {"g": null}}"#, "repos.g is not an array of strings"),
        (r#"{"host": "b", "repos": {"g": {}}}"#, "repos.g is not an array of strings"),
        (r#"{"host": "b", "repos": {"g": ["x", ""]}}"#, "repos.g[1] is not a non-empty string"),
        (r#"{"host": "b", "repos": {"g": ["x", 3]}}"#, "repos.g[1] is not a non-empty string"),
        (r#"{"host": "b", "repos": {"g": [["x"]]}}"#, "repos.g[0] is not a non-empty string"),
        (r#"{"host": "b", "repos": {"g": ["[x"]}}"#, "repos.g[0] is not a valid wildcard pattern"),
        (r#"{"host": "b", "repos": {"g": ["x"]}, "outputs": []}"#, "outputs must be a JSON object"),
        (
            r#"{"host": "b", "repos": {"g": ["x"]}, "outputs": {"g": ["[x]"]}}"#,
            "outputs.g[0] is not a valid output glob: '[' and ']' are not allowed",
        ),
        (
            r#"{"host": "b", "Host": "c", "repos": {}}"#,
            "Cannot convert the JSON string because it contains keys with different casing. Please use the -AsHashTable \
             switch instead. The key that was attempted to be added to the existing key 'host' was 'Host'.",
        ),
        (
            r#"{"host": "b", "repos": {"a": ["x"], "A": ["y"]}}"#,
            "Cannot convert the JSON string because it contains keys with different casing. Please use the -AsHashTable \
             switch instead. The key that was attempted to be added to the existing key 'a' was 'A'.",
        ),
    ];
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|(text, expected)| {
            let got = Config::parse(text).map(|_| ());
            (got != Err((*expected).to_owned())).then(|| format!("{text}: expected {expected:?}, got {got:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn text_that_is_not_json_names_the_conversion_failure() {
    for text in ["{", r#"{"host": "b", "repos": {}} x"#] {
        assert!(reason(text).starts_with("Conversion from JSON failed with error: "), "{text}");
    }
}

fn pattern_texts(config: &Config, repo: &str) -> Vec<String> {
    config
        .patterns(repo)
        .expect("listed")
        .iter()
        .map(|pattern| pattern.as_str().to_owned())
        .collect()
}

#[test]
fn a_line_comment_is_skipped() {
    let config = Config::parse("// the builder\n{\"host\": \"builder-1\", // its alias\n \"repos\": {\"g\": [\"x\"]}} // end").expect("usable");
    assert_eq!(config.host(), "builder-1");
    assert_eq!(pattern_texts(&config, "g"), ["x"]);
}

#[test]
fn a_block_comment_is_skipped() {
    let config = Config::parse("/* routing */ {\"host\": /* the\n builder */ \"builder-1\", \"slots\":/**/2, \"repos\": {}}").expect("usable");
    assert_eq!(config.host(), "builder-1");
    assert_eq!(config.slots(), 2);
}

#[test]
fn a_trailing_comma_in_an_object_is_accepted() {
    let config = Config::parse("{\"host\": \"builder-1\", \"repos\": {\"g\": [\"x\"],},\n}").expect("usable");
    assert_eq!(config.host(), "builder-1");
    assert_eq!(pattern_texts(&config, "g"), ["x"]);
}

#[test]
fn a_trailing_comma_in_an_array_even_before_a_comment_is_accepted() {
    let config = Config::parse("{\"host\": \"builder-1\", \"repos\": {\"g\": [\"x\", \"y\", // last\n /* really */ ]}}").expect("usable");
    assert_eq!(pattern_texts(&config, "g"), ["x", "y"]);
}

#[test]
fn comment_and_comma_marks_inside_a_string_are_left_untouched() {
    let config = Config::parse(
        "{\"host\": \"builder-1\", \"repos\": {\"g\": [\"a,}\", \"x /* y */\", \"q,]\", \"e\\\\\", \"b\\\"//c\"], // after an escaped backslash\n}}",
    )
    .expect("usable");
    assert_eq!(config.host(), "builder-1");
    assert_eq!(pattern_texts(&config, "g"), ["a,}", "x /* y */", "q,]", "e\\", "b\"//c"]);
}

#[test]
fn single_quoted_strings_are_refused() {
    let reason = reason("{'host': 'builder-1', 'repos': {}}");
    assert!(reason.starts_with("Conversion from JSON failed with error: "), "{reason}");
}

#[test]
fn a_repeated_key_in_one_letter_case_keeps_the_last_value() {
    let config = Config::parse(r#"{"host": "b", "host": "c", "repos": {}}"#).expect("usable");
    assert_eq!(config.host(), "c");
}

#[test]
fn the_ignored_line_names_the_file_and_the_reason() {
    assert_eq!(
        ignored_line(Path::new("remote.json"), "host is missing"),
        "gate: ignored remote.json: host is missing; running locally"
    );
}

/// A usable routing file but for its `host`, set to the JSON text `host_json`, which may hold escapes.
fn host_file(host_json: &str) -> String {
    format!(r#"{{"host": "{host_json}", "repos": {{}}}}"#)
}

#[test]
fn a_host_that_is_not_an_ssh_alias_or_user_at_host_is_refused() {
    let refused = Err::<(), String>("host must be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'".to_owned());
    // A JSON string cannot hold a raw control character, so each form that is one is written as its JSON escape.
    let forms = [
        ("-oProxyCommand=x", "a leading dash"),
        ("a b", "a space"),
        (r"a\tb", "a tab"),
        (r"a\nb", "a line break"),
        (r"a\u0007b", "a control character"),
        (r"a\u00a0b", "a no-break space"),
        ("user@-x", "a host part starting with a dash"),
        (r"a\u200bb", "a zero-width space"),
        (r"a\u202eb", "a right-to-left override"),
        (r"a\ufeffb", "a byte order mark"),
        (r#"a\"b"#, "a quote"),
        ("a%b", "a percent sign"),
        ("a;b", "a semicolon"),
        ("$(x)", "a command substitution"),
        ("host:22", "a host and a port"),
        ("a/b", "a slash"),
        ("[::1]", "an IPv6 address"),
        ("@host", "an empty user"),
        ("user@", "an empty host"),
        ("a@b@c", "two at signs"),
    ];
    let failures: Vec<String> = forms
        .iter()
        .filter_map(|(host, what)| {
            let got = Config::parse(&host_file(host)).map(|_| ());
            (got != refused).then(|| format!("{what} ({host}): expected {refused:?}, got {got:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn an_ssh_alias_or_user_at_host_of_allowed_characters_is_accepted() {
    for host in ["builder", "build-box.lan", "user@build-box", "10.0.0.5", "b_1"] {
        let config = Config::parse(&host_file(host)).expect("usable");
        assert_eq!(config.host(), host);
    }
}
