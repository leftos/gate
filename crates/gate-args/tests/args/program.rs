//! Program resolution over an injected PATH, PATHEXT, working folder and file lookup.

use gate_args::{ProgramSearch, resolve_program};

/// PATH with an extensionless-shim folder first, a trailing separator and an empty entry; PATHEXT in Windows' order.
const SEARCH: ProgramSearch<'static> = ProgramSearch {
    path: r"C:\d3;C:\d1\;;C:\d2",
    pathext: ".COM;.EXE;.BAT;.CMD",
    folder: r"X:\repo",
};

fn resolve(name: &str, files: &[&str]) -> Option<String> {
    resolve_in(&SEARCH, name, files)
}

fn resolve_in(search: &ProgramSearch<'_>, name: &str, files: &[&str]) -> Option<String> {
    resolve_program(name, search, |path| files.iter().any(|file| file.eq_ignore_ascii_case(path)))
}

#[test]
fn an_extensionless_shim_is_skipped() {
    assert_eq!(resolve("tool", &[r"C:\d3\tool", r"C:\d1\tool.exe"]).as_deref(), Some(r"C:\d1\tool.EXE"));
}

#[test]
fn pathext_order_decides_within_one_folder() {
    assert_eq!(
        resolve("tool", &[r"C:\d1\tool.cmd", r"C:\d1\tool.exe"]).as_deref(),
        Some(r"C:\d1\tool.EXE")
    );
}

#[test]
fn path_order_decides_before_pathext_order() {
    assert_eq!(
        resolve("tool", &[r"C:\d1\tool.cmd", r"C:\d2\tool.exe"]).as_deref(),
        Some(r"C:\d1\tool.CMD")
    );
}

#[test]
fn the_working_folder_is_not_searched() {
    assert_eq!(resolve("tool", &[r"X:\repo\tool.exe"]), None);
    assert_eq!(resolve("tool.exe", &[r"X:\repo\tool.exe"]), None);
}

#[test]
fn a_name_found_nowhere_is_none() {
    assert_eq!(resolve("tool", &[]), None);
    assert_eq!(resolve("tool.exe", &[]), None);
}

#[test]
fn a_name_with_an_extension_is_searched_on_path_as_written() {
    assert_eq!(
        resolve("tool.exe", &[r"C:\d1\tool.exe.EXE", r"C:\d2\tool.exe"]).as_deref(),
        Some(r"C:\d2\tool.exe")
    );
    assert_eq!(resolve(".tool", &[r"C:\d1\.tool.EXE", r"C:\d1\.tool"]).as_deref(), Some(r"C:\d1\.tool"));
}

#[test]
fn a_path_entry_that_is_not_absolute_is_skipped() {
    let search = ProgramSearch {
        path: r"bin;.;\rooted;C:relative;C:\d2",
        ..SEARCH
    };
    let files = [
        r"bin\tool.exe",
        r"X:\repo\bin\tool.exe",
        r".\tool.exe",
        r"\rooted\tool.exe",
        r"C:relative\tool.exe",
    ];
    assert_eq!(resolve_in(&search, "tool", &files), None);
    assert_eq!(
        resolve_in(&search, "tool", &[r"bin\tool.exe", r"C:\d2\tool.exe"]).as_deref(),
        Some(r"C:\d2\tool.EXE")
    );
}

#[test]
fn a_unc_path_entry_is_searched() {
    let search = ProgramSearch {
        path: r"\\server\share\bin",
        ..SEARCH
    };
    assert_eq!(
        resolve_in(&search, "tool", &[r"\\server\share\bin\tool.exe"]).as_deref(),
        Some(r"\\server\share\bin\tool.EXE")
    );
}

#[test]
fn a_name_with_a_directory_is_taken_as_written_made_absolute() {
    for (name, resolved) in [
        (r"C:\other\tool", r"C:\other\tool"),
        ("C:/other/tool", "C:/other/tool"),
        ("C:tool", "C:tool"),
        (r"\\server\share\run.cmd", r"\\server\share\run.cmd"),
        (r"tools\run.cmd", r"X:\repo\tools\run.cmd"),
        ("tools/run.cmd", r"X:\repo\tools\run.cmd"),
        (r".\tool", r"X:\repo\tool"),
        (r"..\other\run.cmd", r"X:\other\run.cmd"),
        (r"\bin\run.cmd", r"X:\bin\run.cmd"),
    ] {
        assert_eq!(resolve(name, &[r"C:\d1\tool.exe"]).as_deref(), Some(resolved), "name {name}");
    }
}

#[test]
fn a_root_folder_takes_no_second_separator() {
    let search = ProgramSearch { folder: r"X:\", ..SEARCH };
    assert_eq!(resolve_in(&search, r"a\b.cmd", &[]).as_deref(), Some(r"X:\a\b.cmd"));
}
