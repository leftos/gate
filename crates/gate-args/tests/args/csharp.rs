//! `gate csharp format`: the verb it needs, the files it keeps as given, and the usage line.

use gate_args::{CsharpFormatRequest, Invocation, USAGE};

use crate::support::{invocation, os, problems};

#[test]
fn a_format_line_keeps_its_files_in_order() {
    assert_eq!(
        invocation(&["csharp", "format", "b.cs", "a.cs"]),
        Invocation::CsharpFormat(CsharpFormatRequest {
            files: os(&["b.cs", "a.cs"])
        })
    );
}

#[test]
fn a_format_line_with_no_files_parses() {
    assert_eq!(
        invocation(&["csharp", "format"]),
        Invocation::CsharpFormat(CsharpFormatRequest { files: Vec::new() })
    );
}

#[test]
fn a_file_word_starting_with_a_dash_stays_a_file() {
    assert_eq!(
        invocation(&["csharp", "format", "--include", "a.cs"]),
        Invocation::CsharpFormat(CsharpFormatRequest {
            files: os(&["--include", "a.cs"])
        })
    );
}

#[test]
fn csharp_alone_is_refused() {
    assert_eq!(problems(&["csharp"]), ["gate: csharp needs the verb format"]);
}

#[test]
fn another_verb_is_refused() {
    assert_eq!(problems(&["csharp", "fix"]), ["gate: csharp takes the verb format, not fix"]);
}

#[test]
fn help_after_format_is_the_usage() {
    assert_eq!(invocation(&["csharp", "format", "--help"]), Invocation::Help);
    assert_eq!(invocation(&["csharp", "-h"]), Invocation::Help);
}

#[test]
fn the_usage_lists_gate_csharp_format() {
    assert!(
        USAGE.lines().any(|line| line == "       gate csharp format [<file>...]"),
        "the usage lists gate csharp format:\n{USAGE}"
    );
}
