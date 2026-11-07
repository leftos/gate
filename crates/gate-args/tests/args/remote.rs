//! `gate remote add|remove`: the verb, section, repo and patterns, `--builder`, `--tag` and `--dry-run` anywhere, each
//! usage error, `--help`, and the usage line; and `gate remote match`'s two operands.

use gate_args::{Invocation, RemoteMatchRequest, RemoteRequest, RemoteVerb, USAGE};

use crate::support::{invocation, problems};

/// The request of `verb` on `section`, `repo` and `patterns`, with no option.
fn remote(verb: RemoteVerb, section: &str, repo: &str, patterns: &[&str]) -> RemoteRequest {
    RemoteRequest {
        verb,
        section: section.to_owned(),
        repo: repo.to_owned(),
        patterns: patterns.iter().map(|pattern| (*pattern).to_owned()).collect(),
        builder: None,
        tag: None,
        dry_run: false,
    }
}

#[test]
fn remote_add_and_remove_take_several_patterns() {
    assert_eq!(
        invocation(&["remote", "add", "repos", "gate", "cargo test*", "cargo build*"]),
        Invocation::Remote(remote(RemoteVerb::Add, "repos", "gate", &["cargo test*", "cargo build*"]))
    );
    assert_eq!(
        invocation(&["remote", "remove", "outputs", "gate", "target/**", "bin/**"]),
        Invocation::Remote(remote(RemoteVerb::Remove, "outputs", "gate", &["target/**", "bin/**"]))
    );
}

#[test]
fn remote_options_are_read_in_any_order_before_or_after_the_positionals() {
    let expected = Invocation::Remote(RemoteRequest {
        builder: Some("one@example".to_owned()),
        tag: Some("census".to_owned()),
        dry_run: true,
        ..remote(RemoteVerb::Add, "outputs", "gate", &["bin/**", "obj/**"])
    });
    let orders: [&[&str]; 4] = [
        &[
            "remote",
            "add",
            "outputs",
            "gate",
            "bin/**",
            "obj/**",
            "--builder",
            "one@example",
            "--tag",
            "census",
            "--dry-run",
        ],
        &[
            "remote",
            "--dry-run",
            "--tag=census",
            "--builder=one@example",
            "add",
            "outputs",
            "gate",
            "bin/**",
            "obj/**",
        ],
        &[
            "remote",
            "add",
            "--builder",
            "one@example",
            "outputs",
            "--dry-run",
            "gate",
            "bin/**",
            "--tag",
            "census",
            "obj/**",
        ],
        &[
            "remote",
            "--tag",
            "census",
            "add",
            "outputs",
            "gate",
            "--dry-run",
            "bin/**",
            "obj/**",
            "--builder",
            "one@example",
        ],
    ];
    for words in orders {
        assert_eq!(invocation(words), expected, "{words:?}");
    }
}

#[test]
fn remote_after_a_separator_every_word_is_a_positional() {
    assert_eq!(
        invocation(&["remote", "add", "repos", "gate", "--", "--dry-run"]),
        Invocation::Remote(remote(RemoteVerb::Add, "repos", "gate", &["--dry-run"]))
    );
}

#[test]
fn remote_missing_words_are_usage_errors() {
    assert_eq!(
        problems(&["remote"]),
        ["gate: remote: missing the verb add, remove or match", "gate: remote: missing the section"]
    );
    assert_eq!(problems(&["remote", "add"]), ["gate: remote: missing the section"]);
    assert_eq!(problems(&["remote", "add", "repos"]), ["gate: remote: missing the repo"]);
    assert_eq!(problems(&["remote", "remove", "repos", "gate"]), ["gate: remote: missing a pattern"]);
}

#[test]
fn remote_unknown_verbs_sections_and_options_are_usage_errors() {
    assert_eq!(
        problems(&["remote", "show", "repos", "gate", "x"]),
        ["gate: remote: unknown verb show; add, remove or match"]
    );
    assert_eq!(
        problems(&["remote", "add", "warmup", "gate", "x"]),
        [concat!(
            "gate: remote: unknown section warmup; one of repos, first, outputs, failureOutputs, inputs, desktop, ",
            "desktopSession, siblings, carriedFolders"
        )]
    );
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--force"]),
        ["gate: unknown option --force"]
    );
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--dry-run=yes"]),
        ["gate: unknown option --dry-run=yes"]
    );
}

#[test]
fn remote_builder_takes_only_a_section_a_builder_has() {
    assert_eq!(
        problems(&["remote", "add", "siblings", "gate", "b", "--builder", "one@example"]),
        ["gate: remote: a builder has no section siblings; with --builder, one of repos, first, outputs, failureOutputs"]
    );
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--builder"]),
        ["gate: --builder needs a value"]
    );
}

#[test]
fn remote_add_and_remove_take_the_first_section_with_and_without_a_builder() {
    assert_eq!(
        invocation(&["remote", "add", "first", "gate", "pwsh *"]),
        Invocation::Remote(remote(RemoteVerb::Add, "first", "gate", &["pwsh *"]))
    );
    assert_eq!(
        invocation(&["remote", "remove", "first", "gate", "pwsh *", "--builder", "mac@example"]),
        Invocation::Remote(RemoteRequest {
            builder: Some("mac@example".to_owned()),
            ..remote(RemoteVerb::Remove, "first", "gate", &["pwsh *"])
        })
    );
}

#[test]
fn remote_an_option_word_is_never_an_option_s_value() {
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--tag", "--dry-run"]),
        ["gate: --tag needs a value"]
    );
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--builder", "--dry-run"]),
        ["gate: --builder needs a value"]
    );
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--tag", "--"]),
        ["gate: --tag needs a value"]
    );
}

#[test]
fn remote_tag_is_for_add_only_and_one_safe_word() {
    assert_eq!(
        problems(&["remote", "remove", "repos", "gate", "x", "--tag", "census"]),
        ["gate: remote: --tag is for add only; remove makes no backup to name"]
    );
    for bad in ["has space", "dot.ted", "slash/ed", &"a".repeat(41)] {
        assert_eq!(
            problems(&["remote", "add", "repos", "gate", "x", "--tag", bad]),
            [format!(
                "gate: remote: --tag must be 1 to 40 ASCII letters, digits, '_' and '-', got '{bad}'"
            )],
            "{bad}"
        );
    }
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--tag="]),
        ["gate: --tag needs a value"]
    );
    let longest = "A-z_9".repeat(8);
    assert_eq!(
        invocation(&["remote", "add", "repos", "gate", "x", "--tag", &longest]),
        Invocation::Remote(RemoteRequest {
            tag: Some(longest.clone()),
            ..remote(RemoteVerb::Add, "repos", "gate", &["x"])
        })
    );
}

#[test]
fn remote_an_option_given_twice_is_a_usage_error() {
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--dry-run", "--dry-run"]),
        ["gate: --dry-run is given twice"]
    );
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--builder", "a@example", "--builder=b@example"]),
        ["gate: --builder is given twice"]
    );
    assert_eq!(
        problems(&["remote", "add", "repos", "gate", "x", "--tag", "a", "--tag", "b"]),
        ["gate: --tag is given twice"]
    );
}

#[test]
fn remote_match_takes_a_repo_and_a_text() {
    let expected = Invocation::RemoteMatch(RemoteMatchRequest {
        repo: "gate".to_owned(),
        text: "cargo test --workspace".to_owned(),
    });
    assert_eq!(invocation(&["remote", "match", "gate", "cargo test --workspace"]), expected);
    assert_eq!(invocation(&["remote", "match", "--", "gate", "cargo test --workspace"]), expected);
}

#[test]
fn remote_match_a_missing_operand_is_a_usage_error() {
    assert_eq!(problems(&["remote", "match"]), ["gate: remote: missing the repo"]);
    assert_eq!(problems(&["remote", "match", "gate"]), ["gate: remote: missing the text"]);
    assert_eq!(problems(&["remote", "match", "gate", ""]), ["gate: remote: missing the text"]);
}

#[test]
fn remote_match_a_third_operand_is_a_usage_error() {
    assert_eq!(
        problems(&["remote", "match", "gate", "cargo test", "--", "extra"]),
        ["gate: remote: match takes a repo and one text, and not extra"]
    );
    assert_eq!(
        problems(&["remote", "match", "gate", "cargo", "test"]),
        ["gate: remote: match takes a repo and one text, and not test"]
    );
}

#[test]
fn remote_match_refuses_the_options_of_an_edit() {
    for (words, option) in [
        (["--dry-run"].as_slice(), "--dry-run"),
        (["--builder", "one@example"].as_slice(), "--builder"),
        (["--tag", "census"].as_slice(), "--tag"),
    ] {
        let line: Vec<&str> = ["remote", "match", "gate", "cargo test"].iter().chain(words).copied().collect();
        assert_eq!(
            problems(&line),
            [format!("gate: remote: {option} is for add and remove only; match changes nothing")],
            "{words:?}"
        );
    }
}

#[test]
fn remote_help_is_the_usage_and_the_usage_lists_remote() {
    assert_eq!(invocation(&["remote", "--help"]), Invocation::Help);
    assert_eq!(invocation(&["remote", "add", "repos", "-h"]), Invocation::Help);
    assert!(
        USAGE.contains("\n       gate remote add <section> <repo> <pattern>... [--builder <host>] [--tag <word>] [--dry-run]\n"),
        "{USAGE}"
    );
    assert!(
        USAGE.contains("\n       gate remote remove <section> <repo> <pattern>... [--builder <host>] [--dry-run]\n"),
        "{USAGE}"
    );
    assert!(
        USAGE.contains("[--builder <host>] [--dry-run]\n       gate remote match <repo> <text>\n"),
        "{USAGE}"
    );
    let prune = USAGE.find("gate prune").expect("prune is listed");
    let remote = USAGE.find("gate remote").expect("remote is listed");
    assert!(prune < remote, "remote follows prune");
}
