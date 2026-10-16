//! Helpers the unit tests of more than one module share.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The folder the unit tests make their own folders and files in, in place of the system temp folder:
/// `target\tmp\unit-gate-win32` in the workspace, made on first use, under the worktree so no virus scanner of the
/// system temp folder reads what they write. Each test makes its own subfolder or file in it.
#[expect(clippy::expect_used, reason = "a test with no folder to work in must fail loudly")]
pub(crate) fn temp_dir() -> &'static Path {
    static FOLDER: OnceLock<PathBuf> = OnceLock::new();
    FOLDER.get_or_init(|| {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"..\..\target\tmp\unit-gate-win32");
        let folder = std::path::absolute(folder).expect("the unit tests' temp folder has an absolute path");
        fs::create_dir_all(&folder).expect("the unit tests' temp folder can be made");
        folder
    })
}
