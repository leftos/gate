//! The caller's environment a routed run does not send: the watch list, the `--env` and gate-name exclusions, and the
//! line the gate says for the names it keeps.

use gate_router::{dropped_env, dropped_env_line};

/// The names of `caller` a routed run does not send, with no `--env` and no name the gate sets.
fn kept(caller: &[&str]) -> Vec<String> {
    dropped_env(caller.iter().copied(), &[], &[])
}

#[test]
fn the_watch_list_holds_each_form_in_any_letter_case_and_sorts_what_it_keeps() {
    assert_eq!(
        kept(&[
            "NuGetAudit",
            "nuget_audit_sources",
            "MSBuildCache",
            "rustflags",
            "RUSTDOCFLAGS",
            "cargo_build_target",
            "Cargo_Profile_Home",
        ]),
        [
            "cargo_build_target",
            "Cargo_Profile_Home",
            "MSBuildCache",
            "nuget_audit_sources",
            "NuGetAudit",
            "RUSTDOCFLAGS",
            "rustflags",
        ],
        "sorted by ASCII-lowercase spelling, each kept as the caller spelled it"
    );
}

#[test]
fn a_name_outside_the_watch_list_is_not_kept() {
    assert_eq!(
        kept(&[
            "PATH",
            "NUGET",
            "RUSTFLAG",
            "RUSTFLAGSX",
            "CARGO_HOME",
            "DOTNET_CLI_USE_MSBUILD_SERVER",
            "GATE_STATE_DIR",
        ]),
        Vec::<String>::new(),
        "a name that only looks like a watched one is not one"
    );
}

#[test]
fn a_name_given_by_env_or_set_by_the_gate_is_not_kept() {
    assert_eq!(
        dropped_env(["NuGetAudit", "RUSTFLAGS"].into_iter(), &["nugetaudit"], &[]),
        ["RUSTFLAGS"],
        "--env sends it, in any letter case"
    );
    assert_eq!(
        dropped_env(
            ["MSBUILDUSESERVER", "MSBUILDNODEHANDSHAKESALT", "MSBUILDDISABLENODEREUSE", "NuGetAudit"].into_iter(),
            &[],
            &["MSBUILDUSESERVER", "MSBUILDNODEHANDSHAKESALT", "MSBUILDDISABLENODEREUSE"]
        ),
        ["NuGetAudit"],
        "the gate sets the MSBuild names on every command it starts, so a nested gate's inherited ones never warn"
    );
}

#[test]
fn one_name_spelled_twice_is_kept_once_as_the_first_spelling() {
    assert_eq!(kept(&["NuGetAudit", "NUGETAUDIT"]), ["NuGetAudit"]);
}

#[test]
fn the_line_names_one_or_many_and_nothing_for_none() {
    assert_eq!(dropped_env_line(&[]), None, "every name is sent");
    assert_eq!(
        dropped_env_line(&["NuGetAudit".to_owned()]),
        Some("gate: a routed run does not send the caller's NuGetAudit; --env NAME sends it".to_owned())
    );
    assert_eq!(
        dropped_env_line(&["NuGetAudit".to_owned(), "RUSTFLAGS".to_owned()]),
        Some("gate: a routed run does not send the caller's NuGetAudit, RUSTFLAGS; --env NAME sends it".to_owned())
    );
}

#[test]
fn the_line_escapes_each_name_that_holds_a_control_character() {
    assert_eq!(
        dropped_env_line(&["A\u{1b}B".to_owned()]),
        Some(r"gate: a routed run does not send the caller's A\u{1b}B; --env NAME sends it".to_owned())
    );
    assert_eq!(
        dropped_env_line(&["NuGetAudit".to_owned(), "A\u{7}B".to_owned()]),
        Some(r"gate: a routed run does not send the caller's NuGetAudit, A\u{7}B; --env NAME sends it".to_owned()),
        "each name on its own"
    );
}
