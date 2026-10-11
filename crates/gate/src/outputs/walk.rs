//! The walk listing the files below a folder that a run's output globs match, as the PowerShell gate's
//! `Get-OutputFile` lists them: an excluded folder is not entered, and a link or a junction, to a file or to a folder,
//! is neither followed nor listed.

use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};

use crate::outputs::glob::Globs;

/// A file's stat stamp: its length and its last write as a Windows file time. A file whose stamp differs from the one
/// the pre-command listing holds is one the run created or changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub size: u64,
    pub modified: u64,
}

/// A file the walk found: its path relative to the walk's root, `/`-separated, its full path and its stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub path: String,
    pub full: PathBuf,
    pub stamp: Stamp,
}

/// Every file below `root` that `globs` match, ordered by relative path; else the first folder or name that cannot be
/// read, its path in the error. A name that is not Unicode cannot be read.
pub(crate) fn walk(root: &Path, globs: &Globs) -> io::Result<Vec<Found>> {
    let mut found = Vec::new();
    let mut folders = vec![(root.to_path_buf(), String::new())];
    while let Some((folder, relative)) = folders.pop() {
        for entry in fs::read_dir(&folder).map_err(|error| in_context(&folder, &error))? {
            let entry = entry.map_err(|error| in_context(&folder, &error))?;
            let full = entry.path();
            let metadata = entry.metadata().map_err(|error| in_context(&full, &error))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{} has a name that is not Unicode", full.display())))?;
            let path = if relative.is_empty() { name } else { format!("{relative}/{name}") };
            let attributes = metadata.file_attributes();
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                continue;
            }
            if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                if globs.matches(&path) {
                    let stamp = Stamp {
                        size: metadata.file_size(),
                        modified: metadata.last_write_time(),
                    };
                    found.push(Found { path, full, stamp });
                }
            } else if globs.enters(&path) {
                folders.push((full, path));
            }
        }
    }
    found.sort_by(|first, second| first.path.cmp(&second.path));
    Ok(found)
}

/// `error` with the `path` it happened at.
pub(crate) fn in_context(path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders or files must fail loudly")]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::walk;
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

    #[test]
    fn a_folder_that_cannot_be_read_is_named() {
        let missing = tree("missing", &[]).join("absent");
        let error = walk(&missing, &globs(&["**"])).expect_err("a missing folder cannot be walked");
        assert!(error.to_string().starts_with(&missing.display().to_string()), "{error}");
    }
}
