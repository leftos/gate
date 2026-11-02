//! `gate csharp format`: nothing to do without files, and the verb it needs.

use crate::support::{Case, USAGE};

#[test]
fn a_format_of_no_files_says_nothing_and_ends_zero() {
    let case = Case::new("a_format_of_no_files_says_nothing_and_ends_zero");
    let outcome = case.run(&["csharp", "format"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
}

#[test]
fn a_bad_verb_is_a_usage_error() {
    let case = Case::new("a_bad_verb_is_a_usage_error");
    let outcome = case.run(&["csharp", "fix"]);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "nothing is formatted");
    assert_eq!(outcome.stderr, format!("gate: csharp takes the verb format, not fix\n{USAGE}"));
}
