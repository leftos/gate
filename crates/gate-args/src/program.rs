//! The program a command's first word names, found as Windows paths are read, over an injected PATH, PATHEXT, working
//! folder and file lookup, so the rules build and test anywhere.

/// Where a bare program name is looked for, and what a relative one is read against.
#[derive(Clone, Copy, Debug)]
pub struct ProgramSearch<'a> {
    /// The command's `PATH`: folders separated by `;`.
    pub path: &'a str,
    /// The command's `PATHEXT`: extensions with their dot, separated by `;`.
    pub pathext: &'a str,
    /// The working folder, absolute.
    pub folder: &'a str,
}

/// The file the command's first word names, the way the PowerShell gate's `Resolve-Runnable` and `Get-CommandLine` find
/// it, but never in the working folder and never passed on unfound; `None` when no file is found.
///
/// A name with a directory part is taken as written and read against the working folder: `.` and `..` folded,
/// separators written `\`; a name with a drive (`C:\x`, or `C:x`, which stays drive-relative) or a UNC name is left as
/// written. Any other name is looked for in each absolute `PATH` folder in order (a relative entry is skipped), the
/// folders outer and the candidates within one folder inner:
///
/// - with no extension, each `PATHEXT` extension in order, `<folder>\<name><ext>`, so an extensionless file (a shim) is
///   never found;
/// - with an extension that is one of `PATHEXT`'s, the extension compared whole, its leading dot included, against
///   whole `PATHEXT` entries and ignoring ASCII case (so a dotless entry such as `EXE` matches nothing), as written
///   only, `<folder>\<name>`, so a name ending in `.cmd` never gains a second extension;
/// - with any other extension, a version-holding name such as `Godot_v4.7.2-stable_mono_win64_console` among them, as
///   written, `<folder>\<name>`, then with `.exe` appended, `<folder>\<name>.exe`; no other `PATHEXT` extension is
///   appended.
///
/// The first that `is_file` finds wins.
#[must_use]
pub fn resolve_program(name: &str, search: &ProgramSearch<'_>, is_file: impl Fn(&str) -> bool) -> Option<String> {
    if has_directory(name) {
        return Some(absolute(name, search.folder));
    }
    let extensions: Vec<&str> = match extension(name) {
        None => search.pathext.split(';').filter(|extension| !extension.is_empty()).collect(),
        Some(extension) if is_pathext_extension(extension, search.pathext) => vec![""],
        Some(_) => vec!["", ".exe"],
    };
    search.path.split(';').filter(|folder| is_absolute(folder)).find_map(|folder| {
        let folder = folder.trim_end_matches(is_separator);
        extensions
            .iter()
            .map(|extension| format!("{folder}\\{name}{extension}"))
            .find(|candidate| is_file(candidate))
    })
}

/// Whether the command's first word `word` names a Godot console program by name alone: no directory part, and a file
/// stem, the name without a trailing `.exe`, that matches `Godot*console*` with no `.` after its last `console`, ASCII
/// case ignored. A version-holding name such as `Godot_v4.7.2-stable_mono_win64_console.exe` matches, and so does
/// `Godot_console.exe`, `godot_console.EXE` and `godot_CONSOLE`; a name with any other extension, as
/// `Godot_console.cmd`, `.bat` or `.ps1`, does not.
#[must_use]
pub fn is_godot_console(word: &str) -> bool {
    if has_directory(word) {
        return false;
    }
    let lower = word.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    stem.strip_prefix("godot")
        .and_then(|rest| rest.rfind("console").map(|at| &rest[at..]))
        .is_some_and(|tail| !tail.contains('.'))
}

/// Whether `folder` is a full path: a drive and a separator (`C:\`), or a UNC path.
fn is_absolute(folder: &str) -> bool {
    is_unc(folder) || (has_drive(folder) && folder[2..].starts_with(is_separator))
}

fn is_separator(character: char) -> bool {
    matches!(character, '\\' | '/')
}

/// The name's extension, its last `.` and what follows it, when that dot is not the segment's last character, as .NET's
/// `Path.GetExtension` reads one; `None` for a name without one.
fn extension(name: &str) -> Option<&str> {
    let segment = name.rsplit(is_separator).next().unwrap_or(name);
    segment.rfind('.').filter(|dot| dot + 1 < segment.len()).map(|dot| &segment[dot..])
}

/// Whether `extension`, the dot included, is one of `pathext`'s entries, compared ignoring ASCII case.
fn is_pathext_extension(extension: &str, pathext: &str) -> bool {
    pathext.split(';').any(|entry| entry.eq_ignore_ascii_case(extension))
}

fn has_directory(name: &str) -> bool {
    name.contains(is_separator) || has_drive(name)
}

fn has_drive(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_unc(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(is_separator) && characters.next().is_some_and(is_separator)
}

/// `name` read against `folder`. A name with a drive (`C:\x`, or `C:x`, which Windows reads against that drive's own
/// folder) or a UNC name is left as written; a rooted name takes the folder's root.
fn absolute(name: &str, folder: &str) -> String {
    if has_drive(name) || is_unc(name) {
        return name.to_owned();
    }
    let root = root_of(folder);
    let rest = if name.starts_with(is_separator) {
        name.to_owned()
    } else {
        format!("{}\\{name}", &folder[root.len()..])
    };
    folded(root, &rest)
}

/// The root of an absolute folder: its drive (`X:`) or its UNC share (`\\server\share`).
fn root_of(folder: &str) -> &str {
    if has_drive(folder) {
        return &folder[..2];
    }
    if is_unc(folder) {
        let share_end = folder[2..]
            .match_indices(is_separator)
            .nth(1)
            .map_or(folder.len(), |(index, _)| index + 2);
        return &folder[..share_end];
    }
    ""
}

/// `root` and the segments of `rest`, `.` and empty segments dropped and `..` taking the one before it off.
fn folded(root: &str, rest: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split(is_separator) {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    format!("{root}\\{}", segments.join("\\"))
}

#[cfg(test)]
mod tests {
    use super::is_godot_console;

    #[test]
    fn a_bare_godot_console_name_is_one_in_any_ascii_case_with_or_without_exe() {
        for word in [
            "Godot_console.exe",
            "Godot_console",
            "godot_console.EXE",
            "godot_CONSOLE",
            "GODOTCONSOLE.EXE",
            "Godot_v4.7.2-stable_mono_win64_console.exe",
            "Godot_v4.7.2-stable_mono_win64_console",
            "Godot_console_extra.exe",
        ] {
            assert!(is_godot_console(word), "{word}");
        }
    }

    #[test]
    fn a_name_with_a_directory_part_or_another_stem_is_no_godot_console() {
        for word in [
            r"C:\Godot\Godot_console.exe",
            r".\Godot_console.exe",
            "bin/Godot_console.exe",
            "C:Godot_console.exe",
            "Godot.exe",
            "Godot_v4.7.2-stable_mono_win64.exe",
            "my_Godot_console.exe",
            "console_Godot.exe",
            "dotnet",
            "",
        ] {
            assert!(!is_godot_console(word), "{word}");
        }
    }

    /// A Godot console name ending in an extension other than `.exe` is a script or another file, not the console
    /// program an install holds, so it is never taken for one.
    #[test]
    fn a_godot_console_name_with_another_extension_is_none() {
        for word in [
            "godot_console.cmd",
            "Godot_console.bat",
            "Godot_console.ps1",
            "Godot_console.exe.cmd",
            "Godot_v4.7.2-stable_mono_win64_console.com",
        ] {
            assert!(!is_godot_console(word), "{word}");
        }
    }
}
