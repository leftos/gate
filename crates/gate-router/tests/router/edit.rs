//! `gate remote`'s edit of the routing file's text: what changes, what is kept, and what refuses the edit.

use gate_router::{Config, Edit, EditRefusal, EditVerb, Edited, edit_remote_json};
use serde_json::Value;

/// A routing file laid out as the rewrite lays it out, in 4 spaces and LF.
const BASE: &str = r#"{
    "host": "builder@example",
    "repos": {
        "gate": [
            "cargo test*"
        ],
        "other": [
            "dotnet test*"
        ]
    },
    "outputs": {
        "gate": [
            "target/**"
        ]
    },
    "slots": 2
}
"#;

/// [`BASE`] with `cargo build*` added to `repos.gate`.
const BASE_ADDED: &str = r#"{
    "host": "builder@example",
    "repos": {
        "gate": [
            "cargo test*",
            "cargo build*"
        ],
        "other": [
            "dotnet test*"
        ]
    },
    "outputs": {
        "gate": [
            "target/**"
        ]
    },
    "slots": 2
}
"#;

/// A file with two `builders` entries.
const BUILDERS: &str = r#"{
    "host": "builder@example",
    "repos": {"gate": ["cargo test*"]},
    "builders": [
        {"host": "one@example", "os": "windows", "repos": {"gate": ["x*"]}, "outputs": {"gate": ["bin/**"]}},
        {"host": "two@example", "os": "windows", "repos": {"gate": ["y*"]}, "outputs": {"gate": ["bin/**"]}}
    ]
}
"#;

/// [`BUILDERS`] with its second entry also listing `other`.
const BUILDERS_TWO_REPOS: &str = r#"{
    "host": "builder@example",
    "repos": {"gate": ["cargo test*"]},
    "builders": [
        {"host": "one@example", "os": "windows", "repos": {"gate": ["x*"]}},
        {"host": "two@example", "os": "windows", "repos": {"gate": ["y*"], "other": ["z*"]}}
    ]
}
"#;

/// A routing file whose top-level `repos` holds an empty list, as an older gate left it.
const EMPTY_LIST: &str = r#"{
    "host": "builder@example",
    "repos": {
        "gate": [],
        "other": [
            "dotnet test*"
        ]
    }
}
"#;

/// A routing file whose top-level `repos` names one repo only.
const ONE_REPO: &str = r#"{
    "host": "builder@example",
    "repos": {
        "gate": [
            "cargo test*"
        ]
    }
}
"#;

/// A file whose top-level `repos` and first builder list `gate`, and whose second builder lists only `other`.
const NEW_BUILDER_LIST: &str = r#"{
    "host": "builder@example",
    "repos": {"gate": ["cargo test*", "cargo build*"]},
    "builders": [
        {"host": "one@example", "os": "windows", "repos": {"gate": ["x*"]}},
        {"host": "two@example", "os": "windows", "repos": {"other": ["z*"]}}
    ]
}
"#;

fn edit(text: &str, verb: EditVerb, section: &str, builder: Option<&str>, repo: &str, patterns: &[&str]) -> Result<Edited, EditRefusal> {
    let patterns: Vec<String> = patterns.iter().map(|pattern| (*pattern).to_owned()).collect();
    edit_remote_json(
        text,
        &Edit {
            verb,
            section,
            builder,
            repo,
            patterns: &patterns,
        },
    )
}

fn add(text: &str, section: &str, repo: &str, patterns: &[&str]) -> Result<Edited, EditRefusal> {
    edit(text, EditVerb::Add, section, None, repo, patterns)
}

fn remove(text: &str, section: &str, repo: &str, patterns: &[&str]) -> Result<Edited, EditRefusal> {
    edit(text, EditVerb::Remove, section, None, repo, patterns)
}

fn value(text: &str) -> Value {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text)).expect("the text is JSON")
}

fn keys(value: &Value) -> Vec<String> {
    value.as_object().expect("an object").keys().cloned().collect()
}

/// `text` laid out in `indent` per level in place of 4 spaces, and `eol` in place of LF.
fn laid_out(text: &str, indent: &str, eol: &str) -> String {
    text.replace("    ", indent).replace('\n', eol)
}

#[test]
fn a_new_pattern_is_appended_and_the_file_rewritten_in_its_own_layout() {
    let edited = add(BASE, "repos", "gate", &["cargo build*"]).expect("the edit is made");
    assert!(edited.changed);
    assert_eq!(edited.lines, ["remote: repos.gate: + cargo build*"]);
    assert_eq!(edited.text, BASE_ADDED);
}

#[test]
fn a_pattern_already_present_is_unchanged_and_the_text_kept() {
    let edited = add(BASE, "repos", "gate", &["cargo test*"]).expect("nothing to do is no refusal");
    assert!(!edited.changed);
    assert_eq!(edited.lines, ["remote: repos.gate: unchanged (already has cargo test*)"]);
    assert_eq!(edited.text, BASE);
}

#[test]
fn a_present_pattern_is_removed() {
    let edited = remove(BASE_ADDED, "repos", "gate", &["cargo build*"]).expect("the edit is made");
    assert!(edited.changed);
    assert_eq!(edited.lines, ["remote: repos.gate: - cargo build*"]);
    assert_eq!(edited.text, BASE);
}

#[test]
fn an_absent_pattern_or_repo_is_reported_absent_and_the_text_kept() {
    let edited = remove(BASE, "repos", "gate", &["cargo bench*"]).expect("nothing to do is no refusal");
    assert!(!edited.changed);
    assert_eq!(edited.lines, ["remote: repos.gate: absent (no cargo bench*)"]);
    assert_eq!(edited.text, BASE);

    let edited = remove(BASE, "inputs", "nowhere", &["a/**"]).expect("nothing to do is no refusal");
    assert!(!edited.changed);
    assert_eq!(edited.lines, ["remote: inputs.nowhere: absent (no a/**)"]);
    assert_eq!(edited.text, BASE);
}

#[test]
fn removing_the_last_repos_pattern_deletes_the_repo_key() {
    let edited = remove(BASE, "repos", "other", &["dotnet test*"]).expect("the edit is made");
    assert_eq!(
        edited.lines,
        ["remote: repos.other: - dotnet test*", "remote: repos.other: removed the empty list"]
    );
    assert_eq!(keys(&value(&edited.text)["repos"]), ["gate"]);
}

#[test]
fn removing_from_an_already_empty_repos_list_deletes_the_key() {
    let edited = remove(EMPTY_LIST, "repos", "gate", &["cargo test*"]).expect("the edit is made");
    assert!(edited.changed);
    assert_eq!(
        edited.lines,
        [
            "remote: repos.gate: absent (no cargo test*)",
            "remote: repos.gate: removed the empty list"
        ]
    );
    assert_eq!(keys(&value(&edited.text)["repos"]), ["other"]);
}

#[test]
fn removing_the_last_outputs_pattern_keeps_an_empty_list() {
    let edited = remove(BASE, "outputs", "gate", &["target/**"]).expect("the edit is made");
    assert_eq!(edited.lines, ["remote: outputs.gate: - target/**"]);
    assert_eq!(value(&edited.text)["outputs"]["gate"], Value::Array(Vec::new()));
}

#[test]
fn removing_the_last_repos_key_of_the_file_leaves_an_empty_section() {
    let edited = remove(ONE_REPO, "repos", "gate", &["cargo test*"]).expect("the edit is made");
    assert_eq!(
        edited.lines,
        ["remote: repos.gate: - cargo test*", "remote: repos.gate: removed the empty list"]
    );
    assert_eq!(value(&edited.text)["repos"], serde_json::json!({}));
}

#[test]
fn removing_the_last_repos_pattern_of_a_builder_deletes_its_key() {
    let edited = edit(BUILDERS_TWO_REPOS, EditVerb::Remove, "repos", Some("two@example"), "other", &["z*"]).expect("the edit is made");
    assert_eq!(
        edited.lines,
        [
            "remote: builders[1].repos.other: - z*",
            "remote: builders[1].repos.other: removed the empty list"
        ]
    );
    let after = value(&edited.text);
    assert_eq!(keys(&after["builders"][1]["repos"]), ["gate"]);
    assert_eq!(after["builders"][1]["repos"]["gate"], serde_json::json!(["y*"]));
}

#[test]
fn removing_the_only_repo_of_a_routed_builder_is_refused() {
    let refusal = edit(BUILDERS, EditVerb::Remove, "repos", Some("two@example"), "gate", &["y*"]).expect_err("refused");
    assert_eq!(
        refusal,
        EditRefusal::Guard("the result would be refused: builders[1].repos must name at least one repo".to_owned())
    );
}

#[test]
fn adding_a_builder_s_first_repos_pattern_warns_of_the_patterns_its_pins_stop_matching() {
    let edited = edit(NEW_BUILDER_LIST, EditVerb::Add, "repos", Some("two@example"), "gate", &["y*"]).expect("the edit is made");
    let warning = "remote: warning: builders[1].repos.gate is new, so a run pinned to this builder matches only its patterns, \
                   no longer the 3 in repos.gate, builders[0].repos.gate; to keep them, put the new pattern in the top-level \
                   repos and pin the command";
    assert_eq!(edited.lines, ["remote: builders[1].repos.gate: + y*", warning]);
    assert_eq!(value(&edited.text)["builders"][1]["repos"]["gate"], serde_json::json!(["y*"]));
}

#[test]
fn adding_to_a_builder_s_existing_repos_list_does_not_warn() {
    let edited = edit(BUILDERS, EditVerb::Add, "repos", Some("one@example"), "gate", &["w*"]).expect("the edit is made");
    assert_eq!(edited.lines, ["remote: builders[0].repos.gate: + w*"]);
}

#[test]
fn adding_a_builder_s_first_outputs_glob_does_not_warn() {
    let edited = edit(NEW_BUILDER_LIST, EditVerb::Add, "outputs", Some("two@example"), "gate", &["bin/**"]).expect("the edit is made");
    assert_eq!(edited.lines, ["remote: builders[1].outputs.gate: + bin/**"]);
}

#[test]
fn an_unknown_section_is_refused() {
    assert_eq!(add(BASE, "warmup", "gate", &["x"]), Err(EditRefusal::UnknownSection("warmup".to_owned())));
    let refusal = add(BASE, "nonsense", "gate", &["x"]).expect_err("refused");
    assert_eq!(
        refusal.to_string(),
        "unknown section nonsense; one of repos, first, outputs, failureOutputs, inputs, desktop, desktopSession, siblings, carriedFolders"
    );
}

#[test]
fn a_carried_folder_glob_is_added_and_a_bad_one_refused() {
    let edited = add(BASE, "carriedFolders", "gate", &[".tmp/mocks/*-src"]).expect("the edit is made");
    assert_eq!(edited.lines, ["remote: carriedFolders.gate: + .tmp/mocks/*-src"]);
    assert_eq!(value(&edited.text)["carriedFolders"]["gate"], serde_json::json!([".tmp/mocks/*-src"]));
    let removed = remove(&edited.text, "carriedFolders", "gate", &[".tmp/mocks/*-src"]).expect("the edit is made");
    assert_eq!(removed.lines, ["remote: carriedFolders.gate: - .tmp/mocks/*-src"]);

    for bad in ["/abs", r".tmp\mocks", "../x", "./x", "a//b", "**/x", "a/**"] {
        let refusal = add(BASE, "carriedFolders", "gate", &[bad]).expect_err(bad).to_string();
        assert!(
            refusal.starts_with(r#"the result has a problem the file did not have: carriedFolders."gate"[0] is not a folder glob"#),
            "{bad}: {refusal}"
        );
    }
    let refusal = add(BASE, "carriedFolders", "gate", &[""]).expect_err("an empty glob").to_string();
    assert_eq!(
        refusal,
        r#"the result has a problem the file did not have: carriedFolders."gate"[0] is not a non-empty string"#
    );
}

#[test]
fn a_missing_repo_is_appended_to_its_section_and_a_missing_section_to_the_file() {
    let edited = add(BASE, "repos", "new", &["make*"]).expect("the edit is made");
    let after = value(&edited.text);
    assert_eq!(keys(&after["repos"]), ["gate", "other", "new"]);
    assert_eq!(after["repos"]["new"], serde_json::json!(["make*"]));

    let edited = add(BASE, "inputs", "gate", &["fixtures/**"]).expect("the edit is made");
    let after = value(&edited.text);
    assert_eq!(keys(&after), ["host", "repos", "outputs", "slots", "inputs"]);
    assert_eq!(edited.lines, ["remote: inputs.gate: + fixtures/**"]);
}

#[test]
fn a_repo_named_in_another_letter_case_keeps_the_file_s_spelling() {
    let edited = add(BASE, "repos", "GATE", &["cargo build*"]).expect("the edit is made");
    assert_eq!(edited.lines, ["remote: repos.gate: + cargo build*"]);
    assert_eq!(edited.text, BASE_ADDED);
}

#[test]
fn a_result_the_router_would_refuse_is_refused() {
    let refusal = add(BASE, "outputs", "gate", &["bin/[ab]"]).expect_err("an output glob holding [ is refused");
    assert!(
        matches!(&refusal, EditRefusal::Guard(why) if why.starts_with("the result would be refused: ")),
        "{refusal:?}"
    );
}

#[test]
fn other_repos_and_other_keys_are_kept_as_values() {
    let edited = add(BASE, "repos", "gate", &["cargo build*"]).expect("the edit is made");
    let (before, after) = (value(BASE), value(&edited.text));
    for key in ["host", "outputs", "slots"] {
        assert_eq!(before[key], after[key], "{key}");
    }
    assert_eq!(before["repos"]["other"], after["repos"]["other"]);
}

#[test]
fn a_two_space_and_a_four_space_file_keep_their_indent_and_a_crlf_file_keeps_crlf() {
    for (indent, eol) in [("  ", "\n"), ("    ", "\n"), ("    ", "\r\n"), ("  ", "\r\n")] {
        let edited = add(&laid_out(BASE, indent, eol), "repos", "gate", &["cargo build*"]).expect("the edit is made");
        assert_eq!(edited.text, laid_out(BASE_ADDED, indent, eol), "indent {indent:?}, eol {eol:?}");
    }
}

#[test]
fn a_file_with_a_comment_or_a_trailing_comma_is_refused() {
    let commented = BASE.replace("    \"slots\": 2", "    // two at once\n    \"slots\": 2");
    Config::parse(&commented).expect("the router reads it");
    assert_eq!(add(&commented, "repos", "gate", &["cargo build*"]), Err(EditRefusal::Comments));

    let trailing = BASE.replace("\"slots\": 2", "\"slots\": 2,");
    Config::parse(&trailing).expect("the router reads it");
    assert_eq!(add(&trailing, "repos", "gate", &["cargo build*"]), Err(EditRefusal::Comments));
}

#[test]
fn a_file_the_router_refuses_is_refused_with_its_reason() {
    let refusal = add("{\"repos\": ", "repos", "gate", &["x"]).expect_err("refused");
    let reason = Config::parse("{\"repos\": ").map(|_| ()).expect_err("the router refuses it");
    assert_eq!(refusal, EditRefusal::Unusable(reason));
}

#[test]
fn a_builder_s_list_is_addressed_by_its_host_and_an_unknown_builder_refused() {
    let edited = edit(BUILDERS, EditVerb::Add, "outputs", Some("TWO@example"), "gate", &["obj/**"]).expect("the edit is made");
    assert_eq!(edited.lines, ["remote: builders[1].outputs.gate: + obj/**"]);
    let (before, after) = (value(BUILDERS), value(&edited.text));
    assert_eq!(after["builders"][1]["outputs"]["gate"], serde_json::json!(["bin/**", "obj/**"]));
    assert_eq!(before["builders"][0], after["builders"][0]);
    assert_eq!(before["repos"], after["repos"]);

    let refusal = edit(BUILDERS, EditVerb::Add, "outputs", Some("three@example"), "gate", &["obj/**"]).expect_err("refused");
    assert_eq!(refusal, EditRefusal::UnknownBuilder("three@example".to_owned()));
    assert_eq!(
        refusal.to_string(),
        "three@example is not a builders entry of remote.json; --builder edits builders[] entries only, and the top-level host's lists are \
         edited without it"
    );

    let refusal = edit(BUILDERS, EditVerb::Add, "outputs", Some("Builder@Example"), "gate", &["obj/**"]).expect_err("refused");
    assert_eq!(
        refusal.to_string(),
        "Builder@Example is not a builders entry of remote.json; --builder edits builders[] entries only, and the top-level host's lists \
         are edited without it",
        "the top-level host is no builders entry, and the line echoes the caller's own word"
    );

    let refusal = edit(BUILDERS, EditVerb::Add, "siblings", Some("one@example"), "gate", &["b"]).expect_err("refused");
    assert_eq!(refusal, EditRefusal::NotABuilderSection("siblings".to_owned()));
}

/// [`BASE`] with `first.gate` holding `pwsh *`, in 2 spaces.
const BASE_FIRST: &str = r#"{
  "host": "builder@example",
  "repos": {
    "gate": [
      "cargo test*"
    ],
    "other": [
      "dotnet test*"
    ]
  },
  "outputs": {
    "gate": [
      "target/**"
    ]
  },
  "slots": 2,
  "first": {
    "gate": [
      "pwsh *"
    ]
  }
}
"#;

#[test]
fn a_first_pattern_is_added_and_removed_at_the_top_level_in_the_file_s_own_indent() {
    let two_space = laid_out(BASE, "  ", "\n");
    let edited = add(&two_space, "first", "gate", &["pwsh *"]).expect("the edit is made");
    assert_eq!(edited.lines, ["remote: first.gate: + pwsh *"]);
    assert_eq!(edited.text, BASE_FIRST);

    let widened = add(BASE_FIRST, "first", "gate", &["cargo test*"]).expect("the edit is made");
    let edited = remove(&widened.text, "first", "gate", &["cargo test*"]).expect("the edit is made");
    assert_eq!(edited.lines, ["remote: first.gate: - cargo test*"]);
    assert_eq!(edited.text, BASE_FIRST, "only that pattern goes");
}

/// Two `builders` entries laid out as the rewrite lays them out, in 2 spaces, the second one's `first` listing
/// `{first}` for `gate` (no `first` key when `None`).
fn builders_laid_out(first: Option<&str>) -> String {
    let first = first.map_or_else(String::new, |list| format!(",\n      \"first\": {{\n        \"gate\": {list}\n      }}"));
    format!(
        r#"{{
  "host": "builder@example",
  "repos": {{
    "gate": [
      "cargo test*"
    ]
  }},
  "builders": [
    {{
      "host": "one@example",
      "os": "windows",
      "repos": {{
        "gate": [
          "x*"
        ]
      }}
    }},
    {{
      "host": "two@example",
      "os": "windows",
      "repos": {{
        "gate": [
          "y*"
        ]
      }}{first}
    }}
  ]
}}
"#
    )
}

#[test]
fn a_first_pattern_is_added_and_removed_in_a_builder_entry() {
    let base = builders_laid_out(None);
    let edited = edit(&base, EditVerb::Add, "first", Some("two@example"), "gate", &["y*"]).expect("the edit is made");
    assert_eq!(edited.lines, ["remote: builders[1].first.gate: + y*"]);
    assert_eq!(
        edited.text,
        builders_laid_out(Some("[\n          \"y*\"\n        ]")),
        "only the entry's new list, in the file's own indent"
    );

    let removed = edit(&edited.text, EditVerb::Remove, "first", Some("two@example"), "gate", &["y*"]).expect("the edit is made");
    assert_eq!(removed.lines, ["remote: builders[1].first.gate: - y*"]);
    assert_eq!(removed.text, builders_laid_out(Some("[]")), "the list is left empty");
}

/// A file holding every kind of JSON value outside the list an edit touches, laid out as the rewrite lays it out.
const WIDE: &str = r#"{
    "host": "builder@example",
    "repos": {
        "gate": [
            "cargo test*"
        ]
    },
    "notes": {
        "quote": "a\"b",
        "backslash": "c\\d",
        "control": "\u0001",
        "newline": "x\ny",
        "text": "Grüße ☃"
    },
    "flags": {
        "on": true,
        "off": false,
        "none": null,
        "count": 42,
        "negative": -7,
        "nested": {
            "deep": {
                "list": [
                    1,
                    2
                ]
            }
        }
    },
    "emptyList": [],
    "emptyObject": {}
}
"#;

/// `text` with `cargo build*` after its one `cargo test*` line.
fn with_build_added(text: &str) -> String {
    assert_eq!(text.matches("\"cargo test*\"").count(), 1, "one line to add after");
    text.replace("\"cargo test*\"", "\"cargo test*\",\n            \"cargo build*\"")
}

#[test]
fn every_kind_of_value_outside_the_list_reads_back_equal_and_is_written_as_it_stood() {
    let edited = add(WIDE, "repos", "gate", &["cargo build*"]).expect("the edit is made");
    assert_eq!(edited.text, with_build_added(WIDE));
    let (before, after) = (value(WIDE), value(&edited.text));
    for key in ["host", "notes", "flags", "emptyList", "emptyObject"] {
        assert_eq!(before[key], after[key], "{key}");
    }
    assert_eq!(after["notes"]["control"], Value::String("\u{1}".to_owned()));
    assert_eq!(after["notes"]["text"], Value::String("Grüße ☃".to_owned()));
}

#[test]
fn a_byte_order_mark_is_kept() {
    let marked = format!("\u{feff}{BASE}");
    let edited = add(&marked, "repos", "gate", &["cargo build*"]).expect("the edit is made");
    assert_eq!(edited.text, format!("\u{feff}{BASE_ADDED}"));
}

#[test]
fn a_tab_indented_file_keeps_its_tabs() {
    let edited = add(&laid_out(BASE, "\t", "\n"), "repos", "gate", &["cargo build*"]).expect("the edit is made");
    assert_eq!(edited.text, laid_out(BASE_ADDED, "\t", "\n"));
}

#[test]
fn a_soft_problem_that_predates_the_edit_does_not_refuse_it_and_one_it_brings_does() {
    let bad_warmup = BASE.replace("    \"slots\": 2", "    \"slots\": 2,\n    \"warmup\": 5");
    Config::parse(&bad_warmup).expect("the router still reads it");
    let edited = add(&bad_warmup, "repos", "gate", &["cargo build*"]).expect("a problem the file had does not refuse the edit");
    assert!(edited.changed);

    let refusal = add(BASE, "siblings", "gate", &["bad/name"]).expect_err("an invalid sibling name is refused");
    assert!(
        matches!(&refusal, EditRefusal::Guard(why) if why.starts_with("the result has a problem the file did not have: ")),
        "{refusal:?}"
    );
}
