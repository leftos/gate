//! The walk listing the files below a folder that a run's output globs match, as the PowerShell gate's
//! `Get-OutputFile` lists them: an excluded folder is not entered, and a link or a junction, to a file or to a folder,
//! is neither followed nor listed. A file whose name does not fit the outputs archive, or is not Unicode, is not listed,
//! nor is a folder whose name is not plain or not Unicode entered; the box's pack also notes each such file the globs
//! would otherwise select, a folder's read through a verbatim path, to say it is skipped.

use std::ffi::OsStr;
use std::fs::{self, DirEntry, Metadata};
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};

use crate::outputs::glob::Globs;

/// A file's stat stamp: its length and its last write as a Windows file time. A file whose stamp differs from the one
/// the pre-command listing holds is one the run created or changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub size: u64,
    pub modified: u64,
}

impl Stamp {
    /// The stamp `metadata` holds: a file's length and its last write as a Windows file time, whether the metadata came
    /// from the folder listing the walk reads ([`DirEntry::metadata`]) or from an open file's handle.
    pub(crate) fn of(metadata: &Metadata) -> Stamp {
        Stamp {
            size: metadata.file_size(),
            modified: metadata.last_write_time(),
        }
    }
}

/// A file the walk found: its path relative to the walk's root, `/`-separated, its full path and its stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub path: String,
    pub full: PathBuf,
    pub stamp: Stamp,
}

/// What a walk found below a folder: the files its globs match, and the files they would match but for a name that is
/// not plain ([`Globs::refuses_name`]), each ordered by relative path.
#[derive(Debug, Default)]
pub(crate) struct Walked {
    pub found: Vec<Found>,
    pub refused: Vec<Found>,
}

impl Walked {
    /// The list the file at the relative `path` goes on under `globs`; `None` when it goes on neither. An `unfit` file,
    /// below a folder read only to count or with a name that is not Unicode, goes on `refused` when the globs select it.
    fn list_for(&mut self, globs: &Globs, path: &str, unfit: bool) -> Option<&mut Vec<Found>> {
        if unfit {
            globs.selects(path).then_some(&mut self.refused)
        } else if globs.matches(path) {
            Some(&mut self.found)
        } else {
            globs.refuses_name(path).then_some(&mut self.refused)
        }
    }
}

/// A folder a walk reads: its full path, its path relative to the walk's root, and whether the walk reads it only to
/// count the files below it.
struct Folder {
    full: PathBuf,
    relative: String,
    counting: bool,
}

/// One walk's state: whether it notes the refused files, what it found and the folders left to read.
struct Walker<'globs> {
    globs: &'globs Globs,
    noting: bool,
    walked: Walked,
    folders: Vec<Folder>,
}

/// Every file below `root` that `globs` match, ordered by relative path; else the first folder that cannot be read, or
/// entry whose metadata cannot be, its path in the error. A file or a folder whose name is not Unicode is passed over.
pub(crate) fn walk(root: &Path, globs: &Globs) -> io::Result<Vec<Found>> {
    walk_tree(root, globs, false).map(|walked| walked.found)
}

/// [`walk`]'s files below `root`, and beside them the files `globs` refuse for their names ([`Globs::refuses_name`]),
/// a name that is not Unicode included, named lossily. A folder whose name is not plain or not Unicode is read through
/// a verbatim path only to count the files below it ([`Globs::counts_below`]), which are only ever refused.
pub(crate) fn walk_with_refused(root: &Path, globs: &Globs) -> io::Result<Walked> {
    walk_tree(root, globs, true)
}

/// The walk below `root`, noting the refused files when `noting`.
fn walk_tree(root: &Path, globs: &Globs, noting: bool) -> io::Result<Walked> {
    let mut walker = Walker {
        globs,
        noting,
        walked: Walked::default(),
        folders: vec![Folder {
            full: root.to_path_buf(),
            relative: String::new(),
            counting: false,
        }],
    };
    while let Some(folder) = walker.folders.pop() {
        if let Err(error) = walker.read(&folder)
            && !folder.counting
        {
            return Err(error);
        }
    }
    let mut walked = walker.walked;
    walked.found.sort_by(|first, second| first.path.cmp(&second.path));
    walked.refused.sort_by(|first, second| first.path.cmp(&second.path));
    Ok(walked)
}

impl Walker<'_> {
    /// Visits each entry of `folder`; else the error that stopped the read. A folder read only to count goes on past an
    /// entry whose metadata cannot be read, which is then not counted: the count only names files the run leaves out.
    fn read(&mut self, folder: &Folder) -> io::Result<()> {
        for entry in fs::read_dir(&folder.full).map_err(|error| in_context(&folder.full, &error))? {
            let entry = entry.map_err(|error| in_context(&folder.full, &error))?;
            let visited = self.visit(&entry, folder);
            if !folder.counting {
                visited?;
            }
        }
        Ok(())
    }

    /// Lists `entry`, read from `folder`, when it is a file the globs match or refuse, or queues it when it is a folder
    /// to enter or to count below; a link or a junction is passed over.
    fn visit(&mut self, entry: &DirEntry, folder: &Folder) -> io::Result<()> {
        let full = entry.path();
        let metadata = entry.metadata().map_err(|error| in_context(&full, &error))?;
        let attributes = metadata.file_attributes();
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(());
        }
        let name = entry.file_name();
        let path = relative_path(&folder.relative, &name);
        let unfit = folder.counting || name.to_str().is_none();
        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            self.queue_folder(folder, &name, path, unfit);
        } else if let Some(list) = self.walked.list_for(self.globs, &path, unfit) {
            let stamp = Stamp::of(&metadata);
            list.push(Found { path, full, stamp });
        }
        Ok(())
    }

    /// Queues the folder `name` of `parent`, at the relative `path`, to enter when the globs enter it and it is not
    /// `unfit`, else to count below through a verbatim path when the walk notes refused files and the globs count below
    /// it. A verbatim path that cannot be made leaves the folder uncounted.
    fn queue_folder(&mut self, parent: &Folder, name: &OsStr, path: String, unfit: bool) {
        if !unfit && self.globs.enters(&path) {
            self.folders.push(Folder {
                full: parent.full.join(name),
                relative: path,
                counting: false,
            });
        } else if self.noting
            && self.globs.counts_below(&path)
            && let Ok(full) = verbatim(&parent.full, name)
        {
            self.folders.push(Folder {
                full,
                relative: path,
                counting: true,
            });
        }
    }
}

/// The path of the entry `name` of the folder at the relative path `folder`, `/`-separated; a name that is not Unicode
/// is read lossily.
fn relative_path(folder: &str, name: &OsStr) -> String {
    let name = name.to_string_lossy();
    if folder.is_empty() {
        name.into_owned()
    } else {
        format!("{folder}/{name}")
    }
}

/// The entry `name` of `folder` as a verbatim `\\?\` path, which Windows opens under its very name, a trailing dot, a
/// trailing space or a device name included; a `folder` already verbatim keeps its form.
fn verbatim(folder: &Path, name: &OsStr) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(folder).map_err(|error| in_context(folder, &error))?;
    let mut parts = absolute.components();
    let mut out = match parts.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) => PathBuf::from(format!(r"\\?\{}:\", char::from(letter))),
            Prefix::UNC(server, share) => [OsStr::new(r"\\?\UNC"), server, share].iter().collect(),
            _ => return Ok(absolute.join(name)),
        },
        _ => return Ok(absolute.join(name)),
    };
    out.extend(parts.filter(|part| matches!(part, Component::Normal(_))));
    out.push(name);
    Ok(out)
}

/// `error` with the `path` it happened at.
pub(crate) fn in_context(path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders or files must fail loudly")]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    use super::{Found, verbatim, walk, walk_with_refused};
    use crate::outputs::glob::Globs;

    /// A scratch folder for one test, emptied first, holding a file at each `/`-separated path of `files`, its text
    /// the path.
    fn tree(test: &str, files: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("gate-walk-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for file in files {
            let full = root.join(file);
            fs::create_dir_all(full.parent().expect("a file has a folder")).expect("the folder can be made");
            fs::write(&full, file).expect("the file can be written");
        }
        root
    }

    fn globs(globs: &[&str]) -> Globs {
        let owned: Vec<String> = globs.iter().map(|glob| (*glob).to_owned()).collect();
        Globs::new(&owned).expect("the globs can be matched")
    }

    #[test]
    fn a_walk_finds_the_matching_files_non_ascii_names_included_and_none_excluded() {
        let root = tree(
            "finds",
            &[
                "bin/a.dll",
                "bin/sub/b.pdb",
                "bin/Ünïcødé ✓.dll",
                "src/x.cs",
                ".tmp/bin/c.dll",
                "src/TestResults/bin/d.dll",
                ".git/bin/e",
                "bin/.env",
                "bin/app.env",
                "bin/.env.local",
            ],
        );
        let found = walk(&root, &globs(&["**/bin/**"])).expect("the tree can be walked");
        let paths: Vec<&str> = found.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["bin/a.dll", "bin/sub/b.pdb", "bin/Ünïcødé ✓.dll"]);
        for file in &found {
            assert_eq!(file.full, root.join(&file.path), "{}", file.path);
            assert_eq!(file.stamp.size, u64::try_from(file.path.len()).expect("a short path"), "{}", file.path);
            assert_ne!(file.stamp.modified, 0, "{}", file.path);
        }
    }

    #[test]
    fn walk_skips_a_file_symlink() {
        let root = tree("links", &["bin/a.dll"]);
        let outside = tree("links-outside", &["secret.txt"]);
        std::os::windows::fs::symlink_file(outside.join("secret.txt"), root.join("bin").join("link.dll"))
            .expect("this machine lets a test make a symbolic link");
        std::os::windows::fs::symlink_dir(&outside, root.join("bin").join("linked")).expect("a folder link can be made");
        let found = walk(&root, &globs(&["**"])).expect("the tree can be walked");
        let paths: Vec<&str> = found.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["bin/a.dll"], "neither link is reported nor followed");
    }

    #[test]
    fn a_walk_with_no_globs_finds_nothing() {
        let root = tree("none", &["bin/a.dll"]);
        assert_eq!(walk(&root, &globs(&[])).expect("the tree can be walked"), []);
    }

    /// The `/`-separated paths of `files`.
    fn paths_of(files: &[Found]) -> Vec<&str> {
        files.iter().map(|file| file.path.as_str()).collect()
    }

    #[test]
    fn a_walk_notes_an_alias_shaped_name_but_not_a_git_lfsconfig_or_excluded_one() {
        let root = tree(
            "aliases",
            &[
                "ok.txt",
                "a/.gitattributes",
                "b/.lfsconfig",
                ".tmp/m~1.txt",
                "c/l~10.txt",
                "d/GITATT~1",
                "e/.github/x~1.txt",
            ],
        );
        let walked = walk_with_refused(&root, &globs(&["**"])).expect("the tree can be walked");
        assert_eq!(paths_of(&walked.found), ["ok.txt"]);
        assert_eq!(paths_of(&walked.refused), ["c/l~10.txt", "d/GITATT~1"]);
    }

    #[test]
    fn a_walk_notes_the_files_below_a_folder_whose_name_is_not_plain_and_never_lists_them() {
        let root = tree(
            "folders",
            &[
                "bin/ok.txt",
                "bin/x~1/a.dll",
                "bin/x~1/sub/b.dll",
                "bin/x~1/c.txt",
                ".tmp/x~1/d.dll",
                ".github/x~1/e.dll",
            ],
        );
        let verbatim = PathBuf::from(format!(r"\\?\{}", root.display()));
        for folder in ["report.", "con"] {
            let made = verbatim.join("bin").join(folder);
            fs::create_dir_all(&made).expect("a verbatim path makes the folder under its very name");
            fs::write(made.join("f.dll"), "f").expect("a file can be written in it");
        }
        let matcher = globs(&["**/*.dll", "bin/ok.txt"]);
        assert_eq!(paths_of(&walk(&root, &matcher).expect("the tree can be walked")), ["bin/ok.txt"]);
        let walked = walk_with_refused(&root, &matcher).expect("the tree can be walked");
        assert_eq!(paths_of(&walked.found), ["bin/ok.txt"]);
        assert_eq!(
            paths_of(&walked.refused),
            ["bin/con/f.dll", "bin/report./f.dll", "bin/x~1/a.dll", "bin/x~1/sub/b.dll"]
        );
        for file in &walked.refused {
            assert!(
                fs::metadata(&file.full).is_ok_and(|metadata| metadata.is_file()),
                "{} opens as itself",
                file.path
            );
        }
    }

    #[test]
    fn a_walk_notes_a_name_that_is_not_unicode_and_lists_it_lossily() {
        let root = tree("unicode", &["bin/ok.txt"]);
        let lone = |tail: &str| OsString::from_wide(&[0xD800].into_iter().chain(tail.encode_utf16()).collect::<Vec<_>>());
        fs::write(root.join("bin").join(lone("x.dll")), "x").expect("a name that is not Unicode can be written");
        let folder = root.join("bin").join(lone("d"));
        fs::create_dir(&folder).expect("a folder whose name is not Unicode can be made");
        fs::write(folder.join("y.dll"), "y").expect("a file can be written in it");
        assert_eq!(paths_of(&walk(&root, &globs(&["**"])).expect("the walk does not fail")), ["bin/ok.txt"]);
        let walked = walk_with_refused(&root, &globs(&["**"])).expect("the walk does not fail");
        assert_eq!(paths_of(&walked.refused), ["bin/\u{fffd}d/y.dll", "bin/\u{fffd}x.dll"]);
    }

    #[test]
    fn a_walk_lists_no_file_whose_name_the_archive_reader_refuses() {
        let root = tree("refused", &["bin/ok.txt", "bin/m~10.txt", "bin/a\u{202e}b.dll"]);
        let verbatim = PathBuf::from(format!(r"\\?\{}", root.display()));
        fs::write(verbatim.join("bin").join("report."), "x").expect("a verbatim path keeps the trailing dot");
        assert!(
            fs::read_dir(root.join("bin"))
                .expect("the folder can be read")
                .any(|entry| entry.expect("an entry").file_name() == "report."),
            "the trailing dot was kept"
        );
        let found = walk(&root, &globs(&["**"])).expect("the tree can be walked");
        let paths: Vec<&str> = found.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["bin/ok.txt"]);
        let walked = walk_with_refused(&root, &globs(&["**"])).expect("the tree can be walked");
        let refused: Vec<&str> = walked.refused.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            refused,
            ["bin/a\u{202e}b.dll", "bin/m~10.txt", "bin/report."],
            "each refused name is noted"
        );
    }

    #[test]
    fn a_folder_read_only_to_count_that_cannot_be_read_is_not_counted_and_the_walk_goes_on() {
        let root = tree("uncounted", &["bin/ok.txt", "bin/x~1/a.dll", "bin/y~1/b.dll"]);
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(root.join("bin").join("x~1"))
            .expect("the folder can be held open sharing nothing");
        let walked = walk_with_refused(&root, &globs(&["**/*.dll", "bin/ok.txt"])).expect("a count that fails does not fail the walk");
        drop(held);
        assert_eq!(paths_of(&walked.found), ["bin/ok.txt"]);
        assert_eq!(
            paths_of(&walked.refused),
            ["bin/y~1/b.dll"],
            "the held folder is not counted, the other one is"
        );
    }

    #[test]
    fn a_verbatim_path_keeps_a_unc_share() {
        let full = verbatim(Path::new(r"\\srv\sh\a"), OsStr::new("con")).expect("a UNC path is absolute");
        assert_eq!(full.as_os_str(), r"\\?\UNC\srv\sh\a\con");
    }

    #[test]
    fn a_folder_that_cannot_be_read_is_named() {
        let missing = tree("missing", &[]).join("absent");
        let error = walk(&missing, &globs(&["**"])).expect_err("a missing folder cannot be walked");
        assert!(error.to_string().starts_with(&missing.display().to_string()), "{error}");
    }
}
