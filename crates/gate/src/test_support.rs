//! Helpers the unit tests of more than one module share.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// `host`, the Windows builder `builder` with no id, as the unit tests' calls go to it.
#[cfg(feature = "test-seams")]
pub(crate) static HOST: std::sync::LazyLock<gate_router::Builder> = std::sync::LazyLock::new(|| gate_router::Builder {
    alias: "builder".to_owned(),
    os: gate_router::BuilderOs::Windows,
    root: None,
    slots: 1,
    id: None,
    data: None,
});

/// The folder the unit tests make their own folders and files in, in place of the system temp folder:
/// `target\tmp\unit-gate` in the workspace, made on first use, under the worktree so no virus scanner of the system
/// temp folder reads what they write. Each test makes its own subfolder or file in it.
#[expect(clippy::expect_used, reason = "a test with no folder to work in must fail loudly")]
pub(crate) fn temp_dir() -> &'static Path {
    static FOLDER: OnceLock<PathBuf> = OnceLock::new();
    FOLDER.get_or_init(|| {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"..\..\target\tmp\unit-gate");
        let folder = std::path::absolute(folder).expect("the unit tests' temp folder has an absolute path");
        fs::create_dir_all(&folder).expect("the unit tests' temp folder can be made");
        folder
    })
}

/// Makes `link` a junction to the folder `target`: std makes no junction and an unprivileged symbolic link fails, so
/// `cmd`'s `mklink /J` makes one, with `/d` so no `AutoRun` command runs first.
#[expect(clippy::disallowed_methods, reason = "a test's one mklink takes no spawn lock")]
#[expect(clippy::expect_used, reason = "a junction that cannot be made must fail the test loudly")]
pub(crate) fn junction(link: &Path, target: &Path) {
    let made = Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("cmd.exe runs");
    assert!(made.status.success(), "a junction needs no privilege: {made:?}");
}
