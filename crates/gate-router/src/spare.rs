//! The repo's spare declaration, `.gate/spare.toml`, read from its bytes ([`read_spare`]): the folders below the
//! worktree root whose files the box's clear after an interrupted command leaves alone, and which folders a list of
//! them spares ([`Spared`]). Nothing here reads a file; the caller hands over its bytes.
//!
//! The file holds one key, `folders`, an array of at most [`MOST_FOLDERS`] strings, each a folder path below the
//! worktree root as a `carriedFolders` entry is written: `/`-separated names, none empty, `.` or `..`, none holding
//! `**`, no `\`, `:`, `[`, `]`, `"`, `<`, `>`, `|` or backtick, with `*` and `?` matching within one name in any letter
//! case. An entry matches a folder of as many names, and spares the whole subtree below it. An empty array declares
//! none. Any other key is a problem. The reader stops at the first problem it finds: the count of entries is checked
//! against the cap before any entry, then the entries in file order. A declaration the gate cannot read is said
//! ([`problem_line`]) and the run routes with no folder spared.

use crate::config::FolderGlob;
use crate::decide::fold_lines;
use crate::escape::escaped;
use crate::list::read_list;

/// The most folders a spare file may declare.
pub const MOST_FOLDERS: usize = 32;

/// What a folder path must be, as a problem says it.
const FOLDER_RULE: &str = r#"names separated by /, * and ? within a name, no . or .. name, no ** and none of \ : [ ] " < > | `"#;

/// The folders the spare file whose bytes are `bytes` declares, in file order, as written, a leading byte order mark
/// skipped.
///
/// # Errors
///
/// The file's first problem, such as `folders is not an array (TOML string)`: bytes that are not UTF-8 or not TOML, a
/// key other than `folders`, no `folders`, a `folders` that is not an array, an entry that is not a string, is empty or
/// is no folder path (`entry <n> '<e>' is not a folder path: <why>`), or more than [`MOST_FOLDERS`] entries.
pub fn read_spare(bytes: &[u8]) -> Result<Vec<String>, String> {
    read_list(bytes, "folders", MOST_FOLDERS, folder_of)
}

/// The line the gate says when the spare file the lines name `file` has the problem `problem`: both shown
/// [`escaped`], the problem's line breaks folded into one space.
#[must_use]
pub fn problem_line(file: &str, problem: &str) -> String {
    format!(
        "gate: {}: {}; the box's clear spares no folder",
        escaped(file),
        escaped(&fold_lines(problem))
    )
}

/// The folder path the non-empty entry `text` at `index` gives; `Err` with its problem when it is no folder path.
fn folder_of(index: usize, text: &str) -> Result<String, String> {
    match FolderGlob::parse(text) {
        Some(_) => Ok(text.to_owned()),
        None => Err(format!("entry {index} '{text}' is not a folder path: {FOLDER_RULE}")),
    }
}

/// The folders a list of spare entries spares, each entry read once as a folder path ([`FolderGlob`]); an entry that
/// is no folder path spares nothing.
#[derive(Debug, Clone, Default)]
pub struct Spared {
    globs: Vec<FolderGlob>,
}

impl Spared {
    /// The folders `entries` spare.
    #[must_use]
    pub fn new(entries: &[String]) -> Self {
        Self {
            globs: entries.iter().filter_map(|entry| FolderGlob::parse(entry)).collect(),
        }
    }

    /// Whether no entry spares any folder.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }

    /// Whether the folder whose names below the tree are `names` is one an entry matches: as many names as the entry
    /// has, each matching its own in any letter case.
    #[must_use]
    pub fn spares(&self, names: &[&str]) -> bool {
        self.globs.iter().any(|glob| glob.matches(names))
    }
}
