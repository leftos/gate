//! The path names a run never brings back, whatever its output globs: a component whose name starts with `.git`, and a
//! `.lfsconfig` at any depth, both as this PC's filesystem reads them, and a path that is not plain, which the outputs
//! archive's reader refuses. The sync-back and the outputs install refuse the `.git*` and `.lfsconfig` names, and the
//! walk, the pack and the unpack the paths that are not plain, so they live here.

use gate_router::is_reordering_mark;

/// The most `/`-separated parts a plain path may have, its file's name included.
pub const PATH_DEPTH_LIMIT: usize = 64;

/// What the name of a path's component starts with, its letter case aside, when git reads the file itself.
pub const GIT_NAME: &str = ".git";
/// The name of the file git-lfs reads its settings from in the worktree, its server's URL among them.
pub const LFS_CONFIG: &str = ".lfsconfig";
/// The name every `.git*` name starts with as an 8.3 alias reads it: upper case, its dot removed.
pub const GIT_ALIAS: &str = "GIT";
/// [`LFS_CONFIG`] as an 8.3 alias reads it: upper case, its dot removed.
pub const LFS_ALIAS: &str = "LFSCONFIG";

/// Whether the file `path` names is a `.lfsconfig`, its letter case aside, or any component of it looks like the 8.3
/// alias of one, each read by [`components`].
#[must_use]
pub fn is_lfs_config(path: &str) -> bool {
    is_spelled_lfs_config(path) || components(path).any(|name| alias_stem(name).is_some_and(|stem| LFS_ALIAS.starts_with(&stem)))
}

/// Whether the file `path` names, read by [`components`], is a `.lfsconfig`, its letter case aside; an 8.3 alias
/// aside.
#[must_use]
pub fn is_spelled_lfs_config(path: &str) -> bool {
    components(path).last().is_some_and(|name| name.eq_ignore_ascii_case(LFS_CONFIG))
}

/// The names of `path`'s components as Windows opens them: split at `/` and at `\`, each cut at its first `:`, after
/// which an alternate stream is named, and stripped of the trailing dots and spaces Windows drops.
pub fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split(['/', '\\'])
        .map(|part| part.split_once(':').map_or(part, |(name, _)| name).trim_end_matches(['.', ' ']))
}

/// What the component `name` holds before its first `~` followed by a digit, upper-cased, as an 8.3 alias such as
/// `GITATT~1` is shaped; `None` when it holds no such `~`.
#[must_use]
pub fn alias_stem(name: &str) -> Option<String> {
    let at = name.as_bytes().windows(2).position(|pair| pair[0] == b'~' && pair[1].is_ascii_digit())?;
    name.get(..at).map(str::to_ascii_uppercase)
}

/// Whether `path` has a component, read by [`components`], whose name starts with `.git`, its letter case aside, or
/// that looks like the 8.3 alias of such a name: what comes before its `~` and digit is a start of `GIT` or starts
/// with it.
#[must_use]
pub fn is_git_s(path: &str) -> bool {
    components(path).any(|part| starts_git(part) || alias_stem(part).is_some_and(|stem| GIT_ALIAS.starts_with(&stem) || stem.starts_with(GIT_ALIAS)))
}

/// Whether `path` has a component, read by [`components`], whose name starts with `.git`, its letter case aside; an
/// 8.3 alias aside.
#[must_use]
pub fn is_spelled_git_s(path: &str) -> bool {
    components(path).any(starts_git)
}

/// Whether the component `part` starts with `.git`, its letter case aside.
#[must_use]
pub fn starts_git(part: &str) -> bool {
    part.get(..GIT_NAME.len()).is_some_and(|start| start.eq_ignore_ascii_case(GIT_NAME))
}

/// Whether the `/`-separated `path` is relative, of at most [`PATH_DEPTH_LIMIT`] parts, each one [`is_plain_part`].
#[must_use]
pub fn is_plain_path(path: &str) -> bool {
    path.split('/').count() <= PATH_DEPTH_LIMIT && path.split('/').all(is_plain_part)
}

/// Whether `part` names a file or a folder that Windows would create under that very name and git does not own: not
/// empty, not ending in `.` or a space (so neither `.` nor `..`), with no `\ : < > " | ? *`, control character or
/// bidirectional mark, no `~` before a digit (an 8.3 short name), not `.git` in any case, and not a device name.
#[must_use]
pub fn is_plain_part(part: &str) -> bool {
    !part.is_empty()
        && !part.ends_with(['.', ' '])
        && !part.contains(['\\', ':', '<', '>', '"', '|', '?', '*'])
        && !part.chars().any(|character| character.is_control() || is_reordering_mark(character))
        && !part.as_bytes().windows(2).any(|pair| pair[0] == b'~' && pair[1].is_ascii_digit())
        && !part.trim_end_matches(['.', ' ']).eq_ignore_ascii_case(".git")
        && !is_device(part)
}

/// Whether the name `part` opens a Windows device, whatever its extension: `CON`, `PRN`, `AUX`, `NUL`, `CONIN$`,
/// `CONOUT$`, or `COM` or `LPT` and one digit, `0` to `9`, `¹`, `²` or `³`.
#[must_use]
pub fn is_device(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ').to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|digit| matches!(digit, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
    };
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT")
}
