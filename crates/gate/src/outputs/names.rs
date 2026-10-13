//! The path names a run never brings back, whatever its output globs: a component whose name starts with `.git`, and a
//! `.lfsconfig` at any depth, both as this PC's filesystem reads them. The sync-back and the outputs install refuse the
//! same names, so they live here.

/// What the name of a path's component starts with, its letter case aside, when git reads the file itself.
const GIT_NAME: &str = ".git";
/// The name of the file git-lfs reads its settings from in the worktree, its server's URL among them.
const LFS_CONFIG: &str = ".lfsconfig";
/// The name every `.git*` name starts with as an 8.3 alias reads it: upper case, its dot removed.
const GIT_ALIAS: &str = "GIT";
/// [`LFS_CONFIG`] as an 8.3 alias reads it: upper case, its dot removed.
const LFS_ALIAS: &str = "LFSCONFIG";

/// Whether the file `path` names is a `.lfsconfig`, its letter case aside, or any component of it looks like the 8.3
/// alias of one, each read by [`components`].
pub(crate) fn is_lfs_config(path: &str) -> bool {
    components(path).last().is_some_and(|name| name.eq_ignore_ascii_case(LFS_CONFIG))
        || components(path).any(|name| alias_stem(name).is_some_and(|stem| LFS_ALIAS.starts_with(&stem)))
}

/// The names of `path`'s components as Windows opens them: split at `/` and at `\`, each cut at its first `:`, after
/// which an alternate stream is named, and stripped of the trailing dots and spaces Windows drops.
fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split(['/', '\\'])
        .map(|part| part.split_once(':').map_or(part, |(name, _)| name).trim_end_matches(['.', ' ']))
}

/// What the component `name` holds before its first `~` followed by a digit, upper-cased, as an 8.3 alias such as
/// `GITATT~1` is shaped; `None` when it holds no such `~`.
fn alias_stem(name: &str) -> Option<String> {
    let at = name.as_bytes().windows(2).position(|pair| pair[0] == b'~' && pair[1].is_ascii_digit())?;
    name.get(..at).map(str::to_ascii_uppercase)
}

/// Whether `path` has a component, read by [`components`], whose name starts with `.git`, its letter case aside, or
/// that looks like the 8.3 alias of such a name: what comes before its `~` and digit is a start of `GIT` or starts
/// with it.
pub(crate) fn is_git_s(path: &str) -> bool {
    components(path).any(|part| {
        part.get(..GIT_NAME.len()).is_some_and(|start| start.eq_ignore_ascii_case(GIT_NAME))
            || alias_stem(part).is_some_and(|stem| GIT_ALIAS.starts_with(&stem) || stem.starts_with(GIT_ALIAS))
    })
}
