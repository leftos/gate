//! A user-level tool's copy on a builder: the folder a `userTools` path names under a home folder, the fingerprint of
//! the copy on this PC, which `gate push` writes into the builder's copy and a routed run compares with the one the
//! builder's copy records, and how that record reads.

use std::fs;
use std::path::{Path, PathBuf};

use gate_wire::{SHA256_DIGITS, is_lower_hex, lower_hex};
use sha2::{Digest, Sha256};

/// The record in the builder's copy of a user tool: one ASCII line, its fingerprint, a space, and its lock hash.
pub(crate) const RECORD: &str = ".installed-from";

/// The folder the home-relative `path` (`~/` and then `/`-separated names) names under `home`: each name pushed in
/// turn. Only a path the routing file's `userTools` rule passed reaches it.
pub(crate) fn folder(home: &Path, path: &str) -> PathBuf {
    let mut folder = home.to_path_buf();
    for name in path.strip_prefix("~/").unwrap_or(path).split('/') {
        folder.push(name);
    }
    folder
}

/// A user tool's copy on this PC as a builder's copy is checked against: its fingerprint, the hash of its
/// `pnpm-lock.yaml` (`-` when it has none), both lower-case hex, and its tracked files, each with its bytes, in the
/// order the fingerprint reads them.
pub(crate) struct Print {
    pub(crate) fingerprint: String,
    pub(crate) lock: String,
    pub(crate) files: Vec<(String, Vec<u8>)>,
}

/// The print of the tool folder `folder`, given `listing`, what `git ls-files -z` wrote run in it: each tracked path,
/// `/`-separated, read from `folder` as its working tree holds it, sorted by its bytes. The fingerprint is the SHA-256
/// over each file in turn of its path, a NUL, its length in decimal, a NUL, then its bytes. Else why the folder cannot be
/// printed: a path that is not UTF-8, a file that cannot be read, or no tracked file at all.
pub(crate) fn print(folder: &Path, listing: &[u8]) -> Result<Print, String> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in listing.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()) {
        let path = std::str::from_utf8(entry).map_err(|_| "a tracked path is not UTF-8".to_owned())?;
        let mut file = folder.to_path_buf();
        for name in path.split('/') {
            file.push(name);
        }
        let bytes = fs::read(&file).map_err(|error| format!("cannot read {path}: {error}"))?;
        files.push((path.to_owned(), bytes));
    }
    if files.is_empty() {
        return Err(format!("{} tracks no file", folder.display()));
    }
    files.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut hasher = Sha256::new();
    for (path, bytes) in &files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(bytes.len().to_string().as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
    }
    let lock = files
        .iter()
        .find(|(path, _)| path == "pnpm-lock.yaml")
        .map_or_else(|| "-".to_owned(), |(_, bytes)| lower_hex(&Sha256::digest(bytes)));
    Ok(Print {
        fingerprint: lower_hex(&hasher.finalize()),
        lock,
        files,
    })
}

/// The print of this PC's copy of the user tool at the home-relative `path`, under [`home`] ([`read_in`]); else why it
/// cannot be read.
pub(crate) fn read(path: &str, list: impl FnOnce(&Path) -> Result<Vec<u8>, String>) -> Result<Print, String> {
    read_in(&home()?, path, list)
}

/// The print of the user tool at `path` under `home`: its [`folder`], listed by `list`, the caller's own `git ls-files
/// -z` run in that folder, and printed from that listing ([`print`]); else why it cannot be read.
fn read_in(home: &Path, path: &str, list: impl FnOnce(&Path) -> Result<Vec<u8>, String>) -> Result<Print, String> {
    let folder = folder(home, path);
    let listing = list(&folder)?;
    print(&folder, &listing)
}

/// The home folder a user tool's path is read under, on this PC and on the builder alike: `USERPROFILE`; else why
/// there is none.
pub(crate) fn home() -> Result<PathBuf, String> {
    std::env::var_os("USERPROFILE")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "USERPROFILE is not set".to_owned())
}

/// The fingerprint a builder copy's record `text` holds: its first whitespace-separated token when that is exactly 64
/// lower-case hex digits; else `None`.
pub(crate) fn recorded(text: &str) -> Option<&str> {
    text.split_whitespace().next().filter(|token| is_lower_hex(token, SHA256_DIGITS))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a case that cannot set up its folder must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use sha2::{Digest, Sha256};

    use gate_wire::lower_hex;

    use super::{Print, folder, print, read_in, recorded};
    use crate::test_support::temp_dir;

    /// A fresh folder `name` under the unit tests' folder holding each `(path, bytes)` of `files`.
    fn tool(name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let root = temp_dir().join("user-tool").join(name);
        let _ = fs::remove_dir_all(&root);
        for (path, bytes) in files {
            let file = root.join(path);
            fs::create_dir_all(file.parent().expect("a file has a folder")).expect("the folder can be made");
            fs::write(file, bytes).expect("the file can be written");
        }
        fs::create_dir_all(&root).expect("the tool folder can be made");
        root
    }

    /// `paths` as `git ls-files -z` lists them.
    fn listing(paths: &[&str]) -> Vec<u8> {
        paths.iter().flat_map(|path| path.bytes().chain([0])).collect()
    }

    fn printed(root: &Path, paths: &[&str]) -> Print {
        print(root, &listing(paths)).expect("the folder prints")
    }

    #[test]
    fn the_fingerprint_is_the_sha_256_of_each_path_its_length_and_its_bytes() {
        let root = tool("rule", &[("b.txt", b"yz"), ("a/c.ts", b"x")]);

        let print = printed(&root, &["b.txt", "a/c.ts"]);

        let expected = lower_hex(&Sha256::digest(b"a/c.ts\x001\x00xb.txt\x002\x00yz"));
        assert_eq!(print.fingerprint, expected);
        assert_eq!(print.fingerprint.len(), 64);
        let files: Vec<&str> = print.files.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(files, ["a/c.ts", "b.txt"], "sorted by path");
    }

    #[test]
    fn the_fingerprint_does_not_depend_on_listing_order() {
        let root = tool("order", &[("b.txt", b"yz"), ("a/c.ts", b"x"), ("z", b"")]);

        let one = printed(&root, &["b.txt", "a/c.ts", "z"]);
        let other = printed(&root, &["z", "a/c.ts", "b.txt"]);

        assert_eq!(one.fingerprint, other.fingerprint);
    }

    #[test]
    fn a_changed_byte_a_renamed_file_and_bytes_moved_between_files_each_change_the_fingerprint() {
        let base = printed(&tool("base", &[("a", b"xy"), ("b", b"z")]), &["a", "b"]).fingerprint;
        let changed = printed(&tool("changed", &[("a", b"xq"), ("b", b"z")]), &["a", "b"]).fingerprint;
        let renamed = printed(&tool("renamed", &[("a", b"xy"), ("c", b"z")]), &["a", "c"]).fingerprint;
        let moved = printed(&tool("moved", &[("a", b"x"), ("b", b"yz")]), &["a", "b"]).fingerprint;

        assert_ne!(base, changed, "a changed byte");
        assert_ne!(base, renamed, "a renamed file");
        assert_ne!(base, moved, "bytes moved between files");
    }

    #[test]
    fn a_folder_without_a_pnpm_lock_has_lock_dash() {
        let root = tool("no-lock", &[("package.json", b"{}"), ("sub/pnpm-lock.yaml", b"lock")]);

        assert_eq!(printed(&root, &["package.json", "sub/pnpm-lock.yaml"]).lock, "-");
    }

    #[test]
    fn the_lock_is_the_sha_256_of_pnpm_lock_yaml() {
        let root = tool("lock", &[("package.json", b"{}"), ("pnpm-lock.yaml", b"lockfileVersion: '9.0'\n")]);

        let print = printed(&root, &["package.json", "pnpm-lock.yaml"]);

        assert_eq!(print.lock, lower_hex(&Sha256::digest(b"lockfileVersion: '9.0'\n")));
    }

    #[test]
    fn a_listed_file_that_cannot_be_read_names_it() {
        let root = tool("unreadable", &[("a", b"x")]);

        let why = print(&root, &listing(&["a", "gone/b.txt"]))
            .err()
            .expect("a missing file fails the print");

        assert!(why.starts_with("cannot read gone/b.txt: "), "{why}");
    }

    #[test]
    fn an_empty_listing_names_the_folder() {
        let root = tool("empty", &[]);

        let why = print(&root, b"").err().expect("an empty listing fails the print");

        assert_eq!(why, format!("{} tracks no file", root.display()));
    }

    #[test]
    fn recorded_reads_the_first_token_only_when_it_is_64_lower_hex() {
        let good = "0123456789abcdef".repeat(4);
        let upper = good.to_uppercase();
        let short = &good[..63];
        let line = format!("{good} -\n");
        let upper_line = format!("{upper} -");
        let old = format!("{good} {}\r\n", "f".repeat(64));
        let rows: [(&str, Option<&str>); 5] = [(&line, Some(&good)), (&upper_line, None), (short, None), ("", None), (&old, Some(&good))];
        for (text, expected) in rows {
            assert_eq!(recorded(text), expected, "{text:?}");
        }
    }

    #[test]
    fn read_lists_the_tool_folder_under_home() {
        let home = tool("read-home", &[]);
        let tool_folder = home.join(".claude").join("tools").join("t");
        fs::create_dir_all(&tool_folder).expect("the tool folder can be made");
        fs::write(tool_folder.join("a.txt"), b"one").expect("the file can be written");
        let mut asked: Option<PathBuf> = None;

        let read = read_in(&home, "~/.claude/tools/t", |folder| {
            asked = Some(folder.to_path_buf());
            Ok(listing(&["a.txt"]))
        })
        .expect("the tool reads");

        assert_eq!(asked, Some(tool_folder.clone()), "the listing is asked of the tool folder under home");
        assert_eq!(
            read.fingerprint,
            print(&tool_folder, &listing(&["a.txt"])).expect("it prints").fingerprint
        );
    }

    #[test]
    fn folder_joins_each_segment_under_home() {
        assert_eq!(
            folder(Path::new(r"C:\Users\someone"), "~/.claude/tools/mock-three"),
            PathBuf::from(r"C:\Users\someone\.claude\tools\mock-three")
        );
    }
}
