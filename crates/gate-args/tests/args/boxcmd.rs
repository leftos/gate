//! The hidden `gate box`: its five verbs, its one payload word, and the one usage line every other shape gets.

use std::ffi::OsString;

use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation, USAGE};

use crate::support::{invocation, os, problems, problems_of, unreadable_word};

const BOX_USAGE: &str = "usage: gate box preflight|run|stop|boot|outputs|inflight <base64 JSON payload>";

#[test]
fn each_verb_parses_with_its_payload_word_unread() {
    for (word, verb) in [
        ("preflight", BoxVerb::Preflight),
        ("run", BoxVerb::Run),
        ("stop", BoxVerb::Stop),
        ("boot", BoxVerb::Boot),
        ("outputs", BoxVerb::Outputs),
        ("inflight", BoxVerb::Inflight),
    ] {
        assert_eq!(
            invocation(&["box", word, "not even base64"]),
            Invocation::Box(BoxRequest {
                verb,
                payload: OsString::from("not even base64"),
                seams: BoxSeams::default(),
            }),
            "{word}"
        );
        assert_eq!(verb.as_str(), word, "the verb as the CLI spells it");
    }
}

#[test]
fn a_missing_or_unknown_verb_a_missing_payload_or_an_extra_word_is_the_box_usage_alone() {
    let shapes: [&[&str]; 7] = [
        &["box"],
        &["box", "preflight"],
        &["box", "preflight", ""],
        &["box", "launch", "e30="],
        &["box", "Preflight", "e30="],
        &["box", "boot", "e30=", "more"],
        &["box", "--state-dir", "root", "boot", "e30="],
    ];
    for words in shapes {
        assert_eq!(problems(words), [BOX_USAGE], "{words:?}");
    }
    let mut unreadable = os(&["box"]);
    unreadable.extend([unreadable_word(), OsString::from("e30=")]);
    assert_eq!(problems_of(unreadable), [BOX_USAGE], "a verb that is not Unicode is no verb");
}

/// The hidden probe: a verb of its own, whose one payload word is passed on unread like the others'.
#[test]
fn box_takes_the_inflight_verb() {
    assert_eq!(
        invocation(&["box", "inflight", "not even base64"]),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Inflight,
            payload: OsString::from("not even base64"),
            seams: BoxSeams::default(),
        })
    );
    assert_eq!(BoxVerb::Inflight.as_str(), "inflight", "the verb as the CLI spells it");
}

#[test]
fn a_payload_that_is_not_unicode_is_passed_on() {
    let mut words = os(&["box", "boot"]);
    words.push(unreadable_word());
    assert_eq!(
        gate_args::parse(words).expect("the command line parses"),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Boot,
            payload: unreadable_word(),
            seams: BoxSeams::default(),
        })
    );
}

#[test]
fn box_stays_out_of_the_usage() {
    assert!(!USAGE.contains("box"), "gate box is hidden:\n{USAGE}");
}
