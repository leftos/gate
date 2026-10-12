//! File calls the standard library does not make: a rename that never replaces the file its target names.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

/// Renames the file `source` to `target` on the same volume, never replacing a file `target` already names.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::AlreadyExists`] when `target` names a file already, of kind
/// [`io::ErrorKind::InvalidInput`] for a path holding a NUL, else the system's error, such as a sharing violation for a
/// file another process holds open without delete sharing.
pub fn move_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    let source = wide_path(source)?;
    let target = wide_path(target)?;
    // SAFETY: both are NUL-terminated UTF-16 buffers that outlive the call; with no flags the call neither copies across
    // volumes nor replaces the target.
    let moved = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `path` as a NUL-terminated UTF-16 buffer; else the error for a path that holds a NUL, which would cut it short.
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the path {} holds a NUL", path.display()),
        ));
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose files cannot be made must fail loudly")]
mod tests {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::move_no_replace;

    /// A new, empty folder of the test `name`'s own.
    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-win32-file-{name}-{}", std::process::id()));
        if folder.exists() {
            std::fs::remove_dir_all(&folder).expect("an earlier run's folder is removed");
        }
        std::fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    #[test]
    fn a_file_is_moved_to_a_free_name() {
        let folder = folder("free");
        let (source, target) = (folder.join("a.txt"), folder.join("b.txt"));
        std::fs::write(&source, "a").expect("the file can be written");

        move_no_replace(&source, &target).expect("the rename goes through");

        assert!(!source.exists(), "the source is gone");
        assert_eq!(std::fs::read_to_string(&target).expect("the target is there"), "a");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_file_the_target_names_already_is_never_replaced() {
        let folder = folder("taken");
        let (source, target) = (folder.join("a.txt"), folder.join("b.txt"));
        std::fs::write(&source, "a").expect("the file can be written");
        std::fs::write(&target, "b").expect("the file can be written");

        let moved = move_no_replace(&source, &target);

        assert_eq!(moved.map_err(|error| error.kind()), Err(io::ErrorKind::AlreadyExists));
        assert_eq!(std::fs::read_to_string(&source).expect("the source is there"), "a");
        assert_eq!(std::fs::read_to_string(&target).expect("the target is there"), "b");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_path_holding_a_nul_is_refused() {
        let moved = move_no_replace(Path::new("a\0b"), Path::new("c"));

        assert_eq!(moved.map_err(|error| error.kind()), Err(io::ErrorKind::InvalidInput));
    }
}
