//! The folders of a sibling pair, held open from their check until the client lets them go: the worktree root's parent,
//! opened once, and the root and each sibling opened beneath that one handle as themselves, never through a link. Each
//! is checked against the exact path the route was decided for, and held sharing reading and writing but not deletion,
//! so none can be renamed, removed or swapped for a link while it is held; [`Held::recheck`] reads every path again
//! before the hold ends.

use std::fs::{self, File};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use gate_router::FolderId;
use gate_win32::Share;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE};

use super::Sibling;
use super::git::without_verbatim;
use crate::outputs::archive::final_path;

/// Why a folder is refused when it is a link or a junction (a name-surrogate reparse point).
const LINK: &str = "is a link";
/// Why a folder is refused when it is a file.
const NOT_A_FOLDER: &str = "is not a folder";
/// Why a folder is refused when its path or its id is not the one the route was decided for.
const OTHER_FOLDER: &str = "is not the folder the route was decided for";
/// Why a folder is refused when its path on disk differs from the decided one in letter case alone.
const CASE: &str = "differs in case from its declared name";
/// Why a sibling is refused when its `.git` is a link or a junction.
const GIT_LINK: &str = "has a .git that is a link";
/// Why a held folder is refused when its path changed while it was held.
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

/// One held handle: the open file, the path it was checked at, normalized ([`normalized`]), and the name a refusal says.
struct Entry {
    file: File,
    path: String,
    name: String,
}

/// The pair's folders, held open until dropped: the root's parent, the root, and each sibling with its `.git`.
pub(crate) struct Held {
    entries: Vec<Entry>,
}

impl Held {
    /// Holds the folders of the worktree root `root`, whose folder name is `leaf`, and of `siblings`: the root's parent
    /// opened once, following links, its final path the root's lexical parent; the root opened beneath it as itself, a
    /// folder and no link, its final path `root`; each sibling opened beneath it the same way, its id the one the
    /// decision read and its final path its root, and its `.git` opened beneath it, no link.
    ///
    /// # Errors
    ///
    /// The first folder that fails a check or cannot be opened, named; nothing stays held.
    pub(crate) fn hold(root: &str, leaf: &str, siblings: &[Sibling<'_>]) -> Result<Held, NotHeld> {
        let Some(parent_path) = Path::new(root).parent() else {
            return Err(NotHeld::new(leaf, OTHER_FOLDER));
        };
        let parent_text = parent_path.to_string_lossy();
        let parent = open_parent(parent_path).map_err(|error| cannot_hold(leaf, &error))?;
        let parent_path = checked(&parent, &parent_text, leaf)?;
        let root_folder = folder_beneath(&parent, leaf)?;
        let root_path = checked(&root_folder, root, leaf)?;
        let mut held = Vec::with_capacity(2 * siblings.len());
        for sibling in siblings {
            held.extend(sibling_entries(&parent, sibling)?);
        }
        let mut entries = vec![
            Entry {
                file: parent,
                path: parent_path,
                name: leaf.to_owned(),
            },
            Entry {
                file: root_folder,
                path: root_path,
                name: leaf.to_owned(),
            },
        ];
        entries.append(&mut held);
        Ok(Held { entries })
    }

    /// Reads every held folder's final path again.
    ///
    /// # Errors
    ///
    /// The first folder whose path changed since it was checked, or whose path cannot be read, named.
    pub(crate) fn recheck(&self) -> Result<(), NotHeld> {
        for entry in &self.entries {
            let found = final_path(&entry.file).map_err(|error| cannot_hold(&entry.name, &error))?;
            if normalized(&found) != entry.path {
                return Err(NotHeld::new(&entry.name, MOVED));
            }
        }
        Ok(())
    }
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
    let git = gate_win32::open_beneath(&folder, ".git", Share::NoDelete).map_err(|error| cannot_hold(name, &error))?;
    if git.metadata().map_err(|error| cannot_hold(name, &error))?.file_type().is_symlink() {
        return Err(NotHeld::new(name, GIT_LINK));
    }
    let git_path = normalized(&final_path(&git).map_err(|error| cannot_hold(name, &error))?);
    Ok([
        Entry {
            file: folder,
            path,
            name: name.to_owned(),
        },
        Entry {
            file: git,
            path: git_path,
            name: name.to_owned(),
        },
    ])
}

/// The folder `name` opened beneath `parent` as itself, without delete sharing; refused when it is a link or a junction
/// or not a folder.
fn folder_beneath(parent: &File, name: &str) -> Result<File, NotHeld> {
    let folder = gate_win32::open_beneath(parent, name, Share::NoDelete).map_err(|error| cannot_hold(name, &error))?;
    let metadata = folder.metadata().map_err(|error| cannot_hold(name, &error))?;
    if metadata.file_type().is_symlink() {
        return Err(NotHeld::new(name, LINK));
    }
    if !metadata.is_dir() {
        return Err(NotHeld::new(name, NOT_A_FOLDER));
    }
    Ok(folder)
}

/// The folder at `path` opened for reading, a link or a junction on the way followed, sharing reading and writing but
/// not deletion.
fn open_parent(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// The final path of the open `file`, normalized, when it is the path `expected` exactly; else the refusal naming
/// `name`, for a letter case of its own or for another folder.
fn checked(file: &File, expected: &str, name: &str) -> Result<String, NotHeld> {
    let found = final_path(file).map_err(|error| cannot_hold(name, &error))?;
    let (found, expected) = (normalized(&found), expected.replace('\\', "/"));
    if found == expected {
        Ok(found)
    } else if found.to_lowercase() == expected.to_lowercase() {
        Err(NotHeld::new(name, CASE))
    } else {
        Err(NotHeld::new(name, OTHER_FOLDER))
    }
}

/// `path` as the route's strings spell a folder: without the `\\?\` prefix, a `\\?\UNC\server\share` as
/// `//server/share`, and every `\` a `/`.
fn normalized(path: &Path) -> String {
    without_verbatim(path).to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose folders cannot be made must fail loudly")]
mod tests {
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};

    use gate_router::{CarriedSibling, FolderId, RemoteRun, sibling_root};
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    use super::{Held, NotHeld, normalized};
    use crate::remote::carried;

    /// The folders of a pair under a new temporary folder `top`: `dev\app`, the worktree root, and beside it each
    /// sibling, a folder holding a `.git` folder.
    struct Pair {
        top: PathBuf,
        /// The worktree root as the route spells it.
        root: String,
    }

    impl Pair {
        fn new(test: &str, names: &[&str]) -> Pair {
            let top = std::env::temp_dir().join(format!("gate-held-{test}-{}", std::process::id()));
            if top.exists() {
                fs::remove_dir_all(&top).expect("an earlier run's folder is removed");
            }
            fs::create_dir_all(top.join("dev").join("app")).expect("the root can be made");
            for name in names {
                fs::create_dir_all(top.join("dev").join(name).join(".git")).expect("the sibling can be made");
            }
            let dev = normalized(&fs::canonicalize(top.join("dev")).expect("the parent has a final path"));
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

        fn remove(self) {
            fs::remove_dir_all(&self.top).expect("the pair's folder can be removed");
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
    fn a_plain_pair_is_held_and_rechecks() {
        let pair = Pair::new("plain", &["b", "c.d"]);
        let run = pair.run(&pair.root, &["b", "c.d"]);

        let held = hold(&run).expect("a plain pair is held");

        assert_eq!(held.entries.len(), 2 + 2 * 2, "the parent, the root, and each sibling with its .git");
        assert_eq!(held.recheck(), Ok(()));
        drop(held);
        pair.remove();
    }

    #[test]
    fn a_sibling_replaced_by_a_junction_is_refused() {
        let pair = Pair::new("junction", &["b"]);
        let run = pair.run(&pair.root, &["b"]);
        fs::rename(pair.folder("b"), pair.folder("b-real")).expect("the sibling can be moved away");
        junction(&pair.folder("b"), &pair.folder("b-real"));

        let refused = refusal(hold(&run));

        assert_eq!(refused.as_deref(), Some("gate: not routed: sibling b is a link"));
        pair.remove();
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
        pair.remove();
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
        pair.remove();
    }

    #[test]
    fn a_root_junctioned_into_a_sibling_is_refused() {
        let pair = Pair::new("root-junction", &["b"]);
        let run = pair.run(&pair.root, &["b"]);
        fs::create_dir(pair.folder("b").join("inner")).expect("the sibling's inner folder can be made");
        fs::remove_dir(pair.folder("app")).expect("the root can be removed");
        junction(&pair.folder("app"), &pair.folder("b").join("inner"));

        let refused = refusal(hold(&run));

        assert_eq!(refused.as_deref(), Some("gate: not routed: sibling app is a link"));
        pair.remove();
    }

    #[test]
    fn a_junction_in_an_ancestor_is_refused() {
        let pair = Pair::new("ancestor", &["b"]);
        junction(&pair.top.join("link"), &pair.top.join("dev"));
        let root = format!("{}/link/app", normalized(&fs::canonicalize(&pair.top).expect("the top has a final path")));
        let run = pair.run(&root, &["b"]);

        let refused = refusal(hold(&run));

        assert_eq!(
            refused.as_deref(),
            Some("gate: not routed: sibling app is not the folder the route was decided for")
        );
        pair.remove();
    }

    #[test]
    fn a_sibling_git_that_is_a_junction_is_refused() {
        let pair = Pair::new("git-junction", &["b"]);
        let run = pair.run(&pair.root, &["b"]);
        fs::rename(pair.folder("b").join(".git"), pair.folder("b-git")).expect("the .git can be moved away");
        junction(&pair.folder("b").join(".git"), &pair.folder("b-git"));

        let refused = refusal(hold(&run));

        assert_eq!(refused.as_deref(), Some("gate: not routed: sibling b has a .git that is a link"));
        pair.remove();
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
        drop(held);
        pair.remove();
    }
}
