//! `gate prune --builders`: the one switch it needs, no words, the usage line, and its hidden seams.

use gate_args::{Invocation, PruneRequest, PruneSeams, USAGE};

use crate::support::{invocation, problems};

#[test]
fn prune_builders_parses() {
    assert_eq!(
        invocation(&["prune", "--builders"]),
        Invocation::Prune(PruneRequest {
            seams: PruneSeams::default(),
            quiet: false,
        })
    );
}

#[test]
fn prune_builders_quiet_parses_in_every_build() {
    assert_eq!(
        invocation(&["prune", "--builders", "--quiet"]),
        Invocation::Prune(PruneRequest {
            seams: PruneSeams::default(),
            quiet: true,
        })
    );
    assert_eq!(problems(&["prune", "--quiet"]), ["gate: missing --builders"], "--quiet is no --builders");
    assert_eq!(problems(&["prune", "--builders", "--quiet", "--quiet"]), ["gate: --quiet is given twice"]);
}

#[test]
fn prune_without_builders_is_refused() {
    assert_eq!(problems(&["prune"]), ["gate: missing --builders"]);
    assert_eq!(
        problems(&["prune", "now"]),
        ["gate: unexpected argument now", "gate: missing --builders"],
        "a word is no --builders"
    );
}

#[test]
fn prune_takes_no_words() {
    assert_eq!(problems(&["prune", "--builders", "extra"]), ["gate: unexpected argument extra"]);
    assert_eq!(problems(&["prune", "--builders", "--"]), ["gate: unexpected argument --"]);
    assert_eq!(problems(&["prune", "--builders", "--builders"]), ["gate: --builders is given twice"]);
    assert_eq!(
        problems(&["prune", "--builders=yes"]),
        ["gate: unknown option --builders=yes", "gate: missing --builders"],
        "a switch takes no value"
    );
    assert_eq!(
        problems(&["prune", "--builders", "--builder", "box-1"]),
        ["gate: unknown option --builder", "gate: unexpected argument box-1"],
        "a drain option is not a prune option"
    );
}

#[test]
fn prune_help_prints_the_usage() {
    assert_eq!(invocation(&["prune", "--help"]), Invocation::Help);
    assert_eq!(invocation(&["prune", "--builders", "-h"]), Invocation::Help);
}

#[test]
fn the_usage_lists_prune_after_drain() {
    let lines: Vec<&str> = USAGE.lines().collect();
    let at = lines
        .iter()
        .position(|line| *line == "       gate prune --builders")
        .expect("the usage lists gate prune");
    assert_eq!(lines[at - 1], "       gate drain [--lift | --status] [--builder <host>]", "{USAGE}");
}

#[cfg(feature = "test-seams")]
#[test]
fn prune_seams_parse_with_the_feature() {
    use std::path::PathBuf;

    let words = [
        "prune",
        "--builders",
        "--test-remote-transport",
        r"local:C:\builder",
        "--test-remote-ssh",
        "fake-ssh.exe",
        "--test-prune-gone-age-ms=60000",
    ];
    assert_eq!(
        invocation(&words),
        Invocation::Prune(PruneRequest {
            seams: PruneSeams {
                remote_transport: Some(r"local:C:\builder".to_owned()),
                remote_ssh: Some(PathBuf::from("fake-ssh.exe")),
                gone_age_ms: Some(60000),
            },
            quiet: false,
        })
    );
    assert_eq!(
        invocation(&["prune", "--builders", "--test-prune-gone-age-ms", "0"]),
        Invocation::Prune(PruneRequest {
            seams: PruneSeams {
                gone_age_ms: Some(0),
                ..PruneSeams::default()
            },
            quiet: false,
        }),
        "a gone worktree may be pruned at once"
    );
    assert_eq!(
        problems(&["prune", "--builders", "--test-prune-gone-age-ms"]),
        ["gate: --test-prune-gone-age-ms needs a value"]
    );
    assert_eq!(
        problems(&["prune", "--builders", "--test-remote-ssh", "fake-ssh.exe"]),
        ["gate: --test-remote-ssh needs --test-remote-transport"]
    );
    assert_eq!(
        problems(&["prune", "--builders", "--test-poll-ms", "5"]),
        ["gate: unknown option --test-poll-ms", "gate: unexpected argument 5"],
        "a drain seam is not a prune seam"
    );
}

#[cfg(not(feature = "test-seams"))]
#[test]
fn prune_seams_are_unknown_without_the_feature() {
    for seam in ["--test-remote-transport", "--test-remote-ssh", "--test-prune-gone-age-ms"] {
        assert_eq!(
            problems(&["prune", "--builders", seam, "5"]),
            [format!("gate: unknown option {seam}"), "gate: unexpected argument 5".to_owned()],
            "{seam}"
        );
    }
}
