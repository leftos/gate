//! Helpers the unit tests of more than one module share.

use std::path::Path;
use std::process::Command;

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
