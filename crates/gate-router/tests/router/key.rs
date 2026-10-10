//! The work-folder key and the repo name. Every expected value was produced by dot-sourcing `remote.ps1` in
//! `pwsh -NoProfile` and calling `Get-RemoteKey -Root <root>` or `Get-RemoteName <name>`.

use gate_router::{repo_name, work_folder_key};

#[test]
fn the_key_is_the_one_powershell_computes() {
    let cases = [
        ("X:/dev/gate.wt/gate-66", "gate-66-4adbee7e"),
        ("X:/dev/yaat.wt/a", "a-3f6aa27c"),
        ("C:/Work/My.Repo", "My-Repo-ca89f5fe"),
        ("C:\\Work\\Gate", "Gate-c403bca7"),
        ("X:/dev/.dotfiles", "dotfiles-d7a903db"),
        ("X:/a/b/", "b-fb354439"),
        ("X:/", "X---77ecb441"),
        ("D:/\u{dc}n\u{ef}code/Stra\u{df}e", "Stra-e-fd657ecd"),
        ("D:/\u{130}stanbul/Caf\u{e9}", "Caf--9667f612"),
    ];
    for (root, expected) in cases {
        assert_eq!(work_folder_key(root), expected, "{root}");
    }
}

#[test]
fn the_repo_name_keeps_what_a_ref_and_a_path_take() {
    let cases = [
        ("gate", "gate"),
        (".config", "config"),
        ("...", "repo"),
        ("", "repo"),
        ("my repo", "my-repo"),
        ("Caf\u{e9}", "Caf-"),
        ("a\u{1f600}b", "a--b"),
    ];
    for (folder, expected) in cases {
        assert_eq!(repo_name(folder), expected, "{folder:?}");
    }
}
