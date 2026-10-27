//! The routing file. The reasons are the PowerShell gate's own (`remote.ps1`'s `Read-RemoteConfig`), each confirmed
//! by feeding the same text to it in `pwsh -NoProfile`, but for a host whose characters ssh or git would misread: that
//! refusal is this gate's own and was not confirmed against `pwsh`.

use std::path::Path;

use gate_router::{Builder, BuilderOs, Config, Pattern, builder_id, ignored_line};

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
    assert_eq!(config.own_outputs("gate"), Some(vec!["target/**".to_owned()]));
    assert_eq!(config.own_outputs("other"), None, "a repo outputs does not name has no list of its own");
}

#[test]
fn a_failure_outputs_entry_is_read_and_a_repo_it_does_not_name_has_none() {
    let config = Config::parse(
        r#"{"host": "builder-1", "repos": {"gate": [], "other": []},
            "outputs": {"gate": ["target/**"]},
            "failureOutputs": {"Gate": ["dumps/*.dmp", "**/*.crashreport.json"]}}"#,
    )
    .expect("usable");
    assert_eq!(config.failure_outputs("gate"), ["dumps/*.dmp", "**/*.crashreport.json"]);
    assert_eq!(config.failure_outputs("other"), Vec::<String>::new());
    assert_eq!(config.own_outputs("gate"), Some(vec!["target/**".to_owned()]), "outputs is its own list");
    assert_eq!(
        Config::parse(r#"{"host": "builder-1", "repos": {"gate": []}}"#)
            .expect("usable")
            .failure_outputs("gate"),
        Vec::<String>::new(),
        "absent, it has no defaults"
    );
}

#[test]
fn an_unusable_failure_outputs_entry_is_refused_as_an_outputs_entry_is() {
    let cases = [
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "failureOutputs": []}"#,
            "failureOutputs must be a JSON object",
        ),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "failureOutputs": {"g": ["x", ""]}}"#,
            "failureOutputs.g[1] is not a non-empty string",
        ),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "failureOutputs": {"g": ["dumps/[x"]}}"#,
            "failureOutputs.g[0] is not a valid wildcard pattern",
        ),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "failureOutputs": {"g": ["dumps/[x]"]}}"#,
            "failureOutputs.g[0] is not a valid output glob: '[' and ']' are not allowed",
        ),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]},
                "builders": [{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"g": ["x"]},
                              "failureOutputs": {"g": ["x]"]}}]}"#,
            "builders[0].failureOutputs.g[0] is not a valid output glob: '[' and ']' are not allowed",
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(reason(text), expected, "{text}");
    }
}

#[test]
fn an_inputs_entry_is_read_and_a_repo_it_does_not_name_has_none() {
    let config = Config::parse(
        r#"{"host": "builder-1", "repos": {"gate": [], "other": []},
            "inputs": {"Gate": [".tmp/helper/**", "secrets.json"]}}"#,
    )
    .expect("usable");
    assert_eq!(config.inputs("gate"), [".tmp/helper/**", "secrets.json"]);
    assert_eq!(config.inputs("other"), Vec::<String>::new());
    assert_eq!(
        Config::parse(r#"{"host": "builder-1", "repos": {}}"#).expect("usable").inputs("gate"),
        Vec::<String>::new()
    );
}

#[test]
fn an_unusable_inputs_entry_is_refused_as_an_outputs_entry_is() {
    let cases = [
        (r#"{"host": "bx", "repos": {"g": ["x"]}, "inputs": []}"#, "inputs must be a JSON object"),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "inputs": {"g": "x"}}"#,
            "inputs.g is not an array of strings",
        ),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "inputs": {"g": ["x", ""]}}"#,
            "inputs.g[1] is not a non-empty string",
        ),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "inputs": {"g": ["x", 3]}}"#,
            "inputs.g[1] is not a non-empty string",
        ),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "inputs": {"g": ["[x]"]}}"#,
            "inputs.g[0] is not a valid input glob: '[' and ']' are not allowed",
        ),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "inputs": {"g": ["a", "x]"]}}"#,
            "inputs.g[1] is not a valid input glob: '[' and ']' are not allowed",
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
fn keys_and_repo_names_match_in_any_letter_case() {
    let config = Config::parse(r#"{"HOST": "builder-1", "Slots": 2, "Repos": {"Gate": ["x"]}}"#).expect("usable");
    assert_eq!(config.host(), "builder-1");
    assert_eq!(config.slots(), 2);
    assert!(config.patterns("gate").is_some());
    assert!(config.patterns("GATE").is_some());
}

#[test]
fn unknown_keys_are_ignored() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"gate": []}, "extras": {"gate": 5}, "other": [1]}"#).expect("usable");
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
        (r#"{"host": "bx"}"#, "repos is missing"),
        (r#"{"host": "bx", "repos": []}"#, "repos must be a JSON object"),
        (r#"{"host": "bx", "repos": {"g": "x"}}"#, "repos.g is not an array of strings"),
        (r#"{"host": "bx", "repos": {"g": null}}"#, "repos.g is not an array of strings"),
        (r#"{"host": "bx", "repos": {"g": {}}}"#, "repos.g is not an array of strings"),
        (r#"{"host": "bx", "repos": {"g": ["x", ""]}}"#, "repos.g[1] is not a non-empty string"),
        (r#"{"host": "bx", "repos": {"g": ["x", 3]}}"#, "repos.g[1] is not a non-empty string"),
        (r#"{"host": "bx", "repos": {"g": [["x"]]}}"#, "repos.g[0] is not a non-empty string"),
        (r#"{"host": "bx", "repos": {"g": ["[x"]}}"#, "repos.g[0] is not a valid wildcard pattern"),
        (r#"{"host": "bx", "repos": {"g": ["x"]}, "outputs": []}"#, "outputs must be a JSON object"),
        (
            r#"{"host": "bx", "repos": {"g": ["x"]}, "outputs": {"g": ["[x]"]}}"#,
            "outputs.g[0] is not a valid output glob: '[' and ']' are not allowed",
        ),
        (
            r#"{"host": "bx", "Host": "c", "repos": {}}"#,
            "Cannot convert the JSON string because it contains keys with different casing. Please use the -AsHashTable \
             switch instead. The key that was attempted to be added to the existing key 'host' was 'Host'.",
        ),
        (
            r#"{"host": "bx", "repos": {"a": ["x"], "A": ["y"]}}"#,
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
    for text in ["{", r#"{"host": "bx", "repos": {}} x"#] {
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
    let config = Config::parse(r#"{"host": "bx", "host": "cx", "repos": {}}"#).expect("usable");
    assert_eq!(config.host(), "cx");
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
fn the_host_allowlist_refuses_one_letter_which_git_would_read_as_a_drive() {
    let refused = Err::<(), String>("host must not be a single letter, which a git host:path URL reads as a drive".to_owned());
    for host in ["b", "Z"] {
        assert_eq!(Config::parse(&host_file(host)).map(|_| ()), refused, "{host}");
    }
    for host in ["user@b", "b1", "7"] {
        let config = Config::parse(&host_file(host)).expect("usable");
        assert_eq!(config.host(), host, "{host} reads as no drive");
    }
}

#[test]
fn hosts_adds_builders_after_host() {
    let config = Config::parse(r#"{"host": "builder-1", "Hosts": ["builder-2", "user@build-box.lan"], "repos": {}}"#).expect("usable");
    assert_eq!(config.host(), "builder-1", "routing keeps the one host");
    assert_eq!(
        config.builders(),
        Ok(["builder-1", "builder-2", "user@build-box.lan"].map(String::from).as_slice())
    );
}

#[test]
fn a_hosts_entry_equal_to_host_is_skipped() {
    let config = Config::parse(r#"{"host": "builder-1", "hosts": ["BUILDER-1", "b2", "B2", "b3"], "repos": {}}"#).expect("usable");
    assert_eq!(
        config.builders(),
        Ok(["builder-1", "b2", "b3"].map(String::from).as_slice()),
        "an entry equal to an earlier one in any letter case is skipped"
    );
}

#[test]
fn a_bad_hosts_entry_ignores_the_file() {
    let cases = [
        (
            r#"["b2", "z"]"#,
            "hosts entries must not be a single letter, which a git host:path URL reads as a drive",
        ),
        (
            r#"["a b"]"#,
            "hosts entries must each be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'",
        ),
        (
            r#"["-oProxyCommand=x"]"#,
            "hosts entries must each be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'",
        ),
        (r#"["b2", ""]"#, "hosts entries must each be a non-empty string"),
        ("[3]", "hosts entries must each be a non-empty string"),
    ];
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|(hosts, expected)| {
            let config = Config::parse(&format!(r#"{{"host": "builder-1", "hosts": {hosts}, "repos": {{}}}}"#)).expect("usable for routing");
            let got = config.builders().map(<[String]>::to_vec);
            (got != Err(*expected)).then(|| format!("{hosts}: expected {expected:?}, got {got:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn hosts_that_is_not_an_array_ignores_the_file() {
    for hosts in [r#""builder-2""#, "{}", "null", "2"] {
        let config = Config::parse(&format!(r#"{{"host": "builder-1", "hosts": {hosts}, "repos": {{}}}}"#)).expect("usable for routing");
        assert_eq!(config.builders(), Err("hosts must be an array of strings"), "{hosts}");
    }
}

#[test]
fn a_bad_hosts_value_leaves_routing_alone() {
    for hosts in [r#"["z"]"#, r#"["a b"]"#, "[3]", "null", r#""b2""#] {
        let config = Config::parse(&format!(
            r#"{{"host": "builder-1", "hosts": {hosts}, "slots": 2, "repos": {{"g": ["x"]}}}}"#
        ))
        .expect("routing reads the file whatever hosts holds");
        assert_eq!(config.host(), "builder-1", "{hosts}");
        assert_eq!(config.slots(), 2, "{hosts}");
        assert_eq!(pattern_texts(&config, "g"), ["x"], "{hosts}");
        assert!(config.builders().is_err(), "the push still refuses {hosts}");
    }
}

#[test]
fn no_hosts_means_host_alone() {
    let host = ["builder-1".to_owned()];
    let alone = Ok(host.as_slice());
    let config = Config::parse(&host_file("builder-1")).expect("usable");
    assert_eq!(config.builders(), alone);
    let config = Config::parse(r#"{"host": "builder-1", "hosts": [], "repos": {}}"#).expect("usable");
    assert_eq!(config.builders(), alone, "an empty hosts adds none");
}

/// The routing file listing `g` with `siblings` holding `value`.
fn siblings_file(value: &str) -> Config {
    Config::parse(&format!(
        r#"{{"host": "builder-1", "slots": 2, "repos": {{"g": ["x"]}}, "siblings": {value}}}"#
    ))
    .expect("routing reads the file whatever siblings holds")
}

#[test]
fn siblings_gives_each_repo_s_allowed_names_as_written() {
    let config = siblings_file(r#"{"Gate": ["yaat-server", "B.x"], "other": []}"#);
    let allowed = |repo: &str| config.allowed_siblings(repo).map(<[String]>::to_vec);
    assert_eq!(
        allowed("gate"),
        Ok(vec!["yaat-server".to_owned(), "B.x".to_owned()]),
        "the repo name in any case"
    );
    assert_eq!(allowed("other"), Ok(Vec::new()), "an empty list");
    assert_eq!(allowed("missing"), Ok(Vec::new()), "a repo it does not name");
    let none = Config::parse(r#"{"host": "builder-1", "repos": {}}"#).expect("usable");
    assert_eq!(none.allowed_siblings("gate"), Ok(&[][..]), "no siblings key allows none");
    let keyed = Config::parse(r#"{"host": "builder-1", "repos": {}, "SIBLINGS": {"gate": ["b"]}}"#).expect("usable");
    assert_eq!(keyed.allowed_siblings("gate"), Ok(&["b".to_owned()][..]), "the key in any case");
}

#[test]
fn a_bad_siblings_value_allows_none_and_leaves_routing_alone() {
    for (value, problem) in [
        (r#"["b"]"#, "siblings must be a JSON object"),
        ("null", "siblings must be a JSON object"),
        (r#""b""#, "siblings must be a JSON object"),
        (r#"{"g": "b"}"#, r#"siblings."g" is not an array of strings"#),
        (r#"{"g": {}}"#, r#"siblings."g" is not an array of strings"#),
        (r#"{"g": ["b", ""]}"#, r#"siblings."g"[1] is not a non-empty string"#),
        (r#"{"g": [3]}"#, r#"siblings."g"[0] is not a non-empty string"#),
        (r#"{"other": [null], "g": ["b"]}"#, r#"siblings."other"[0] is not a non-empty string"#),
        (
            r#"{"g": ["b", "x\\y"]}"#,
            r#"siblings."g"[1] is not a folder name the builder can lay out"#,
        ),
        (r#"{"g": [".."]}"#, r#"siblings."g"[0] is not a folder name the builder can lay out"#),
        (r#"{"\u001b[2J": 5}"#, r#"siblings."\u{1b}[2J" is not an array of strings"#),
    ] {
        let config = siblings_file(value);
        assert_eq!(config.allowed_siblings("g"), Err(problem), "{value}");
        assert_eq!(config.host(), "builder-1", "{value}");
        assert_eq!(config.slots(), 2, "{value}");
        assert_eq!(pattern_texts(&config, "g"), ["x"], "{value}");
    }
}

/// The routing file listing `g` with `warmup` holding `value`.
fn warmup_file(value: &str) -> Config {
    Config::parse(&format!(
        r#"{{"host": "builder-1", "slots": 2, "repos": {{"g": ["x"]}}, "siblings": {{"g": ["b"]}}, "warmup": {value}}}"#
    ))
    .expect("routing reads the file whatever warmup holds")
}

#[test]
fn no_warmup_key_consents_to_no_warm_up() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"g": ["x"]}}"#).expect("usable");

    assert_eq!(config.warmup_consented("g"), Ok(false));
}

#[test]
fn warmup_consents_to_each_repo_it_lists_in_any_letter_case() {
    let config = warmup_file(r#"["Yaat", "gate"]"#);

    assert_eq!(config.warmup_consented("yaat"), Ok(true), "the repo name in any case");
    assert_eq!(config.warmup_consented("GATE"), Ok(true), "the repo name in any case");
    assert_eq!(config.warmup_consented("g"), Ok(false), "a repo it does not list");
    assert_eq!(warmup_file("[]").warmup_consented("g"), Ok(false), "an empty list");
    let keyed = Config::parse(r#"{"host": "builder-1", "repos": {}, "WarmUp": ["g"]}"#).expect("usable");
    assert_eq!(keyed.warmup_consented("g"), Ok(true), "the key in any case");
}

#[test]
fn a_bad_warmup_value_consents_to_none_and_leaves_the_rest_of_the_file_alone() {
    for (value, problem) in [
        (r#"{"g": true}"#, "warmup must be an array of repo names"),
        (r#""g""#, "warmup must be an array of repo names"),
        ("null", "warmup must be an array of repo names"),
        (r#"["g", ""]"#, "warmup[1] is not a non-empty string"),
        (r#"["g", 3]"#, "warmup[1] is not a non-empty string"),
        (r#"[["g"]]"#, "warmup[0] is not a non-empty string"),
    ] {
        let config = warmup_file(value);
        assert_eq!(config.warmup_consented("g"), Err(problem), "{value}");
        assert_eq!(config.host(), "builder-1", "{value}");
        assert_eq!(config.slots(), 2, "{value}");
        assert_eq!(pattern_texts(&config, "g"), ["x"], "{value}");
        assert_eq!(config.allowed_siblings("g"), Ok(&["b".to_owned()][..]), "{value}");
    }
}

/// The routing file listing `g` with `hooks` holding `value`.
fn hooks_file(value: &str) -> Config {
    Config::parse(&format!(
        r#"{{"host": "builder-1", "slots": 2, "repos": {{"g": ["x"]}}, "siblings": {{"g": ["b"]}}, "hooks": {value}}}"#
    ))
    .expect("routing reads the file whatever hooks holds")
}

#[test]
fn hooks_names_a_repo_in_any_letter_case() {
    let config = hooks_file(r#"["Yaat", "gate"]"#);

    assert_eq!(config.hooks_allowed("yaat"), Ok(true), "the repo name in any case");
    assert_eq!(config.hooks_allowed("GATE"), Ok(true), "the repo name in any case");
    assert_eq!(config.hooks_allowed("g"), Ok(false), "a repo it does not list");
    assert!(config.hooks_name_a_repo(), "the list names repos");
    let empty = hooks_file("[]");
    assert_eq!(empty.hooks_allowed("g"), Ok(false), "an empty list");
    assert!(!empty.hooks_name_a_repo(), "an empty list names no repo");
    let keyed = Config::parse(r#"{"host": "builder-1", "repos": {}, "Hooks": ["g"]}"#).expect("usable");
    assert_eq!(keyed.hooks_allowed("g"), Ok(true), "the key in any case");
}

#[test]
fn hooks_absent_consents_no_repo() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"g": ["x"]}}"#).expect("usable");

    assert_eq!(config.hooks_allowed("g"), Ok(false));
    assert!(!config.hooks_name_a_repo(), "no key names no repo");
}

/// The routing file listing `g` with `sendBuild` holding `value`.
fn send_build_file(value: &str) -> Config {
    Config::parse(&format!(
        r#"{{"host": "builder-1", "slots": 2, "repos": {{"g": ["x"]}}, "siblings": {{"g": ["b"]}}, "sendBuild": {value}}}"#
    ))
    .expect("routing reads the file whatever sendBuild holds")
}

#[test]
fn send_build_names_a_repo_in_any_letter_case() {
    let config = send_build_file(r#"["Yaat", "gate"]"#);

    assert_eq!(config.send_build_allowed("yaat"), Ok(true), "the repo name in any case");
    assert_eq!(config.send_build_allowed("GATE"), Ok(true), "the repo name in any case");
    assert_eq!(config.send_build_allowed("g"), Ok(false), "a repo it does not list");
    assert_eq!(send_build_file("[]").send_build_allowed("g"), Ok(false), "an empty list");
    let keyed = Config::parse(r#"{"host": "builder-1", "repos": {}, "SENDBUILD": ["g"]}"#).expect("usable");
    assert_eq!(keyed.send_build_allowed("g"), Ok(true), "the key in any case");
    let absent = Config::parse(r#"{"host": "builder-1", "repos": {"g": ["x"]}}"#).expect("usable");
    assert_eq!(absent.send_build_allowed("g"), Ok(false), "no key lets no repo send");
}

#[test]
fn an_unusable_send_build_value_lets_no_repo_send_and_keeps_the_rest_of_the_file() {
    for (value, problem) in [
        (r#"{"g": true}"#, "sendBuild must be an array of repo names"),
        (r#""g""#, "sendBuild must be an array of repo names"),
        ("null", "sendBuild must be an array of repo names"),
        (r#"["g", ""]"#, "sendBuild[1] is not a non-empty string"),
        (r#"["g", 3]"#, "sendBuild[1] is not a non-empty string"),
        (r#"[["g"]]"#, "sendBuild[0] is not a non-empty string"),
    ] {
        let config = send_build_file(value);
        assert_eq!(config.send_build_allowed("g"), Err(problem), "{value}");
        assert_eq!(config.host(), "builder-1", "{value}");
        assert_eq!(config.slots(), 2, "{value}");
        assert_eq!(pattern_texts(&config, "g"), ["x"], "{value}");
        assert_eq!(config.allowed_siblings("g"), Ok(&["b".to_owned()][..]), "{value}");
    }
}

/// The routing file listing `g` with `declarations` holding `value`.
fn declarations_file(value: &str) -> Config {
    Config::parse(&format!(
        r#"{{"host": "builder-1", "slots": 2, "repos": {{"g": ["x"]}}, "siblings": {{"g": ["b"]}}, "declarations": {value}}}"#
    ))
    .expect("routing reads the file whatever declarations holds")
}

#[test]
fn declarations_absent_names_no_folder() {
    let config = Config::parse(r#"{"host": "builder-1", "repos": {"g": ["x"]}}"#).expect("usable");

    assert_eq!(config.declarations_folder("g"), Ok(None));
}

#[test]
fn declarations_names_a_repo_s_folder_in_any_letter_case() {
    let config = declarations_file(r#"{"UE-mcp": "D:\\notes\\ue-mcp\\gate"}"#);

    assert_eq!(
        config.declarations_folder("ue-mcp"),
        Ok(Some(r"D:\notes\ue-mcp\gate")),
        "the repo name in any case"
    );
    assert_eq!(config.declarations_folder("g"), Ok(None), "a repo it does not name");
    let keyed = Config::parse(r#"{"host": "builder-1", "repos": {}, "Declarations": {"g": "relative"}}"#).expect("usable");
    assert_eq!(
        keyed.declarations_folder("G"),
        Ok(Some("relative")),
        "the key in any case, the folder kept as written"
    );
}

#[test]
fn an_unusable_declarations_value_names_no_folder_and_keeps_the_rest_of_the_file() {
    for (value, problem) in [
        (r#"["g"]"#, "declarations must be a JSON object"),
        (r#""D:\\x""#, "declarations must be a JSON object"),
        ("null", "declarations must be a JSON object"),
        (r#"{"g": 3}"#, r#"declarations."g" is not a non-empty string"#),
        (r#"{"g": ["D:\\x"]}"#, r#"declarations."g" is not a non-empty string"#),
        (r#"{"h": "D:\\x", "g": ""}"#, r#"declarations."g" is not a non-empty string"#),
    ] {
        let config = declarations_file(value);
        assert_eq!(config.declarations_folder("g"), Err(problem), "{value}");
        assert_eq!(config.host(), "builder-1", "{value}");
        assert_eq!(config.slots(), 2, "{value}");
        assert_eq!(pattern_texts(&config, "g"), ["x"], "{value}");
        assert_eq!(config.allowed_siblings("g"), Ok(&["b".to_owned()][..]), "{value}");
    }
}

#[test]
fn an_unusable_hooks_value_consents_no_repo_and_keeps_the_rest_of_the_file() {
    for (value, problem) in [
        (r#"{"g": true}"#, "hooks must be an array of repo names"),
        (r#""g""#, "hooks must be an array of repo names"),
        ("null", "hooks must be an array of repo names"),
        (r#"["g", ""]"#, "hooks[1] is not a non-empty string"),
        (r#"["g", 3]"#, "hooks[1] is not a non-empty string"),
        (r#"[["g"]]"#, "hooks[0] is not a non-empty string"),
    ] {
        let config = hooks_file(value);
        assert_eq!(config.hooks_allowed("g"), Err(problem), "{value}");
        assert!(!config.hooks_name_a_repo(), "{value}");
        assert_eq!(config.host(), "builder-1", "{value}");
        assert_eq!(config.slots(), 2, "{value}");
        assert_eq!(pattern_texts(&config, "g"), ["x"], "{value}");
        assert_eq!(config.allowed_siblings("g"), Ok(&["b".to_owned()][..]), "{value}");
    }
}

/// The routing file listing `g` with `desktop` holding `value`.
fn desktop_file(value: &str) -> Config {
    Config::parse(&format!(
        r#"{{"host": "builder-1", "slots": 2, "repos": {{"g": ["x"]}}, "siblings": {{"g": ["b"]}}, "warmup": ["g"], "desktop": {value}}}"#
    ))
    .expect("routing reads the file whatever desktop holds")
}

#[test]
fn desktop_gives_each_repo_s_allowed_patterns_as_written() {
    let config = desktop_file(r#"{"Gate": ["dotnet test *", "Tool*"], "other": []}"#);
    let allowed = |repo: &str| config.allowed_desktop(repo).map(<[String]>::to_vec);
    assert_eq!(
        allowed("gate"),
        Ok(vec!["dotnet test *".to_owned(), "Tool*".to_owned()]),
        "the repo name in any case"
    );
    assert_eq!(allowed("other"), Ok(Vec::new()), "an empty list");
    assert_eq!(allowed("missing"), Ok(Vec::new()), "a repo it does not name consents to none");
    let none = Config::parse(r#"{"host": "builder-1", "repos": {}}"#).expect("usable");
    assert_eq!(none.allowed_desktop("gate"), Ok(&[][..]), "no desktop key consents to none");
    let keyed = Config::parse(r#"{"host": "builder-1", "repos": {}, "DeskTop": {"gate": ["x*"]}}"#).expect("usable");
    assert_eq!(keyed.allowed_desktop("gate"), Ok(&["x*".to_owned()][..]), "the key in any case");
}

#[test]
fn a_bad_desktop_value_consents_to_none_and_leaves_the_rest_of_the_file_alone() {
    for (value, problem) in [
        (r#"["x"]"#, "desktop must be a JSON object"),
        ("null", "desktop must be a JSON object"),
        (r#""x""#, "desktop must be a JSON object"),
        (r#"{"g": "x"}"#, r#"desktop."g" is not an array of strings"#),
        (r#"{"g": {}}"#, r#"desktop."g" is not an array of strings"#),
        (r#"{"g": ["x", ""]}"#, r#"desktop."g"[1] is not a non-empty string"#),
        (r#"{"g": [3]}"#, r#"desktop."g"[0] is not a non-empty string"#),
        (r#"{"other": [null], "g": ["x"]}"#, r#"desktop."other"[0] is not a non-empty string"#),
    ] {
        let config = desktop_file(value);
        assert_eq!(config.allowed_desktop("g"), Err(problem), "{value}");
        assert_eq!(config.host(), "builder-1", "{value}");
        assert_eq!(config.slots(), 2, "{value}");
        assert_eq!(pattern_texts(&config, "g"), ["x"], "{value}");
        assert_eq!(config.allowed_siblings("g"), Ok(&["b".to_owned()][..]), "{value}");
        assert_eq!(config.warmup_consented("g"), Ok(true), "{value}");
    }
}

#[test]
fn an_ssh_alias_or_user_at_host_of_allowed_characters_is_accepted() {
    for host in ["builder", "build-box.lan", "user@build-box", "10.0.0.5", "b_1"] {
        let config = Config::parse(&host_file(host)).expect("usable");
        assert_eq!(config.host(), host);
    }
}

#[test]
fn a_builders_alias_that_repeats_a_hosts_entry_refuses_the_file() {
    let text = r#"{"host": "builder-1", "hosts": [3, "b-2", "Mac-1"], "repos": {},
        "builders": [{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"g": []}}]}"#;
    assert_eq!(reason(text), "builders[0].host repeats hosts[2]");
}

#[test]
fn a_macos_root_of_letters_digits_dots_underscores_and_dashes_is_accepted() {
    let config = Config::parse(&builders_file(
        r#"[{"host": "mac-1", "os": "macos", "root": "/Users/me_2/gate-remote.d", "repos": {"gate": []}}]"#,
    ))
    .expect("usable");
    assert_eq!(
        config.builder("mac-1").and_then(|builder| builder.root.as_deref()),
        Some("/Users/me_2/gate-remote.d")
    );
}

#[test]
fn a_repo_listed_only_in_a_builders_entry_is_routed() {
    let config = Config::parse(&builders_file(
        r#"[{"host": "mac-1", "os": "macos", "root": "/m", "repos": {"Solo": ["*"]}}]"#,
    ))
    .expect("usable");
    assert!(config.is_routed("solo"));
    assert!(config.is_routed("gate"), "the top-level repos");
    assert!(!config.is_routed("other"));
}

#[test]
fn a_builder_entry_marked_not_routed_is_read_with_no_repos() {
    for entry in [
        r#"[{"host": "mac-1", "os": "macos", "root": "/m", "routed": false}]"#,
        r#"[{"host": "mac-1", "os": "macos", "root": "/m", "Routed": false, "repos": {}}]"#,
    ] {
        let config = Config::parse(&builders_file(entry)).expect("usable");
        let mac = config.builder("MAC-1").expect("a not-routed entry is still a builder");
        assert_eq!(
            (mac.alias.as_str(), mac.os, mac.root.as_deref()),
            ("mac-1", BuilderOs::Macos, Some("/m")),
            "{entry}"
        );
        assert_eq!(config.builder_entries().count(), 1, "{entry}");
    }
    let routed = Config::parse(&builders_file(r#"[{"host": "mac-1", "os": "macos", "root": "/m", "routed": true}]"#));
    assert_eq!(
        routed.err(),
        Some("builders[0].repos is missing".to_owned()),
        "a routed entry still needs its repos"
    );
}

#[test]
fn a_not_routed_builder_is_not_among_the_routes() {
    let config = Config::parse(&builders_file(
        r#"[{"host": "mac-1", "os": "macos", "root": "/m", "routed": false, "repos": {"Solo": ["*"]}},
            {"host": "mac-2", "os": "macos", "root": "/n", "routed": true, "repos": {"pair": ["*"]}}]"#,
    ))
    .expect("usable");
    assert!(!config.is_routed("solo"), "only the not-routed entry lists it");
    assert!(config.is_routed("pair"), "routed: true is the default's meaning");
    assert!(config.builder("mac-1").is_some());
}

#[test]
fn routed_must_be_a_boolean() {
    for value in ["\"false\"", "0", "null", "[]"] {
        let file = builders_file(&format!(
            r#"[{{"host": "mac-1", "os": "macos", "root": "/m", "routed": {value}, "repos": {{"gate": ["x"]}}}}]"#
        ));
        assert_eq!(reason(&file), "builders[0].routed must be true or false", "{value}");
    }
}

#[test]
fn a_repo_listed_only_by_a_not_routed_entry_is_listed_but_not_routed() {
    let config = Config::parse(&builders_file(
        r#"[{"host": "mac-1", "os": "macos", "root": "/m", "routed": false, "repos": {"Solo": ["*"]}}]"#,
    ))
    .expect("usable");
    assert!(config.lists("solo"), "a not-routed entry's repos list it");
    assert!(!config.is_routed("solo"));
    assert!(config.lists("gate"), "the top-level repos list it");
    assert!(!config.lists("other"));
}

#[test]
fn a_pin_to_a_builders_entry_is_found_beside_an_unusable_hosts_value() {
    let config = Config::parse(
        r#"{"host": "builder-1", "hosts": ["a b"], "repos": {"gate": ["x"]},
            "builders": [{"host": "mac-1", "os": "macos", "root": "/m", "routed": false}]}"#,
    )
    .expect("usable for routing");
    let found = |host: &str| config.named_builder(host).map(|builder| builder.map(|builder| builder.alias));
    assert_eq!(found("MAC-1"), Ok(Some("mac-1".to_owned())), "a builders entry");
    assert_eq!(found("Builder-1"), Ok(Some("builder-1".to_owned())), "host");
    assert_eq!(
        found("win-7"),
        Err("hosts entries must each be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'"),
        "a name found nowhere else is refused for why hosts cannot be used"
    );
}

/// A Windows entry's root reaches the payload, the push URL, git's pack command and the `PathMap` word as written, so
/// it is held to the names the builder lays out, joined by `\`: a character pwsh, cmd or `MSBuild` reads, a `/`, or a
/// device name refuses the file.
#[test]
fn a_windows_root_is_held_to_layout_names_joined_by_backslashes() {
    let entry = |root: &str| {
        let root = serde_json::to_string(root).expect("a root can be written as JSON");
        builders_file(&format!(
            r#"[{{"host": "win-2", "os": "windows", "root": {root}, "repos": {{"gate": []}}}}]"#
        ))
    };
    for root in [
        r"D:\x&y", r"D:\it's", r"D:\a;b", r"D:\a,b", "D:/a", r"D:\con", r"D:\a=b", r"D:\a b", r"D:\a.", r"D:\100%", r"D:\a^b", r"D:\.x",
    ] {
        assert_eq!(
            reason(&entry(root)),
            "builders[0].root must be an absolute drive path for a windows builder",
            "{root}"
        );
    }
    for root in [r"D:\gate-remote", r"E:\build_2"] {
        let config = Config::parse(&entry(root)).expect("usable");
        assert_eq!(config.builder("win-2").and_then(|builder| builder.root.as_deref()), Some(root));
    }
}

/// A routing file whose `host` is `builder-1`, with `builders` holding `builders`.
fn builders_file(builders: &str) -> String {
    format!(r#"{{"host": "builder-1", "slots": 2, "repos": {{"gate": ["x"]}}, "builders": {builders}}}"#)
}

#[test]
fn a_pool_id_is_the_first_four_bytes_of_the_alias_hash() {
    assert_eq!(builder_id("mac-1"), "1f0efc62", "the first 4 bytes of the SHA-256, as 8 hex digits");
    assert_eq!(builder_id("MAC-1"), "1f0efc62", "the alias is lower-cased before the hash");
    let config = Config::parse(&builders_file(
        r#"[{"host": "MAC-1", "os": "macos", "root": "/Users/me/gate", "routed": false}]"#,
    ))
    .expect("usable");
    let entry = config.builder_entries().next().expect("the file names one entry");
    assert_eq!(
        entry.id.as_deref(),
        Some(builder_id(&entry.alias).as_str()),
        "an entry's id is the same function's, over its alias as spelled"
    );
}

#[test]
fn a_builders_entry_parses_a_macos_builder_with_its_root_and_a_windows_one_with_the_default_root() {
    let config = Config::parse(&builders_file(
        r#"[{"Host": "mac-1", "OS": "macos", "Root": "/Users/me/gate", "Slots": 3, "Repos": {"gate": ["cargo *"]}},
            {"host": "win-2", "os": "Windows", "repos": {"other": []}},
            {"host": "win-3", "os": "windows", "root": "E:\\gate", "repos": {"gate": []}}]"#,
    ))
    .expect("usable");
    let mac = Builder {
        alias: "mac-1".to_owned(),
        os: BuilderOs::Macos,
        root: Some("/Users/me/gate".to_owned()),
        slots: 3,
        id: Some("1f0efc62".to_owned()),
    };
    let win = Builder {
        alias: "win-2".to_owned(),
        os: BuilderOs::Windows,
        root: None,
        slots: 1,
        id: Some("084c286b".to_owned()),
    };
    let entries: Vec<&Builder> = config.builder_entries().collect();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0], &mac);
    assert_eq!(entries[1], &win);
    assert_eq!(entries[2].root.as_deref(), Some(r"E:\gate"));
    assert_eq!(config.builder("MAC-1"), Some(&mac), "found in any ASCII letter case");
    let host = config.builder("builder-1").expect("host is a builder");
    assert_eq!(
        (host.alias.as_str(), host.os, host.root.as_deref(), host.slots, host.id.as_deref()),
        ("builder-1", BuilderOs::Windows, None, 2, None)
    );
    assert_eq!(config.builder("nobody"), None);
    assert_eq!(config.host(), "builder-1", "host is unchanged");
    assert_eq!(config.slots(), 2, "the top-level slots are host's");
    assert_eq!(config.builders(), Ok(&["builder-1".to_owned()][..]), "builders() stays host and hosts");
}

#[test]
fn a_bad_builders_entry_makes_the_whole_file_unusable() {
    let entry = |fields: &str| format!(r#"[{{"host": "mac-1", "os": "macos", {fields}}}]"#);
    let repos = r#""repos": {"gate": ["x"]}"#;
    let macos_root = "builders[0].root must be an absolute path for a macos builder";
    let windows_root = "builders[0].root must be an absolute drive path for a windows builder";
    let slots = "builders[0].slots must be a whole number 1 to 16";
    let cases = [
        (r#"{"mac-1": {}}"#.to_owned(), "builders must be an array of objects"),
        ("[5]".to_owned(), "builders[0] must be a JSON object"),
        (entry(repos), macos_root),
        (entry(&format!(r#""root": "Users/me", {repos}"#)), macos_root),
        (entry(&format!(r#""root": "/Users\\me", {repos}"#)), macos_root),
        (entry(&format!(r#""root": "/Users/../me", {repos}"#)), macos_root),
        (entry(&format!(r#""root": "/Users/./me", {repos}"#)), macos_root),
        (entry(&format!(r#""root": "/", {repos}"#)), macos_root),
        (entry(&format!(r#""root": "/Users/me/", {repos}"#)), macos_root),
        (entry(&format!(r#""root": "/Users/m\te", {repos}"#)), macos_root),
        (entry(&format!(r#""root": 5, {repos}"#)), macos_root),
        (
            format!(r#"[{{"host": "w", "os": "windows", "root": "gate", {repos}}}]"#),
            "builders[0].host must not be a single letter, which a git host:path URL reads as a drive",
        ),
        (format!(r#"[{{"host": "win", "os": "windows", "root": "gate", {repos}}}]"#), windows_root),
        (
            format!(r#"[{{"host": "win", "os": "windows", "root": "D:\\..\\gate", {repos}}}]"#),
            windows_root,
        ),
        (format!(r#"[{{"host": "win", "os": "windows", "root": "D:\\", {repos}}}]"#), windows_root),
        (
            format!(r#"[{{"host": "Builder-1", "os": "windows", {repos}}}]"#),
            "builders[0].host repeats host",
        ),
        (
            format!(r#"[{{"host": "win", "os": "windows", {repos}}}, {{"host": "WIN", "os": "windows", {repos}}}]"#),
            "builders[1].host repeats builders[0].host",
        ),
        (entry(r#""root": "/m", "repos": {}"#), "builders[0].repos must name at least one repo"),
        (entry(r#""root": "/m""#), "builders[0].repos is missing"),
        (entry(r#""root": "/m", "repos": []"#), "builders[0].repos must be a JSON object"),
        (
            entry(r#""root": "/m", "repos": {"gate": [""]}"#),
            "builders[0].repos.gate[0] is not a non-empty string",
        ),
        (
            entry(&format!(r#""root": "/m", {repos}, "outputs": {{"gate": ["[x]"]}}"#)),
            "builders[0].outputs.gate[0] is not a valid output glob: '[' and ']' are not allowed",
        ),
        (entry(&format!(r#""root": "/m", "slots": 0, {repos}"#)), slots),
        (entry(&format!(r#""root": "/m", "slots": 17, {repos}"#)), slots),
        (
            format!(r#"[{{"host": "mac-1", "os": "linux", "root": "/m", {repos}}}]"#),
            r#"builders[0].os must be "windows" or "macos""#,
        ),
        (format!(r#"[{{"host": "mac-1", "root": "/m", {repos}}}]"#), "builders[0].os is missing"),
        (format!(r#"[{{"os": "macos", "root": "/m", {repos}}}]"#), "builders[0].host is missing"),
        (
            format!(r#"[{{"host": "x;y", "os": "macos", "root": "/m", {repos}}}]"#),
            "builders[0].host must be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'",
        ),
        (entry(&format!(r#""root": "/Users/me gate", {repos}"#)), macos_root),
        (entry(&format!(r#""root": "/Users/me\"gate", {repos}"#)), macos_root),
        (entry(&format!(r#""root": "/Users/$HOME", {repos}"#)), macos_root),
    ];
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|(builders, expected)| {
            let got = Config::parse(&builders_file(builders)).map(|_| ());
            (got != Err((*expected).to_owned())).then(|| format!("{builders}: expected {expected:?}, got {got:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
    assert_eq!(
        ignored_line(Path::new("remote.json"), &reason(&builders_file(&entry(repos)))),
        format!("gate: ignored remote.json: {macos_root}; running locally")
    );
}
