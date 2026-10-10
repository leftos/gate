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
/// written. Any other name is looked for in each absolute `PATH` folder in order (a relative entry is skipped): a name
/// with an extension as written, `<folder>\<name>`; a bare name with each `PATHEXT` extension in order within each
/// folder, `<folder>\<name><ext>`, so an extensionless file (a shim) is never found. The first that `is_file` finds
/// wins.
#[must_use]
pub fn resolve_program(name: &str, search: &ProgramSearch<'_>, is_file: impl Fn(&str) -> bool) -> Option<String> {
    if has_directory(name) {
        return Some(absolute(name, search.folder));
    }
    let extensions: Vec<&str> = if has_extension(name) {
        vec![""]
    } else {
        search.pathext.split(';').filter(|extension| !extension.is_empty()).collect()
    };
    search.path.split(';').filter(|folder| is_absolute(folder)).find_map(|folder| {
        let folder = folder.trim_end_matches(is_separator);
        extensions
            .iter()
            .map(|extension| format!("{folder}\\{name}{extension}"))
            .find(|candidate| is_file(candidate))
    })
}

/// Whether `folder` is a full path: a drive and a separator (`C:\`), or a UNC path.
fn is_absolute(folder: &str) -> bool {
    is_unc(folder) || (has_drive(folder) && folder[2..].starts_with(is_separator))
}

fn is_separator(character: char) -> bool {
    matches!(character, '\\' | '/')
}

/// Whether the name's last segment has a `.` that is not its last character, as .NET's `Path.GetExtension` reads it.
fn has_extension(name: &str) -> bool {
    let segment = name.rsplit(is_separator).next().unwrap_or(name);
    segment.rfind('.').is_some_and(|dot| dot + 1 < segment.len())
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
