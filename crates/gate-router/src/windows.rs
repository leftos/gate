//! The repo's Windows declaration, `.gate/windows.toml`, read from its bytes ([`read_windows`]): the `-like` patterns
//! of the paths whose change keeps a run off a macOS builder, and the first changed path one of them matches
//! ([`first_touched`]). Nothing here reads a file or runs git; the caller hands over the bytes and the changed paths.
//!
//! The file holds one key, `paths`, an array of at most [`MOST_PATHS`] non-empty strings, each a valid `-like` pattern
//! ([`Pattern`]) matched against a whole `/`-separated path relative to the worktree root, letter case ignored and `*`
//! crossing `/`; an empty array declares none. An entry starting `../{name}/` matches the changed paths of the declared
//! sibling `name`, each read in the sibling's own root and written with that prefix ([`sibling_path`]); an entry whose
//! `{name}` is no declared sibling is a problem, as is any other key. The reader stops at the first problem it finds:
//! the count of entries is checked against the cap before any entry, then the entries in file order.

use crate::escape::escaped;
use crate::like::Pattern;
use crate::list::read_list;

/// The name of the Windows declaration in `.gate`.
pub const WINDOWS_NAME: &str = "windows.toml";

/// The most paths a Windows file may declare.
pub const MOST_PATHS: usize = 32;

/// What the change of a worktree, and of the siblings its Windows file names, is to that file's paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowsChange {
    /// The worktree has no `.gate/windows.toml`, or the change touches none of its paths.
    Untouched,
    /// The change touches `path`, the first changed path one of the file's paths matches.
    Touched { path: String },
    /// Whether the change touches the file's paths cannot be told, for `why`.
    Unknown { why: String },
}

impl WindowsChange {
    /// Why a run of this change is kept off a macOS builder, naming the Windows file as `file` (how the lines name the
    /// file read: `.gate/windows.toml`, or the declarations-folder copy's path), the file, path and reason shown
    /// [`escaped`]; `None` for [`WindowsChange::Untouched`].
    #[must_use]
    pub fn reason(&self, file: &str) -> Option<String> {
        let file = escaped(file);
        match self {
            Self::Untouched => None,
            Self::Touched { path } => Some(format!("the change touches {} ({file})", escaped(path))),
            Self::Unknown { why } => Some(format!("cannot tell whether the change touches {file}'s paths: {}", escaped(why))),
        }
    }
}

/// The paths the Windows file whose bytes are `bytes` declares, in file order, a leading byte order mark skipped,
/// each `../{name}/{path}` entry's `name` one of the declared `siblings`; `siblings` is `Err` with how the lines name
/// the siblings file when that file has a problem, and then declares none.
///
/// # Errors
///
/// The file's first problem, such as `paths[1] is empty`: bytes that are not UTF-8 or not TOML, a key other than
/// `paths`, no `paths`, a `paths` that is not an array, an entry that is not a string, is empty, is no valid wildcard
/// pattern, names a sibling but no path in it, names a sibling not declared or one while the siblings file has a
/// problem, or more than [`MOST_PATHS`] entries.
pub fn read_windows(bytes: &[u8], siblings: Result<&[&str], &str>) -> Result<Vec<Pattern>, String> {
    read_list(bytes, "paths", MOST_PATHS, |index, text| path_of(index, text, siblings))
}

/// The `-like` pattern the `paths` entry `text` at `index` gives; `Err` with its problem when it names a sibling but
/// no path in it, names a sibling not among `siblings` or while the siblings file has a problem, or is no valid
/// wildcard pattern.
fn path_of(index: usize, text: &str, siblings: Result<&[&str], &str>) -> Result<Pattern, String> {
    if let Some(rest) = text.strip_prefix("../") {
        let Some((name, _)) = rest.split_once('/').filter(|(_, path)| !path.is_empty()) else {
            return Err(format!("paths[{index}] names a sibling but no path in it"));
        };
        match siblings {
            Err(file) => return Err(format!("paths[{index}] names a sibling, but {file} has a problem")),
            Ok(names) if !names.contains(&name) => {
                return Err(format!(
                    "paths[{index}] names the sibling {name:?}, which .gate/siblings.toml does not declare"
                ));
            }
            Ok(_) => {}
        }
    }
    Pattern::parse(text).map_err(|_| format!("paths[{index}] is not a valid wildcard pattern: {text:?}"))
}

/// The entries of `patterns` that the changed paths of `sibling` are matched against: those starting `../{name}/` for
/// the sibling `name`, and for the worktree's own (`None`) those that do not start `../`, so an entry of one never
/// matches a path of another, as a root's `*.props` would a sibling's `../{name}/Directory.Build.props`.
#[must_use]
pub fn entries_for(patterns: &[Pattern], sibling: Option<&str>) -> Vec<Pattern> {
    patterns
        .iter()
        .filter(|pattern| {
            let text = pattern.as_str();
            match sibling {
                None => !text.starts_with("../"),
                Some(name) => text
                    .strip_prefix("../")
                    .and_then(|rest| rest.strip_prefix(name))
                    .is_some_and(|rest| rest.starts_with('/')),
            }
        })
        .cloned()
        .collect()
}

/// A changed path `path` of the declared sibling `name`, relative to the sibling's root, as the file's entries name it:
/// `../{name}/{path}`.
#[must_use]
pub fn sibling_path(name: &str, path: &str) -> String {
    format!("../{name}/{path}")
}

/// The first of `paths`, in their order, that one of `patterns` matches whole, letter case ignored.
#[must_use]
pub fn first_touched<'p>(patterns: &[Pattern], paths: &'p [String]) -> Option<&'p str> {
    paths
        .iter()
        .find(|path| patterns.iter().any(|pattern| pattern.is_match(path)))
        .map(String::as_str)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a fixture that does not read must fail loudly")]
mod tests {
    use super::{WindowsChange, entries_for, first_touched, read_windows, sibling_path};
    use crate::like::Pattern;

    const SIBLINGS: [&str; 1] = ["yaat-server"];

    fn texts(bytes: &[u8]) -> Vec<String> {
        read_windows(bytes, Ok(&SIBLINGS))
            .expect("the declaration reads")
            .iter()
            .map(|pattern| pattern.as_str().to_owned())
            .collect()
    }

    fn problem(bytes: &[u8]) -> String {
        read_windows(bytes, Ok(&SIBLINGS)).expect_err("the declaration has a problem")
    }

    fn patterns(texts: &[&str]) -> Vec<Pattern> {
        texts.iter().map(|text| Pattern::parse(text).expect("the pattern is valid")).collect()
    }

    fn paths(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    #[test]
    fn the_paths_read_in_order_a_byte_order_mark_skipped() {
        assert_eq!(
            texts(b"\xef\xbb\xbfpaths = ['tools/Hook/*', '../yaat-server/tools/X/*']\n"),
            ["tools/Hook/*", "../yaat-server/tools/X/*"]
        );
        assert_eq!(texts(b"paths = []\n"), Vec::<String>::new());
    }

    #[test]
    fn more_than_thirty_two_paths_is_a_problem() {
        let list = |count: usize| (0..count).map(|index| format!("'p{index}'")).collect::<Vec<_>>().join(", ");
        assert_eq!(texts(format!("paths = [{}]\n", list(32)).as_bytes()).len(), 32);
        assert_eq!(problem(format!("paths = [{}]\n", list(33)).as_bytes()), "declares more than 32 paths");
        assert_eq!(
            problem(format!("paths = ['', {}]\n", list(32)).as_bytes()),
            "declares more than 32 paths",
            "the cap is checked before any entry"
        );
    }

    #[test]
    fn each_bad_shape_or_entry_is_a_problem_and_the_first_wins() {
        for (text, why) in [
            ("", "declares no paths"),
            ("paths = 'a*'\n", "paths is not an array (TOML string)"),
            ("paths = ['a*', 3]\n", "paths[1] is not a string (TOML integer)"),
            ("paths = ['', 3]\n", "paths[0] is empty"),
            ("paths = ['a', '']\n", "paths[1] is empty"),
            ("paths = ['a[']\n", "paths[0] is not a valid wildcard pattern: \"a[\""),
            ("patterns = ['a']\n", "patterns is not a key (paths)"),
        ] {
            assert_eq!(problem(text.as_bytes()), why, "{text:?}");
        }
    }

    #[test]
    fn an_entry_naming_a_sibling_not_declared_is_a_problem() {
        for (text, why) in [
            (
                "paths = ['a/*', '../other/tools/*']\n",
                "paths[1] names the sibling \"other\", which .gate/siblings.toml does not declare",
            ),
            (
                "paths = ['../Yaat-Server/*']\n",
                "paths[0] names the sibling \"Yaat-Server\", which .gate/siblings.toml does not declare",
            ),
        ] {
            assert_eq!(problem(text.as_bytes()), why, "{text:?}");
        }
        assert_eq!(
            read_windows(b"paths = ['../yaat-server/a/*']\n", Ok(&[])),
            Err("paths[0] names the sibling \"yaat-server\", which .gate/siblings.toml does not declare".to_owned()),
            "no sibling declared"
        );
    }

    #[test]
    fn an_entry_naming_a_sibling_but_no_path_in_it_is_a_problem() {
        for text in [
            "paths = ['a/*', '../yaat-server']\n",
            "paths = ['a/*', '../yaat-server/']\n",
            "paths = ['a/*', '../yaat-server*']\n",
        ] {
            assert_eq!(problem(text.as_bytes()), "paths[1] names a sibling but no path in it", "{text:?}");
        }
    }

    #[test]
    fn an_entry_naming_a_sibling_while_the_siblings_file_has_a_problem_names_that_file() {
        assert_eq!(
            read_windows(b"paths = ['a/*', '../yaat-server/a/*']\n", Err(".gate/siblings.toml")),
            Err("paths[1] names a sibling, but .gate/siblings.toml has a problem".to_owned())
        );
        assert_eq!(
            read_windows(b"paths = ['a/*']\n", Err(".gate/siblings.toml")).map(|read| read.len()),
            Ok(1),
            "an entry naming no sibling still reads"
        );
    }

    #[test]
    fn a_change_is_named_after_the_file_read_escaped() {
        let touched = WindowsChange::Touched {
            path: "tools/a\nb.cs".to_owned(),
        };
        assert_eq!(
            touched.reason(".gate/windows.toml").as_deref(),
            Some(r"the change touches tools/a\nb.cs (.gate/windows.toml)")
        );
        let unknown = WindowsChange::Unknown { why: "no base".to_owned() };
        assert_eq!(
            unknown.reason(r"X:\decl\yaat\windows.toml").as_deref(),
            Some(r"cannot tell whether the change touches X:\decl\yaat\windows.toml's paths: no base")
        );
        assert_eq!(WindowsChange::Untouched.reason(".gate/windows.toml"), None);
    }

    #[test]
    fn the_first_changed_path_a_pattern_matches_is_touched() {
        let declared = patterns(&["tools/Hook/*", "src/Win.props"]);
        assert_eq!(
            first_touched(&declared, &paths(&["README.md", "tools/Hook/a.cs"])),
            Some("tools/Hook/a.cs")
        );
        assert_eq!(first_touched(&declared, &paths(&["README.md", "tools/Other/a.cs"])), None, "no match");
        assert_eq!(first_touched(&declared, &[]), None, "no change");
        assert_eq!(first_touched(&[], &paths(&["tools/Hook/a.cs"])), None, "no pattern");
    }

    #[test]
    fn a_path_matches_whole_with_its_case_ignored_and_a_star_crossing_slashes() {
        let declared = patterns(&["tools/Hook/*"]);
        assert_eq!(
            first_touched(&declared, &paths(&["TOOLS/hook/A.cs"])),
            Some("TOOLS/hook/A.cs"),
            "case ignored"
        );
        assert_eq!(
            first_touched(&declared, &paths(&["tools/Hook/deep/er/a.cs"])),
            Some("tools/Hook/deep/er/a.cs")
        );
        assert_eq!(first_touched(&declared, &paths(&["x/tools/Hook/a.cs"])), None, "the whole path matches");
        assert_eq!(first_touched(&patterns(&["src/Win.props"]), &paths(&["src/Win.props.bak"])), None);
    }

    #[test]
    fn a_sibling_path_matches_only_with_its_prefix() {
        let declared = patterns(&["../yaat-server/tools/X/*"]);
        assert_eq!(first_touched(&declared, &paths(&["tools/X/a.cs"])), None, "the root's own path");
        let prefixed = sibling_path("yaat-server", "tools/X/a.cs");
        assert_eq!(prefixed, "../yaat-server/tools/X/a.cs");
        assert_eq!(first_touched(&declared, std::slice::from_ref(&prefixed)), Some(prefixed.as_str()));
        let texts = |entries: Vec<Pattern>| entries.iter().map(|entry| entry.as_str().to_owned()).collect::<Vec<_>>();
        assert_eq!(texts(entries_for(&declared, Some("yaat-server"))), ["../yaat-server/tools/X/*"]);
        assert!(entries_for(&declared, Some("yaat")).is_empty(), "a name the entry only starts with");
        assert!(entries_for(&declared, None).is_empty(), "a sibling-only file never reads the root");
    }

    #[test]
    fn a_root_entry_never_matches_a_sibling_path_nor_a_sibling_entry_a_root_one() {
        let declared = patterns(&["*.props", "../yaat-server/tools/*"]);
        let sibling = [sibling_path("yaat-server", "Directory.Build.props")];
        assert_eq!(
            first_touched(&declared, &sibling),
            Some("../yaat-server/Directory.Build.props"),
            "all entries at once"
        );
        assert_eq!(first_touched(&entries_for(&declared, Some("yaat-server")), &sibling), None);
        assert_eq!(
            first_touched(&entries_for(&declared, None), &paths(&["Directory.Build.props"])),
            Some("Directory.Build.props")
        );
        assert_eq!(first_touched(&entries_for(&declared, None), &paths(&["../yaat-server/tools/a.cs"])), None);
    }

    #[test]
    fn the_first_changed_path_in_input_order_wins() {
        let declared = patterns(&["b/*", "a/*"]);
        let changed = paths(&["c/x", "a/1", "b/2"]);
        assert_eq!(first_touched(&declared, &changed), Some("a/1"), "input order, not pattern order");
    }
}
