//! The hidden `gate box`: its eight verbs that read a request, each followed by the literal `--stdin` (the request comes
//! on stdin), `register-desktop` alone, `prune --root <folder> [--keys <key>...]`, and the one usage every other shape
//! gets.

use std::ffi::OsString;
use std::path::PathBuf;

use gate_args::{BoxRequest, BoxSeams, BoxVerb, HoldRequest, Invocation, USAGE};

use crate::support::{invocation, os, problems, problems_of, unreadable_word};

const BOX_USAGE: &str = concat!(
    "usage: gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs|drain --stdin (the base64 JSON request as stdin's first line)\n",
    "       gate box prune --root <folder> [--keys <key>...]\n",
    "       gate box hold --root <root> --holder <text> --ceiling <seconds> [--wait <seconds>] -- <command> [args...]",
);

/// The hold of a slot under `C:\x` by `ci 7` for a ceiling of 60 s and a wait of `wait` s, running `command`.
fn held(wait: u64, command: &[&str]) -> Invocation {
    held_with(BoxSeams::default(), wait, command.iter().map(OsString::from).collect())
}

/// [`held`] with the seams `seams` and the command words `command`.
fn held_with(seams: BoxSeams, wait: u64, command: Vec<OsString>) -> Invocation {
    Invocation::Box(BoxRequest {
        verb: BoxVerb::Hold,
        seams,
        root: Some(PathBuf::from(r"C:\x")),
        keys: Vec::new(),
        hold: Some(HoldRequest {
            holder: "ci 7".to_owned(),
            ceiling: 60,
            wait,
            command,
        }),
    })
}

/// `gate box hold` with `options`, then `--` and `command`.
fn hold_words<'a>(options: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec!["box", "hold"];
    words.extend_from_slice(options);
    words.push("--");
    words.extend_from_slice(command);
    words
}

#[test]
fn hold_parses_its_options_in_any_order_and_keeps_every_word_after_the_boundary() {
    let options = ["--root", r"C:\x", "--holder", "ci 7", "--ceiling", "60"];
    assert_eq!(
        invocation(&hold_words(&options, &["pwsh", "-File", "run.ps1"])),
        held(0, &["pwsh", "-File", "run.ps1"])
    );
    assert_eq!(
        invocation(&hold_words(
            &["--ceiling", "60", "--wait", "300", "--holder", "ci 7", "--root", r"C:\x"],
            &["a"]
        )),
        held(300, &["a"]),
        "any order, and the wait"
    );
    assert_eq!(
        invocation(&hold_words(&options, &["--", "--root", "--stdin", "-x", ""])),
        held(0, &["--", "--root", "--stdin", "-x", ""]),
        "after the boundary every word is the command's, as given"
    );
    assert_eq!(
        invocation(&hold_words(
            &["--root", r"C:\x", "--holder", "ci 7", "--ceiling", "60", "--wait", "0"],
            &["a"]
        )),
        held(0, &["a"]),
        "a wait of 0 is no wait"
    );
    assert_eq!(
        invocation(&hold_words(
            &["--root", r"C:\x", "--holder", "ci 7", "--ceiling", "604800", "--wait", "604800"],
            &["a"]
        )),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Hold,
            seams: BoxSeams::default(),
            root: Some(PathBuf::from(r"C:\x")),
            keys: Vec::new(),
            hold: Some(HoldRequest {
                holder: "ci 7".to_owned(),
                ceiling: 604_800,
                wait: 604_800,
                command: vec![OsString::from("a")],
            }),
        }),
        "seven days is the longest ceiling and wait"
    );
    assert_eq!(BoxVerb::Hold.as_str(), "hold", "the verb as the CLI spells it");
    assert!(!BoxVerb::Hold.takes_request(), "hold reads no request");
}

/// `parts` joined, in order.
fn cat<'a>(parts: &[&[&'a str]]) -> Vec<&'a str> {
    parts.concat()
}

#[test]
fn hold_refuses_every_other_shape_as_the_box_usage() {
    let root = ["--root", r"C:\x"];
    let holder = ["--holder", "ci 7"];
    let ceiling = ["--ceiling", "60"];
    let all = cat(&[&root, &holder, &ceiling]);
    let refused: Vec<Vec<&str>> = vec![
        vec!["box", "hold"],
        vec!["box", "hold", "--stdin"],
        cat(&[&["box", "hold"], &all]),
        hold_words(&all, &[]),
        hold_words(&cat(&[&holder, &ceiling]), &["a"]),
        hold_words(&cat(&[&root, &ceiling]), &["a"]),
        hold_words(&cat(&[&root, &holder]), &["a"]),
        hold_words(&cat(&[&all, &["--root", r"C:\y"]]), &["a"]),
        hold_words(&cat(&[&all, &["--holder", "again"]]), &["a"]),
        hold_words(&cat(&[&all, &["--wait", "1", "--wait", "2"]]), &["a"]),
        hold_words(&cat(&[&root, &holder, &["--ceiling", "0"]]), &["a"]),
        hold_words(&cat(&[&root, &holder, &["--ceiling", "-5"]]), &["a"]),
        hold_words(&cat(&[&root, &holder, &["--ceiling", "1.5"]]), &["a"]),
        hold_words(&cat(&[&root, &holder, &["--ceiling", ""]]), &["a"]),
        hold_words(&cat(&[&root, &holder, &["--ceiling", "99999999999999999999999"]]), &["a"]),
        hold_words(&cat(&[&all, &["--wait", "-1"]]), &["a"]),
        hold_words(&cat(&[&all, &["--wait", "soon"]]), &["a"]),
        hold_words(&cat(&[&all, &["--wait", "604801"]]), &["a"]),
        hold_words(&cat(&[&all, &["--wait", "18446744073709551615"]]), &["a"]),
        hold_words(&cat(&[&root, &holder, &["--ceiling", "604801"]]), &["a"]),
        hold_words(&cat(&[&["--root", ""], &holder, &ceiling]), &["a"]),
        hold_words(&cat(&[&all, &["--keys", "k"]]), &["a"]),
        hold_words(&cat(&[&all, &["extra"]]), &["a"]),
        hold_words(&[r"--root=C:\x", "--holder", "ci 7", "--ceiling", "60"], &["a"]),
        hold_words(&["--ROOT", r"C:\x", "--holder", "ci 7", "--ceiling", "60"], &["a"]),
        vec!["box", "hold", "--root", r"C:\x", "--holder", "ci 7", "--ceiling"],
        vec!["box", "Hold", "--root", r"C:\x", "--holder", "ci 7", "--ceiling", "60", "--", "a"],
    ];
    for words in &refused {
        assert_eq!(problems(words), [BOX_USAGE], "{words:?}");
    }
    let mut unreadable = os(&["box", "hold", "--root", r"C:\x", "--holder"]);
    unreadable.push(unreadable_word());
    unreadable.extend(os(&["--ceiling", "60", "--", "a"]));
    assert_eq!(problems_of(unreadable), [BOX_USAGE], "a holder that is not Unicode");
}

#[test]
fn hold_passes_a_command_word_that_is_not_unicode_as_given() {
    let mut words = os(&hold_words(&["--root", r"C:\x", "--holder", "ci 7", "--ceiling", "60"], &["a"]));
    words.push(unreadable_word());
    assert_eq!(
        gate_args::parse(words).expect("the hold parses"),
        held_with(BoxSeams::default(), 0, vec![OsString::from("a"), unreadable_word()])
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn hold_takes_the_poll_seam_before_its_verb() {
    let mut words = vec!["box", "--test-box-hold-poll-ms", "20"];
    words.extend_from_slice(&hold_words(&["--root", r"C:\x", "--holder", "ci 7", "--ceiling", "60"], &["a"])[1..]);
    let seams = BoxSeams {
        hold_poll_ms: Some(20),
        ..BoxSeams::default()
    };
    assert_eq!(invocation(&words), held_with(seams, 0, vec![OsString::from("a")]));
    words[2] = "0";
    let refused = problems(&words);
    assert_eq!(refused.len(), 1, "a poll of 0 is refused: {refused:?}");
    assert!(refused[0].contains("--test-box-hold-poll-ms"), "{refused:?}");
}

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
        ("lfs", BoxVerb::Lfs),
    ] {
        assert_eq!(
            invocation(&["box", word, "--stdin"]),
            Invocation::Box(BoxRequest {
                verb,
                seams: BoxSeams::default(),
                root: None,
                keys: Vec::new(),
                hold: None,
            }),
            "{word}"
        );
        assert_eq!(verb.as_str(), word, "the verb as the CLI spells it");
        assert!(verb.takes_request(), "{word} reads a request");
    }
    assert!(!BoxVerb::RegisterDesktop.takes_request(), "register-desktop reads none");
    assert!(!BoxVerb::Prune.takes_request(), "prune reads none");
}

/// The sweep of idle work folders: a verb that takes no request, only its root as the one word after `--root`.
#[test]
fn prune_parses_its_root_and_refuses_any_other_shape() {
    assert_eq!(
        invocation(&["box", "prune", "--root", r"C:\x"]),
        Invocation::Box(BoxRequest {
            verb: BoxVerb::Prune,
            seams: BoxSeams::default(),
            root: Some(PathBuf::from(r"C:\x")),
            keys: Vec::new(),
            hold: None,
        })
    );
    assert_eq!(BoxVerb::Prune.as_str(), "prune", "the verb as the CLI spells it");
    let shapes: [&[&str]; 7] = [
        &["box", "prune"],
        &["box", "prune", "--root"],
        &["box", "prune", "--stdin"],
        &["box", "prune", "--root", r"C:\x", "extra"],
        &["box", "prune", r"--root=C:\x"],
        &["box", "prune", "--ROOT", r"C:\x"],
        &["box", "prune", "--root", r"C:\x", "--stdin"],
    ];
    for words in shapes {
        assert_eq!(problems(words), [BOX_USAGE], "{words:?}");
    }
}

/// The words `gate box prune --root C:\x --keys` and then `keys`.
fn keyed_words<'a>(keys: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec!["box", "prune", "--root", r"C:\x", "--keys"];
    words.extend_from_slice(keys);
    words
}

/// The keyed prune of the root `C:\x` that names `keys`.
fn keyed(keys: &[&str]) -> Invocation {
    Invocation::Box(BoxRequest {
        verb: BoxVerb::Prune,
        seams: BoxSeams::default(),
        root: Some(PathBuf::from(r"C:\x")),
        keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        hold: None,
    })
}

#[test]
fn prune_takes_one_key_after_its_root() {
    assert_eq!(invocation(&keyed_words(&["app-1a2b3c4d"])), keyed(&["app-1a2b3c4d"]));
}

#[test]
fn prune_takes_a_key_in_upper_case_as_given() {
    assert_eq!(invocation(&keyed_words(&["App_X-1A2B"])), keyed(&["App_X-1A2B"]));
}

#[test]
fn prune_takes_fifty_keys_and_refuses_fifty_one() {
    let names: Vec<String> = (1..=51).map(|n| format!("k{n}")).collect();
    let fifty: Vec<&str> = names[..50].iter().map(String::as_str).collect();
    let fifty_one: Vec<&str> = names.iter().map(String::as_str).collect();

    assert_eq!(invocation(&keyed_words(&fifty)), keyed(&fifty));
    assert_eq!(problems(&keyed_words(&fifty_one)), [BOX_USAGE]);
}

#[test]
fn prune_takes_a_repeated_key_once() {
    assert_eq!(invocation(&keyed_words(&["k", "j", "k"])), keyed(&["k", "j"]));
}

/// The builder's file system knows no case, so `K` names the same work folder as `k`: the first spelling is kept.
#[test]
fn prune_takes_a_key_repeated_in_another_case_once_as_first_spelled() {
    assert_eq!(invocation(&keyed_words(&["App-1", "j", "app-1", "APP-1"])), keyed(&["App-1", "j"]));
}

#[test]
fn prune_refuses_keys_with_no_key_after_them() {
    assert_eq!(problems(&keyed_words(&[])), [BOX_USAGE]);
}

#[test]
fn prune_refuses_a_word_that_is_no_key_or_a_flag_among_its_keys() {
    for word in ["a.b", r"a\b", "a/b", "-a", "--stdin", "", "a b", ".."] {
        assert_eq!(problems(&keyed_words(&[word])), [BOX_USAGE], "{word:?} alone");
        assert_eq!(problems(&keyed_words(&["k", word])), [BOX_USAGE], "{word:?} after a key");
    }
}

#[test]
fn prune_refuses_keys_before_its_root_or_spelled_otherwise() {
    let shapes: [&[&str]; 4] = [
        &["box", "prune", "--keys", "k", "--root", r"C:\x"],
        &["box", "prune", "--root", r"C:\x", "--keys=k"],
        &["box", "prune", "--root", r"C:\x", "--KEYS", "k"],
        &["box", "prune", "--root", r"C:\x", "k"],
    ];
    for words in shapes {
        assert_eq!(problems(words), [BOX_USAGE], "{words:?}");
    }
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
            root: None,
            keys: Vec::new(),
            hold: None,
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
            root: None,
            keys: Vec::new(),
            hold: None,
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
