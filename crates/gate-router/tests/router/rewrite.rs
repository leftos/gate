//! The in-worktree path rewrite. [`POWERSHELL_TABLE`] is `remote.selftest.ps1`'s `Get-RoutePathTable`, row for row,
//! with the worktree `X:/dev/yaat.wt/a`; [`SPACES_GLOBS_AND_FLAGS_TABLE`] holds the cases the PowerShell rewrite gets
//! wrong.

use gate_router::rewrite_words;
use proptest::prelude::*;

use crate::support::{ROOT, words};

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
    sent(
        "",
        &["Set-Location X:\\dev\\yaat.wt\\a\\x\n..\\..\\..\\y"],
        &["Set-Location x\n..\\..\\..\\y"],
    ),
    outside("", &[r"X:\dev\yaat.wt\a.old\x"], r"X:\dev\yaat.wt\a.old\x"),
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
    sent(
        "",
        &["/p:Configuration=Release", "/c", r"\foo", "a/b"],
        &["/p:Configuration=Release", "/c", r"\foo", "a/b"],
    ),
    outside("", &["dotnet", "X:/dev/yaat.wt/ab/tests"], "X:/dev/yaat.wt/ab/tests"),
    outside("", &["dotnet", "X:/dev/yaat/tests"], "X:/dev/yaat/tests"),
    outside("", &["dotnet", r"\\server\share\x"], r"\\server\share\x"),
    outside("", &["dotnet", "//server/share/x"], "//server/share/x"),
    outside("", &["dotnet", r"X:\"], r"X:\"),
    outside("", &["dotnet", r"D:\yaat.wt\a\tests"], r"D:\yaat.wt\a\tests"),
    outside(
        "",
        &[r"C:\Program Files\dotnet\dotnet.exe", "test"],
        r"C:\Program Files\dotnet\dotnet.exe",
    ),
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
    on_drives(outside("", &[r"D:\yaat.wt\ab\x"], r"D:\yaat.wt\ab\x"), DEV),
    on_drives(outside("", &[r"Q:\yaat.wt\a\x"], r"Q:\yaat.wt\a\x"), DEV),
    on_drives(outside("", &[r"D:\v\yaat.wt\a\x"], r"D:\v\yaat.wt\a\x"), &[('D', r"X:\de")]),
];

/// The cases the PowerShell rewrite gets wrong: one argument holding a path with spaces runs through them, while a
/// script's text still ends a path at white space; globs and flags that are not paths are left alone.
const SPACES_GLOBS_AND_FLAGS_TABLE: &[Row] = &[
    sent("", &["X:/dev/yaat.wt/a/my dir/x.cs"], &["my dir/x.cs"]),
    sent("src/", &[r"X:\dev\yaat.wt\a\my dir\x.cs"], &[r"..\my dir\x.cs"]),
    sent("my dir/", &["X:/dev/yaat.wt/a/my dir"], &["."]),
    sent("", &["--project=X:/dev/yaat.wt/a/my dir/p.csproj"], &["--project=my dir/p.csproj"]),
    outside("", &[r"X:\dev\yaat.wt\a\b c\..\..\x"], r"X:\dev\yaat.wt\a\b c\..\..\x"),
    outside("", &["X:/dev/yaat.wt/a old/x"], "X:/dev/yaat.wt/a old/x"),
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
            let got = rewrite_words(ROOT, row.prefix, &words(row.words), &drives);
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
    let got = rewrite_words("X:/my wt", "", &words(&["X:/my wt/a", r"X:\my wt"]), &[]);
    assert_eq!(got.words, ["a", "."]);
    assert_eq!(got.outside, None);
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
    "[a-z0-9][a-z0-9 ._-]{0,7}"
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
        let got = rewrite_words(ROOT, &prefix, &[word], &[]);
        prop_assert_eq!(&got.outside, &None);
        prop_assert_eq!(resolve(&folder, &got.words[0]), path);
    }
}
