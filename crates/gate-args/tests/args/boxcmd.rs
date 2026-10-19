//! The hidden `gate box`: its seven verbs, each followed by the literal `--stdin` (the request comes on stdin), and the one
//! usage line every other shape gets.

use std::ffi::OsString;

use gate_args::{BoxRequest, BoxSeams, BoxVerb, Invocation, USAGE};

use crate::support::{invocation, os, problems, problems_of, unreadable_word};

const BOX_USAGE: &str = "usage: gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs --stdin (the base64 JSON request as stdin's first line)";

#[test]
fn each_verb_parses_with_stdin() {
    for (word, verb) in [
        ("preflight", BoxVerb::Preflight),
        ("run", BoxVerb::Run),
        ("stop", BoxVerb::Stop),
        ("boot", BoxVerb::Boot),
        ("outputs", BoxVerb::Outputs),
        ("inflight", BoxVerb::Inflight),
        ("inputs", BoxVerb::Inputs),
    ] {
        assert_eq!(
            invocation(&["box", word, "--stdin"]),
            Invocation::Box(BoxRequest {
                verb,
                seams: BoxSeams::default(),
            }),
            "{word}"
        );
        assert_eq!(verb.as_str(), word, "the verb as the CLI spells it");
        assert!(verb.takes_request(), "{word} reads a request");
    }
    assert!(!BoxVerb::RegisterDesktop.takes_request(), "register-desktop reads none");
}

#[test]
fn a_missing_or_unknown_verb_a_payload_word_a_missing_stdin_or_an_extra_word_is_the_box_usage_alone() {
    let shapes: [&[&str]; 11] = [
        &["box"],
        &["box", "--stdin"],
        &["box", "preflight"],
        &["box", "preflight", ""],
        &["box", "preflight", "e30="],
        &["box", "preflight", "--STDIN"],
        &["box", "preflight", "--stdin=e30="],
        &["box", "launch", "--stdin"],
        &["box", "Preflight", "--stdin"],
        &["box", "boot", "--stdin", "more"],
        &["box", "--state-dir", "root", "boot", "--stdin"],
    ];
    for words in shapes {
        assert_eq!(problems(words), [BOX_USAGE], "{words:?}");
    }
    let mut unreadable = os(&["box"]);
    unreadable.extend([unreadable_word(), OsString::from("--stdin")]);
    assert_eq!(problems_of(unreadable), [BOX_USAGE], "a verb that is not Unicode is no verb");
    let mut unreadable = os(&["box", "boot"]);
    unreadable.push(unreadable_word());
    assert_eq!(
        problems_of(unreadable),
        [BOX_USAGE],
        "a word after the verb that is not Unicode is no --stdin"
    );
}

/// The hidden probe: a verb of its own, its request on stdin like the others'.
#[test]
fn box_takes_the_inflight_verb() {
    assert_eq!(
        invocation(&["box", "inflight", "--stdin"]),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Inflight,
            seams: BoxSeams::default(),
        })
    );
    assert_eq!(BoxVerb::Inflight.as_str(), "inflight", "the verb as the CLI spells it");
}

/// The registration of the desktop task: a verb that takes no request, so no `--stdin` and nothing after it.
#[test]
fn register_desktop_parses_alone_and_refuses_stdin_or_another_word() {
    assert_eq!(
        invocation(&["box", "register-desktop"]),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::RegisterDesktop,
            seams: BoxSeams::default(),
        })
    );
    assert_eq!(BoxVerb::RegisterDesktop.as_str(), "register-desktop", "the verb as the CLI spells it");
    let shapes: [&[&str]; 5] = [
        &["box", "register-desktop", "--stdin"],
        &["box", "register-desktop", "more"],
        &["box", "register-desktop", ""],
        &["box", "register-desktop", "--stdin", "more"],
        &["box", "Register-Desktop"],
    ];
    for words in shapes {
        assert_eq!(problems(words), [BOX_USAGE], "{words:?}");
    }
    let mut unreadable = os(&["box", "register-desktop"]);
    unreadable.push(unreadable_word());
    assert_eq!(problems_of(unreadable), [BOX_USAGE], "a word after it that is not Unicode is refused too");
}

#[test]
fn box_stays_out_of_the_usage() {
    assert!(!USAGE.contains("box"), "gate box is hidden:\n{USAGE}");
}
