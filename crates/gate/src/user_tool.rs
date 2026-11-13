//! A user-level tool's copy on a builder: the folder a `userTools` path names under a home folder, the fingerprint of
//! the copy on this PC, which `gate push` writes into the builder's copy and a routed run compares with the one the
//! builder's copy records, and how that record reads.

use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use gate_router::{InstalledLayout, UsedTool, UserToolKind};
use gate_wire::{SHA256_DIGITS, is_lower_hex, lower_hex};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

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

/// A user tool's copy on this PC as a builder's copy is checked against: its fingerprint and its lock hash, both
/// lower-case hex (`-` for no lock), and the files a push ships, each with its bytes, in the order the builder writes
/// them: a git folder's tracked files in the order its fingerprint reads them ([`print`]), an installed folder's in its
/// layout's order ([`installed_in`]).
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
fn read(path: &str, list: impl FnOnce(&Path) -> Result<Vec<u8>, String>) -> Result<Print, String> {
    read_in(&home()?, path, list)
}

/// The print of the user tool at `path` under `home`: its [`folder`], listed by `list`, the caller's own `git ls-files
/// -z` run in that folder, and printed from that listing ([`print`]); else why it cannot be read.
fn read_in(home: &Path, path: &str, list: impl FnOnce(&Path) -> Result<Vec<u8>, String>) -> Result<Print, String> {
    let folder = folder(home, path);
    let listing = list(&folder)?;
    print(&folder, &listing)
}

/// This PC's print of the user tool `tool` as `gate push` ships it: a git folder's tracked files as `list`, the
/// caller's own `git ls-files -z`, lists them ([`read`]), or an installed folder's layout read whole
/// ([`installed_in`]); else why it cannot be read.
pub(crate) fn shipped(tool: &UsedTool, list: impl FnOnce(&Path) -> Result<Vec<u8>, String>) -> Result<Print, String> {
    match tool.kind {
        UserToolKind::Git => read(&tool.path, list),
        UserToolKind::Installed(layout) => installed_in(&folder(&home()?, &tool.path), layout, || {}),
    }
}

/// The fingerprint of this PC's copy of the user tool `tool` a routed run compares: a git folder's print as `list`
/// lists it ([`read`]), or an installed folder's, which reads only its `VERSION` and `current`
/// ([`installed_fingerprint_in`]); else why it cannot be read.
pub(crate) fn fingerprint(tool: &UsedTool, list: impl FnOnce(&Path) -> Result<Vec<u8>, String>) -> Result<String, String> {
    match tool.kind {
        UserToolKind::Git => read(&tool.path, list).map(|print| print.fingerprint),
        UserToolKind::Installed(layout) => installed_fingerprint_in(&folder(&home()?, &tool.path), layout),
    }
}

/// The launcher, the version file and the pointer file of a godot-mcp install, and the folder of its version folders.
const LAUNCHER: &str = "godot-mcp.exe";
const VERSION: &str = "VERSION";
const CURRENT: &str = "current";
const VERSIONS: &str = "versions";

/// What an installed folder's fingerprint is made of, and the version folder its pointer names.
struct Pointer {
    version: Vec<u8>,
    current: Vec<u8>,
    name: String,
    fingerprint: String,
}

/// The pointer of the install at `folder`: its `VERSION` and `current` as they are, the version folder name the
/// first line of `current` holds, trimmed, and the fingerprint: the SHA-256 over the tag `installed godot-mcp`, a NUL,
/// then for `VERSION` and then `current` its name, a NUL, its length in decimal, a NUL and its bytes. Else why it
/// cannot be read: either file unreadable, or `current` naming no single folder name.
fn pointer(folder: &Path) -> Result<Pointer, String> {
    let read = |name: &str| fs::read(folder.join(name)).map_err(|error| format!("cannot read {name}: {error}"));
    let version = read(VERSION)?;
    let current = read(CURRENT)?;
    let name = version_name(&current)?;
    let mut hasher = Sha256::new();
    hasher.update(b"installed godot-mcp\0");
    for (file, bytes) in [(VERSION, &version), (CURRENT, &current)] {
        hasher.update(file.as_bytes());
        hasher.update([0]);
        hasher.update(bytes.len().to_string().as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
    }
    Ok(Pointer {
        version,
        current,
        name,
        fingerprint: lower_hex(&hasher.finalize()),
    })
}

/// The version folder name `current`'s first line holds, after a leading byte order mark, trimmed, as the launcher
/// reads it; else why it names none: not UTF-8, blank, `.`, `..`, or holding a character no Windows file name may hold.
fn version_name(current: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(without_bom(current)).map_err(|_| "current is not UTF-8".to_owned())?;
    let name = text.lines().next().unwrap_or("").trim();
    let invalid = |character: char| character < ' ' || r#"<>:"/\|?*"#.contains(character);
    if name.is_empty() || name == "." || name == ".." || name.contains(invalid) {
        return Err(format!("current names {name:?}, which is not a version folder name"));
    }
    Ok(name.to_owned())
}

/// The fingerprint of the install at `folder` of `layout`, read from its `VERSION` and `current` alone ([`pointer`]);
/// no version folder is read.
fn installed_fingerprint_in(folder: &Path, layout: InstalledLayout) -> Result<String, String> {
    let InstalledLayout::GodotMcp = layout;
    pointer(folder).map(|pointer| pointer.fingerprint)
}

/// The print of the install at `folder` of `layout`, in the order a builder writes it: every plain file below
/// `versions/<name>/`, `<name>` the folder `current` names, sorted by path bytes; then `godot-mcp.exe`, `VERSION`, and
/// `current` last; lock `-`; the fingerprint [`pointer`]'s. Nothing else ships: no `install.lock`, no launcher left
/// aside, no other version folder. `during` runs after the files are read, before `VERSION` and `current` are read
/// again. Else why it cannot ship: [`pointer`]'s refusals, no `versions/<name>/godot-mcp.exe`, a link or an unreadable
/// file below the version folder, `versions/<name>/VERSION` other than `VERSION`, or `VERSION` or `current` changed
/// during the read.
fn installed_in(folder: &Path, layout: InstalledLayout, during: impl FnOnce()) -> Result<Print, String> {
    let InstalledLayout::GodotMcp = layout;
    let before = pointer(folder)?;
    let shown = format!("{VERSIONS}/{}", before.name);
    let versioned = folder.join(VERSIONS).join(&before.name);
    for (at, named) in [(folder.join(VERSIONS), VERSIONS), (versioned.clone(), shown.as_str())] {
        if fs::symlink_metadata(&at).is_ok_and(|metadata| is_link(&metadata)) {
            return Err(format!("{named} is a link"));
        }
    }
    if !versioned.join(LAUNCHER).is_file() {
        return Err(format!("{shown}/{LAUNCHER} is missing"));
    }
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    walk(&versioned, &shown, &mut files)?;
    let inner = format!("{shown}/{VERSION}");
    if !files
        .iter()
        .any(|(path, bytes)| *path == inner && without_bom(bytes) == without_bom(&before.version))
    {
        return Err(format!("{inner} differs from {VERSION}"));
    }
    let launcher = fs::read(folder.join(LAUNCHER)).map_err(|error| format!("cannot read {LAUNCHER}: {error}"))?;
    files.push((LAUNCHER.to_owned(), launcher));
    during();
    let after = pointer(folder)?;
    if after.version != before.version || after.current != before.current {
        return Err(format!("{VERSION} or {CURRENT} changed during the read"));
    }
    files.push((VERSION.to_owned(), before.version));
    files.push((CURRENT.to_owned(), before.current));
    Ok(Print {
        fingerprint: before.fingerprint,
        lock: "-".to_owned(),
        files,
    })
}

/// Adds every plain file below `folder`, shown as `shown`, to `files` as its `/`-separated path and its bytes, each
/// folder's names in byte order and a folder's files before the next name; else why it cannot: a name that is not
/// UTF-8, a link anywhere, something neither a file nor a folder, or one that cannot be read.
fn walk(folder: &Path, shown: &str, files: &mut Vec<(String, Vec<u8>)>) -> Result<(), String> {
    let entries = fs::read_dir(folder).map_err(|error| format!("cannot read {shown}: {error}"))?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {shown}: {error}"))?;
        names.push(entry.file_name().into_string().map_err(|_| format!("a name in {shown} is not UTF-8"))?);
    }
    names.sort_unstable();
    for name in names {
        let (path, inner) = (folder.join(&name), format!("{shown}/{name}"));
        let metadata = fs::symlink_metadata(&path).map_err(|error| format!("cannot read {inner}: {error}"))?;
        if is_link(&metadata) {
            return Err(format!("{inner} is a link"));
        }
        if metadata.is_dir() {
            walk(&path, &inner, files)?;
        } else if metadata.is_file() {
            files.push((inner.clone(), fs::read(&path).map_err(|error| format!("cannot read {inner}: {error}"))?));
        } else {
            return Err(format!("{inner} is not a plain file"));
        }
    }
    Ok(())
}

/// Whether `metadata`, read without following links, is a link: a symbolic link or any reparse point, a junction
/// included.
fn is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// `bytes` without a leading UTF-8 byte order mark, as a .NET `StreamReader` reads them.
fn without_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
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

    use gate_router::InstalledLayout;

    use super::{Print, folder, installed_fingerprint_in, installed_in, print, read_in, recorded};
    use crate::test_support::{junction, temp_dir};

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

    /// The version folder name every installed case's `current` names.
    const NAME: &str = "0.42.0-736e8ee";

    /// A fresh godot-mcp install `name` with the version folder `versions/<named>`, whose `VERSION` and version
    /// folder's `VERSION` hold `version` and whose `current` holds `current`; with an older version folder, an
    /// `install.lock` and a launcher left aside, none of which ships.
    fn install(name: &str, named: &str, version: &str, current: &str) -> PathBuf {
        let inner = format!("versions/{named}");
        tool(
            name,
            &[
                (&format!("{inner}/godot-mcp.exe"), b"server"),
                (&format!("{inner}/VERSION"), version.as_bytes()),
                (&format!("{inner}/bridge/b.gd"), b"bridge"),
                (&format!("{inner}/a.dll"), b"dll"),
                ("versions/0.41.1-89c4095/godot-mcp.exe", b"old server"),
                ("godot-mcp.exe", b"launcher"),
                ("godot-mcp.exe.old-08c96", b"old launcher"),
                ("install.lock", b""),
                ("VERSION", version.as_bytes()),
                ("current", current.as_bytes()),
            ],
        )
    }

    /// The print of the install at `root`, read whole.
    fn installed(root: &Path) -> Result<Print, String> {
        installed_in(root, InstalledLayout::GodotMcp, || {})
    }

    #[test]
    fn an_installed_tool_ships_its_version_folder_then_the_launcher_version_and_current_last() {
        let root = install("installed-order", NAME, "0.42.0+736e8ee\n", "0.42.0-736e8ee\r\n");

        let print = installed(&root).expect("the install prints");

        let files: Vec<&str> = print.files.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            files,
            [
                "versions/0.42.0-736e8ee/VERSION",
                "versions/0.42.0-736e8ee/a.dll",
                "versions/0.42.0-736e8ee/bridge/b.gd",
                "versions/0.42.0-736e8ee/godot-mcp.exe",
                "godot-mcp.exe",
                "VERSION",
                "current",
            ]
        );
        assert_eq!(print.files[4].1, b"launcher");
        assert_eq!(print.files[6].1, b"0.42.0-736e8ee\r\n");
        assert_eq!(print.lock, "-", "an installed tool installs no packages");
        assert_eq!(print.fingerprint.len(), 64);
    }

    #[test]
    fn an_installed_tool_s_fingerprint_follows_version_and_current_only() {
        let one = installed(&install("installed-same-1", "1.0.0-a", "1.0.0+a\n", "1.0.0-a\n")).expect("it prints");
        let two_root = install("installed-same-2", "1.0.0-a", "1.0.0+a\n", "1.0.0-a\n");
        fs::write(two_root.join("godot-mcp.exe"), b"another launcher").expect("the launcher can be written");
        let two = installed(&two_root).expect("it prints");
        let version = installed(&install("installed-version", "1.0.0-a", "1.0.1+a\n", "1.0.0-a\n")).expect("it prints");
        let current = installed(&install("installed-current", "1.0.0-a", "1.0.0+a\n", "1.0.0-a")).expect("it prints");

        assert_eq!(one.fingerprint, two.fingerprint, "the same VERSION and current");
        assert_ne!(one.fingerprint, version.fingerprint, "another VERSION");
        assert_ne!(one.fingerprint, current.fingerprint, "another current");
    }

    #[test]
    fn a_routed_run_s_installed_fingerprint_reads_no_version_folder() {
        let root = install("installed-routed", NAME, "1.0.0+a\n", &format!("{NAME}\n"));
        let whole = installed(&root).expect("it prints");
        fs::remove_file(root.join("versions").join(NAME).join("a.dll")).expect("the file can be deleted");
        fs::remove_file(root.join("versions").join(NAME).join("godot-mcp.exe")).expect("the file can be deleted");

        assert_eq!(installed_fingerprint_in(&root, InstalledLayout::GodotMcp), Ok(whole.fingerprint));
    }

    #[test]
    fn an_installed_tool_that_cannot_ship_says_why() {
        let no_current = install("installed-no-current", NAME, "1\n", "x\n");
        fs::remove_file(no_current.join("current")).expect("current can be deleted");
        let why = installed(&no_current).err().expect("no current fails");
        assert!(why.starts_with("cannot read current: "), "{why}");
        assert_eq!(installed_fingerprint_in(&no_current, InstalledLayout::GodotMcp).err(), Some(why));

        for (case, text) in [
            ("dots", "..\n"),
            ("dot", ".\n"),
            ("separator", "a/b\n"),
            ("colon", "a:b\n"),
            ("blank", "\n"),
        ] {
            let root = install(&format!("installed-bad-current-{case}"), NAME, "1\n", text);
            let name = text.trim();
            assert_eq!(
                installed(&root).err(),
                Some(format!("current names {name:?}, which is not a version folder name")),
                "{text:?}"
            );
        }

        let no_server = install("installed-no-server", NAME, "1\n", &format!("{NAME}\n"));
        fs::remove_file(no_server.join("versions").join(NAME).join("godot-mcp.exe")).expect("the server can be deleted");
        assert_eq!(installed(&no_server).err(), Some(format!("versions/{NAME}/godot-mcp.exe is missing")));

        let mismatch = install("installed-version-mismatch", NAME, "1\n", &format!("{NAME}\n"));
        fs::write(mismatch.join("VERSION"), b"2\n").expect("VERSION can be written");
        assert_eq!(installed(&mismatch).err(), Some(format!("versions/{NAME}/VERSION differs from VERSION")));
    }

    #[test]
    fn an_installed_tool_with_a_link_in_its_version_folder_cannot_ship() {
        let root = install("installed-link", NAME, "1\n", &format!("{NAME}\n"));
        let outside = tool("installed-link-outside", &[("x.txt", b"x")]);
        junction(&root.join("versions").join(NAME).join("linked"), &outside);

        assert_eq!(installed(&root).err(), Some(format!("versions/{NAME}/linked is a link")));
    }

    #[test]
    fn an_installed_tool_whose_version_folder_is_itself_a_link_cannot_ship() {
        let outside = install("installed-folder-link-outside", NAME, "1\n", &format!("{NAME}\n"));
        let root = install("installed-folder-link", "elsewhere", "1\n", &format!("{NAME}\n"));
        junction(&root.join("versions").join(NAME), &outside.join("versions").join(NAME));

        assert_eq!(installed(&root).err(), Some(format!("versions/{NAME} is a link")));
    }

    #[test]
    fn an_installed_tool_whose_current_and_version_start_with_a_bom_reads_the_same_name() {
        let root = install("installed-bom", NAME, "1\n", &format!("\u{feff}{NAME}\r\n"));
        fs::write(root.join("VERSION"), b"\xef\xbb\xbf1\n").expect("VERSION can be written");

        let print = installed(&root).expect("a BOM is not part of the name");

        assert_eq!(print.files[0].0, format!("versions/{NAME}/VERSION"));
        assert_eq!(
            print.files.last().map(|(_, bytes)| bytes.as_slice()),
            Some(format!("\u{feff}{NAME}\r\n").as_bytes())
        );
    }

    #[test]
    fn an_installed_tool_whose_version_changes_during_the_read_cannot_ship() {
        let root = install("installed-changed", NAME, "1\n", &format!("{NAME}\n"));

        let read = installed_in(&root, InstalledLayout::GodotMcp, || {
            fs::write(root.join("VERSION"), b"2\n").expect("VERSION can be written");
        });

        assert_eq!(read.err(), Some("VERSION or current changed during the read".to_owned()));
    }

    #[test]
    fn folder_joins_each_segment_under_home() {
        assert_eq!(
            folder(Path::new(r"C:\Users\someone"), "~/.claude/tools/mock-three"),
            PathBuf::from(r"C:\Users\someone\.claude\tools\mock-three")
        );
    }
}
