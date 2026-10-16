//! The folders of a sibling pair, held open from their check until the client lets them go. The worktree root's parent
//! is opened once, following links, and must be on a local volume; the root and each sibling are opened beneath that one
//! handle as themselves, never through a link, and each one's `.git` entry, a folder or a linked worktree's file, is
//! opened beneath it the same way. Each folder is checked against the exact path the route was decided for, a sibling
//! against its decided id too, and none of them, nor any `.git` entry, may be a reparse point of any kind. Every handle
//! shares reading and writing but not deletion, so none of these folders and entries can be renamed, removed or
//! swapped for a link while it is held; [`Held::recheck`] reads every path again before the hold ends, and opens each
//! by path as git does to see that it still leads to the held folder. The hold does not cover the repository a
//! `.git` file names, nor its common folder.

use std::fs::{File, Metadata};
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::Path;

use gate_router::FolderId;
use gate_win32::Share;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

use super::Sibling;
use super::git::without_verbatim;
use crate::outputs::archive::final_path;
use crate::route::{SIBLING_GIT_LINK, SIBLING_LINK, SIBLING_NOT_A_FOLDER, open_followed};

/// Why a folder is refused when its path or its id is not the one the route was decided for.
const OTHER_FOLDER: &str = "is not the folder the route was decided for";
/// Why a folder is refused when its path on disk differs from the decided one in letter case alone.
const CASE: &str = "differs in case from its declared name";
/// Why the root is refused when its parent, or the path to it, is not the folder the route was decided for.
const UNDER_OTHER: &str = "is under a folder that is not the one the route was decided for";
/// Why a folder is refused when its final path is not valid Unicode, so it cannot be compared with the route's.
const NOT_UNICODE: &str = "cannot be held: its path is not valid Unicode";
/// Why the root is refused when its parent is on a network volume.
const NETWORK: &str = "cannot be held: it is on a network volume";
/// Why a held folder is refused when its path changed, or no longer leads to it, while it was held.
const MOVED: &str = "moved while it was held";

/// Why a pair's folder cannot be held: the sibling's name, or the worktree root's folder name for the root and its
/// parent, and why.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NotHeld {
    name: String,
    why: String,
}

impl NotHeld {
    fn new(name: &str, why: impl Into<String>) -> NotHeld {
        NotHeld {
            name: name.to_owned(),
            why: why.into(),
        }
    }

    /// The line the run says as it stays here: `gate: not routed: sibling {name} {why}`.
    pub(crate) fn line(&self) -> String {
        format!("gate: not routed: sibling {} {}", self.name, self.why)
    }
}

/// The refusal of the folder `name` that could not be opened or read, for `error`.
fn cannot_hold(name: &str, error: &io::Error) -> NotHeld {
    NotHeld::new(name, format!("cannot be held: {error}"))
}

/// How a final path differs from the path the route decided on.
#[derive(Debug, PartialEq, Eq)]
enum Mismatch {
    /// The final path is not valid Unicode.
    NotUnicode,
    /// The two differ in letter case alone.
    Case,
    /// The two name other folders.
    Other,
}

impl Mismatch {
    /// Why a folder whose final path differs so is refused.
    fn why(&self) -> &'static str {
        match self {
            Mismatch::NotUnicode => NOT_UNICODE,
            Mismatch::Case => CASE,
            Mismatch::Other => OTHER_FOLDER,
        }
    }
}

/// One held handle: the open file, the path it was checked at, normalized ([`normalized`]), and the name a refusal says.
struct Entry {
    file: File,
    path: String,
    name: String,
}

/// The pair's folders, held open until dropped: the root's parent, the root and each sibling, each with its `.git`.
pub(crate) struct Held {
    entries: Vec<Entry>,
}

impl Held {
    /// Holds the folders of the worktree root `root`, whose folder name is `leaf`, and of `siblings`: the root's parent
    /// opened once, following links, on a local volume, its final path the root's lexical parent; the root opened
    /// beneath it as itself, a folder and no reparse point, its final path `root`; each sibling opened beneath it the
    /// same way, its id the one the decision read and its final path its root; and the `.git` of the root and of each
    /// sibling opened beneath it, no reparse point.
    ///
    /// # Errors
    ///
    /// The first folder that fails a check or cannot be opened, named; nothing stays held.
    pub(crate) fn hold(root: &str, leaf: &str, siblings: &[Sibling<'_>]) -> Result<Held, NotHeld> {
        let parent = held_parent(root, leaf)?;
        let mut entries = Vec::with_capacity(3 + 2 * siblings.len());
        let root_folder = folder_beneath(&parent.file, leaf)?;
        let root_path = checked(&root_folder, root, leaf)?;
        let root_git = git_beneath(&root_folder, leaf)?;
        for sibling in siblings {
            entries.extend(sibling_entries(&parent.file, sibling)?);
        }
        entries.push(parent);
        entries.push(Entry {
            file: root_folder,
            path: root_path,
            name: leaf.to_owned(),
        });
        entries.push(root_git);
        Ok(Held { entries })
    }

    /// Reads every held folder's final path again, and opens each one's path as git does, following links, to see that
    /// it still leads to the held folder.
    ///
    /// # Errors
    ///
    /// The first folder whose path changed since it was checked or no longer leads to it, or that cannot be read again,
    /// named.
    pub(crate) fn recheck(&self) -> Result<(), NotHeld> {
        for entry in &self.entries {
            let found = final_path(&entry.file).map_err(|error| cannot_hold(&entry.name, &error))?;
            if compare(&found, &entry.path).is_err() {
                return Err(NotHeld::new(&entry.name, MOVED));
            }
            still_leads(entry)?;
        }
        Ok(())
    }
}

/// The root's parent, opened by the lexical parent of `root` following links, held without delete sharing: on a local
/// volume, no reparse point, and its final path that parent. Any failure is said as the root's, named `leaf`.
fn held_parent(root: &str, leaf: &str) -> Result<Entry, NotHeld> {
    let Some(parent_text) = Path::new(root).parent().and_then(Path::to_str) else {
        return Err(NotHeld::new(leaf, UNDER_OTHER));
    };
    let parent = open_followed(Path::new(parent_text), Share::NoDelete).map_err(|error| cannot_hold(leaf, &error))?;
    let found = final_path(&parent).map_err(|error| cannot_hold(leaf, &error))?;
    if is_unc(&found) || gate_win32::on_network_volume(&parent).map_err(|error| cannot_hold(leaf, &error))? {
        return Err(NotHeld::new(leaf, NETWORK));
    }
    let metadata = parent.metadata().map_err(|error| cannot_hold(leaf, &error))?;
    if is_reparse_point(&metadata) {
        return Err(NotHeld::new(leaf, UNDER_OTHER));
    }
    let path = compare(&found, parent_text).map_err(|mismatch| match mismatch {
        Mismatch::NotUnicode => NotHeld::new(leaf, NOT_UNICODE),
        Mismatch::Case | Mismatch::Other => NotHeld::new(leaf, UNDER_OTHER),
    })?;
    Ok(Entry {
        file: parent,
        path,
        name: leaf.to_owned(),
    })
}

/// The sibling `sibling`'s folder and its `.git`, each opened beneath `parent` as itself and checked.
fn sibling_entries(parent: &File, sibling: &Sibling<'_>) -> Result<[Entry; 2], NotHeld> {
    let name = sibling.name();
    let folder = folder_beneath(parent, name)?;
    let (volume, file) = gate_win32::file_id(&folder).map_err(|error| cannot_hold(name, &error))?;
    if (FolderId { volume, file }) != sibling.id() {
        return Err(NotHeld::new(name, OTHER_FOLDER));
    }
    let path = checked(&folder, sibling.root(), name)?;
    let git = git_beneath(&folder, name)?;
    Ok([
        Entry {
            file: folder,
            path,
            name: name.to_owned(),
        },
        git,
    ])
}

/// The `.git` entry of the open folder `folder`, said as `name`'s, opened beneath it as itself without delete sharing;
/// refused when it is a reparse point of any kind.
fn git_beneath(folder: &File, name: &str) -> Result<Entry, NotHeld> {
    let git = gate_win32::open_beneath(folder, ".git", Share::NoDelete).map_err(|error| cannot_hold(name, &error))?;
    if is_reparse_point(&git.metadata().map_err(|error| cannot_hold(name, &error))?) {
        return Err(NotHeld::new(name, SIBLING_GIT_LINK));
    }
    let found = final_path(&git).map_err(|error| cannot_hold(name, &error))?;
    let path = normalized(&found).ok_or_else(|| NotHeld::new(name, NOT_UNICODE))?;
    Ok(Entry {
        file: git,
        path,
        name: name.to_owned(),
    })
}

/// The folder `name` opened beneath `parent` as itself, without delete sharing; refused when it is a reparse point of
/// any kind, a link, a junction or another, or not a folder.
fn folder_beneath(parent: &File, name: &str) -> Result<File, NotHeld> {
    let folder = gate_win32::open_beneath(parent, name, Share::NoDelete).map_err(|error| cannot_hold(name, &error))?;
    let metadata = folder.metadata().map_err(|error| cannot_hold(name, &error))?;
    if is_reparse_point(&metadata) {
        return Err(NotHeld::new(name, SIBLING_LINK));
    }
    if !metadata.is_dir() {
        return Err(NotHeld::new(name, SIBLING_NOT_A_FOLDER));
    }
    Ok(folder)
}

/// Whether the final path `found` names a network share: it starts with `\\?\UNC\`, in any letter case.
fn is_unc(found: &Path) -> bool {
    const UNC: &[u8] = br"\\?\UNC\";
    found
        .as_os_str()
        .as_encoded_bytes()
        .get(..UNC.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(UNC))
}

/// Whether a file with `metadata` is a reparse point of any kind: a link, a junction, or one a filter serves.
fn is_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Whether the held `entry`'s path, opened as git opens it, following links and sharing everything, still leads to the
/// file `entry` holds; refused as moved when it leads elsewhere or nowhere.
fn still_leads(entry: &Entry) -> Result<(), NotHeld> {
    let by_path = match open_followed(Path::new(&entry.path), Share::All) {
        Ok(by_path) => by_path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(NotHeld::new(&entry.name, MOVED)),
        Err(error) => return Err(cannot_hold(&entry.name, &error)),
    };
    if gate_win32::same_file(&entry.file, &by_path).map_err(|error| cannot_hold(&entry.name, &error))? {
        Ok(())
    } else {
        Err(NotHeld::new(&entry.name, MOVED))
    }
}

/// The final path of the open `file`, normalized, when it is the path `expected` exactly; else the refusal naming
/// `name` for how it differs.
fn checked(file: &File, expected: &str, name: &str) -> Result<String, NotHeld> {
    let found = final_path(file).map_err(|error| cannot_hold(name, &error))?;
    compare(&found, expected).map_err(|mismatch| NotHeld::new(name, mismatch.why()))
}

/// The final path `found`, normalized ([`normalized`]), when it is `expected` with every `\` a `/`; else how they
/// differ: `found` not valid Unicode, a letter case of its own, or another folder.
fn compare(found: &Path, expected: &str) -> Result<String, Mismatch> {
    let found = normalized(found).ok_or(Mismatch::NotUnicode)?;
    let expected = expected.replace('\\', "/");
    if found == expected {
        Ok(found)
    } else if found.to_lowercase() == expected.to_lowercase() {
        Err(Mismatch::Case)
    } else {
        Err(Mismatch::Other)
    }
}

/// `path` as the route's strings spell a folder: without the `\\?\` prefix, a `\\?\UNC\server\share` as
/// `//server/share`, and every `\` a `/`; `None` when it is not valid Unicode.
fn normalized(path: &Path) -> Option<String> {
    without_verbatim(path).to_str().map(|text| text.replace('\\', "/"))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose folders cannot be made must fail loudly")]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::thread;

    use gate_router::{CarriedSibling, FolderId, RemoteRun, sibling_root};
    use gate_win32::Share;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    use super::{Entry, Held, Mismatch, NotHeld, compare, is_unc, normalized, still_leads};
    use crate::remote::carried;
    use crate::route::open_followed;

    /// The folders of a pair under a new temporary folder `top`: `dev\app`, the worktree root, and beside it each
    /// sibling, each a folder holding a `.git` folder. Dropping it removes `top`, also when a test failed.
    struct Pair {
        top: PathBuf,
        /// The worktree root as the route spells it.
        root: String,
    }

    impl Pair {
        fn new(test: &str, names: &[&str]) -> Pair {
            let top = crate::test_support::temp_dir().join(format!("gate-held-{test}-{}", std::process::id()));
            if top.exists() {
                fs::remove_dir_all(&top).expect("an earlier run's folder is removed");
            }
            for name in names.iter().chain(&["app"]) {
                fs::create_dir_all(top.join("dev").join(name).join(".git")).expect("the folder can be made");
            }
            let dev = fs::canonicalize(top.join("dev")).expect("the parent has a final path");
            let dev = normalized(&dev).expect("the parent's path is Unicode");
            Pair {
                top,
                root: format!("{dev}/app"),
            }
        }

        fn folder(&self, name: &str) -> PathBuf {
            self.top.join("dev").join(name)
        }

        /// A routed run from the root `root` carrying the siblings `names`, each with its folder's id as it is now.
        fn run(&self, root: &str, names: &[&str]) -> RemoteRun {
            RemoteRun {
                host: "builder".to_owned(),
                slots: 1,
                root: root.to_owned(),
                leaf: "app".to_owned(),
                siblings: names
                    .iter()
                    .map(|name| CarriedSibling {
                        name: (*name).to_owned(),
                        root: sibling_root(root, name),
                        id: id_of(&self.folder(name)),
                    })
                    .collect(),
                repo: "app".to_owned(),
                key: "app-1a2b3c4d".to_owned(),
                subfolder: String::new(),
                pattern: "*".to_owned(),
                words: Vec::new(),
                fallback: Vec::new(),
                outputs: Vec::new(),
                dropped_no_build: false,
                outputs_withheld: false,
            }
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            let removed = fs::remove_dir_all(&self.top);
            // A test that already failed keeps its own failure; a folder left behind then is removed by the next run.
            assert!(
                removed.is_ok() || thread::panicking(),
                "the pair's folder {} can be removed: {removed:?}",
                self.top.display()
            );
        }
    }

    /// Holds the folders of `run`, its leaf `app`.
    fn hold(run: &RemoteRun) -> Result<Held, NotHeld> {
        let siblings = carried(run).expect("the run's siblings check");
        Held::hold(&run.root, "app", &siblings)
    }

    /// The line a refused hold says, or `None` for a hold that went through.
    fn refusal(held: Result<Held, NotHeld>) -> Option<String> {
        held.err().map(|refused| refused.line())
    }

    fn id_of(path: &Path) -> FolderId {
        let folder = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("the folder can be opened");
        let (volume, file) = gate_win32::file_id(&folder).expect("the folder's id can be read");
        FolderId { volume, file }
    }

    /// Runs `program` with `args` to its end.
    #[expect(clippy::disallowed_methods, reason = "a test's helper process takes no spawn lock")]
    fn ran(program: &str, args: &[&std::ffi::OsStr]) -> Output {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("the program starts")
    }

    /// Makes a junction at `link` that leads to `target`, with `mklink /J`.
    fn junction(link: &Path, target: &Path) {
        let made = ran(
            "cmd",
            &[
                "/d".as_ref(),
                "/c".as_ref(),
                "mklink".as_ref(),
                "/J".as_ref(),
                link.as_os_str(),
                target.as_os_str(),
            ],
        );
        assert!(made.status.success(), "the junction {} is made: {made:?}", link.display());
    }

    #[test]
    fn the_path_comparison_normalizes_and_tells_case_from_another_folder() {
        for (found, expected, compared) in [
            (r"\\?\X:\a\b", "X:/a/b", Ok("X:/a/b".to_owned())),
            (r"\\?\UNC\srv\share\a", "//srv/share/a", Ok("//srv/share/a".to_owned())),
            (r"\\?\X:\a\b", r"X:\a\b", Ok("X:/a/b".to_owned())),
            (r"X:\a\b", "X:/a/b", Ok("X:/a/b".to_owned())),
            (r"\\?\X:\a\B", "X:/a/b", Err(Mismatch::Case)),
            (r"\\?\X:\a\c", "X:/a/b", Err(Mismatch::Other)),
            (r"\\?\X:\a\b\c", "X:/a/b", Err(Mismatch::Other)),
        ] {
            assert_eq!(compare(Path::new(found), expected), compared, "{found} against {expected}");
        }
    }

    #[test]
    fn the_path_comparison_refuses_a_path_that_is_not_unicode() {
        let lone_surrogate = OsString::from_wide(&[u16::from(b'X'), u16::from(b':'), u16::from(b'\\'), 0xD800]);

        assert_eq!(compare(Path::new(&lone_surrogate), "X:/a"), Err(Mismatch::NotUnicode));
    }

    #[test]
    fn a_parent_whose_final_path_is_unc_is_refused() {
        for (found, unc) in [
            (r"\\?\UNC\srv\share\dev", true),
            (r"\\?\unc\srv\share", true),
            (r"\\?\X:\dev", false),
            (r"\\?\UNCX:\dev", false),
            (r"\\?\UN", false),
        ] {
            assert_eq!(is_unc(Path::new(found)), unc, "{found}");
        }
    }

    #[test]
    fn a_plain_pair_is_held_and_rechecks() {
        let pair = Pair::new("plain", &["b", "c.d"]);
        let run = pair.run(&pair.root, &["b", "c.d"]);

        let held = hold(&run).expect("a plain pair is held");

        assert_eq!(
            held.entries.len(),
            3 + 2 * 2,
            "the parent, the root and its .git, and each sibling with its .git"
        );
        assert_eq!(held.recheck(), Ok(()));
    }

    #[test]
    fn a_sibling_replaced_by_a_junction_is_refused() {
        let pair = Pair::new("junction", &["b"]);
        let run = pair.run(&pair.root, &["b"]);
        fs::rename(pair.folder("b"), pair.folder("b-real")).expect("the sibling can be moved away");
        junction(&pair.folder("b"), &pair.folder("b-real"));

        let refused = refusal(hold(&run));

        assert_eq!(refused.as_deref(), Some("gate: not routed: sibling b is a link or a junction"));
    }

    #[test]
    fn a_sibling_renamed_away_and_recreated_is_refused() {
        let pair = Pair::new("recreated", &["b"]);
        let run = pair.run(&pair.root, &["b"]);
        fs::rename(pair.folder("b"), pair.folder("b-old")).expect("the sibling can be moved away");
        fs::create_dir_all(pair.folder("b").join(".git")).expect("a new sibling can be made");

        let refused = refusal(hold(&run));

        assert_eq!(
            refused.as_deref(),
            Some("gate: not routed: sibling b is not the folder the route was decided for")
        );
    }

    #[test]
    fn a_sibling_whose_case_differs_is_refused() {
        let pair = Pair::new("case", &["b"]);
        let run = pair.run(&pair.root, &["b"]);
        fs::rename(pair.folder("b"), pair.folder("B")).expect("the sibling can be renamed");

        let refused = refusal(hold(&run));

        assert_eq!(
            refused.as_deref(),
            Some("gate: not routed: sibling b differs in case from its declared name")
        );
    }

    #[test]
    fn a_root_junctioned_into_a_sibling_is_refused() {
        let pair = Pair::new("root-junction", &["b"]);
        let run = pair.run(&pair.root, &["b"]);
        fs::create_dir(pair.folder("b").join("inner")).expect("the sibling's inner folder can be made");
        fs::remove_dir_all(pair.folder("app")).expect("the root can be removed");
        junction(&pair.folder("app"), &pair.folder("b").join("inner"));

        let refused = refusal(hold(&run));

        assert_eq!(refused.as_deref(), Some("gate: not routed: sibling app is a link or a junction"));
    }

    #[test]
    fn a_junction_in_an_ancestor_is_refused() {
        let pair = Pair::new("ancestor", &["b"]);
        junction(&pair.top.join("link"), &pair.top.join("dev"));
        let top = normalized(&fs::canonicalize(&pair.top).expect("the top has a final path")).expect("the top's path is Unicode");
        let run = pair.run(&format!("{top}/link/app"), &["b"]);

        let refused = refusal(hold(&run));

        assert_eq!(
            refused.as_deref(),
            Some("gate: not routed: sibling app is under a folder that is not the one the route was decided for")
        );
    }

    #[test]
    fn an_ancestor_whose_case_differs_is_refused() {
        let pair = Pair::new("ancestor-case", &["b"]);
        let run = pair.run(&pair.root, &["b"]);
        fs::rename(pair.top.join("dev"), pair.top.join("DEV")).expect("the parent can be renamed");

        let refused = refusal(hold(&run));

        assert_eq!(
            refused.as_deref(),
            Some("gate: not routed: sibling app is under a folder that is not the one the route was decided for")
        );
    }

    #[test]
    fn a_sibling_git_that_is_a_junction_is_refused() {
        let pair = Pair::new("git-junction", &["b"]);
        let run = pair.run(&pair.root, &["b"]);
        fs::rename(pair.folder("b").join(".git"), pair.folder("b-git")).expect("the .git can be moved away");
        junction(&pair.folder("b").join(".git"), &pair.folder("b-git"));

        let refused = refusal(hold(&run));

        assert_eq!(
            refused.as_deref(),
            Some("gate: not routed: sibling b has a .git that is a link or a junction")
        );
    }

    #[test]
    fn a_held_sibling_cannot_be_renamed_and_git_still_reads_it() {
        let pair = Pair::new("rename", &["b"]);
        let sibling = pair.folder("b");
        let made = ran("git", &["-C".as_ref(), sibling.as_os_str(), "init".as_ref(), "--quiet".as_ref()]);
        assert!(made.status.success(), "the sibling is a repository: {made:?}");
        let run = pair.run(&pair.root, &["b"]);

        let held = hold(&run).expect("the pair is held");
        let renamed = fs::rename(&sibling, pair.folder("b-away"));
        let status = ran("git", &["-C".as_ref(), sibling.as_os_str(), "status".as_ref(), "--porcelain".as_ref()]);

        assert_eq!(
            renamed.map_err(|error| error.raw_os_error()),
            Err(Some(32)),
            "a sharing violation while held"
        );
        assert!(status.status.success(), "git reads the held sibling: {status:?}");
        assert_eq!(held.recheck(), Ok(()));
    }

    /// Asserts the system's behaviour the hold relies on, not the hold's own: while the parent is held without delete
    /// sharing, the folder above it cannot be renamed either (access denied, OS error 5), so no path above the held
    /// folders can be moved to lead elsewhere.
    #[test]
    fn an_ancestor_of_a_held_pair_cannot_be_renamed() {
        let pair = Pair::new("ancestor-rename", &["b"]);
        let run = pair.run(&pair.root, &["b"]);

        let held = hold(&run).expect("the pair is held");
        let renamed = fs::rename(&pair.top, pair.top.with_extension("away"));

        assert_eq!(renamed.map_err(|error| error.raw_os_error()), Err(Some(5)), "access denied while held");
        drop(held);
    }

    /// No local construction moves a held folder's path while it is held (the ancestor rename above is refused), so the
    /// check that a path still leads to the held folder is shown on an entry whose path names another folder.
    #[test]
    fn a_path_that_no_longer_leads_to_the_held_folder_is_refused_by_recheck() {
        let pair = Pair::new("leads", &["b", "c"]);
        let entry = |name: &str, path: &str| Entry {
            file: open_followed(&pair.folder(name), Share::NoDelete).expect("the folder can be opened"),
            path: path.to_owned(),
            name: name.to_owned(),
        };
        let path_of = |name: &str| sibling_root(&pair.root, name);

        let leads = still_leads(&entry("b", &path_of("b")));
        let elsewhere = still_leads(&entry("b", &path_of("c"))).map_err(|refused| refused.line());
        let nowhere = still_leads(&entry("b", &path_of("gone"))).map_err(|refused| refused.line());

        assert_eq!(leads, Ok(()));
        assert_eq!(elsewhere, Err("gate: not routed: sibling b moved while it was held".to_owned()));
        assert_eq!(nowhere, Err("gate: not routed: sibling b moved while it was held".to_owned()));
    }
}
