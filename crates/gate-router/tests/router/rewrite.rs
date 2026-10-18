//! The in-worktree path rewrite. [`POWERSHELL_TABLE`] is `remote.selftest.ps1`'s `Get-RoutePathTable`, row for row,
//! with the worktree `X:/dev/yaat.wt/a`, apart from the rows where a command's first word names a program by an
//! absolute path outside the worktree: this gate sends the program's name there, where the table sends the path and
//! refuses the run as outside. [`SPACES_GLOBS_AND_FLAGS_TABLE`] holds the cases the PowerShell rewrite gets wrong.

use std::path::PathBuf;

use gate_router::{Place, RewritePlace, Rewritten, SiblingRoot, rewrite_words};
use proptest::prelude::*;

use crate::support::{ROOT, place_at, sibling, words};

/// [`rewrite_words`] at `place`, its leaf the root's last segment.
fn rewrite(place: &Place, words: &[String], drives: &[(char, String)], siblings: &[SiblingRoot]) -> Rewritten {
    let leaf = place.root.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or_default();
    let place = RewritePlace {
        root: &place.root,
        prefix: &place.prefix,
        folder: &place.folder,
        leaf,
    };
    rewrite_words(&place, words, drives, siblings)
}

/// One row: the gate's prefix, the words, the words expected back (`None`: not checked) and the first outside word.
#[derive(Clone, Copy)]
struct Row {
    prefix: &'static str,
    words: &'static [&'static str],
    expect: Option<&'static [&'static str]>,
    outside: Option<&'static str>,
    drives: &'static [(char, &'static str)],
}

const fn sent(prefix: &'static str, words: &'static [&'static str], expect: &'static [&'static str]) -> Row {
    Row {
        prefix,
        words,
        expect: Some(expect),
        outside: None,
        drives: &[],
    }
}

const fn outside(prefix: &'static str, words: &'static [&'static str], outside: &'static str) -> Row {
    Row {
        prefix,
        words,
        expect: None,
        outside: Some(outside),
        drives: &[],
    }
}

const fn on_drives(row: Row, drives: &'static [(char, &'static str)]) -> Row {
    Row { drives, ..row }
}

const fn with_expect(row: Row, expect: &'static [&'static str]) -> Row {
    Row { expect: Some(expect), ..row }
}

const DEV: &[(char, &str)] = &[('D', r"X:\dev")];

const POWERSHELL_TABLE: &[Row] = &[
    sent("", &["dotnet", "test", "X:/dev/yaat.wt/a/tests/T"], &["dotnet", "test", "tests/T"]),
    sent("", &[r"X:\dev\yaat.wt\a\tests\T"], &[r"tests\T"]),
    sent("", &[r"X:\dev/yaat.wt\a/tests/T"], &["tests/T"]),
    sent("", &[r"& X:\dev\yaat.wt\a\t.ps1 -Filter A/B"], &["& t.ps1 -Filter A/B"]),
    sent("", &[r"X:\dev\yaat.wt\a\t.ps1 -Uri https://h/p"], &["t.ps1 -Uri https://h/p"]),
    sent("src/c/", &["X:/dev/yaat.wt/a/src/c/x"], &["../../src/c/x"]),
    sent("", &[r"X:\dev\yaat.wt\a\\tests\x"], &[r"tests\x"]),
    sent("", &[r"Set-Location 'X:\dev\yaat.wt\a'; dotnet test"], &["Set-Location '.'; dotnet test"]),
    sent("", &[r"(X:\dev\yaat.wt\a)"], &["(.)"]),
    sent("", &[r"X:\dev\yaat.wt\a|x"], &[".|x"]),
    with_expect(
        outside(
            "",
            &["Set-Location X:\\dev\\yaat.wt\\a\\x\n..\\..\\..\\y"],
            "Set-Location X:\\dev\\yaat.wt\\a\\x\n..\\..\\..\\y",
        ),
        &["Set-Location x\n..\\..\\..\\y"],
    ),
    outside("", &["dotnet", r"X:\dev\yaat.wt\a.old\x"], r"X:\dev\yaat.wt\a.old\x"),
    sent("", &[r"X:\dev\yaat.wt\a.old\x"], &["x"]),
    sent("src/c/", &[r"--results-directory=..\..\r"], &[r"--results-directory=..\..\r"]),
    sent("", &["x:/DEV/Yaat.WT/A/Tests/T"], &["Tests/T"]),
    sent("", &["--results-directory=X:/dev/yaat.wt/a/out"], &["--results-directory=out"]),
    sent("", &[r"-p:OutDir=X:\dev\yaat.wt\a\bin\"], &[r"-p:OutDir=bin\"]),
    sent("", &["/p:A=1;B=X:/dev/yaat.wt/a/x"], &["/p:A=1;B=x"]),
    sent("", &[r#""X:\dev\yaat.wt\a\p;X:/dev/yaat.wt/a/q""#], &[r#""p;q""#]),
    sent("", &["X:/dev/yaat.wt/a"], &["."]),
    sent("src/c/", &["X:/dev/yaat.wt/a/src/c"], &["."]),
    sent("src/c/", &[r"X:\dev\yaat.wt\a\tests\X"], &[r"..\..\tests\X"]),
    sent("src/c/", &[r"..\..\tests\X", "a/b"], &[r"..\..\tests\X", "a/b"]),
    sent("", &["/p:Configuration=Release", "/c", "a/b"], &["/p:Configuration=Release", "/c", "a/b"]),
    outside("", &["dotnet", r"\foo"], r"\foo"),
    outside(
        "",
        &[r"Copy-Item X:\dev\yaat.wt\a\x ..\..\secret"],
        r"Copy-Item X:\dev\yaat.wt\a\x ..\..\secret",
    ),
    outside("", &["dotnet", "X:/dev/yaat.wt/ab/tests"], "X:/dev/yaat.wt/ab/tests"),
    outside("", &["dotnet", "X:/dev/yaat/tests"], "X:/dev/yaat/tests"),
    outside("", &["dotnet", r"\\server\share\x"], r"\\server\share\x"),
    outside("", &["dotnet", "//server/share/x"], "//server/share/x"),
    outside("", &["dotnet", r"X:\"], r"X:\"),
    outside("", &["dotnet", r"D:\yaat.wt\a\tests"], r"D:\yaat.wt\a\tests"),
    outside(
        "",
        &["dotnet", r"C:\Program Files\dotnet\dotnet.exe"],
        r"C:\Program Files\dotnet\dotnet.exe",
    ),
    sent("", &[r"C:\Program Files\dotnet\dotnet.exe", "test"], &["dotnet", "test"]),
    outside("", &["dotnet", r"..\other\x"], r"..\other\x"),
    outside("src/", &["dotnet", r"..\..\x"], r"..\..\x"),
    outside("", &["dotnet", "X:/dev/yaat.wt/a/../b/x"], "X:/dev/yaat.wt/a/../b/x"),
    outside("", &["dotnet", "--project=../other/x.csproj"], "--project=../other/x.csproj"),
    outside("", &["dotnet", r"-p:OutDir=..\out\"], r"-p:OutDir=..\out\"),
    outside("src/", &["dotnet", r"--results-directory=..\..\..\r"], r"--results-directory=..\..\..\r"),
    with_expect(
        outside("", &[r"-p:A=X:\dev\yaat.wt\a\b,X:\other"], r"-p:A=X:\dev\yaat.wt\a\b,X:\other"),
        &[r"-p:A=b,X:\other"],
    ),
    with_expect(
        outside(
            "",
            &["pwsh", "-c", r"dotnet test X:\dev\yaat.wt\a\x.sln && type C:\secret\y"],
            r"dotnet test X:\dev\yaat.wt\a\x.sln && type C:\secret\y",
        ),
        &["pwsh", "-c", r"dotnet test x.sln && type C:\secret\y"],
    ),
    with_expect(
        outside(
            "",
            &["X:/dev/yaat.wt/a/t", r#""X:\dev\yaat.wt\a\p;X:\elsewhere""#],
            r#""X:\dev\yaat.wt\a\p;X:\elsewhere""#,
        ),
        &["t", r#""p;X:\elsewhere""#],
    ),
    on_drives(sent("", &[r"D:\yaat.wt\a\tests\T"], &[r"tests\T"]), DEV),
    on_drives(sent("", &["d:/yaat.wt/a/tests/T"], &["tests/T"]), DEV),
    on_drives(sent("src/c/", &[r"D:\yaat.wt\a\src\c\x"], &[r"..\..\src\c\x"]), DEV),
    on_drives(sent("", &[r"D:\yaat.wt\a\tools\hidden-desktop.ps1"], &[r"tools\hidden-desktop.ps1"]), DEV),
    on_drives(sent("", &[r"D:\x"], &["x"]), &[('D', r"X:\dev\yaat.wt\a")]),
    on_drives(outside("", &["dotnet", r"D:\yaat.wt\ab\x"], r"D:\yaat.wt\ab\x"), DEV),
    on_drives(sent("", &[r"D:\yaat.wt\ab\x"], &["x"]), DEV),
    on_drives(outside("", &["dotnet", r"Q:\yaat.wt\a\x"], r"Q:\yaat.wt\a\x"), DEV),
    on_drives(sent("", &[r"Q:\yaat.wt\a\x"], &["x"]), DEV),
    on_drives(outside("", &["dotnet", r"D:\v\yaat.wt\a\x"], r"D:\v\yaat.wt\a\x"), &[('D', r"X:\de")]),
    on_drives(sent("", &[r"D:\v\yaat.wt\a\x"], &["x"]), &[('D', r"X:\de")]),
];

/// The cases the PowerShell rewrite gets wrong: one argument holding a path with spaces runs through them, while a
/// script's text still ends a path at white space; globs and flags that are not paths are left alone.
const SPACES_GLOBS_AND_FLAGS_TABLE: &[Row] = &[
    sent("", &["X:/dev/yaat.wt/a/my dir/x.cs"], &["my dir/x.cs"]),
    sent("src/", &[r"X:\dev\yaat.wt\a\my dir\x.cs"], &[r"..\my dir\x.cs"]),
    sent("my dir/", &["X:/dev/yaat.wt/a/my dir"], &["."]),
    sent("", &["--project=X:/dev/yaat.wt/a/my dir/p.csproj"], &["--project=my dir/p.csproj"]),
    outside("", &[r"X:\dev\yaat.wt\a\b c\..\..\x"], r"X:\dev\yaat.wt\a\b c\..\..\x"),
    outside("", &["dotnet", "X:/dev/yaat.wt/a old/x"], "X:/dev/yaat.wt/a old/x"),
    sent("", &["X:/dev/yaat.wt/a old/x"], &["x"]),
    sent("", &[r"cd X:\dev\yaat.wt\a\b c; dir"], &["cd b c; dir"]),
    sent(
        "",
        &["tests/**/*.cs", "*.csproj", "src/[ab]?.cs"],
        &["tests/**/*.cs", "*.csproj", "src/[ab]?.cs"],
    ),
    sent(
        "src/",
        &[
            "dotnet",
            "test",
            "--no-build",
            "-c",
            "Release",
            "--filter",
            "Category=Unit",
            "-v:q",
            "/nologo",
        ],
        &[
            "dotnet",
            "test",
            "--no-build",
            "-c",
            "Release",
            "--filter",
            "Category=Unit",
            "-v:q",
            "/nologo",
        ],
    ),
];

fn check(table: &[Row]) -> Vec<String> {
    table
        .iter()
        .filter_map(|row| {
            let drives: Vec<(char, String)> = row.drives.iter().map(|(letter, path)| (*letter, (*path).to_owned())).collect();
            let got = rewrite(&place_at(row.prefix), &words(row.words), &drives, &[]);
            let name = format!("{:?} at {:?}", row.words, row.prefix);
            if got.outside.as_deref() != row.outside {
                return Some(format!("{name} named {:?} as outside, not {:?}", got.outside, row.outside));
            }
            let expect = row.expect.map(words)?;
            (got.words != expect).then(|| format!("{name} rewrote to {:?}, not {expect:?}", got.words))
        })
        .collect()
}

#[test]
fn the_powershell_path_table_rewrites_the_same() {
    let wrong = check(POWERSHELL_TABLE);
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn a_path_with_spaces_runs_through_them_and_globs_and_flags_are_left_alone() {
    let wrong = check(SPACES_GLOBS_AND_FLAGS_TABLE);
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn a_root_holding_a_space_is_recognised() {
    let place = Place {
        root: "X:/my wt".to_owned(),
        folder: "X:/my wt".to_owned(),
        ..place_at("")
    };
    let got = rewrite(&place, &words(&["X:/my wt/a", r"X:\my wt"]), &[], &[]);
    assert_eq!(got.words, ["a", "."]);
    assert_eq!(got.outside, None);
}

/// The words at `prefix` rewritten with the siblings `yaat-server` and `b` declared, on the subst table `drives`.
fn with_siblings(prefix: &str, list: &[&str], drives: &[(char, &str)]) -> Rewritten {
    let drives: Vec<(char, String)> = drives.iter().map(|(letter, path)| (*letter, (*path).to_owned())).collect();
    rewrite(&place_at(prefix), &words(list), &drives, &[sibling("yaat-server"), sibling("b")])
}

/// Asserts each word at `prefix` is rewritten to its pair's second, none outside, all left as paths.
fn assert_sent(prefix: &str, pairs: &[(&str, &str)], drives: &[(char, &str)]) {
    for (word, expect) in pairs {
        let got = with_siblings(prefix, &[word], drives);
        assert_eq!(
            (got.words, got.outside, got.program_named),
            (words(&[expect]), None, false),
            "{word:?} at {prefix:?}"
        );
    }
}

/// Asserts each word at `prefix` is outside the worktree, naming no program.
fn assert_outside(prefix: &str, list: &[&str]) {
    for word in list {
        let got = with_siblings(prefix, &[word], &[]);
        assert_eq!(got.outside.as_deref(), Some(*word), "{word:?} at {prefix:?}");
        assert!(!got.program_named, "{word:?} at {prefix:?} names a program");
    }
}

/// Asserts each word at `prefix` is outside the worktree as a command's second word, after `dotnet`, where the rule for
/// a command's first word does not read it.
fn assert_outside_after_first(prefix: &str, list: &[&str]) {
    for word in list {
        let got = with_siblings(prefix, &["dotnet", word], &[]);
        assert_eq!(got.outside.as_deref(), Some(*word), "{word:?} after a first word at {prefix:?}");
        assert!(!got.program_named, "{word:?} after a first word at {prefix:?} names a program");
    }
}

#[test]
fn a_path_inside_a_sibling_becomes_dotdot_name_rest() {
    assert_sent(
        "",
        &[
            (r"X:\dev\yaat.wt\yaat-server\src\S.csproj", r"..\yaat-server\src\S.csproj"),
            ("X:/dev/yaat.wt/yaat-server/src/S.csproj", "../yaat-server/src/S.csproj"),
            ("X:/dev/yaat.wt/yaat-server", "../yaat-server"),
            ("--project=x:/DEV/yaat.wt/Yaat-Server/a", "--project=../yaat-server/a"),
            (r"-p:P=X:\dev\yaat.wt\b\x;X:\dev\yaat.wt\a\y", r"-p:P=..\b\x;y"),
        ],
        &[],
    );
    assert_sent(
        "src/c/",
        &[
            ("X:/dev/yaat.wt/yaat-server/src/x", "../../../yaat-server/src/x"),
            (r"X:\dev\yaat.wt\b", r"..\..\..\b"),
        ],
        &[],
    );
}

#[test]
fn a_relative_climb_into_a_declared_sibling_is_inside() {
    let at_root = [
        "../yaat-server/src/x",
        r"--project=..\yaat-server\x.csproj",
        "-p:P=../b",
        "../Yaat-Server/a/../c",
        ".././/b",
    ];
    assert_sent("", &at_root.map(|word| (word, word)), &[]);
    let below = [r"..\..\..\yaat-server\src", "../../.././b/x"];
    assert_sent("tools/g/", &below.map(|word| (word, word)), &[]);
}

#[test]
fn a_climb_into_an_undeclared_folder_is_outside() {
    assert_outside("", &["../other/x", "../../yaat.wt/yaat-server"]);
    assert_outside_after_first("", &["X:/dev/yaat.wt/other/x"]);
    assert_outside("src/", &["../../other"]);
}

#[test]
fn a_sibling_name_prefix_is_not_the_sibling() {
    assert_outside("", &["../yaat-server2/x", "../bb"]);
    assert_outside_after_first("", &["X:/dev/yaat.wt/yaat-server2/x", "X:/dev/yaat.wt/yaat-serve/x"]);
}

#[test]
fn a_sibling_path_on_a_subst_drive_is_inside() {
    assert_sent(
        "",
        &[(r"D:\yaat.wt\yaat-server\src", r"..\yaat-server\src"), ("d:/yaat.wt/b", "../b")],
        &[('D', r"X:\dev")],
    );
}

#[test]
fn a_climb_out_of_a_sibling_is_outside() {
    assert_outside(
        "",
        &["../yaat-server/../x", "X:/dev/yaat.wt/yaat-server/../x", "..", "../", r"X:\dev\yaat.wt\"],
    );
    assert_outside_after_first("", &["X:/dev/yaat.wt"]);
    assert_outside("src/", &["../..", "../../yaat-server/../../x"]);
}

#[test]
fn an_absolute_path_that_climbs_from_the_root_into_a_sibling_is_inside() {
    assert_sent(
        "",
        &[
            ("X:/dev/yaat.wt/a/../yaat-server/src/x", "../yaat-server/src/x"),
            (r"X:\dev\yaat.wt\a\src\..\..\B\y\.\z", r"..\b\y\z"),
            ("X:/dev/yaat.wt/a/../b", "../b"),
            ("--project=X:/dev/yaat.wt/a/../b/c/../d", "--project=../b/d"),
        ],
        &[],
    );
    assert_sent("src/c/", &[(r"X:\dev\yaat.wt\a\..\yaat-server\x", r"..\..\..\yaat-server\x")], &[]);
    assert_sent("", &[(r"D:\yaat.wt\a\..\b\x", r"..\b\x")], &[('D', r"X:\dev")]);
    assert_outside(
        "",
        &[
            "X:/dev/yaat.wt/a/../other/x",
            "X:/dev/yaat.wt/a/..",
            "X:/dev/yaat.wt/a/../b/../x",
            "X:/dev/yaat.wt/a/../../x",
        ],
    );
}

#[test]
fn a_climb_into_a_sibling_keeps_a_trailing_separator_as_the_direct_path_does() {
    for (climbed, direct, sent) in [
        (r"X:\dev\yaat.wt\a\..\b\out\", r"X:\dev\yaat.wt\b\out\", r"..\b\out\"),
        ("X:/dev/yaat.wt/a/../b/", "X:/dev/yaat.wt/b/", "../b/"),
    ] {
        assert_sent("", &[(climbed, sent), (direct, sent)], &[]);
    }
}

#[test]
fn an_absolute_path_that_climbs_from_a_sibling_into_the_worktree_or_another_sibling_is_inside() {
    assert_sent(
        "",
        &[
            ("X:/dev/yaat.wt/yaat-server/../a/src/x.csproj", "src/x.csproj"),
            ("X:/dev/yaat.wt/yaat-server/../b/x", "../b/x"),
            ("X:/dev/yaat.wt/yaat-server/../A", "."),
            (r"X:\dev\yaat.wt\b\c\..\..\yaat-server\y\", r"..\yaat-server\y\"),
        ],
        &[],
    );
    assert_sent(
        "src/",
        &[
            ("X:/dev/yaat.wt/yaat-server/../a/src/x.csproj", "../src/x.csproj"),
            ("X:/dev/yaat.wt/b/../yaat-server/z", "../../yaat-server/z"),
        ],
        &[],
    );
    assert_outside(
        "",
        &[
            "X:/dev/yaat.wt/yaat-server/../undeclared/x",
            "X:/dev/yaat.wt/yaat-server/../a/../../x",
            "X:/dev/yaat.wt/b/..",
        ],
    );
}

#[test]
fn a_subst_drive_standing_for_a_root_never_climbs_past_it() {
    let sibling_drive = [('S', r"X:\dev\yaat.wt\yaat-server")];
    let root_drive = [('G', r"X:\dev\yaat.wt\a")];
    for (word, drives) in [(r"S:\..\a\src\x.csproj", &sibling_drive), (r"G:\..\b\x", &root_drive)] {
        let got = with_siblings("", &[word], drives);
        assert_eq!(got.outside.as_deref(), Some(word), "{word:?}: Windows stops `..` at a drive's root");
    }
    assert_sent("", &[(r"S:\src\x", r"..\yaat-server\src\x")], &sibling_drive);
    assert_sent("", &[(r"G:\src\x", "src\\x")], &root_drive);
}

/// The words of a gate at `prefix` in the worktree `X:/dev/gate`, its folder spelled `folder`, with D: standing for
/// `X:\dev` and S: for `D:\gate`, the sibling `yaat-server` declared beside the root.
fn in_gate(prefix: &str, folder: &str, word: &str) -> Rewritten {
    let place = Place {
        root: "X:/dev/gate".to_owned(),
        prefix: prefix.to_owned(),
        checkout: "gate".to_owned(),
        folder: folder.to_owned(),
    };
    let drives = [('S', r"D:\gate".to_owned()), ('D', r"X:\dev".to_owned())];
    let sibling = SiblingRoot {
        name: "yaat-server".to_owned(),
        root: "X:/dev/yaat-server".to_owned(),
    };
    rewrite(&place, &words(&[word]), &drives, &[sibling])
}

#[test]
fn a_relative_climb_past_the_root_of_the_drive_or_share_the_folder_is_spelled_on_is_outside() {
    for (prefix, folder, word) in [
        ("", r"S:\", r"..\yaat-server\src\x.csproj"),
        ("", r"\\?\S:\", r"..\yaat-server\src\x.csproj"),
        ("", r"\\host\gate\", r"..\yaat-server\x"),
        ("crates/", r"S:\crates", r"..\..\yaat-server\x"),
    ] {
        let got = in_gate(prefix, folder, word);
        assert_eq!(
            got.outside.as_deref(),
            Some(word),
            "{word:?} from {folder:?}: Windows stops `..` at the drive's root"
        );
    }
    for (folder, word) in [(r"S:\crates", r"..\src\x"), (r"X:\dev\gate\crates", r"..\..\yaat-server\x")] {
        let got = in_gate("crates/", folder, word);
        assert_eq!((got.words, got.outside), (words(&[word]), None), "{word:?} from {folder:?}");
    }
}

/// Asserts each word at the root is sent as written, none outside.
fn assert_unchanged(list: &[&str]) {
    for word in list {
        assert_sent("", &[(word, word)], &[]);
    }
}

#[test]
fn a_drive_relative_path_is_outside() {
    assert_outside("", &[r"C:..\..\secret", "C:", r"--x=C:..\y", r#""C:x""#]);
    assert_unchanged(&["-c:Release", "/t:Build", "$env:PATH", "https://h/p", "crate::mod", "10:30", "a/C:x"]);
}

#[test]
fn a_root_relative_path_is_outside() {
    assert_outside("", &[r"\src\x", r"-p:OutDir=\out\", r"\??\X:\x", "--x=/y", "@/x.rsp", r"type \x"]);
    assert_unchanged(&["/p:Configuration=Release", "/c", "test(/^run::/)", "a/b", "/nologo"]);
}

#[test]
fn a_share_or_device_path_is_outside_at_every_path_start() {
    assert_outside(
        "",
        &[
            r"--x=\\srv\s\x",
            r"-p:A=\\?\X:\dev\yaat.wt\a\src",
            "a;//srv/s",
            r"\/srv/s",
            r"/\srv\s",
            r"x\\?\X:\dev\yaat.wt\a\src",
            "x//./X:/dev/yaat.wt/a",
        ],
    );
    assert_unchanged(&["--uri=https://h/p"]);
}

#[test]
fn a_drive_relative_path_at_a_switch_colon_is_outside() {
    assert_outside("", &[r"-bl:C:..\x.binlog", r"--output:C:..\..\out", "-o:C:x", r"--x:C:..\y"]);
    assert_unchanged(&[
        "-c:Release",
        "-p:Configuration=Release",
        "/t:Build",
        "/p:X=1",
        "crate::mod",
        "$env:PATH",
        "-o:C",
    ]);
}

#[test]
fn a_switch_colon_starts_a_path_in_every_token() {
    assert_outside("", &[r"dotnet build -bl:..\x.binlog", r"Get-Content -Path:\secret", r"gc -Path:C:..\x"]);
    assert_unchanged(&["dotnet build -c:Release /t:Build"]);
}

#[test]
fn a_switch_colon_starts_a_path_after_a_quote_or_comma() {
    assert_outside(
        "",
        &[
            r#"dotnet build "-bl:C:..\x""#,
            r#"dotnet build "-bl:..\..\x""#,
            r"dotnet build '-o:..\..\out'",
            r"dotnet build a,-o:..\..\out",
        ],
    );
    assert_unchanged(&[r#"dotnet build "-c:Release""#]);
}

#[test]
fn an_at_sign_after_any_path_start_begins_a_value() {
    assert_outside("", &[r"dotnet build @..\..\x.rsp", r"type '@..\..\x.rsp'", "dotnet build @/x.rsp"]);
    assert_sent("src/", &[(r"dotnet build @..\x.rsp", r"dotnet build @..\x.rsp")], &[]);
    assert_unchanged(&["dotnet build @x.rsp", r"@..\b\x.rsp"]);
}

#[test]
fn each_kind_of_path_start_is_judged() {
    assert_outside(
        "",
        &[r"-p:\out", r"(..\..\x)", r"x,\y", r"x\\.\X:\dev\yaat.wt\a", "x//?/X:/dev/yaat.wt/a"],
    );
}

#[test]
fn a_climb_at_any_path_start_is_outside() {
    assert_outside(
        "",
        &[
            r"dotnet test X:\dev\yaat.wt\a\x.sln && type ..\..\secret",
            r"Get-Content ..\..\secret",
            r"@..\..\x.rsp",
            "Set-Location X:\\dev\\yaat.wt\\a\\x\n..\\..\\..\\y",
            r"a;..\..\x",
        ],
    );
    let script = with_siblings("", &["pwsh", "-c", r"Get-Content ..\..\secret"], &[]);
    assert_eq!(script.outside.as_deref(), Some(r"Get-Content ..\..\secret"));
    assert_sent(
        "",
        &[(r"dotnet test X:\dev\yaat.wt\a\x.sln && type ..\b\y", r"dotnet test x.sln && type ..\b\y")],
        &[],
    );
    assert_unchanged(&["@x.rsp", r"@..\b\x.rsp", r"type src\..\x"]);
}

#[test]
fn a_climb_compares_sibling_names_in_ascii_case_only() {
    let kelvin = rewrite(&place_at(""), &words(&["../\u{212a}/x"]), &[], &[sibling("k")]);
    assert_eq!(kelvin.outside.as_deref(), Some("../\u{212a}/x"), "the Kelvin sign is no k");
    let ascii = rewrite(&place_at(""), &words(&["../K/x"]), &[], &[sibling("k")]);
    assert_eq!(ascii.outside, None);
}

/// `path`'s segments below `base`, each `..` taking one off, as a reader of the rewritten word resolves it.
fn resolve(base: &[String], path: &str) -> Vec<String> {
    let mut segments = base.to_vec();
    for segment in path.split(['\\', '/']) {
        match segment {
            ".." => {
                segments.pop();
            }
            "" | "." => {}
            _ => segments.push(segment.to_owned()),
        }
    }
    segments
}

/// A folder or file name, spaces and dots included, never `.` or `..`.
fn segment() -> impl Strategy<Value = String> {
    // Win32 strips a name's trailing space or dot, and a token `..` after a space is a climb from a path start.
    "[a-z0-9][a-z0-9 ._-]{0,7}".prop_filter("no trailing space or dot token, no `..` token", |segment| {
        !segment.ends_with(' ') && !segment.ends_with(" .") && !segment.split(' ').any(|token| token == "..")
    })
}

proptest! {
    /// A path inside the worktree, written with either separator, rewritten from the gate's folder and joined back
    /// onto that folder names the same path below the root.
    #[test]
    fn a_rewritten_inside_path_names_the_same_path(
        path in prop::collection::vec(segment(), 1..5),
        folder in prop::collection::vec("[a-z][a-z0-9]{0,4}", 0..3),
        backslash in any::<bool>(),
    ) {
        let separator = if backslash { "\\" } else { "/" };
        let root = if backslash { r"X:\dev\yaat.wt\a" } else { ROOT };
        let prefix = folder.iter().fold(String::new(), |mut prefix, segment| {
            prefix.push_str(segment);
            prefix.push('/');
            prefix
        });
        let word = format!("{root}{separator}{}", path.join(separator));
        let got = rewrite(&place_at(&prefix), &[word], &[], &[]);
        prop_assert_eq!(&got.outside, &None);
        prop_assert_eq!(resolve(&folder, &got.words[0]), path);
    }
}

/// Asserts the words at the root rewrite to `expect`, with `outside` the first word judged outside the worktree, when
/// any.
fn assert_words(list: &[&str], expect: &[&str], outside: Option<&str>) {
    let got = rewrite(&place_at(""), &words(list), &[], &[]);
    assert_eq!(
        (got.words, got.outside, got.program_named),
        (words(expect), outside.map(str::to_owned), false),
        "{list:?}"
    );
}

/// A command's first word naming a program by an absolute path outside the worktree is sent as the program's name: its
/// last segment with a trailing `.exe`, in any letter case, removed.
#[test]
fn a_first_word_naming_a_program_outside_the_worktree_becomes_its_name() {
    for (given, name) in [
        (r"F:\Godot\Godot_console.exe", "Godot_console"),
        (r"C:\Program Files\dotnet\dotnet.exe", "dotnet"),
        ("c:/tools/Run.EXE", "Run"),
        (r"F:\tools\build.cmd", "build.cmd"),
        (r"F:\tools\tool", "tool"),
        (r"F:\tools\тест.exe", "тест"),
        (r"F:\tools\日本.exe", "日本"),
        (r"F:\tools\日本", "日本"),
    ] {
        let got = rewrite(&place_at(""), &words(&[given, "rest"]), &[], &[]);
        assert_eq!((got.words, got.outside), (words(&[name, "rest"]), None), "{given:?}");
        assert!(got.program_named, "{given:?} names a program");
    }
}

/// A first word that names no program by a drive path stays outside the worktree, sent as written, or stands as it is.
#[test]
fn a_first_word_naming_no_program_by_a_drive_path_stays_outside() {
    // A share, device, drive-relative or root-relative form names no program by an absolute path.
    assert_outside("", &[r"\\host\share\x.exe", r"\\?\C:\x.exe", "//host/x.exe", "C:x.exe", r"\x.exe"]);
    // A segment after the drive that is empty, `.` or `..` names no file by a path.
    assert_outside("", &[r"C:\a\\x.exe", r"C:\a\.\x.exe", r"C:\a\..\b\x.exe"]);
    // A trailing separator, or a name empty once a trailing `.exe` is removed, names no file.
    assert_outside("", &[r"F:\tools\", r"F:\tools\.exe"]);
    // A file name holding a wildcard, a redirect, a quote or a line break names no program.
    assert_outside("", &[r"F:\tools\x*.exe", r"F:\tools\a|b.exe", r"F:\tools\a?b.exe"]);
    assert_outside("", &[r#"F:\tools\a"b.exe"#, r"F:\tools\a<b.exe", r"F:\tools\a>b.exe"]);
    assert_outside("", &["F:\\tools\\a\nb.exe", "F:\\tools\\a\rb.exe"]);
    // A name holding an alternate stream's colon, white space, a trailing dot or space, or an 8.3 short name's `~` and
    // digit names no program, and neither does a word holding a second drive path.
    assert_outside(
        "",
        &[
            r"F:\tools\tool.exe:s",
            r"F:\tools\my tool.exe",
            r"F:\tools\tool.exe.",
            r"F:\tools\tool.exe ",
            r"F:\tools\TOOLNA~1.EXE",
            r"X:\a\x.exe;C:\other",
        ],
    );
    // A folder segment Win32 reads another way names no program either: a single trailing dot is dropped, an 8.3 short
    // name, a colon and a trailing space each name a folder the builder may not mean.
    assert_outside(
        "",
        &[
            r"X:\dev\yaat.wt\a.\tools\x.exe",
            r"X:\dev\YAAT~1.WT\a\.dotnet\dotnet.exe",
            r"X:\dev\yaat.wt\a::$INDEX_ALLOCATION\tools\x.exe",
            r"X:\dev\yaat.wt\a \tools\x.exe",
        ],
    );
    // A relative word is no drive path: one in the gate's folder stands as written, one climbing out is outside.
    assert_unchanged(&[r".\x.exe"]);
    assert_outside("", &[r"..\x\x.exe"]);
    // Only the first word is read this way: a path after it, a wrapping script's `--` included, is judged by the path
    // rules alone.
    assert_words(
        &["dotnet", "build", "-o", r"F:\out"],
        &["dotnet", "build", "-o", r"F:\out"],
        Some(r"F:\out"),
    );
    assert_words(
        &["pwsh", "-File", "x.ps1", "--", r#""C:\Program Files\dotnet\dotnet.exe""#, "test"],
        &["pwsh", "-File", "x.ps1", "--", r#""C:\Program Files\dotnet\dotnet.exe""#, "test"],
        Some(r#""C:\Program Files\dotnet\dotnet.exe""#),
    );
    // A first word inside the worktree stays a path, a subst drive standing for the root included.
    let subst = rewrite(&place_at(""), &words(&[r"D:\x"]), &[('D', r"X:\dev\yaat.wt\a".to_owned())], &[]);
    assert_eq!((subst.words, subst.outside, subst.program_named), (words(&["x"]), None, false));
}

#[test]
fn an_outside_word_names_the_path_that_is_outside() {
    let path_of = |prefix: &str, list: &[&str], drives: &[(char, String)]| {
        let got = rewrite(&place_at(prefix), &words(list), drives, &[sibling("b")]);
        assert!(got.outside.is_some(), "{list:?} names a path outside");
        got.outside_path
    };
    let named = |path: &str| Some(PathBuf::from(path));
    assert_eq!(
        path_of("", &["dotnet", "test", r"X:\dev\other.wt\c\src\x.csproj"], &[]),
        named(r"X:\dev\other.wt\c\src\x.csproj"),
        "a drive path into another folder"
    );
    assert_eq!(
        path_of("", &["dotnet", "test", "X:/dev/yaat.wt/b/../c/./y/"], &[]),
        named(r"X:\dev\yaat.wt\c\y"),
        "a drive path leaving a sibling, its climb resolved and written with backslashes"
    );
    assert_eq!(
        path_of("src/", &["dotnet", "test", r"..\..\other\x.csproj"], &[]),
        named(r"X:\dev\yaat.wt\other\x.csproj"),
        "a climb out of the root, resolved against the gate's folder"
    );
    assert_eq!(
        path_of("", &["dotnet", "build", r"-p:Root=..\other"], &[]),
        named(r"X:\dev\yaat.wt\other"),
        "a climb in a switch's value"
    );
    assert_eq!(
        path_of("", &["dotnet", "test", r"S:\..\src\x"], &[('S', r"X:\dev\other".to_owned())]),
        named(r"X:\dev\other\src\x"),
        "a path on a subst drive, named by what the drive stands for, its climb stopped at the drive's root"
    );
    assert_eq!(path_of("", &["dotnet", "test", r"\\host\share\x"], &[]), None, "a share path");
    assert_eq!(
        path_of("", &["dotnet", "test", r"U:\x"], &[('U', r"UNC\host\share".to_owned())]),
        None,
        "a subst drive standing for a share"
    );
    assert_eq!(path_of("", &["dotnet", "test", r"\\?\X:\dev\other\x"], &[]), None, "a device path");
    assert_eq!(
        path_of("", &["pwsh", "-c", r"Get-Content X:\dev\other\x; exit 1"], &[]),
        None,
        "a drive path in a script's text"
    );
    assert_eq!(
        path_of("", &["dotnet", "test", r"\\host\share\x", r"X:\dev\other\y"], &[]),
        None,
        "only the first outside word is read for its path"
    );
    let inside = rewrite(&place_at(""), &words(&["dotnet", "test", r"X:\dev\yaat.wt\a\x"]), &[], &[]);
    assert_eq!((inside.outside, inside.outside_path), (None, None), "a word inside the worktree");
}
